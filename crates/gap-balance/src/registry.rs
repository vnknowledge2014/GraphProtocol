//! Lock-free read, two-level copy-on-write [`TelemetryRegistry`] (`RTE-072`, `RTE-078`, `INV-09`, `INV-10`).

use std::fmt;
use std::time::Instant;

use arc_swap::ArcSwap;
use gap_core::clock::Clock;
use gap_core::snapshot::TelemetrySlotKey;
use thiserror::Error;

use crate::slot::{SlotParams, TelemetrySlot};
use crate::sync::{Arc, Mutex};
use crate::time::Now;

/// Number of [`TelemetrySlot`] entries per slab chunk (`m3-01-design.md` §4).
const CHUNK_SIZE: usize = 256;

/// Number of copy-on-write hash buckets in the key lookup index.
const INDEX_BUCKETS: usize = 256;

/// Maximum byte length of `SlotParams.label` (`m3-01-design.md` §5).
const MAX_LABEL_BYTES: usize = 64;

/// Dense, never-reused index into a [`TelemetryRegistry`] (`RTE-078`, `INV-09`).
///
/// Never constructed from an arbitrary `u32` outside `gap-balance`'s own unit tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchSlot(pub(crate) u32);

impl BranchSlot {
    /// Returns the dense `0`-based slot index (`RTE-078`).
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }

    #[cfg(test)]
    pub(crate) const fn from_raw_for_test(raw: u32) -> Self {
        Self(raw)
    }
}

/// Registration failure returned by [`TelemetryRegistry::register`] (`RTE-078`, `OPS-020`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RegistryError {
    /// Registry already holds `max_slots` entries (`RTE-078`).
    #[error("telemetry registry full at max_slots={max_slots}")]
    Full {
        /// Configured slot ceiling of the registry.
        max_slots: u32,
    },
    /// `SlotParams.label` violated `1..=64` bytes or contained control characters (`OPS-020`).
    #[error("invalid slot metric label: expected 1..=64 bytes with no control characters")]
    InvalidLabel,
}

#[inline]
fn validate_label(label: &str) -> Result<(), RegistryError> {
    if label.is_empty() || label.len() > MAX_LABEL_BYTES || label.chars().any(char::is_control) {
        return Err(RegistryError::InvalidLabel);
    }
    Ok(())
}

/// Deterministic 64-bit FNV-1a bucket selector over `(tenant, branch_uid)`.
#[inline]
fn key_bucket_index(key: &TelemetrySlotKey) -> usize {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for &b in key.tenant.as_str().as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash ^= 0xff;
    hash = hash.wrapping_mul(PRIME);
    for &b in key.branch_uid.as_uuid().as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    ((hash ^ (hash >> 32)) as usize) & (INDEX_BUCKETS - 1)
}

#[derive(Debug)]
struct Chunk {
    slots: Vec<Arc<TelemetrySlot>>,
}

struct Slab {
    len: usize,
    chunks: Vec<Arc<Chunk>>,
    key_buckets: [Arc<Vec<(TelemetrySlotKey, BranchSlot)>>; INDEX_BUCKETS],
}

impl Slab {
    fn empty() -> Self {
        let empty_bucket: Arc<Vec<(TelemetrySlotKey, BranchSlot)>> = Arc::new(Vec::new());
        Self {
            len: 0,
            chunks: Vec::new(),
            key_buckets: std::array::from_fn(|_| Arc::clone(&empty_bucket)),
        }
    }

    #[inline]
    fn find(&self, key: &TelemetrySlotKey) -> Option<BranchSlot> {
        let b = key_bucket_index(key);
        self.key_buckets[b]
            .iter()
            .find_map(|(k, slot)| if k == key { Some(*slot) } else { None })
    }

    #[inline]
    fn get(&self, slot: BranchSlot) -> Option<Arc<TelemetrySlot>> {
        let idx = slot.0 as usize;
        if idx >= self.len {
            return None;
        }
        let chunk = self.chunks.get(idx / CHUNK_SIZE)?;
        chunk.slots.get(idx % CHUNK_SIZE).cloned()
    }
}

