# Unsafe Code Ledger

Normative policy: `ADR-002` ([01-DECISIONS.md](01-DECISIONS.md)), `ARC-003` ([01-architecture.md](../spec/01-architecture.md)), release gate `G-5` ([03-VERIFICATION.md](03-VERIFICATION.md) §9).

## Policy

1. **Workspace-wide prohibition (`ADR-002`, `ARC-003`):** Every crate in the GAP workspace carries `#![forbid(unsafe_code)]` at crate root, with the **sole permitted exception** of `gap-s1` (isolated memory-mapping of `safetensors` model weights, planned in `WP-M5-02`).
2. **Mandatory inline justification:** Every `unsafe` block inside `gap-s1` MUST be minimized to the smallest enclosing expression, wrapped in a safe abstraction, and preceded by a `// SAFETY:` comment stating the exact preconditions required by the callee and how those invariants are guaranteed at call time.
3. **Mandatory verification (`VER-020`, `G-5`):** Any code path crossing an `unsafe` block MUST be covered by unit/golden tests and clean sanitizer runs (`ASan`/`TSan` in CI per [03-VERIFICATION.md](03-VERIFICATION.md) §1 & §8).
4. **Same-change-set ledger update:** No PR introducing, modifying, or removing an `unsafe` block may merge without updating the ledger table below in the same change set.

## Active `unsafe` Blocks

*(Empty ledger — 0 `unsafe` blocks currently exist in the repository.)*

| ID | Crate & Location | Operation | Safety Invariants Upheld (`// SAFETY:`) | Verification | Work Package |
|---|---|---|---|---|---|
