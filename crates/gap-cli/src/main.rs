//! `gap` command-line interface (`serve`, `init`, `doctor`, `proposals`, `calibrate`, `audit`) (`L5`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod calibrate;
pub mod commands;
pub mod proposals;

fn main() {}
