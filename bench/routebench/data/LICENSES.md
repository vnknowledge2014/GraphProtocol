# RouteBench v1 Dataset Licenses and Provenance

All specifications in `bench/routebench/data/` are checked into the repository for offline, deterministic evaluation under `ADR-007` and `RTE-060`.

| File | Domain | Source / Derivation | License |
|---|---|---|---|
| `walking_skeleton.json` | Shop (`customers`, `orders`) & Notes (`customer_notes`) | Derived directly from `examples/walking-skeleton/shop.openapi.json` and `crates/gap-server/tests/e2e.rs` | Apache-2.0 |
| `petstore_v3.openapi.json` | Pet Store catalog, inventory, and store orders | Canonical OpenAPI 3.0 Petstore v3 benchmark fixture authored cleanly for GAP RouteBench from public OpenAPI 3.0 Petstore conventions | Apache-2.0 |
| `github_issues.openapi.json` | Repositories, issues, comments, and pull requests | Benchmark OpenAPI 3.0 fixture modeled on public REST repository issue-tracking endpoints | MIT |
| `stripe_billing.openapi.json` | Billing customers, invoices, and subscriptions | Benchmark OpenAPI 3.0 fixture modeled on public REST billing subscription endpoints | MIT |
