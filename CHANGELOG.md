# Changelog

All notable changes to GAP (Graph Agent Protocol) are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
generated and curated from Conventional Commits (`OPS-040`), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) (`ADR-024`).
Before `1.0.0`, breaking wire or API changes occur only in minor versions and
must include a migration note here alongside the corresponding conformance diff.

## [Unreleased]

### Added

- Normative GAP `0.1.0-draft` specification (`spec/00`–`spec/09`), wire contracts (`proto/`), architecture decision records (`docs/01-DECISIONS.md`), verification gates (`docs/03-VERIFICATION.md`), and 85-package execution plan (`work/work-packages.toml`).
- Repository bootstrap (`WP-M0-01`): `.gitignore`, `.editorconfig`, pinned Rust `1.98.1` toolchain (`rust-toolchain.toml`), `rustfmt.toml`, `clippy.toml` (`ARC-003`), Apache-2.0 `LICENSE`, `NOTICE`, `CONTRIBUTING.md`, `SECURITY.md`, and this changelog skeleton.
