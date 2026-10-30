//! Ordering and pagination checks (`NGC-021`, `NGC-022`).

use std::collections::BTreeSet;

use gap_core::error::NgcErrorCode;
use gap_proto::ngc::v1::{OrderBy, Page, PaginationKind};
use prost_types::Struct;

use super::{Verdict, not_applicable, not_covered, pass};
use crate::ngc_kit::kit::{
    Collected, KIT_OTHER_TENANT, KIT_TENANT, Kit, canon_all, compare_scalars, field,
    first_difference,
};
use crate::ngc_kit::profile::PageProbe;
use crate::ngc_kit::profile::table_to_struct;

/// A cursor no connector ever issued.
const GARBAGE_CURSOR: &str = "kit-garbage-cursor";

fn pagination(kit: &Kit<'_>) -> PaginationKind {
    kit.desc
        .capabilities
        .as_ref()
        .and_then(|c| PaginationKind::try_from(c.pagination).ok())
        .unwrap_or(PaginationKind::PaginationNone)
}

fn order_by_supported(kit: &Kit<'_>) -> bool {
    kit.desc.capabilities.as_ref().is_some_and(|c| c.order_by)
}

/// One page request.
fn page_request(
    kit: &Kit<'_>,
    probe: &PageProbe,
    args: &toml::Table,
    tenant: &str,
    page: Page,
) -> gap_proto::ngc::v1::QueryRequest {
    let mut req = kit.query_request(&probe.operation, args);
    let timeout = i64::try_from(kit.profile.timing.call_timeout_ms).unwrap_or(i64::MAX);
    req.ctx = Some(kit.ctx(tenant, timeout, ""));
    req.page = Some(page);
    req.arguments = Some(table_to_struct(args));
    req
}

fn rows_in_order(got: &Collected) -> Vec<Struct> {
    got.all_rows().into_iter().cloned().collect()
}

/// Walks a whole traversal with page size `limit`; returns the rows of each page and the first cursor issued.
async fn walk(
    kit: &Kit<'_>,
    probe: &PageProbe,
    args: &toml::Table,
    limit: u32,
) -> Result<(Vec<Vec<Struct>>, Option<String>), String> {
    let kind = pagination(kit);
    let mut pages: Vec<Vec<Struct>> = Vec::new();
    let mut first_cursor = None;
    let mut cursor = String::new();
    let mut offset = 0_u32;
    for _ in 0..probe.max_pages {
        let page = Page {
            limit,
            cursor: cursor.clone(),
            offset,
        };
        let got = kit
            .rows_of(page_request(kit, probe, args, KIT_TENANT, page))
            .await
            .map_err(|e| format!("page {} failed: {e}", pages.len() + 1))?;
        let rows = rows_in_order(&got);
        if rows.len() > usize::try_from(limit).unwrap_or(usize::MAX) {
            return Err(format!(
                "page {} holds {} rows, more than the page limit of {limit}",
                pages.len() + 1,
                rows.len()
            ));
        }
        let next = got
            .complete()
            .map(|c| c.next_cursor.clone())
            .unwrap_or_default();
        let count = rows.len();
        pages.push(rows);
        if kind == PaginationKind::PaginationOffset {
            if count < usize::try_from(limit).unwrap_or(usize::MAX) {
                return Ok((pages, first_cursor));
            }
            offset += limit;
        } else {
            if next.is_empty() {
                return Ok((pages, first_cursor));
            }
            if first_cursor.is_none() {
                first_cursor = Some(next.clone());
            }
            cursor = next;
        }
    }
    Err(format!(
        "the traversal did not end within {} pages",
        probe.max_pages
    ))
}

fn flatten(pages: &[Vec<Struct>]) -> Vec<String> {
    canon_all(pages.iter().flatten())
}

/// `query.order` (`NGC-021`).
pub(crate) async fn order(kit: &Kit<'_>, notes: &mut Vec<String>) -> Verdict {
    let kind = pagination(kit);
    let ordered = order_by_supported(kit);
    if !ordered && kind == PaginationKind::PaginationNone {
        return not_applicable(
            "neither order_by nor pagination is advertised: row order is unconstrained (NGC-021)",
        );
    }
    let Some(probe) = kit.profile.page.as_ref() else {
        return not_covered("ordering is constrained but the profile has no [page] section");
    };

    if kind != PaginationKind::PaginationNone {
        // Deterministic when a page is used: the same first page twice.
        let mut firsts = Vec::new();
        for _ in 0..2 {
            let got = kit
                .rows_of(page_request(
                    kit,
                    probe,
                    &probe.arguments,
                    KIT_TENANT,
                    Page {
                        limit: probe.limit,
                        cursor: String::new(),
                        offset: 0,
                    },
                ))
                .await
                .map_err(|e| format!("first page failed: {e}"))?;
            firsts.push(canon_all(got.all_rows()));
        }
        if firsts[0] != firsts[1] {
            return Err(format!(
                "the same first page came back in a different order ({}): paged results must be deterministic (NGC-021)",
                first_difference(&firsts[0], &firsts[1])
            ));
        }
    }

    if ordered {
        let Some(path) = probe.order_by.as_deref() else {
            notes.push("order_by is advertised but the profile has no page.order_by: sorting was not checked".to_owned());
            return pass();
        };
        for descending in [false, true] {
            let mut req = kit.query_request(&probe.operation, &probe.arguments);
            req.order_by = vec![OrderBy {
                path: path.to_owned(),
                descending,
            }];
            let got = kit
                .rows_of(req)
                .await
                .map_err(|e| format!("order_by {path} failed: {e}"))?;
            let values: Vec<_> = got
                .all_rows()
                .into_iter()
                .filter_map(|row| field(row, path))
                .collect();
            if values.len() < 2 {
                return Err(format!(
                    "order_by `{path}` needs at least two rows carrying that field to be checked"
                ));
            }
            for pair in values.windows(2) {
                let Some(ordering) = compare_scalars(&pair[0], &pair[1]) else {
                    return Err(format!(
                        "order_by `{path}`: values {} and {} cannot be compared",
                        pair[0], pair[1]
                    ));
                };
                let wrong = if descending {
                    ordering == std::cmp::Ordering::Less
                } else {
                    ordering == std::cmp::Ordering::Greater
                };
                if wrong {
                    return Err(format!(
                        "rows are not sorted {} by `{path}` ({} came before {}): order_by must be honored (NGC-021)",
                        if descending {
                            "descending"
                        } else {
                            "ascending"
                        },
                        pair[0],
                        pair[1]
                    ));
                }
            }
        }
    }
    pass()
}

