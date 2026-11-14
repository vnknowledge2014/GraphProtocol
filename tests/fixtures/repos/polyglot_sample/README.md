# Polyglot Authentication Sample Repository

Central overview of the polyglot token validation pipeline used to verify GAP code and document graph indexing.

## Architecture Overview

Inbound traffic enters through `RequestRouter` in [src/router.go](src/router.go), which verifies requests via `validate_token` defined in [src/lib.rs](src/lib.rs).
Background jobs run inside [src/worker.py](src/worker.py), browser SDK calls live in [src/client.ts](src/client.ts), and Java verification sits in [src/Validator.java](src/Validator.java).

For caching design trade-offs and TTL invalidation rules, read [ADR-001 Token Caching](docs/adr-001-caching.md).
