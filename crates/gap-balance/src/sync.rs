//! Concurrency primitives shim switching between `std` and `shuttle` under `cfg(gap_shuttle)`.
//!
//! Every hot-path atomic (`DecayedEwma`, `TelemetrySlot::in_flight`, `TelemetrySlot::capacity`,
//! `Breaker`, bucket words, `RetryBudget`) and cold-path lock goes through this module so
//! `shuttle` schedules can deterministically interleave the real implementation (`RTE-085`).

#![allow(unused_imports)]

#[cfg(not(gap_shuttle))]
pub(crate) use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};
#[cfg(not(gap_shuttle))]
pub(crate) use std::thread;

#[cfg(gap_shuttle)]
pub(crate) use shuttle::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};
#[cfg(gap_shuttle)]
pub(crate) use shuttle::thread;
