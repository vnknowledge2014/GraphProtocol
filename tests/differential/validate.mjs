// Validates tests/differential/corpus.json with graphql-js and prints {id: {verdict, message}} as JSON.
// Usage: GAP_GRAPHQL_JS_DIR=/path/to/node_modules/graphql node tests/differential/validate.mjs
// verdict: "valid" | "invalid" (parsed, failed validation) | "syntax" (failed to parse)
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dir = process.env.GAP_GRAPHQL_JS_DIR;
if (!dir) {
  console.error("GAP_GRAPHQL_JS_DIR must point at an installed graphql-js 16 package directory");
  process.exit(2);
}
const { buildSchema, parse, validate, version } = await import(pathToFileURL(join(dir, "index.mjs")).href);
const corpus = JSON.parse(readFileSync(join(here, "corpus.json"), "utf8"));
const schema = buildSchema(readFileSync(join(here, corpus.schema), "utf8"));
const out = { __graphql_js_version: version };
for (const { id, doc } of corpus.documents) {
  let ast;
  try {
    ast = parse(doc);
  } catch (e) {
    out[id] = { verdict: "syntax", message: String(e.message).slice(0, 200) };
    continue;
  }
  const errors = validate(schema, ast);
  out[id] = errors.length === 0 ? { verdict: "valid" } : { verdict: "invalid", message: errors[0].message.slice(0, 200) };
}
process.stdout.write(JSON.stringify(out));
