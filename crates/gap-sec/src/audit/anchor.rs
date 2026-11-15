//! Ed25519 audit-head anchoring, multi-sink export, and truncation detection (`SEC-091`, `SEC-092`, `TM-17`).

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;

use gap_core::clock::Timestamp;
use gap_core::error::{ErrorCode, GapError, GapResult};

use crate::authn::crypto::{
    Ed25519SigningKey, Ed25519VerifyingKey, base64_decode, base64_encode, ct_eq, hex_decode,
    hex_encode, sha256,
};
use crate::authn::json::JsonValue;

/// Default interval (`10 minutes`) between signed audit head anchors (`SEC-091`).
pub const DEFAULT_ANCHOR_INTERVAL_SECS: u64 = 600;

/// Domain separation prefix included in every Ed25519 audit-anchor signature (`SEC-091`).
pub const ANCHOR_DOMAIN_SEPARATOR: &str = "gap.audit.anchor.v1:";

/// Ed25519-signed checkpoint of an audit hash chain head (`SEC-091`, `TM-17`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditAnchor {
    /// Target chain identifier (`"system"` or `"tenant:<tenant_id>"`).
    pub chain_id: String,
    /// Chain head sequence number at anchor time.
    pub head_seq: u64,
    /// Chain head 32-byte BLAKE3 digest at `head_seq`.
    pub head_hash: [u8; 32],
    /// Gateway timestamp when the anchor was minted (`UTC`).
    pub anchored_at: Timestamp,
    /// Gateway signing key identifier.
    pub key_id: String,
    /// 64-byte RFC 8032 Ed25519 signature over [`Self::canonical_signing_bytes`].
    pub signature: [u8; 64],
}

impl AuditAnchor {
    /// Constructs deterministic domain-separated RFC 8785 JCS bytes covered by the Ed25519 signature.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if `head_seq` exceeds `i64::MAX`.
    pub fn canonical_signing_bytes(
        chain_id: &str,
        head_seq: u64,
        head_hash: &[u8; 32],
        anchored_at: Timestamp,
        key_id: &str,
    ) -> GapResult<Vec<u8>> {
        let seq_i64 = i64::try_from(head_seq).map_err(|_| {
            GapError::new(
                ErrorCode::ValidationError,
                "audit anchor sequence exceeds representable range",
            )
        })?;
        let mut obj = BTreeMap::new();
        obj.insert(
            "anchored_at".to_owned(),
            JsonValue::String(anchored_at.to_string()),
        );
        obj.insert(
            "chain_id".to_owned(),
            JsonValue::String(chain_id.to_owned()),
        );
        obj.insert(
            "head_hash_hex".to_owned(),
            JsonValue::String(hex_encode(head_hash)),
        );
        obj.insert("head_seq".to_owned(), JsonValue::Int(seq_i64));
        obj.insert("key_id".to_owned(), JsonValue::String(key_id.to_owned()));

        let canonical = JsonValue::Object(obj).to_jcs_string();
        let mut bytes = Vec::with_capacity(ANCHOR_DOMAIN_SEPARATOR.len() + canonical.len());
        bytes.extend_from_slice(ANCHOR_DOMAIN_SEPARATOR.as_bytes());
        bytes.extend_from_slice(canonical.as_bytes());
        Ok(bytes)
    }

