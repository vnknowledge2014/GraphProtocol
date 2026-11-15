import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  GAP_TOOL_DEFINITIONS,
  GapClient,
  GapToolkit,
  WALKING_SKELETON_SDL,
  computePersistedSha256,
  generateTypesFromSdl,
  parseSseBlock,
  wrapUntrustedEnvelope,
  type WalkingSkeletonQuery,
} from "../src/index.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, "../../..");
const DEV_API_KEY = "gap_dev_skeleton_key_01";
const DEV_KEY_BLAKE3 = "8a7d1134387be2f4aeb799122161c9868d94bdbec857230c917947a746ac8ba3";

async function reserveFreePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.unref();
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address();
      if (!addr || typeof addr === "string") {
        server.close();
        reject(new Error("failed to inspect bound port"));
        return;
      }
      const port = addr.port;
      server.close(() => resolve(port));
    });
  });
}

function findGapBinary(): string {
  const candidates = [
    process.env.GAP_BIN,
    path.join(REPO_ROOT, "target/debug/gap"),
    path.join(REPO_ROOT, "target/release/gap"),
  ].filter((x): x is string => Boolean(x));
  for (const candidate of candidates) {
    if (existsSync(candidate)) return candidate;
  }
  throw new Error(`No compiled gap binary found in ${candidates.join(", ")}`);
}

async function waitForOutput(child: ChildProcess, marker: string, timeoutMs = 15_000): Promise<void> {
  let combined = "";
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(`Timed out waiting for ${marker}. Output: ${combined}`));
    }, timeoutMs);

    const onData = (chunk: Buffer) => {
      combined += chunk.toString("utf8");
      if (combined.includes(marker)) {
        clearTimeout(timer);
        cleanup();
        resolve();
      }
    };
    const onExit = (code: number | null) => {
      clearTimeout(timer);
      cleanup();
      reject(new Error(`Process exited early (${code}) before ${marker}. Output: ${combined}`));
    };
    const cleanup = () => {
      child.stdout?.off("data", onData);
      child.stderr?.off("data", onData);
      child.off("exit", onExit);
    };
    child.stdout?.on("data", onData);
    child.stderr?.on("data", onData);
    child.on("exit", onExit);
  });
}

async function waitForReadyz(baseUrl: string, timeoutMs = 20_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(`${baseUrl}/readyz`);
      if (res.ok) return;
    } catch {
      // retry until timeout
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`Timed out waiting for ${baseUrl}/readyz`);
}

