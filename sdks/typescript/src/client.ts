import { createHash } from "node:crypto";
import {
  GAP_MCP_PROTOCOL_VERSION,
  GapSdkError,
  type ExecuteOptions,
  type GapClientOptions,
  type GapDescribeTypesOptions,
  type GapSearchOptions,
  type GapTaskArgs,
  type GraphqlResponse,
  type McpToolCallResult,
  type McpToolDefinition,
  type PersistedOperationEntry,
  type SseEventFrame,
  type TaskDecision,
  type TaskRecord,
} from "./types.ts";

const TERMINAL_TASK_STATES = new Set(["completed", "failed", "canceled", "rejected"]);

export function computePersistedSha256(document: string): string {
  return createHash("sha256").update(document, "utf8").digest("hex");
}

export function* parseSseBlock(rawBlock: string): Generator<SseEventFrame, void, void> {
  const trimmed = rawBlock.trim();
  if (!trimmed) return;
  let eventName = "message";
  const dataLines: string[] = [];
  for (const line of trimmed.split(/\r?\n/)) {
    if (line.startsWith(":")) continue;
    const colon = line.indexOf(":");
    const field = colon === -1 ? line : line.slice(0, colon);
    let value = colon === -1 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    if (field === "event") eventName = value;
    else if (field === "data") dataLines.push(value);
  }
  const rawData = dataLines.join("\n");
  let parsed: GraphqlResponse | null | undefined = undefined;
  if (rawData.length > 0) {
    try {
      parsed = JSON.parse(rawData) as GraphqlResponse;
    } catch {
      parsed = null;
    }
  }
  yield { event: eventName, data: parsed, rawData };
}

export class GapClient {
  readonly baseUrl: string;
  private readonly token?: string;
  private readonly protocolVersion: string;
  private readonly defaultHeaders: Record<string, string>;
  private readonly fetchImpl: typeof fetch;
  private readonly persistedByHash = new Map<string, PersistedOperationEntry>();
  private readonly persistedByName = new Map<string, PersistedOperationEntry>();
  private rpcCounter = 0;

  constructor(options: GapClientOptions) {
    this.baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.token = options.token;
    this.protocolVersion = options.protocolVersion ?? GAP_MCP_PROTOCOL_VERSION;
    this.defaultHeaders = { ...(options.defaultHeaders ?? {}) };
    this.fetchImpl = options.fetchImpl ?? globalThis.fetch;
  }

  private buildHeaders(extra?: Record<string, string>): Record<string, string> {
    const headers: Record<string, string> = {
      ...this.defaultHeaders,
      ...(extra ?? {}),
    };
    if (this.token && !headers.authorization && !headers.Authorization) {
      headers.Authorization = `Bearer ${this.token}`;
    }
    return headers;
  }

  private nextRpcId(): number {
    this.rpcCounter += 1;
    return this.rpcCounter;
  }

  async healthz(signal?: AbortSignal): Promise<{ status: string }> {
    const res = await this.fetchImpl(`${this.baseUrl}/healthz`, {
      headers: this.buildHeaders(),
      signal,
    });
    if (!res.ok) {
      throw new GapSdkError(`healthz returned HTTP ${res.status}`, { status: res.status });
    }
    return (await res.json()) as { status: string };
  }

  async readyz(signal?: AbortSignal): Promise<{ status: string; generation?: number }> {
    const res = await this.fetchImpl(`${this.baseUrl}/readyz`, {
      headers: this.buildHeaders(),
      signal,
    });
    if (!res.ok) {
      throw new GapSdkError(`readyz returned HTTP ${res.status}`, { status: res.status });
    }
    return (await res.json()) as { status: string; generation?: number };
  }

  async execute<
    TData = Record<string, unknown>,
    TVariables extends Record<string, unknown> = Record<string, unknown>,
  >(
    query: string,
    variables?: TVariables,
    options?: ExecuteOptions,
  ): Promise<GraphqlResponse<TData>> {
    return this.postGraphql<TData>({
      query,
      variables,
      operationName: options?.operationName,
      extensions: this.buildGapExtensions(options),
    }, options);
  }