    /// Creates and signs an [`AuditAnchor`] using the gateway Ed25519 signing key (`SEC-091`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if `chain_id` or `key_id` is empty or signing fails.
    pub fn sign(
        chain_id: impl Into<String>,
        head_seq: u64,
        head_hash: [u8; 32],
        anchored_at: Timestamp,
        key_id: impl Into<String>,
        signing_key: &Ed25519SigningKey,
    ) -> GapResult<Self> {
        let chain_id = chain_id.into();
        let key_id = key_id.into();
        if chain_id.trim().is_empty() || key_id.trim().is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "audit anchor chain_id and key_id must be non-empty",
            ));
        }
        let payload =
            Self::canonical_signing_bytes(&chain_id, head_seq, &head_hash, anchored_at, &key_id)?;
        let signature = signing_key.sign(&payload)?;
        Ok(Self {
            chain_id,
            head_seq,
            head_hash,
            anchored_at,
            key_id,
            signature,
        })
    }

    /// Verifies this anchor's Ed25519 signature against `verifying_key` (`SEC-091`, `TM-17`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Unauthenticated`] or [`ErrorCode::ValidationError`] if verification fails.
    pub fn verify_signature(&self, verifying_key: &Ed25519VerifyingKey) -> GapResult<()> {
        let payload = Self::canonical_signing_bytes(
            &self.chain_id,
            self.head_seq,
            &self.head_hash,
            self.anchored_at,
            &self.key_id,
        )?;
        verifying_key.verify(&payload, &self.signature)
    }

    /// Serializes this signed anchor into deterministic RFC 8785 JCS JSON.
    #[must_use]
    pub fn to_json_string(&self) -> String {
        let mut obj = BTreeMap::new();
        obj.insert(
            "anchored_at".to_owned(),
            JsonValue::String(self.anchored_at.to_string()),
        );
        obj.insert(
            "chain_id".to_owned(),
            JsonValue::String(self.chain_id.clone()),
        );
        obj.insert(
            "head_hash_hex".to_owned(),
            JsonValue::String(hex_encode(&self.head_hash)),
        );
        obj.insert(
            "head_seq".to_owned(),
            JsonValue::Int(i64::try_from(self.head_seq).unwrap_or(i64::MAX)),
        );
        obj.insert("key_id".to_owned(), JsonValue::String(self.key_id.clone()));
        obj.insert(
            "signature_b64".to_owned(),
            JsonValue::String(base64_encode(&self.signature)),
        );
        JsonValue::Object(obj).to_jcs_string()
    }

    /// Parses a JSON-serialized [`AuditAnchor`].
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if required fields are missing or malformed.
    pub fn from_json_str(raw: &str) -> GapResult<Self> {
        let val = JsonValue::parse(raw)?;
        let chain_id = val
            .get_str("chain_id")
            .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "missing anchor chain_id"))?
            .to_owned();
        let key_id = val
            .get_str("key_id")
            .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "missing anchor key_id"))?
            .to_owned();
        let seq_i64 = val
            .get_i64("head_seq")
            .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "missing anchor head_seq"))?;
        let head_seq = u64::try_from(seq_i64)
            .map_err(|_| GapError::new(ErrorCode::ValidationError, "negative anchor head_seq"))?;

        let hash_hex = val.get_str("head_hash_hex").ok_or_else(|| {
            GapError::new(ErrorCode::ValidationError, "missing anchor head_hash_hex")
        })?;
        let hash_bytes = hex_decode(hash_hex)?;
        let head_hash: [u8; 32] = hash_bytes.try_into().map_err(|_| {
            GapError::new(
                ErrorCode::ValidationError,
                "anchor head_hash_hex must decode to 32 bytes",
            )
        })?;

        let anchored_raw = val.get_str("anchored_at").ok_or_else(|| {
            GapError::new(ErrorCode::ValidationError, "missing anchor anchored_at")
        })?;
        let anchored_at: Timestamp = anchored_raw.parse().map_err(|_| {
            GapError::new(
                ErrorCode::ValidationError,
                "invalid anchor RFC 3339 timestamp",
            )
        })?;

        let sig_b64 = val.get_str("signature_b64").ok_or_else(|| {
            GapError::new(ErrorCode::ValidationError, "missing anchor signature_b64")
        })?;
        let sig_bytes = base64_decode(sig_b64)?;
        let signature: [u8; 64] = sig_bytes.try_into().map_err(|_| {
            GapError::new(
                ErrorCode::ValidationError,
                "anchor signature_b64 must decode to 64 bytes",
            )
        })?;

        Ok(Self {
            chain_id,
            head_seq,
            head_hash,
            anchored_at,
            key_id,
            signature,
        })
    }
}