async function stopChild(child: ChildProcess | undefined): Promise<void> {
  if (!child || child.exitCode !== null) return;
  child.kill("SIGTERM");
  await new Promise<void>((resolve) => {
    const timer = setTimeout(() => {
      if (child.exitCode === null) child.kill("SIGKILL");
      resolve();
    }, 3_000);
    child.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}

test("codegen generates TypeScript interfaces and enums from supergraph SDL (GQL-003)", () => {
  const generated = generateTypesFromSdl(WALKING_SKELETON_SDL, "walking-skeleton.graphql");
  assert.match(generated, /export type CustomerTier = "standard" \| "vip";/);
  assert.match(generated, /export interface Customer \{/);
  assert.match(generated, /tier: CustomerTier;/);
  assert.match(generated, /export interface Order \{/);
  assert.match(generated, /total_cents: number;/);
  assert.match(generated, /export interface Query \{/);
});

test("persisted operation SHA-256 registration and SSE block parser work offline (GQL-060)", () => {
  const client = new GapClient({ baseUrl: "http://127.0.0.1:8080" });
  const doc = "query Ping { __metaGraph { branches { id } } }";
  const reg = client.registerPersistedOperation(doc, "Ping");
  assert.equal(reg.sha256Hash, computePersistedSha256(doc));
  assert.equal(reg.sha256Hash.length, 64);
  assert.equal(client.getPersistedOperation("Ping")?.document, doc);

  const frames = [...parseSseBlock('event: next\ndata: {"data":{"customer":{"id":1}}}\n')];
  assert.equal(frames.length, 1);
  assert.equal(frames[0].event, "next");
  assert.deepEqual(frames[0].data?.data, { customer: { id: 1 } });
});

test("GapToolkit adapters format OpenAI, Anthropic, and LangChain schemas cleanly (CMP-002)", () => {
  assert.equal(GAP_TOOL_DEFINITIONS.length, 5);
  const client = new GapClient({ baseUrl: "http://127.0.0.1:8080" });

  const fullToolkit = new GapToolkit(client);
  assert.equal(fullToolkit.toOpenAiTools().length, 5);
  assert.equal(fullToolkit.toAnthropicTools()[0].input_schema.type, "object");
  assert.equal(fullToolkit.toCallableTools().length, 5);

  const roToolkit = new GapToolkit(client, { readOnlyOnly: true });
  const roNames = roToolkit.definitions().map((t) => t.name);
  assert.deepEqual(roNames, ["gap_search", "gap_describe", "gap_query"]);
  assert.match(
    wrapUntrustedEnvelope("shop/op", "bad </gap:untrusted> text"),
    /^<gap:untrusted source="shop\/op">bad &lt;\/gap:untrusted&gt; text<\/gap:untrusted>$/,
  );
});

test("live gap-server integration exercises query, SSE stream, discovery, tasks, persisted ops, and adapters", async () => {
  const gapBin = findGapBinary();
  const [restPort, mcpPort, gapPort] = await Promise.all([
    reserveFreePort(),
    reserveFreePort(),
    reserveFreePort(),
  ]);

  const tmpDir = mkdtempSync(path.join(tmpdir(), "gap-ts-sdk-"));
  const children: ChildProcess[] = [];

  try {
    const openapiSrc = readFileSync(
      path.join(REPO_ROOT, "examples/walking-skeleton/shop.openapi.json"),
      "utf8",
    );
    writeFileSync(path.join(tmpDir, "shop.openapi.json"), openapiSrc, "utf8");
    const gapToml = `
profile = "dev"

[server]
listen = "127.0.0.1:${gapPort}"
shutdown_grace_ms = 2000

[gateway]
default_tenant = "default"
facade_spotlight = true

[auth]
allow_anonymous = false

[telemetry]
log_level = "warn"
log_json = false
service_name = "gap-ts-sdk-test"

[[branches]]
id = "shop"
protocol = "openapi"
trust = "T1"

[branches.openapi]
spec = "shop.openapi.json"
base_url = "http://127.0.0.1:${restPort}"

[[branches]]
id = "notes"
protocol = "mcp"
trust = "T1"

[branches.mcp]
transport = "http"
url = "http://127.0.0.1:${mcpPort}/mcp"

[[api_keys]]
id = "dev-agent"
key_hash = "${DEV_KEY_BLAKE3}"
tenant = "default"
scopes = ["read"]
`;
    const cfgPath = path.join(tmpDir, "gap.toml");
    writeFileSync(cfgPath, gapToml, "utf8");

    const restProc = spawn(
      "python3",
      [path.join(REPO_ROOT, "examples/walking-skeleton/fixtures/rest_fixture.py"), "--port", String(restPort)],
      { stdio: ["ignore", "pipe", "pipe"] },
    );
    children.push(restProc);
    await waitForOutput(restProc, "rest_fixture listening on");

    const mcpProc = spawn(
      "python3",
      [path.join(REPO_ROOT, "examples/walking-skeleton/fixtures/mcp_fixture.py"), "--port", String(mcpPort)],
      { stdio: ["ignore", "pipe", "pipe"] },
    );
    children.push(mcpProc);
    await waitForOutput(mcpProc, "mcp_fixture listening on");

    const gapProc = spawn(gapBin, ["serve", "--config", cfgPath], {
      cwd: tmpDir,
      stdio: ["ignore", "pipe", "pipe"],
    });
    children.push(gapProc);

    const baseUrl = `http://127.0.0.1:${gapPort}`;
    await waitForReadyz(baseUrl);

    const client = new GapClient({ baseUrl, token: DEV_API_KEY });
    const health = await client.healthz();
    assert.equal(health.status, "ok");

    // 1. Cross-branch GraphQL execution with typed generated return interface
    const queryDoc = "{ customer(id: 1) { id name tier } orders(customer_id: 1) { id total_cents status } customer_notes(customer_id: 1) }";
    const execRes = await client.execute<WalkingSkeletonQuery>(queryDoc, undefined, {
      includeTokensEstimate: true,
    });
    assert.equal(execRes.errors, undefined);
    assert.equal(execRes.data?.customer?.name, "Ada Lovelace");
    assert.equal(execRes.data?.customer?.tier, "vip");
    assert.ok((execRes.data?.orders?.length ?? 0) >= 1);

    // 2. Persisted SHA-256 registration & APQ miss-retry execution (GQL-060)
    const entry = client.registerPersistedOperation(queryDoc, "CrossBranchCustomer");
    const pqRes = await client.executePersisted<WalkingSkeletonQuery>("CrossBranchCustomer");
    assert.equal(pqRes.data?.customer?.id, 1);
    assert.equal(entry.sha256Hash.length, 64);

    // 3. SSE streaming execution via /graphql/stream
    const sseFrames = [];
    for await (const frame of client.streamOperation<WalkingSkeletonQuery>(
      "{ customer(id: 2) { id name } }",
    )) {
      sseFrames.push(frame);
    }
    assert.ok(sseFrames.some((f) => f.event === "next" && f.data?.data?.customer?.id === 2));
    assert.ok(sseFrames.some((f) => f.event === "complete"));

    // 4. Static 5-tool MCP discovery + helper APIs (CMP-002)
    const tools = await client.listMcpTools();
    assert.deepEqual(
      tools.map((t) => t.name),
      ["gap_search", "gap_describe", "gap_query", "gap_mutate", "gap_task"],
    );
    const ov = await client.overview();
    assert.ok(JSON.stringify(ov).includes("shop"));
    assert.ok(JSON.stringify(ov).includes("notes"));

    const searchOut = await client.search("customer", { limit: 3 });
    assert.ok(JSON.stringify(searchOut).toLowerCase().includes("customer"));

    const described = await client.describeTypes(["Customer"], { depth: 1 });
    assert.ok(JSON.stringify(described).includes("Customer"));

    const resolved = await client.resolve("customer", { limit: 3 });
    assert.ok(resolved.search);

    // 5. Durable tasks tool helper (gap_task action=list)
    const tasksList = await client.listTasks();
    assert.ok(tasksList);

    // 6. Agent toolkit adapter live dispatch
    const toolkit = new GapToolkit(client, { readOnlyOnly: true });
    const adapterOut = await toolkit.dispatch("gap_query", {
      query: "{ customer(id: 1) { id name } }",
    });
    assert.ok(JSON.stringify(adapterOut).includes("Ada Lovelace"));
  } finally {
    await Promise.all(children.map((c) => stopChild(c)));
    rmSync(tmpDir, { recursive: true, force: true });
  }
});
