# Unsafe Code Ledger

Normative policy: `ADR-002` ([01-DECISIONS.md](01-DECISIONS.md)), `ARC-003` ([01-architecture.md](../spec/01-architecture.md)), release gate `G-5` ([03-VERIFICATION.md](03-VERIFICATION.md) §9).

## Policy

1. **Workspace-wide prohibition (`ADR-002`, `ARC-003`):** Every crate in the GAP workspace carries `#![forbid(unsafe_code)]` at crate root, with the **sole permitted exception** of `gap-s1` (isolated memory-mapping of `safetensors` model weights, implemented in `WP-M5-02`).
2. **Mandatory inline justification:** Every `unsafe` block inside `gap-s1` MUST be minimized to the smallest enclosing expression, wrapped in a safe abstraction, and preceded by a `// SAFETY:` comment stating the exact preconditions required by the callee and how those invariants are guaranteed at call time.
3. **Mandatory verification (`VER-020`, `G-5`):** Any code path crossing an `unsafe` block MUST be covered by unit/golden tests and clean sanitizer runs (`ASan`/`TSan` in CI per [03-VERIFICATION.md](03-VERIFICATION.md) §1 & §8).
4. **Same-change-set ledger update:** No PR introducing, modifying, or removing an `unsafe` block may merge without updating the ledger table below in the same change set.

## Active `unsafe` Blocks

| ID | Crate & Location | Operation | Safety Invariants Upheld (`// SAFETY:`) | Verification | Work Package |
|---|---|---|---|---|---|
| `UNS-001` | `crates/gap-s1/src/candle/mmap.rs` (`extern "C"` block) | POSIX `mmap` / `munmap` FFI declarations | ABI matches IEEE Std 1003.1 (`c_void`, `usize`, `c_int`, `RawFd`, `off_t = i64` on 64-bit Unix); symbols provided by linked system libc/`libSystem` | `cargo nextest run -p gap-s1 --release -E 'test(golden)'`, `gap-s1` unit tests | `WP-M5-02` |
| `UNS-002` | `crates/gap-s1/src/candle/mmap.rs` (`MmapBytes::open`) | Call `mmap(NULL, len, PROT_READ, MAP_PRIVATE, fd, 0)` | File is open regular file (`is_file()`), `0 < len <= isize::MAX`, `offset == 0`, return checked against `MAP_FAILED` (`usize::MAX as *mut c_void`) and `NULL` before constructing `NonNull<u8>` | `golden_safetensors_mmap_f32_and_f16_parity_over_210_cases`, `mmap::tests` | `WP-M5-02` |
| `UNS-003` | `crates/gap-s1/src/candle/mmap.rs` (`MmapBytes::as_slice`) | Construct `&[u8]` via `core::slice::from_raw_parts` | `ptr` is non-null from successful `PROT_READ | MAP_PRIVATE` `mmap` covering `len` initialized bytes (`len <= isize::MAX`), immutable `&self` lifetime prevents use-after-`munmap` | `golden_safetensors_mmap_f32_and_f16_parity_over_210_cases`, `mmap::tests` | `WP-M5-02` |
| `UNS-004` | `crates/gap-s1/src/candle/mmap.rs` (`<MmapBytes as Drop>::drop`) | Unmap region via `munmap(ptr, len)` | `ptr` and `len` originate exclusively from a live `UNS-002` mapping; `Drop` executes once after all `&[u8]` borrows end | `golden_safetensors_mmap_f32_and_f16_parity_over_210_cases`, `mmap::tests` | `WP-M5-02` |
| `UNS-005` | `crates/gap-s1/src/candle/mmap.rs` (`Send`/`Sync` impls for `MmapBytes`) | Implement `Send` and `Sync` for `MmapBytes` | Mapping is read-only (`PROT_READ | MAP_PRIVATE`) with no thread-local state or interior mutability; `&MmapBytes` exposes immutable `&[u8]` only and `Drop` runs on exclusive ownership | `mmap::tests::mmap_bytes_is_send_and_sync` | `WP-M5-02` |