/// A failed call that must have been refused for the right reason.
fn refusal(got: &Collected, what: &str, strict_invalid: bool) -> Result<(), String> {
    match &got.failure {
        None => Err(format!("{what} was accepted (NGC-022)")),
        Some(err) => {
            let acceptable = if strict_invalid {
                err.code == NgcErrorCode::InvalidArgument
            } else {
                matches!(
                    err.code,
                    NgcErrorCode::InvalidArgument
                        | NgcErrorCode::PermissionDenied
                        | NgcErrorCode::FailedPrecondition
                        | NgcErrorCode::NotFound
                )
            };
            if acceptable {
                Ok(())
            } else {
                Err(format!(
                    "{what} failed {:?} instead of INVALID_ARGUMENT (NGC-022)",
                    err.code
                ))
            }
        }
    }
}

/// `page.stability` (`NGC-021`, `NGC-022`).
pub(crate) async fn stability(kit: &Kit<'_>, notes: &mut Vec<String>) -> Verdict {
    let kind = pagination(kit);
    if kind == PaginationKind::PaginationNone {
        return not_applicable("pagination is not advertised");
    }
    let Some(probe) = kit.profile.page.as_ref() else {
        return not_covered("pagination is advertised but the profile has no [page] section");
    };

    let (first, first_cursor) = walk(kit, probe, &probe.arguments, probe.limit).await?;
    if first.len() < 2 {
        return Err(format!(
            "the traversal ended after {} page(s); page.limit {} is not smaller than the data set, so pagination cannot be exercised",
            first.len(),
            probe.limit
        ));
    }
    let flat = flatten(&first);
    let distinct: BTreeSet<&String> = flat.iter().collect();
    if distinct.len() != flat.len() {
        return Err(
            "a row appears more than once across pages: page boundaries overlap (NGC-021)"
                .to_owned(),
        );
    }

    let (again, _) = walk(kit, probe, &probe.arguments, probe.limit).await?;
    let again_flat = flatten(&again);
    if again_flat != flat {
        return Err(format!(
            "two traversals with page size {} returned different rows or order ({}): paging must be deterministic (NGC-021)",
            probe.limit,
            first_difference(&flat, &again_flat)
        ));
    }

    let max_page = kit.desc.limits.as_ref().map_or(0, |l| l.max_page_size);
    let other_limit = if max_page == 0 || probe.limit < max_page {
        probe.limit + 1
    } else {
        probe.limit.saturating_sub(1).max(1)
    };
    if other_limit != probe.limit {
        let (resized, _) = walk(kit, probe, &probe.arguments, other_limit).await?;
        let resized_flat = flatten(&resized);
        if resized_flat != flat {
            return Err(format!(
                "page size {other_limit} returned different rows than page size {} ({}): the page size must not change the result (NGC-021)",
                probe.limit,
                first_difference(&flat, &resized_flat)
            ));
        }
    }

    if kind == PaginationKind::PaginationCursor {
        let Some(cursor) = first_cursor else {
            return Err("a multi-page traversal issued no cursor".to_owned());
        };
        let with = |args: &toml::Table, tenant: &str, cursor: &str| {
            page_request(
                kit,
                probe,
                args,
                tenant,
                Page {
                    limit: probe.limit,
                    cursor: cursor.to_owned(),
                    offset: 0,
                },
            )
        };
        if let Some(other) = &probe.other_arguments {
            let got = kit.collect(with(other, KIT_TENANT, &cursor)).await;
            refusal(
                &got,
                "a cursor issued for one set of arguments used with another",
                false,
            )?;
        } else {
            notes.push(
                "profile has no page.other_arguments: argument binding of cursors was not checked"
                    .to_owned(),
            );
        }
        let got = kit
            .collect(with(&probe.arguments, KIT_OTHER_TENANT, &cursor))
            .await;
        refusal(&got, "a cursor issued to one tenant used by another", false)?;
        let got = kit
            .collect(with(&probe.arguments, KIT_TENANT, GARBAGE_CURSOR))
            .await;
        refusal(&got, "a cursor the connector never issued", true)?;
    }
    pass()
}
