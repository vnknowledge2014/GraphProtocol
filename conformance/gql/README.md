# GAP-QL conformance corpus (`conformance/gql`)

Black-box fixtures in the format of `spec/02-gap-ql.md` §11, runnable against any GAP endpoint and, offline, against an in-process server:

```bash
cargo run -p gap-conformance -- --class Core --offline conformance/gql          # in-process server, no sockets
cargo run -p gap-conformance -- --class Core --target http://127.0.0.1:8080/graphql --fixtures conformance/gql   # live endpoint
```

## The offline test server

`--offline <DIR>` builds one server per distinct `schema` named by the fixtures (`fixtures/shop.graphql`): the SDL is one **branch** (named after the file), composed with `gap_snapshot::compose`, served through the real `gap_transport` router over an in-memory NGC connection, with one operation per root `Query`/`Mutation` field.
Its connector answers every operation with a completed **empty** row set, so request errors, limits and cost are exercised for real while an executed field resolves to `null` / `[]`. Composition namespaces branch types (`shop_Order`, RUN-009): documents that name a type (fragments, variable types) write the namespaced name. Principals named by fixtures get a generated API key.

## Coverage

| Spec id | Fixtures | Notes |
|---|---|---|
| GQL-001, GQL-002 | `001`..`045` valid documents, parse failures (`400`), validation failures (`422`), variable coercion | `tests/differential/` cross-checks 63 more documents against graphql-js |
| GQL-003 | status, media type `application/graphql-response+json`, operation resolution | transport-level cases (`GET`, `Accept`, `Content-Type`) are not expressible: the fixture format has no method/headers; covered by `gap-transport` tests |
| GQL-040 | depth 12/13, aliases 50/51, root fields 20/21 | limits of spec/02 §8 (`traverse` itself arrives with M2) |
| GQL-041, GQL-050 | planned cost per the formula (`extensions.gap.cost.planned`), cost refusal, `generation`, `partial` | |
| GQL-061 | matching, mismatching and malformed `persistedQuery` hash | registration and `GET` are transport tests |
| GQL-070 | `didYouMean`, `missingArguments` | `template` is not asserted: its text is not normative |
| GQL-072 | `conformance_gql_error_messages_are_short_and_carry_a_correlation_id` | cross-cutting: not expressible in a fixture |

Class `Discovery` fixtures (`__typename`, `__schema`, `gap.metaGraph`; GQL-010, GQL-023, GQL-024) document what the M1 pipeline does **not** serve yet (`GAP_UNSUPPORTED_FEATURE`: the planner refuses built-in root fields); they close with `WP-M2-05`.
Not covered by this corpus, with the owner: GQL-004, GQL-030..032 (composition: `gap-snapshot` tests), GQL-011/012/020..022 (discovery: M2), GQL-042/043 (M2/M5), GQL-051 (needs a failing branch: `gap-exec` tests), GQL-060 (operation mode: `gap-ql` tests), GQL-062 (visibility: needs policy, M6), GQL-071 (M2).

## Regenerating

The fixtures are plain JSON, derived from the specification (expectations are the spec's verdicts, not recordings of the server). Keep them sorted by number; every fixture needs `name`, `class`, `spec`, `request`, `expect` and, for this server, `schema`.