  registerPersistedOperation(document: string, name?: string): PersistedOperationEntry {
    const sha256Hash = computePersistedSha256(document);
    const entry: PersistedOperationEntry = { name, sha256Hash, document };
    this.persistedByHash.set(sha256Hash, entry);
    if (name) {
      this.persistedByName.set(name, entry);
    }
    return entry;
  }

  getPersistedOperation(nameOrHash: string): PersistedOperationEntry | undefined {
    return this.persistedByHash.get(nameOrHash) ?? this.persistedByName.get(nameOrHash);
  }

  async executePersisted<
    TData = Record<string, unknown>,
    TVariables extends Record<string, unknown> = Record<string, unknown>,
  >(
    nameOrSha256: string,
    variables?: TVariables,
    options?: ExecuteOptions & { includeQueryOnMiss?: boolean },
  ): Promise<GraphqlResponse<TData>> {
    const local = this.getPersistedOperation(nameOrSha256);
    const sha256Hash = local?.sha256Hash ?? nameOrSha256;
    const baseExt = this.buildGapExtensions(options) ?? {};
    const ext = {
      ...baseExt,
      persistedQuery: { version: 1 as const, sha256Hash },
    };

    const firstTry = await this.postGraphql<TData>(
      {
        variables,
        operationName: options?.operationName,
        extensions: ext,
      },
      options,
      { allowErrorStatus: Boolean(local && (options?.includeQueryOnMiss ?? true)) },
    );

    if (
      firstTry.errors?.length &&
      local &&
      (options?.includeQueryOnMiss ?? true)
    ) {
      return this.postGraphql<TData>(
        {
          query: local.document,
          variables,
          operationName: options?.operationName,
          extensions: ext,
        },
        options,
      );
    }
    return firstTry;
  }