/// Dense, two-level copy-on-write telemetry slab keyed by [`TelemetrySlotKey`] (`RTE-078`, `INV-02`, `INV-09`).
///
/// Hot-path lookups (`get`, `find`, `now`) are lock-free and allocation-free; only cold-path
/// `register` acquires an internal writer `Mutex` and publishes a new slab snapshot via [`ArcSwap`].
pub struct TelemetryRegistry {
    clock: std::sync::Arc<dyn Clock>,
    epoch: Instant,
    max_slots: u32,
    slab: ArcSwap<Slab>,
    writer_lock: Mutex<()>,
}

impl fmt::Debug for TelemetryRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let guard = self.slab.load();
        f.debug_struct("TelemetryRegistry")
            .field("len", &guard.len)
            .field("max_slots", &self.max_slots)
            .field("chunks", &guard.chunks.len())
            .finish()
    }
}

impl TelemetryRegistry {
    /// Default slot ceiling (`65,536`, `RTE-078`).
    pub const DEFAULT_MAX_SLOTS: u32 = 65_536;

    /// Creates an empty [`TelemetryRegistry`] anchored at `clock.now_instant()` with ceiling `max_slots`.
    #[must_use]
    pub fn new(clock: std::sync::Arc<dyn Clock>, max_slots: u32) -> Self {
        let epoch = clock.now_instant();
        Self {
            clock,
            epoch,
            max_slots,
            slab: ArcSwap::from_pointee(Slab::empty()),
            writer_lock: Mutex::new(()),
        }
    }

    /// Idempotently registers `key` and returns its dense [`BranchSlot`] (`RTE-078`, `INV-09`).
    ///
    /// If `key` was already registered, the existing [`BranchSlot`] is returned and the first
    /// registration's [`SlotParams`] remain in effect.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::InvalidLabel`] if `params.label` is empty, exceeds `64` bytes, or
    /// contains control characters, or [`RegistryError::Full`] if `key` is new and `len >= max_slots`.
    pub fn register(
        &self,
        key: TelemetrySlotKey,
        params: SlotParams,
    ) -> Result<BranchSlot, RegistryError> {
        validate_label(&params.label)?;

        // Fast lock-free check if already registered.
        if let Some(existing) = self.slab.load().find(&key) {
            return Ok(existing);
        }

        let _writer = match self.writer_lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        let cur = self.slab.load_full();
        if let Some(existing) = cur.find(&key) {
            return Ok(existing);
        }

        if cur.len >= (self.max_slots as usize) {
            return Err(RegistryError::Full {
                max_slots: self.max_slots,
            });
        }

        let slot_index = cur.len as u32;
        let branch_slot = BranchSlot(slot_index);
        let new_slot = Arc::new(TelemetrySlot::new(params));

        // Copy-on-write outer chunk table + only the active tail chunk (<= 256 pointers).
        let mut next_chunks = cur.chunks.clone();
        if cur.len.is_multiple_of(CHUNK_SIZE) {
            let mut slots = Vec::with_capacity(CHUNK_SIZE);
            slots.push(new_slot);
            next_chunks.push(Arc::new(Chunk { slots }));
        } else if let Some(last_chunk_arc) = next_chunks.last_mut() {
            let mut slots = Vec::with_capacity(CHUNK_SIZE);
            slots.extend(last_chunk_arc.slots.iter().cloned());
            slots.push(new_slot);
            *last_chunk_arc = Arc::new(Chunk { slots });
        }

        // Copy-on-write key index bucket.
        let bucket_idx = key_bucket_index(&key);
        let mut next_buckets = cur.key_buckets.clone();
        let mut updated_bucket = (*next_buckets[bucket_idx]).clone();
        updated_bucket.push((key, branch_slot));
        next_buckets[bucket_idx] = Arc::new(updated_bucket);

        let next_slab = std::sync::Arc::new(Slab {
            len: cur.len + 1,
            chunks: next_chunks,
            key_buckets: next_buckets,
        });
        self.slab.store(next_slab);

        Ok(branch_slot)
    }

