# Changelog

All notable changes to `gap-proto` wire descriptors and generated bindings are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Per `ADR-024`, breaking wire changes before `1.0` require a minor version bump (`0.x -> 0.(x+1)`)
and must update the corresponding golden SHA-256 file in `crates/gap-proto/golden/` together with
an entry in this changelog citing at least the first 16 hex characters of the new digest.

## [0.1.0] - 2026-10-28

### Added

- Initial `gap.gateway.v1` and `gap.ngc.v1` (`NGC-001`, `NGC-004`, wire version `gap/0.1`) Protobuf/gRPC bindings compiled purely in Rust via `protox` + `tonic-prost-build` (`ADR-002`); `DESCRIPTOR_SET` SHA-256 prefix `bc089113d573086d` (`golden/descriptor.sha256`).
- Optional `lf.a2a.v1` (`CMP-020`, `CMP-021`, `CMP-022`) Protobuf/gRPC bindings behind `feature = "a2a"` compiled from vendored `a2a-v1.0.1` + `googleapis`; `A2A_DESCRIPTOR_SET` SHA-256 prefix `3d0915507b124bae` (`golden/descriptor-a2a.sha256`).