  async *streamOperation<
    TData = Record<string, unknown>,
    TVariables extends Record<string, unknown> = Record<string, unknown>,
  >(
    query: string,
    variables?: TVariables,
    options?: ExecuteOptions,
  ): AsyncGenerator<SseEventFrame<TData>, void, void> {
    const res = await this.fetchImpl(`${this.baseUrl}/graphql/stream`, {
      method: "POST",
      headers: this.buildHeaders({
        "Content-Type": "application/json",
        Accept: "text/event-stream",
        ...(options?.headers ?? {}),
      }),
      body: JSON.stringify({
        query,
        variables,
        operationName: options?.operationName,
        extensions: this.buildGapExtensions(options),
      }),
      signal: options?.signal,
    });
    if (!res.ok) {
      const body = await res.text();
      throw new GapSdkError(`streamOperation HTTP ${res.status}: ${body}`, {
        status: res.status,
      });
    }
    if (!res.body) {
      throw new GapSdkError("SSE response body missing");
    }

    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";

    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true }).replace(/\r\n/g, "\n");
      let sep = buffer.indexOf("\n\n");
      while (sep !== -1) {
        const block = buffer.slice(0, sep);
        buffer = buffer.slice(sep + 2);
        for (const frame of parseSseBlock(block)) {
          yield frame as SseEventFrame<TData>;
        }
        sep = buffer.indexOf("\n\n");
      }
    }
    buffer += decoder.decode();
    if (buffer.trim()) {
      for (const frame of parseSseBlock(buffer)) {
        yield frame as SseEventFrame<TData>;
      }
    }
  }

  async listMcpTools(signal?: AbortSignal): Promise<McpToolDefinition[]> {
    const result = await this.postMcp<{ tools: McpToolDefinition[] }>(
      "tools/list",
      {
        _meta: {
          "io.modelcontextprotocol/protocolVersion": this.protocolVersion,
          "io.modelcontextprotocol/clientCapabilities": {},
        },
      },
      undefined,
      signal,
    );
    return result.tools ?? [];
  }

  async callMcpTool<TStructured = Record<string, unknown>>(
    name: string,
    args: Record<string, unknown>,
    signal?: AbortSignal,
  ): Promise<McpToolCallResult<TStructured>> {
    return this.postMcp<McpToolCallResult<TStructured>>(
      "tools/call",
      {
        _meta: {
          "io.modelcontextprotocol/protocolVersion": this.protocolVersion,
          "io.modelcontextprotocol/clientCapabilities": {},
        },
        name,
        arguments: args,
      },
      name,
      signal,
    );
  }

  async overview<T = Record<string, unknown>>(signal?: AbortSignal): Promise<T> {
    const res = await this.callMcpTool<T>("gap_describe", { overview: true }, signal);
    return (res.structuredContent ?? res) as T;
  }

  async describeTypes<T = Record<string, unknown>>(
    types: string[],
    options?: GapDescribeTypesOptions,
  ): Promise<T> {
    const args: Record<string, unknown> = { types };
    if (options?.depth !== undefined) args.depth = options.depth;
    const res = await this.callMcpTool<T>("gap_describe", args, options?.signal);
    return (res.structuredContent ?? res) as T;
  }

  async listOperations<T = Record<string, unknown>>(signal?: AbortSignal): Promise<T> {
    const res = await this.callMcpTool<T>("gap_describe", { operations: true }, signal);
    return (res.structuredContent ?? res) as T;
  }

  async search<T = Record<string, unknown>>(
    query: string,
    options?: GapSearchOptions,
  ): Promise<T> {
    const args: Record<string, unknown> = { query };
    if (options?.kinds) args.kinds = options.kinds;
    if (options?.limit !== undefined) args.limit = options.limit;
    if (options?.budgetTokens !== undefined) args.budget_tokens = options.budgetTokens;
    const res = await this.callMcpTool<T>("gap_search", args, options?.signal);
    return (res.structuredContent ?? res) as T;
  }

  async resolve(
    intent: string,
    options?: { limit?: number; depth?: number; signal?: AbortSignal },
  ): Promise<{ search: Record<string, unknown>; described?: Record<string, unknown> }> {
    const searchRes = await this.search(intent, {
      limit: options?.limit ?? 5,
      signal: options?.signal,
    });
    const hits = extractCandidateTypeNames(searchRes);
    if (hits.length === 0) {
      return { search: searchRes };
    }
    const described = await this.describeTypes(hits.slice(0, options?.limit ?? 5), {
      depth: options?.depth ?? 1,
      signal: options?.signal,
    });
    return { search: searchRes, described };
  }

  async task<T = Record<string, unknown>>(args: GapTaskArgs, signal?: AbortSignal): Promise<T> {
    const res = await this.callMcpTool<T>("gap_task", { ...args }, signal);
    return (res.structuredContent ?? res) as T;
  }

  async getTask(id: string, signal?: AbortSignal): Promise<TaskRecord> {
    return this.task<TaskRecord>({ action: "get", id }, signal);
  }

  async listTasks(signal?: AbortSignal): Promise<Record<string, unknown>> {
    return this.task({ action: "list" }, signal);
  }

  async cancelTask(id: string, signal?: AbortSignal): Promise<TaskRecord> {
    return this.task<TaskRecord>({ action: "cancel", id }, signal);
  }

  async decideTask(
    id: string,
    decision: TaskDecision,
    note?: string,
    signal?: AbortSignal,
  ): Promise<TaskRecord> {
    return this.task<TaskRecord>({ action: "decide", id, decision, note }, signal);
  }

  async pollTask(
    id: string,
    options?: { intervalMs?: number; timeoutMs?: number; signal?: AbortSignal },
  ): Promise<TaskRecord> {
    const intervalMs = options?.intervalMs ?? 200;
    const timeoutMs = options?.timeoutMs ?? 30_000;
    const started = Date.now();
    while (true) {
      const current = await this.getTask(id, options?.signal);
      const state = extractTaskState(current);
      if (state && TERMINAL_TASK_STATES.has(state.toLowerCase())) {
        return current;
      }
      if (Date.now() - started >= timeoutMs) {
        throw new GapSdkError(`Timed out polling task ${id} after ${timeoutMs}ms`, {
          code: "GAP_TASK_POLL_TIMEOUT",
          details: current,
        });
      }
      await new Promise((resolve) => setTimeout(resolve, intervalMs));
    }
  }

  private buildGapExtensions(options?: ExecuteOptions): Record<string, unknown> | undefined {
    const gap: Record<string, unknown> = {};
    if (options?.deadlineMs !== undefined) gap.deadlineMs = options.deadlineMs;
    if (options?.budget !== undefined) gap.budget = options.budget;
    if (options?.includeTokensEstimate !== undefined) {
      gap.includeTokensEstimate = options.includeTokensEstimate;
    }
    if (Object.keys(gap).length === 0) return undefined;
    return { gap };
  }

  private async postGraphql<TData>(
    payload: Record<string, unknown>,
    options?: ExecuteOptions,
    opts?: { allowErrorStatus?: boolean },
  ): Promise<GraphqlResponse<TData>> {
    const res = await this.fetchImpl(`${this.baseUrl}/graphql`, {
      method: "POST",
      headers: this.buildHeaders({
        "Content-Type": "application/json",
        Accept: "application/graphql-response+json, application/json",
        ...(options?.headers ?? {}),
      }),
      body: JSON.stringify(payload),
      signal: options?.signal,
    });
    const text = await res.text();
    let parsed: GraphqlResponse<TData>;
    try {
      parsed = JSON.parse(text) as GraphqlResponse<TData>;
    } catch {
      throw new GapSdkError(`Invalid GraphQL JSON response (HTTP ${res.status}): ${text}`, {
        status: res.status,
      });
    }
    if (!res.ok && !opts?.allowErrorStatus && !parsed.errors?.length) {
      throw new GapSdkError(`GraphQL request failed with HTTP ${res.status}`, {
        status: res.status,
        details: parsed,
      });
    }
    return parsed;
  }

  private async postMcp<TResult>(
    method: string,
    params: Record<string, unknown>,
    toolName?: string,
    signal?: AbortSignal,
  ): Promise<TResult> {
    const headers: Record<string, string> = {
      "Content-Type": "application/json",
      Accept: "application/json, text/event-stream",
      "MCP-Protocol-Version": this.protocolVersion,
      "Mcp-Method": method,
    };
    if (toolName) headers["Mcp-Name"] = toolName;

    const res = await this.fetchImpl(`${this.baseUrl}/mcp`, {
      method: "POST",
      headers: this.buildHeaders(headers),
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: this.nextRpcId(),
        method,
        params,
      }),
      signal,
    });
    const text = await res.text();
    let envelope: { result?: TResult; error?: { code?: number; message?: string; data?: unknown } };
    try {
      envelope = JSON.parse(text);
    } catch {
      throw new GapSdkError(`Invalid MCP JSON response (HTTP ${res.status}): ${text}`, {
        status: res.status,
      });
    }
    if (!res.ok || envelope.error) {
      throw new GapSdkError(
        envelope.error?.message ?? `MCP call ${method} failed with HTTP ${res.status}`,
        {
          status: res.status,
          code: envelope.error?.code !== undefined ? String(envelope.error.code) : undefined,
          details: envelope.error ?? envelope,
        },
      );
    }
    if (envelope.result === undefined) {
      throw new GapSdkError(`MCP response missing result for ${method}`, { status: res.status });
    }
    return envelope.result;
  }
}

function extractTaskState(record: Record<string, unknown>): string | undefined {
  if (typeof record.state === "string") return record.state;
  const data = record.data as Record<string, unknown> | undefined;
  if (data && typeof data.state === "string") return data.state;
  const task = data?.task as Record<string, unknown> | undefined;
  if (task && typeof task.state === "string") return task.state;
  return undefined;
}

function extractCandidateTypeNames(searchPayload: Record<string, unknown>): string[] {
  const names = new Set<string>();
  const data = (searchPayload.data ?? searchPayload) as Record<string, unknown>;
  const candidates = (data.hits ?? data.results ?? []) as Array<Record<string, unknown>>;
  if (!Array.isArray(candidates)) return [];
  for (const hit of candidates) {
    if (typeof hit.typeName === "string") names.add(hit.typeName);
    if (typeof hit.type === "string" && hit.type !== "Query" && hit.type !== "Mutation") {
      names.add(hit.type);
    }
  }
  return [...names];
}
