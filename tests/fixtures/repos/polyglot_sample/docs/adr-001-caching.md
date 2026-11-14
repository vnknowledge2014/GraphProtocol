# ADR-001: Bounded Token Cache Validation

Supersedes: [README.md](../README.md)

We keep bounded in-memory validation caches synchronized across workers so repeated token verification stays sub-millisecond.

## Decision

The primary store `TokenCache` implements `Validator` and exposes `validate_token` for low-latency signature and revocation checks.

## Consequences

Every downstream dispatcher (`Dispatch` and `fetchProfile`) delegates validation to `validate_token` instead of duplicating cryptographic checks.
