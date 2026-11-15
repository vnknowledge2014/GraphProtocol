//! Deterministic WordPiece / topic-aware tokenizer for `LayaCandle` (`RTE-042`, `WP-M5-02`).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use gap_core::error::{ErrorCode, GapError, GapResult};

use crate::candle::json::parse_json;

/// Padding token ID (`[PAD]`).
pub const PAD_ID: u32 = 0;
/// Unknown token ID (`[UNK]`).
pub const UNK_ID: u32 = 1;
/// Classification / state-start token ID (`[CLS]`).
pub const CLS_ID: u32 = 2;
/// Segment separator token ID (`[SEP]`).
pub const SEP_ID: u32 = 3;
/// Per-option decision marker token ID (`[MASK]`).
pub const MASK_ID: u32 = 4;
/// Minimum ID guaranteed for all non-special vocabulary / hash-bucket tokens.
pub const FIRST_ORDINARY_ID: u32 = 5;
/// Default vocabulary size matching built-in deterministic checkpoints.
pub const DEFAULT_VOCAB_SIZE: usize = 512;

/// Number of curated semantic topic channels encoded cleanly in IDs `5..5+NUM_TOPICS*WORDS_PER_TOPIC`.
pub const NUM_TOPIC_BUCKETS: usize = 16;

const CURATED_TOPICS: &[&[&str]] = &[
    // 0: billing / invoices
    &[
        "billing",
        "invoice",
        "invoices",
        "payment",
        "payments",
        "refund",
        "refunds",
        "subscription",
        "charges",
        "paid",
    ],
    // 1: crm / contacts
    &[
        "crm", "contact", "contacts", "customer", "profile", "account", "accounts", "owners",
        "deals", "history",
    ],
    // 2: inventory / stock
    &[
        "inventory",
        "warehouse",
        "stock",
        "sku",
        "availability",
        "backorders",
        "reorder",
        "thresholds",
        "quantity",
        "lagerbestand",
    ],
    // 3: shipping / parcels
    &[
        "shipping",
        "parcel",
        "shipment",
        "shipments",
        "carrier",
        "delivery",
        "track",
        "customs",
        "estimates",
        "events",
    ],
    // 4: catalog / products
    &[
        "catalog",
        "product",
        "products",
        "titles",
        "categories",
        "pricing",
        "tiers",
        "attribute",
        "attributes",
        "specifications",
    ],
    // 5: hr / employees
    &[
        "hr",
        "employee",
        "directory",
        "department",
        "rosters",
        "leave",
        "balances",
        "shift",
        "schedules",
        "team",
    ],
    // 6: analytics / metrics
    &[
        "analytics",
        "daily",
        "active",
        "users",
        "retention",
        "cohorts",
        "funnel",
        "conversion",
        "revenue",
        "metrics",
    ],
    // 7: ticketing / support
    &[
        "ticketing",
        "tickets",
        "support",
        "sla",
        "timers",
        "escalation",
        "notes",
        "assignee",
        "queues",
        "incidents",
    ],
    // 8: identity / oauth
    &[
        "identity", "verify", "role", "bindings", "oauth", "client", "metadata", "audit",
        "session", "scopes",
    ],
    // 9: documents / runbooks
    &[
        "documents",
        "indexed",
        "design",
        "api",
        "changelogs",
        "runbook",
        "runbooks",
        "ownership",
        "edges",
        "architecture",
    ],
    // 10: calendar / meetings
    &[
        "calendar",
        "room",
        "meeting",
        "participant",
        "recurring",
        "timezone",
        "slots",
        "tomorrow",
        "open",
        "fetch",
    ],
    // 11: payments / settlement
    &[
        "authorize",
        "settlement",
        "batch",
        "batches",
        "reconcile",
        "ledger",
        "payouts",
        "chargebacks",
        "commande",
        "paiement",
    ],
    // 12: notifications / webhooks
    &[
        "notifications",
        "dispatch",
        "transactional",
        "templates",
        "bounce",
        "logs",
        "webhook",
        "deliveries",
        "failures",
        "check",
    ],
    // 13: compliance / pii
    &[
        "compliance",
        "pii",
        "classification",
        "tags",
        "consent",
        "entries",
        "rules",
        "data",
        "policy",
        "policies",
    ],
    // 14: observability / latency
    &[
        "observability",
        "service",
        "latency",
        "quantiles",
        "circuit",
        "breaker",
        "trips",
        "error",
        "budget",
        "p99",
    ],
    // 15: sink signature words exclusively in SINK_OPTION_TEXT
    &["fulfil", "other", "options", "cannot"],
];

