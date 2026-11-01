# gap-transport pieces `WP-M1-12` builds on (done by the reviewer on 2026-11-01, branch `wp/WP-M1-12-transport`)

`WP-M1-12` (gap-server and gap-cli) needs three things from `gap-transport` that no work package owned; they exist now so that `gap-server` only wires.

| Piece | API | What it guarantees |
|---|---|---|
| Engine | `gap_transport::TransportEngine::new(Arc<AppState>)`, implements `gap_core::traits::GraphqlEngine` | runs a `GraphqlRequest` through `Operation -> Authz -> Plan -> Exec` under the caller's `RequestContext` (the snapshot the context pinned is the one answered from: INV-01); `extensions.gap.deadlineMs` is clamped to the time left on `ctx.deadline`; dropping the future cancels connector calls; request errors come back as `data: None` + `errors`, execution errors as `data`/`errors`/`extensions.gap.{generation,cost,partial}` |
| Mounting | `AppState::with_http_module(Arc<dyn HttpModule>)` | a request whose path equals or lies below a prefix the module declares (`mount_prefixes`) is authenticated (`401` + `WWW-Authenticate: Bearer`), admission-checked, pinned to one snapshot, body-bounded (declared `Content-Length` first), given a `RequestContext` (tenant and principal only from authentication, `Budget::new(limits.max_cost_units)`, deadline `min(default_deadline, max_deadline)`) and a credential-free `HttpExchangeRequest` (`authorization`, `x-api-key`, `cookie`, `proxy-authorization` stripped: SEC-011); module errors map to statuses (`gap_transport::modules::status_for_error`); one `route="module"` metric label |
| Listener | `gap_transport::serve(listener, router, shutdown_future, ServeOptions)` | header-read timeout (default 10 s), connection cap (default 10,000: new connections beyond it are closed at once), bounded graceful drain (`shutdown_grace`, default 30 s), listener closed the moment shutdown starts; replaces `axum::serve`, which held half-sent requests forever |

Not done (left to `WP-M1-12` or later): OAuth/JWT and the `401 WWW-Authenticate ... resource_metadata` challenge of CMP-006 (the dev profile and API keys only), TLS, the admin listener, gRPC, signal handling (`gap-server` turns SIGTERM/SIGINT into the `shutdown` future and sets `TransportConfig::draining` first so `/readyz` flips to `503`: RUN-090).

Evidence: `cargo nextest run --workspace` 623/623 (the 6 socket tests of `tests/serve_sockets.rs` run with `-- --include-ignored`), 17 reviewer mutants (15 killed by named tests, `M07` double pin and `E02` redundant cancel token are equivalent: dropping the future already cancels), clean-room offline build.
