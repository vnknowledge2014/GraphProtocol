//! `gap` command-line interface library (`L6`, `OPS-044`, `OPS-001`, `Brief §3.4`).
//!
//! Exposes the `clap`-derived CLI and in-process execution entry points (`cli::run_with_io`) so
//! every subcommand (`serve`, `init`, `doctor`, `config {schema,check}`, and the `OPS-044` later-WP
//! stubs) can be tested deterministically without spawning child processes.
#![forbid(unsafe_code)]
#![deny(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod calibrate;
pub mod cli;
pub mod commands;
pub mod doctor;
pub mod init;
pub mod proposals;