/// Tokenizer mapping ordinary text strictly to IDs `>= 5` (`FIRST_ORDINARY_ID`) so untrusted
/// input can never forge `[PAD]`, `[UNK]`, `[CLS]`, `[SEP]`, or `[MASK]`.
#[derive(Debug, Clone)]
pub struct LayaTokenizer {
    vocab: HashMap<String, u32>,
    topic_by_id: BTreeMap<u32, usize>,
    vocab_size: usize,
    use_hash_fallback: bool,
}

impl LayaTokenizer {
    /// Builds the deterministic built-in vocabulary of size `DEFAULT_VOCAB_SIZE` (`512`).
    #[must_use]
    pub fn builtin_default() -> Self {
        let mut vocab = HashMap::new();
        let mut topic_by_id = BTreeMap::new();
        let mut next_id = FIRST_ORDINARY_ID;

        for (topic_idx, words) in CURATED_TOPICS.iter().enumerate() {
            for &word in *words {
                let entry = vocab.entry(word.to_owned()).or_insert_with(|| {
                    let assigned = next_id;
                    next_id += 1;
                    assigned
                });
                topic_by_id.insert(*entry, topic_idx);
            }
        }

        // Common structural/JSON words used in RTE-042 state/question strings.
        for structural in [
            "constraints",
            "max",
            "side",
            "effect",
            "none",
            "reversible",
            "irreversible",
            "fulfill",
            "request",
            "insufficient",
            "evidence",
            "replica",
            "alternate",
            "query",
            "select",
            "single",
            "capability",
            "group",
            "whose",
            "schema",
            "operations",
            "case",
            "find",
            "status",
            "details",
            "list",
            "priority",
            "high",
            "inspect",
            "active",
            "compute",
            "weekly",
            "search",
            "variant",
            "view",
            "for",
            "and",
            "the",
        ] {
            vocab.entry(structural.to_owned()).or_insert_with(|| {
                let assigned = next_id;
                next_id += 1;
                assigned
            });
        }

        Self {
            vocab,
            topic_by_id,
            vocab_size: DEFAULT_VOCAB_SIZE,
            use_hash_fallback: true,
        }
    }