    /// Lock-free, allocation-free lookup of a [`TelemetrySlot`] by [`BranchSlot`] (`RTE-078`).
    #[must_use]
    pub fn get(&self, slot: BranchSlot) -> Option<Arc<TelemetrySlot>> {
        self.slab.load().get(slot)
    }

    /// Lock-free lookup of a registered [`BranchSlot`] by [`TelemetrySlotKey`] (`INV-09`).
    #[must_use]
    pub fn find(&self, key: &TelemetrySlotKey) -> Option<BranchSlot> {
        self.slab.load().find(key)
    }

    /// Computes a monotonic [`Now`] relative to this registry's epoch from the injected [`Clock`] (`INV-02`, `RTE-077`).
    #[must_use]
    pub fn now(&self) -> Now {
        let elapsed = self
            .clock
            .now_instant()
            .saturating_duration_since(self.epoch);
        let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        Now::from_ms(ms)
    }

    /// Resets EWMA cells older than `5` half-lives and clears stale breaker buckets across all
    /// registered slots without blocking hot-path operations (`RTE-077`, `RTE-084`).
    pub fn maintain(&self) {
        let now = self.now();
        let guard = self.slab.load();
        for chunk in &guard.chunks {
            for slot in &chunk.slots {
                slot.maintain(now);
            }
        }
    }

