# GAP ASD-STE100 Technical Manual

## 1. Manual Purpose `[DESCRIPTIVE]`

This manual applies Andrej Karpathy's ASD-STE100 practice to the Graph Agent Protocol (GAP) system. Traditional software documentation often contains long sentences, vague filler phrases, and shifting synonyms. That bloat slows human engineers and confuses AI agents.

This manual adapts aerospace Simplified Technical English (`ASD-STE100`) to remove ambiguity. Every technical noun keeps one name across the entire document set. Every descriptive sentence contains at most 25 words. Every procedural step contains at most 20 words and one action.

## 2. Manual Rules and Chapter Index `[REFERENCE]`

The automated checker [`dist/check-ste100.py`](../../dist/check-ste100.py) audits every file in `docs/ste100/` against the rules below.

| Rule | Limit or Requirement | Scope |
|---|---|---|
| Descriptive sentence length | Maximum 25 words per sentence | `[DESCRIPTIVE]`, `[REFERENCE]`, and callouts |
| Procedural sentence length | Maximum 20 words per sentence | `[PROCEDURAL]` checklists |
| Paragraph size | Maximum 6 sentences per paragraph | All prose sections |
| Voice and tense | Active voice and simple present tense only | All prose sections |
| Step opening verb | Approved imperative verb or `If` condition first | All `[PROCEDURAL]` steps |
| Controlled vocabulary | One term per concept; zero AI-slop filler | All prose sections |

| Chapter | Focus Area | Spec Anchors |
|---|---|---|
| [`00-DICTIONARY.md`](00-DICTIONARY.md) | Approved nouns, approved verbs, and banned filler | `ARC-001`, `CMP-001` |
| [`01-SYSTEM-OVERVIEW.md`](01-SYSTEM-OVERVIEW.md) | Three-plane architecture, invariants, and crossover limits | `ARC-002`, `INV-01`–`INV-20`, `ADR-017` |
| [`02-OPERATING-PROCEDURES.md`](02-OPERATING-PROCEDURES.md) | Build, HTTP/3, GraphQL, MCP, `AG-UI`, and System-1 startup | `OPS-005`, `CMP-008`, `CMP-025`, `RTE-047` |
| [`03-CONNECTORS-AND-INGESTION.md`](03-CONNECTORS-AND-INGESTION.md) | Connectors, proposals, join checks, and snapshot diffs | `NGC-001`, `ING-052`, `ADR-012` |
| [`04-SECURITY-AND-FAULT-PROCEDURES.md`](04-SECURITY-AND-FAULT-PROCEDURES.md) | Cedar policies, audit verification, drift freeze, and backup/restore | `SEC-020`, `SEC-082`, `SEC-092`, `OPS-043` |

## 3. Audit This Manual `[PROCEDURAL]`

1. Open a shell in the GAP repository root.
2. Run the ASD-STE100 compliance linter against this directory.
3. Verify the tool prints `OK: ASD-STE100 score 100/100`.

```bash
python3 dist/check-ste100.py
```