/// Destination sink interface for signed audit head anchors (`SEC-091`).
pub trait AnchorSink: Send + Sync {
    /// Short identifier for diagnostics and telemetry (`file`, `syslog`, `s3`).
    fn sink_kind(&self) -> &'static str;

    /// Persists or transmits one Ed25519-signed [`AuditAnchor`].
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if writing or transmitting the anchor fails.
    fn emit_anchor(&self, anchor: &AuditAnchor) -> GapResult<()>;
}

/// Append-only local filesystem sink for Ed25519 audit anchors (`SEC-091`).
#[derive(Debug)]
pub struct FileAnchorSink {
    path: PathBuf,
    write_guard: Mutex<()>,
}

impl FileAnchorSink {
    /// Creates a [`FileAnchorSink`] appending NDJSON lines to `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            write_guard: Mutex::new(()),
        }
    }

    /// Path of the underlying NDJSON anchor log file.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl AnchorSink for FileAnchorSink {
    fn sink_kind(&self) -> &'static str {
        "file"
    }

    fn emit_anchor(&self, anchor: &AuditAnchor) -> GapResult<()> {
        let _guard = self
            .write_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|_| {
                GapError::new(
                    ErrorCode::Internal,
                    "failed to create audit anchor directory",
                )
            })?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|_| GapError::new(ErrorCode::Internal, "failed to open audit anchor file"))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Ok(meta) = file.metadata() {
                let mut perms = meta.permissions();
                perms.set_mode(0o600);
                let _ = file.set_permissions(perms);
            }
        }

        let line = format!("{}\n", anchor.to_json_string());
        file.write_all(line.as_bytes())
            .map_err(|_| GapError::new(ErrorCode::Internal, "failed to write audit anchor line"))?;
        file.sync_all()
            .map_err(|_| GapError::new(ErrorCode::Internal, "failed to sync audit anchor file"))?;
        Ok(())
    }
}

/// Pluggable transport for RFC 5424 syslog anchor messages (`SEC-091`).
pub trait SyslogTransport: Send + Sync {
    /// Transmits one formatted RFC 5424 syslog record.
    fn send_syslog_line(&self, line: &str) -> GapResult<()>;
}

/// In-memory [`SyslogTransport`] collector for deterministic testing and integration checks.
#[derive(Debug, Default)]
pub struct InMemorySyslogTransport {
    lines: Mutex<Vec<String>>,
}

impl InMemorySyslogTransport {
    /// Creates an empty collector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of all received RFC 5424 syslog records.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl SyslogTransport for InMemorySyslogTransport {
    fn send_syslog_line(&self, line: &str) -> GapResult<()> {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(line.to_owned());
        Ok(())
    }
}

/// RFC 5424 structured syslog anchor sink (`SEC-091`).
pub struct SyslogAnchorSink<T: SyslogTransport> {
    hostname: String,
    transport: T,
}

impl<T: SyslogTransport> std::fmt::Debug for SyslogAnchorSink<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyslogAnchorSink")
            .field("hostname", &self.hostname)
            .finish_non_exhaustive()
    }
}

impl<T: SyslogTransport> SyslogAnchorSink<T> {
    /// Creates a [`SyslogAnchorSink`] tagging emitted lines with `hostname`.
    #[must_use]
    pub fn new(hostname: impl Into<String>, transport: T) -> Self {
        Self {
            hostname: hostname.into(),
            transport,
        }
    }

    /// Borrows the underlying transport.
    #[must_use]
    pub const fn transport(&self) -> &T {
        &self.transport
    }
}