    /// Returns the number of registered slots (`0..=max_slots`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.slab.load().len
    }

    /// Returns `true` iff no slots have been registered yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the configured maximum slot capacity (`RTE-078`).
    #[must_use]
    pub const fn max_slots(&self) -> u32 {
        self.max_slots
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering as StdOrdering};
    use std::sync::{Arc as StdArc, Barrier};
    use std::time::Duration;

    use gap_core::clock::ManualClock;
    use gap_core::ids::{BranchUid, TenantId};

    use super::*;
    use crate::breaker::BreakerState;
    use crate::ewma::DecayedEwma;
    use crate::slot::{HealthStatus, InFlightGuard, Outcome};

    fn make_uid(seq: u64) -> BranchUid {
        let mut rand_bytes = [0_u8; 10];
        rand_bytes[2..10].copy_from_slice(&seq.to_be_bytes());
        BranchUid::from_unix_ms(1_700_000_000_000 + seq, rand_bytes)
    }

    fn default_params(label: &str, max_in_flight: u32) -> SlotParams {
        SlotParams {
            max_in_flight,
            default_timeout_ms: 1_000,
            label: Arc::from(label),
        }
    }

    // spec: RTE-078
    #[test]
    fn registry_idempotent_dense_no_reuse_full_and_multi_chunk() {
        let clock = StdArc::new(ManualClock::new(1_700_000_000_000));
        let reg = TelemetryRegistry::new(clock, 3);
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert_eq!(reg.max_slots(), 3);

        let tenant = TenantId::new("acme").expect("tenant");
        let k0 = TelemetrySlotKey::new(tenant.clone(), make_uid(1));
        let k1 = TelemetrySlotKey::new(tenant.clone(), make_uid(2));
        let k2 = TelemetrySlotKey::new(tenant.clone(), make_uid(3));
        let k3 = TelemetrySlotKey::new(tenant.clone(), make_uid(4));

        let s0 = reg
            .register(k0.clone(), default_params("b-0", 32))
            .expect("slot 0");
        assert_eq!(s0.index(), 0);
        assert!(!reg.is_empty());
        assert_eq!(reg.len(), 1);

        // Re-registering k0 with different params returns BranchSlot(0) and keeps first params (cap 32).
        let s0_again = reg
            .register(k0.clone(), default_params("b-0-ignored", 999))
            .expect("idempotent slot 0");
        assert_eq!(s0_again, s0);
        assert_eq!(reg.len(), 1);
        let arc0 = reg.get(s0).expect("get slot 0");
        assert_eq!(arc0.capacity(), 32);

        let s1 = reg
            .register(k1.clone(), default_params("b-1", 16))
            .expect("slot 1");
        let s2 = reg
            .register(k2.clone(), default_params("b-2", 16))
            .expect("slot 2");
        assert_eq!(s1.index(), 1);
        assert_eq!(s2.index(), 2);
        assert_eq!(reg.len(), 3);

        // 4th distinct key hits Full; re-registering existing k1 still succeeds!
        assert_eq!(
            reg.register(k3.clone(), default_params("b-3", 16)),
            Err(RegistryError::Full { max_slots: 3 })
        );
        assert_eq!(
            reg.register(k1.clone(), default_params("b-1", 16)),
            Ok(s1)
        );
        assert_eq!(reg.find(&k3), None);
        assert!(reg.get(BranchSlot::from_raw_for_test(3)).is_none());

        // Verify multi-chunk two-level slab across > 256 slots (600 slots = 3 chunks).
        let big_clock = StdArc::new(ManualClock::new(1_700_000_000_000));
        let big_reg = TelemetryRegistry::new(big_clock, TelemetryRegistry::DEFAULT_MAX_SLOTS);
        for i in 0..600_u32 {
            let key = TelemetrySlotKey::new(tenant.clone(), make_uid(u64::from(i) + 100));
            let slot = big_reg
                .register(key.clone(), default_params("multi-chunk", i + 1))
                .expect("register multi-chunk");
            assert_eq!(slot.index(), i);
            assert_eq!(big_reg.find(&key), Some(slot));
        }
        assert_eq!(big_reg.len(), 600);
        for i in 0..600_u32 {
            let got = big_reg
                .get(BranchSlot::from_raw_for_test(i))
                .expect("chunked slot present");
            assert_eq!(got.capacity(), i + 1);
        }
    }

    // spec: RTE-078, OPS-020
    #[test]
    fn registry_validates_slot_label_length_and_control_chars() {
        let clock = StdArc::new(ManualClock::new(1_700_000_000_000));
        let reg = TelemetryRegistry::new(clock, 16);
        let tenant = TenantId::new("acme").expect("tenant");

        let bad_labels = [
            String::new(),
            "a".repeat(65),
            "bad\nlabel".to_owned(),
            "bad\rlabel".to_owned(),
            "bad\tlabel".to_owned(),
            "bad\0label".to_owned(),
            "bad\x7flabel".to_owned(),
            "bad\u{0085}label".to_owned(),
        ];
        for (idx, bad) in bad_labels.iter().enumerate() {
            let key = TelemetrySlotKey::new(tenant.clone(), make_uid(idx as u64 + 1));
            let res = reg.register(key, default_params(bad, 10));
            assert_eq!(res, Err(RegistryError::InvalidLabel), "label {bad:?} must fail");
        }
        assert_eq!(reg.len(), 0);

        // Exact boundary 1 byte and 64 bytes succeed.
        let s_min = reg
            .register(
                TelemetrySlotKey::new(tenant.clone(), make_uid(500)),
                default_params("x", 10),
            )
            .expect("1-byte label valid");
        let s_max = reg
            .register(
                TelemetrySlotKey::new(tenant, make_uid(501)),
                default_params(&"y".repeat(64), 10),
            )
            .expect("64-byte label valid");
        assert_eq!(s_min.index(), 0);
        assert_eq!(s_max.index(), 1);
    }

    // spec: INV-09, INV-10, RTE-072, RTE-078
    #[test]
    fn inv_09_registry_isolates_tenants_sharing_same_branch_uid() {
        let clock = StdArc::new(ManualClock::new(1_700_000_000_000));
        let reg = TelemetryRegistry::new(clock, 64);

        let tenant_a = TenantId::new("tenant-alpha").expect("tenant a");
        let tenant_b = TenantId::new("tenant-bravo").expect("tenant b");
        let tenant_c = TenantId::new("tenant-charlie").expect("tenant c");
        let shared_uid = make_uid(42);

        let key_a = TelemetrySlotKey::new(tenant_a, shared_uid);
        let key_b = TelemetrySlotKey::new(tenant_b, shared_uid);
        let key_c = TelemetrySlotKey::new(tenant_c, shared_uid);

        let slot_a_id = reg
            .register(key_a.clone(), default_params("svc-shared", 20))
            .expect("register tenant A");
        let slot_b_id = reg
            .register(key_b.clone(), default_params("svc-shared", 20))
            .expect("register tenant B");

        assert_ne!(slot_a_id, slot_b_id);
        assert_eq!(reg.find(&key_a), Some(slot_a_id));
        assert_eq!(reg.find(&key_b), Some(slot_b_id));
        assert_eq!(reg.find(&key_c), None);

        let slot_a = reg.get(slot_a_id).expect("slot A");
        let slot_b = reg.get(slot_b_id).expect("slot B");
        assert!(!Arc::ptr_eq(&slot_a, &slot_b));

        // Mutate every telemetry surface on tenant A; tenant B must remain completely pristine.
        let now = reg.now();
        let _guard_a = InFlightGuard::acquire(&slot_a);
        for _ in 0..20 {
            slot_a.record(Outcome::Failure, Duration::from_millis(900), now);
        }
        assert!(slot_a.retry_budget().try_withdraw());
        slot_a.apply_health(HealthStatus::Degraded, Some(0.5), now);

        let view_a = slot_a.view(now);
        assert_eq!(view_a.in_flight, 1);
        assert_eq!(view_a.capacity, 5);
        assert_eq!(view_a.state, BreakerState::Open);
        assert_eq!(slot_a.retry_budget().balance_millis(), 9_000);

        let view_b = slot_b.view(now);
        assert_eq!(view_b.latency_us, None);
        assert_eq!(view_b.error_rate, None);
        assert_eq!(view_b.in_flight, 0);
        assert_eq!(view_b.capacity, 20);
        assert_eq!(view_b.state, BreakerState::Closed);
        assert_eq!(slot_b.retry_budget().balance_millis(), 10_000);
    }

    // spec: RTE-078, INV-02
    #[test]
    fn registry_concurrent_same_key_and_5000_registrations_with_readers() {
        // 1. 16 threads racing to register the exact same key all get BranchSlot(0).
        let clock = StdArc::new(ManualClock::new(1_700_000_000_000));
        let reg = StdArc::new(TelemetryRegistry::new(clock, 16_384));
        let tenant = TenantId::new("acme").expect("tenant");
        let race_key = TelemetrySlotKey::new(tenant.clone(), make_uid(1));

        let barrier = StdArc::new(Barrier::new(16));
        let mut race_handles = Vec::with_capacity(16);
        for tid in 0..16 {
            let reg = StdArc::clone(&reg);
            let barrier = StdArc::clone(&barrier);
            let key = race_key.clone();
            race_handles.push(std::thread::spawn(move || {
                let params = default_params("race-branch", 10 + tid);
                barrier.wait();
                reg.register(key, params).expect("concurrent idempotent register")
            }));
        }
        for h in race_handles {
            assert_eq!(h.join().expect("race worker"), BranchSlot(0));
        }
        assert_eq!(reg.len(), 1);

        // 2. Pre-register 7 more keys (total 8), then 4 reader threads hammer `get` on all
        // published slots while a writer registers 5,000 additional keys.
        for i in 1..8_u32 {
            let s = reg
                .register(
                    TelemetrySlotKey::new(tenant.clone(), make_uid(u64::from(i) + 1)),
                    default_params("seed-branch", 32),
                )
                .expect("seed slot");
            assert_eq!(s.index(), i);
        }

        let published_count = StdArc::new(AtomicU32::new(8));
        let done = StdArc::new(AtomicBool::new(false));
        let start_barrier = StdArc::new(Barrier::new(5));

        let mut reader_handles = Vec::with_capacity(4);
        for _ in 0..4 {
            let reg = StdArc::clone(&reg);
            let published_count = StdArc::clone(&published_count);
            let done = StdArc::clone(&done);
            let start_barrier = StdArc::clone(&start_barrier);
            reader_handles.push(std::thread::spawn(move || {
                start_barrier.wait();
                loop {
                    let count = published_count.load(StdOrdering::Acquire);
                    for raw in 0..count {
                        let got = reg.get(BranchSlot::from_raw_for_test(raw));
                        assert!(got.is_some(), "reader observed None for issued slot {raw}");
                    }
                    if done.load(StdOrdering::Acquire) {
                        let final_count = published_count.load(StdOrdering::Acquire);
                        for raw in 0..final_count {
                            assert!(reg.get(BranchSlot::from_raw_for_test(raw)).is_some());
                        }
                        break;
                    }
                }
            }));
        }

        start_barrier.wait();
        for i in 0..5_000_u32 {
            let slot = reg
                .register(
                    TelemetrySlotKey::new(tenant.clone(), make_uid(u64::from(i) + 1_000)),
                    default_params("grow-branch", 64),
                )
                .expect("register during reader traffic");
            assert_eq!(slot.index(), 8 + i);
            published_count.store(9 + i, StdOrdering::Release);
        }
        done.store(true, StdOrdering::Release);

        for r in reader_handles {
            r.join().expect("reader thread clean");
        }
        assert_eq!(reg.len(), 5_008);
    }

    // spec: RTE-077, RTE-078, RTE-084
    #[test]
    fn registry_maintain_clears_stale_ewmas_and_buckets_preserving_slots() {
        let clock = StdArc::new(ManualClock::new(1_700_000_000_000));
        let reg = TelemetryRegistry::new(clock.clone(), 32);
        let tenant = TenantId::new("acme").expect("tenant");

        let stale_id = reg
            .register(
                TelemetrySlotKey::new(tenant.clone(), make_uid(10)),
                default_params("stale-svc", 16),
            )
            .expect("stale slot");
        let fresh_id = reg
            .register(
                TelemetrySlotKey::new(tenant, make_uid(11)),
                default_params("fresh-svc", 16),
            )
            .expect("fresh slot");

        let stale_slot = reg.get(stale_id).expect("get stale");
        let fresh_slot = reg.get(fresh_id).expect("get fresh");

        // Record at t = 0 ms on stale_slot (including 5 failures in epoch 0).
        let t0 = reg.now();
        assert_eq!(t0, Now { ms: 0, q: 0 });
        for _ in 0..5 {
            stale_slot.record(Outcome::Failure, Duration::from_millis(4), t0);
        }

        // Advance 40,000 ms (40 s): record on fresh_slot.
        clock.advance(Duration::from_millis(40_000));
        let t1 = reg.now();
        assert_eq!(t1.ms, 40_000);
        fresh_slot.record(Outcome::Ok, Duration::from_millis(2), t1);

        // Advance 10,004 ms -> total 50,004 ms (> 5 * 10s latency half-life, < 5 * 30s error half-life).
        clock.advance(Duration::from_millis(10_004));
        reg.maintain();

        assert_eq!(stale_slot.latency_us.raw(), DecayedEwma::UNSET);
        assert_ne!(stale_slot.error_rate.raw(), DecayedEwma::UNSET);
        assert_eq!(stale_slot.breaker().window_totals(reg.now()), (0, 0, 0));
        assert_eq!(fresh_slot.view(reg.now()).latency_us, Some(2_000.0));

        // Advance another 100,000 ms -> total 150,004 ms (> 5 * 30s error half-life for stale_slot);
        // keep fresh_slot alive right before maintain.
        clock.advance(Duration::from_millis(99_000));
        fresh_slot.record(Outcome::Ok, Duration::from_millis(3), reg.now());
        clock.advance(Duration::from_millis(1_000));
        reg.maintain();

        assert_eq!(stale_slot.error_rate.raw(), DecayedEwma::UNSET);
        assert_eq!(fresh_slot.view(reg.now()).latency_us, Some(3_000.0));
        assert_eq!(fresh_slot.view(reg.now()).error_rate, Some(0.0));

        // Both registered slots remain in place in the slab.
        assert_eq!(reg.len(), 2);
        assert!(Arc::ptr_eq(&reg.get(stale_id).expect("still there"), &stale_slot));
        assert!(Arc::ptr_eq(&reg.get(fresh_id).expect("still there"), &fresh_slot));
    }
}
