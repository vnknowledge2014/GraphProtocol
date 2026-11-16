# GAP Controlled Vocabulary `[REFERENCE]`

## 1. Terminology Discipline `[DESCRIPTIVE]`

Controlled vocabulary eliminates ambiguity for human operators and language models. Do not swap synonyms for a technical object across paragraphs. Always use the approved term in column 1 below.

## 2. Approved GAP Nouns `[REFERENCE]`

| Approved Term | Meaning in GAP | Prohibited Synonyms |
|---|---|---|
| `gateway` | The GAP server process listening on HTTP or `stdio` | proxy, router middleware, hub |
| `branch` | One registered downstream API, store, MCP server, CLI, or codebase | backend service, plugin, endpoint group |
| `connector` | A `NGC` adapter translating one protocol into GAP frames | bridge, driver, integration module |
| `snapshot` | An immutable compiled graph, routing index, and policy bundle (`INV-01`) | live config, state map, cache bundle |
| `generation` | A numbered monotonic `snapshot` version (`u64`) | release number, revision tag, epoch |
| `principal` | An authenticated human, service account, or AI agent identity | user entity, caller profile, client |
| `tenant` | An isolated customer or workspace scope inside every key (`INV-09`) | org bucket, namespace, account slice |
| `proposal` | A staged schema or join change waiting for administrator review | draft update, candidate patch |
| `task` | A durable asynchronous A2A workflow unit (`RUN-091`) | background job, worker thread |
| `facade` | The fixed five-tool MCP `2026-07-28` interface (`CMP-008`) | adapter layer, shim |
| `untrusted envelope` | The `<gap:untrusted>` XML boundary around `T1`/`T2` outputs (`SEC-080`) | safety wrapper, sanitized tag |
| `audit chain` | A tamper-evident SQLite log linked by sequential BLAKE3 hashes (`INV-10`) | event trail, journal |
| `visibility bitset` | A precomputed bitmask hiding unauthorized schema items (`INV-05`) | permission mask, access filter |

## 3. Approved Procedural Verbs `[REFERENCE]`

| Approved Verb | Approved Meaning |
|---|---|
| `Build` | Compile workspace Rust crates or container images |
| `Check` | Inspect configuration files, references, or rules without mutating state |
| `Start` | Launch a foreground process or upstream fixture |
| `Stop` | Send a termination signal to a running process |
| `Send` | Transmit an HTTP, JSON-RPC, or GraphQL payload |
| `Verify` | Confirm cryptographic hashes, chains, schemas, or test results |
| `Approve` | Promote a staged `proposal` or Cedar change |
| `Reject` | Deny and close a staged `proposal` |
| `Roll back` | Revert the active `snapshot` to a prior `generation` |
| `Restore` | Recover durable SQLite files from an online backup |

## 4. Prohibited AI-Slop Terms `[REFERENCE]`

| Do Not Write | Write Instead |
|---|---|
| *utilize*, *leverage* | `use` |
| *facilitate* | `enable`, `help`, or an active verb |
| *commence* | `start` |
| *prior to* | `before` |
| *subsequent to* | `after` |
| *in order to* | `to` |
| *ensure* | `make sure` or `verify` |
| *seamless*, *robust*, *holistic*, *cutting-edge* | State the exact measured property or number |