impl<T: SyslogTransport> AnchorSink for SyslogAnchorSink<T> {
    fn sink_kind(&self) -> &'static str {
        "syslog"
    }

    fn emit_anchor(&self, anchor: &AuditAnchor) -> GapResult<()> {
        let json = anchor.to_json_string();
        let hash_hex = hex_encode(&anchor.head_hash);
        let msg = format!(
            "<134>1 {} {} gap-gateway audit - [gapAudit@0 chain=\"{}\" seq=\"{}\" hash=\"{}\" key_id=\"{}\"] {}",
            anchor.anchored_at,
            self.hostname,
            anchor.chain_id,
            anchor.head_seq,
            hash_hex,
            anchor.key_id,
            json
        );
        self.transport.send_syslog_line(&msg)
    }
}

/// Uploaded S3-compatible immutable anchor object (`SEC-091`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorObjectPut {
    /// Target bucket name.
    pub bucket: String,
    /// Object key (`<prefix>/<chain_id>/<seq>-<hash>.json`).
    pub object_key: String,
    /// UTF-8 canonical JSON body.
    pub body: Vec<u8>,
    /// Lowercase hex SHA-256 content digest for object integrity verification.
    pub sha256_hex: String,
}

/// Pluggable S3-compatible object upload transport (`SEC-091`).
pub trait ObjectPutTransport: Send + Sync {
    /// Uploads an immutable object to an S3-compatible endpoint.
    fn put_object(&self, obj: AnchorObjectPut) -> GapResult<()>;
}

/// In-memory S3-compatible bucket for offline testing.
#[derive(Debug, Default)]
pub struct InMemoryObjectBucket {
    objects: Mutex<Vec<AnchorObjectPut>>,
}

impl InMemoryObjectBucket {
    /// Creates an empty bucket.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of all uploaded anchor objects.
    #[must_use]
    pub fn objects(&self) -> Vec<AnchorObjectPut> {
        self.objects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl ObjectPutTransport for InMemoryObjectBucket {
    fn put_object(&self, obj: AnchorObjectPut) -> GapResult<()> {
        self.objects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(obj);
        Ok(())
    }
}

/// S3-compatible WORM object sink for Ed25519 audit anchors (`SEC-091`).
pub struct S3CompatibleAnchorSink<T: ObjectPutTransport> {
    bucket: String,
    prefix: String,
    transport: T,
}

impl<T: ObjectPutTransport> std::fmt::Debug for S3CompatibleAnchorSink<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3CompatibleAnchorSink")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl<T: ObjectPutTransport> S3CompatibleAnchorSink<T> {
    /// Creates an S3-compatible anchor sink writing under `s3://<bucket>/<prefix>/...`.
    #[must_use]
    pub fn new(bucket: impl Into<String>, prefix: impl Into<String>, transport: T) -> Self {
        Self {
            bucket: bucket.into(),
            prefix: prefix.into().trim_matches('/').to_owned(),
            transport,
        }
    }

    /// Borrows the underlying object transport.
    #[must_use]
    pub const fn transport(&self) -> &T {
        &self.transport
    }
}

impl<T: ObjectPutTransport> AnchorSink for S3CompatibleAnchorSink<T> {
    fn sink_kind(&self) -> &'static str {
        "s3"
    }

    fn emit_anchor(&self, anchor: &AuditAnchor) -> GapResult<()> {
        let safe_chain = anchor.chain_id.replace(':', "/");
        let hash_hex = hex_encode(&anchor.head_hash);
        let key = if self.prefix.is_empty() {
            format!("{safe_chain}/{:020}-{hash_hex}.json", anchor.head_seq)
        } else {
            format!(
                "{}/{safe_chain}/{:020}-{hash_hex}.json",
                self.prefix, anchor.head_seq
            )
        };
        let body = anchor.to_json_string().into_bytes();
        let digest_hex = hex_encode(&sha256(&body));
        self.transport.put_object(AnchorObjectPut {
            bucket: self.bucket.clone(),
            object_key: key,
            body,
            sha256_hex: digest_hex,
        })
    }
}

/// Periodic Ed25519 anchor cadence scheduler (`default 10 min`, `SEC-091`).
#[derive(Debug, Clone)]
pub struct AnchorScheduler {
    interval_secs: u64,
    last_anchored_at: BTreeMap<String, Timestamp>,
}

