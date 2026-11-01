//! MCP resource listing, templates, and read handlers (`CMP-005`, `GQL-062`).

use std::sync::LazyLock;

use gap_core::context::RequestContext;
use serde_json::{Map, Value, json};

use super::wire::{PUBLIC_TTL_MS, RpcError, WireEra, server_info_meta};
use super::{FacadeInner, discovery};

static STATIC_RESOURCES: LazyLock<Value> = LazyLock::new(|| {
    json!([
        {
            "uri": "gap://meta",
            "name": "meta",
            "description": "L0 supergraph branch overview",
            "mimeType": "application/json"
        },
        {
            "uri": "gap://operations",
            "name": "operations",
            "description": "Registered persisted GAP-QL operations",
            "mimeType": "application/json"
        }
    ])
});

static STATIC_RESOURCE_TEMPLATES: LazyLock<Value> = LazyLock::new(|| {
    json!([
        {
            "uriTemplate": "gap://schema/{type}",
            "name": "schema_type",
            "description": "GraphQL SDL definition for a named schema type",
            "mimeType": "application/graphql"
        },
        {
            "uriTemplate": "gap://task/{id}",
            "name": "task_snapshot",
            "description": "Current state snapshot of a GAP task",
            "mimeType": "application/json"
        }
    ])
});

/// Handles `resources/list` (`CMP-005`).
pub(crate) fn list_resources(inner: &FacadeInner, era: WireEra) -> Value {
    match era {
        WireEra::Modern => json!({
            "resultType": "complete",
            "resources": *STATIC_RESOURCES,
            "ttlMs": PUBLIC_TTL_MS,
            "cacheScope": "public",
            "_meta": server_info_meta(&inner.config),
        }),
        WireEra::Legacy => json!({
            "resources": *STATIC_RESOURCES,
        }),
    }
}

/// Handles `resources/templates/list` (`CMP-005`).
pub(crate) fn list_resource_templates(inner: &FacadeInner, era: WireEra) -> Value {
    match era {
        WireEra::Modern => json!({
            "resultType": "complete",
            "resourceTemplates": *STATIC_RESOURCE_TEMPLATES,
            "ttlMs": PUBLIC_TTL_MS,
            "cacheScope": "public",
            "_meta": server_info_meta(&inner.config),
        }),
        WireEra::Legacy => json!({
            "resourceTemplates": *STATIC_RESOURCE_TEMPLATES,
        }),
    }
}

/// Handles `resources/read` (`CMP-005`, `GQL-062`).
pub(crate) async fn read_resource(
    inner: &FacadeInner,
    ctx: &RequestContext,
    era: WireEra,
    params: &Map<String, Value>,
) -> Result<Value, RpcError> {
    for k in params.keys() {
        if !matches!(k.as_str(), "uri" | "_meta") {
            return Err(RpcError::invalid_params(format!(
                "unknown property at params.{k}"
            )));
        }
    }
    let Some(uri) = params.get("uri").and_then(Value::as_str) else {
        return Err(RpcError::invalid_params(
            "missing required string at params.uri",
        ));
    };
    if uri.is_empty() {
        return Err(RpcError::invalid_params("params.uri must not be empty"));
    }

    let (mime_type, text, force_private) = if uri == "gap://meta" {
        let overview = discovery::render_overview_json(inner, ctx);
        let encoded = serde_json::to_string(&overview).unwrap_or_else(|_| "{}".to_owned());
        ("application/json", encoded, false)
    } else if uri == "gap://operations" {
        let ops = discovery::render_operations_json(inner, ctx);
        let encoded = serde_json::to_string(&ops).unwrap_or_else(|_| "[]".to_owned());
        ("application/json", encoded, false)
    } else if let Some(type_name) = uri.strip_prefix("gap://schema/") {
        if type_name.is_empty() || type_name.contains('/') {
            return Err(RpcError::invalid_params(format!(
                "invalid schema resource URI: `{uri}`"
            )));
        }
        let sdl = discovery::render_types_sdl(inner, ctx, &[type_name.to_owned()], 1)
            .map_err(|_| RpcError::invalid_params(format!("unknown schema resource: `{uri}`")))?;
        ("application/graphql", sdl, false)
    } else if let Some(task_id) = uri.strip_prefix("gap://task/") {
        if task_id.is_empty() || task_id.contains('/') {
            return Err(RpcError::invalid_params(format!(
                "invalid task resource URI: `{uri}`"
            )));
        }
        let task_val = inner
            .tasks
            .get(ctx, task_id)
            .await
            .map_err(|_| RpcError::invalid_params(format!("unknown task resource: `{uri}`")))?;
        let encoded = serde_json::to_string(&task_val).unwrap_or_else(|_| "{}".to_owned());
        ("application/json", encoded, true)
    } else {
        return Err(RpcError::invalid_params(format!(
            "unknown resource URI: `{uri}`"
        )));
    };

    let is_private = force_private || inner.visibility.is_principal_scoped();
    let (cache_scope, ttl_ms) = if is_private {
        ("private", 0_u64)
    } else {
        ("public", PUBLIC_TTL_MS)
    };

    let contents = json!([
        {
            "uri": uri,
            "mimeType": mime_type,
            "text": text,
        }
    ]);

    match era {
        WireEra::Modern => Ok(json!({
            "resultType": "complete",
            "contents": contents,
            "ttlMs": ttl_ms,
            "cacheScope": cache_scope,
            "_meta": server_info_meta(&inner.config),
        })),
        WireEra::Legacy => Ok(json!({
            "contents": contents,
        })),
    }
}