    /// Loads a HuggingFace `vocab.json` dictionary (`token -> id`), sanitizing ordinary token IDs
    /// so no word maps to `[PAD]`..=`[MASK]` (`0..=4`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if reading or parsing `vocab.json` fails.
    pub fn from_vocab_json(path: &Path) -> GapResult<Self> {
        let raw = fs::read_to_string(path).map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("failed to read tokenizer `{}`: {err}", path.display()),
            )
        })?;
        Self::from_vocab_json_str(&raw)
    }

    /// Parses a HuggingFace `vocab.json` string (`token -> integer_id`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if JSON syntax or token ID integers are invalid.
    pub fn from_vocab_json_str(raw: &str) -> GapResult<Self> {
        let val = parse_json(raw)?;
        let Some(obj) = val.as_object() else {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "vocab.json root must be a JSON object",
            ));
        };
        let mut vocab = HashMap::with_capacity(obj.len());
        let mut max_id = FIRST_ORDINARY_ID;
        for (tok, id_val) in obj {
            let Some(raw_id) = id_val.as_usize() else {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("vocab token `{tok}` has non-integer ID"),
                ));
            };
            let id_u32 = u32::try_from(raw_id).map_err(|_| {
                GapError::new(
                    ErrorCode::ValidationError,
                    format!("vocab token `{tok}` ID overflows u32"),
                )
            })?;
            // Skip inserting special bracket markers into ordinary lookup table so raw text cannot emit them.
            if matches!(
                tok.as_str(),
                "[PAD]" | "[UNK]" | "[CLS]" | "[SEP]" | "[MASK]"
            ) {
                continue;
            }
            if id_u32 >= FIRST_ORDINARY_ID {
                max_id = max_id.max(id_u32);
                vocab.insert(tok.clone(), id_u32);
            }
        }
        Ok(Self {
            vocab,
            topic_by_id: BTreeMap::new(),
            vocab_size: (max_id as usize).saturating_add(1).max(DEFAULT_VOCAB_SIZE),
            use_hash_fallback: false,
        })
    }

    /// Vocabulary size covered by this tokenizer.
    #[must_use]
    pub const fn vocab_size(&self) -> usize {
        self.vocab_size
    }

    /// Returns the curated topic bucket index `0..16` associated with a built-in token ID, if any.
    #[must_use]
    pub fn topic_for_token_id(&self, id: u32) -> Option<usize> {
        self.topic_by_id.get(&id).copied()
    }

    /// Returns `true` if `id` corresponds to a sink phrase token ("fulfil", "other", "options", etc.).
    #[must_use]
    pub fn is_sink_token_id(&self, id: u32) -> bool {
        self.topic_for_token_id(id) == Some(15)
    }

    /// Encodes `text` into ordinary token IDs (guaranteed `id >= FIRST_ORDINARY_ID` unless empty/UNK).
    #[must_use]
    pub fn encode_ordinary(&self, text: &str) -> Vec<u32> {
        let mut ids = Vec::new();
        for piece in split_words_and_punct(text) {
            let lower = piece.to_lowercase();
            if lower.is_empty() {
                continue;
            }
            if let Some(&exact_id) = self.vocab.get(&lower)
                && exact_id >= FIRST_ORDINARY_ID
            {
                ids.push(exact_id);
                continue;
            }
            if !self.use_hash_fallback {
                self.encode_wordpiece_fallback(&lower, &mut ids);
                continue;
            }
            // Deterministic hash bucket strictly in `[FIRST_ORDINARY_ID + hash_base, vocab_size)`.
            ids.push(self.hash_token(&lower));
        }
        ids
    }

    fn encode_wordpiece_fallback(&self, word: &str, out: &mut Vec<u32>) {
        if word.len() > 128 {
            out.push(UNK_ID);
            return;
        }
        let mut start = 0;
        let mut sub_tokens = Vec::new();
        while start < word.len() {
            let mut end = word.len();
            let mut matched = None;
            while start < end {
                if !word.is_char_boundary(end) {
                    end -= 1;
                    continue;
                }
                let candidate = if start == 0 {
                    word[start..end].to_owned()
                } else {
                    format!("##{}", &word[start..end])
                };
                if let Some(&id) = self.vocab.get(&candidate)
                    && id >= FIRST_ORDINARY_ID
                {
                    matched = Some((id, end));
                    break;
                }
                end -= 1;
            }
            let Some((id, next_start)) = matched else {
                out.push(UNK_ID);
                return;
            };
            sub_tokens.push(id);
            start = next_start;
        }
        out.extend(sub_tokens);
    }

    fn hash_token(&self, token: &str) -> u32 {
        // Reserve IDs `5..256` for curated vocab and `256..vocab_size` for deterministic hashed tokens.
        let min_hash_id = 256usize
            .min(self.vocab_size.saturating_sub(1))
            .max(FIRST_ORDINARY_ID as usize);
        let span = self.vocab_size.saturating_sub(min_hash_id).max(1);
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in token.as_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        let offset = usize::try_from(h % (span as u64)).unwrap_or(0);
        let raw = min_hash_id + offset;
        u32::try_from(raw)
            .unwrap_or(FIRST_ORDINARY_ID)
            .max(FIRST_ORDINARY_ID)
    }
}

fn split_words_and_punct(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut word_start: Option<usize> = None;

    for (idx, ch) in input.char_indices() {
        if ch.is_alphanumeric() {
            if word_start.is_none() {
                word_start = Some(idx);
            }
        } else {
            if let Some(start) = word_start.take() {
                out.push(&input[start..idx]);
            }
            if !ch.is_whitespace() && !matches!(ch, '"' | '{' | '}' | '[' | ']' | ':' | ',') {
                let end = idx + ch.len_utf8();
                out.push(&input[idx..end]);
            }
        }
    }
    if let Some(start) = word_start {
        out.push(&input[start..]);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn builtin_tokenizer_never_emits_special_ids_for_ordinary_text() {
        let tok = LayaTokenizer::builtin_default();
        let ids = tok.encode_ordinary("hello billing invoice [MASK] [SEP] [CLS] unicode café №1");
        assert!(!ids.is_empty());
        assert!(ids.iter().all(|&id| id >= FIRST_ORDINARY_ID));
    }

    #[test]
    fn vocab_json_wordpiece_tokenizes_cleanly_without_special_id_forgery() {
        let raw = r#"{"\u005BPAD\u005D":0,"\u005BUNK\u005D":1,"\u005BCLS\u005D":2,"\u005BSEP\u005D":3,"\u005BMASK\u005D":4,"bill":10,"unseen":12}"#;
        let tok = LayaTokenizer::from_vocab_json_str(raw).expect("parse vocab");
        let ids = tok.encode_ordinary("bill unseen [MASK]");
        assert_eq!(ids, vec![10, 12, UNK_ID]);
        assert!(!ids.contains(&MASK_ID));
    }
}