impl Default for AnchorScheduler {
    fn default() -> Self {
        Self::new(DEFAULT_ANCHOR_INTERVAL_SECS)
    }
}

impl AnchorScheduler {
    /// Creates an anchor scheduler firing every `interval_secs` (minimum `1` second).
    #[must_use]
    pub fn new(interval_secs: u64) -> Self {
        Self {
            interval_secs: interval_secs.max(1),
            last_anchored_at: BTreeMap::new(),
        }
    }

    /// Configured anchor interval in seconds.
    #[must_use]
    pub const fn interval_secs(&self) -> u64 {
        self.interval_secs
    }

    /// Returns `true` when `chain_id` has never been anchored or `interval_secs` have elapsed since its last anchor.
    #[must_use]
    pub fn is_due(&self, chain_id: &str, now: Timestamp) -> bool {
        let Some(prev) = self.last_anchored_at.get(chain_id) else {
            return true;
        };
        let elapsed_nanos = now.unix_nanos().saturating_sub(prev.unix_nanos());
        elapsed_nanos >= i128::from(self.interval_secs) * 1_000_000_000
    }

    /// Marks `chain_id` as freshly anchored at `now`.
    pub fn record_anchored(&mut self, chain_id: impl Into<String>, now: Timestamp) {
        self.last_anchored_at.insert(chain_id.into(), now);
    }
}

/// Outcome of validating a local audit chain against an external Ed25519 anchor (`SEC-091`, `TM-17`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorVerifyStatus {
    /// Signature is valid and observed chain state contains the anchored `(head_seq, head_hash)`.
    MatchesAnchor,
    /// Anchor Ed25519 signature failed verification.
    SignatureInvalid(String),
    /// Anchor chain identifier does not match the inspected local chain.
    ChainMismatch(String),
    /// Local chain was truncated below `anchor.head_seq` (`TM-17`).
    Truncated {
        /// Sequence number recorded in the signed external anchor.
        anchor_seq: u64,
        /// Highest sequence number present locally.
        observed_head_seq: u64,
    },
    /// Local chain has a record at `anchor.head_seq`, but its digest diverges from the signed anchor (`TM-17`).
    HashRewritten {
        /// Sequence number recorded in the signed external anchor.
        anchor_seq: u64,
    },
}

/// Verifies that local chain state `(observed_chain_id, observed_head_seq, observed_hash_at_anchor_seq)`
/// has not been truncated or rewritten relative to an externally stored Ed25519 `anchor` (`SEC-091`, `TM-17`).
#[must_use]
pub fn verify_chain_against_anchor(
    anchor: &AuditAnchor,
    verifying_key: &Ed25519VerifyingKey,
    observed_chain_id: &str,
    observed_head_seq: u64,
    observed_hash_at_anchor_seq: Option<[u8; 32]>,
) -> AnchorVerifyStatus {
    if let Err(err) = anchor.verify_signature(verifying_key) {
        return AnchorVerifyStatus::SignatureInvalid(err.message().to_owned());
    }
    if anchor.chain_id != observed_chain_id {
        return AnchorVerifyStatus::ChainMismatch(format!(
            "expected chain `{}`, found `{observed_chain_id}`",
            anchor.chain_id
        ));
    }
    if observed_head_seq < anchor.head_seq {
        return AnchorVerifyStatus::Truncated {
            anchor_seq: anchor.head_seq,
            observed_head_seq,
        };
    }
    match observed_hash_at_anchor_seq {
        Some(observed_hash) if ct_eq(&observed_hash, &anchor.head_hash) => {
            AnchorVerifyStatus::MatchesAnchor
        }
        Some(_) => AnchorVerifyStatus::HashRewritten {
            anchor_seq: anchor.head_seq,
        },
        None => AnchorVerifyStatus::Truncated {
            anchor_seq: anchor.head_seq,
            observed_head_seq,
        },
    }
}
