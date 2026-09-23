#[cfg(test)]
use std::collections::HashMap;
use std::path::Path;

use tracing::info;

use crate::config::{
    GatewayConfig, KeyStrategy, ProviderConfig, ProviderType, ProxyMode, RouteConfig, RouteTarget,
    Strategy,
};
use crate::loader::json_util::{check_json_map, json_to_toml_value};
use crate::toml_edit_util::{
    ensure_array_field, ensure_inline_table_field, ensure_nested_inline_table, prune_keys,
    sync_array_of_tables, sync_inline_table_array_in_place, sync_inline_table_in_place,
    sync_string_array_in_place, upsert_table,
};

/// Persist the in-memory `GatewayConfig` back to the TOML file.
///
/// Uses `toml_edit` to parse the existing file and performs fine-grained
/// in-place updates. **All mutable sections are written back precisely:**
/// `[providers.*]`, `[[routes]]`, and `[backoff]` are all updated in
/// place, preserving comments and field order in any field/table the
/// in-memory config does not explicitly touch.
///
/// `[gateway_auth]` is written back as well: the Settings page manages
/// client API keys, so the admin save path owns `gateway_auth.api_keys`.
/// An empty key list disables authentication (same as omitting the
/// section) and is persisted as `api_keys = []`.
///
/// All other sections — `[server]`, `[analytics]`, `[circuit_breaker]`,
/// `[logging]`, and the top-level scalar `shuffle_cache_ttl_secs` — are
/// intentionally **not** written back. The admin save path only mutates
/// the sections above, so any user-authored values (and comments) in
/// these untouched sections are preserved verbatim across saves. This
/// also means admin-initiated saves won't accidentally revert hand-edited
/// tuning of these read-only fields.
///
/// # Concurrency and crash-safety
///
/// This function performs a **read-then-render-then-atomic-write** (parse
/// existing file -> mutate in-memory -> write temp file -> `sync_all` ->
/// `rename`). The final `rename(2)` is atomic on Unix, so a crash during
/// the write leaves the old file intact rather than a truncated partial
/// write.
///
/// **Process-level serialization**: the `ConfigMutationCoordinator` in
/// `sag-state` serializes all production writers (admin API + hot-reload)
/// via a `tokio::sync::Mutex`. Concurrent mutations within the same
/// process are serialized, so no lost-update occurs.
///
/// **Error-path behaviour**: if `existing.parse()`, `config.validate()`,
/// temp-file creation, or `sync_all` fails, the function returns `Err`
/// without touching the destination file – the original config file is
/// preserved untouched (all-or-nothing). The temp file is cleaned up on
/// failure.
///
/// **Cross-process TOCTOU**: an external editor saving concurrently with
/// an admin API mutation can still race the read and rename. The
/// `ConfigMutationCoordinator` detects external edits by comparing the
/// file's content hash against an accepted revision before writing; if
/// they differ, it returns `AppError::Conflict`. A narrow TOCTOU window
/// remains between the final check and the rename when the external
/// editor does not participate in any lock protocol.
pub fn save_config_to_file(config: &GatewayConfig, path: &Path) -> Result<(), sag_core::AppError> {
    // Validate no null values before writing.
    for (name, provider) in &config.providers {
        check_json_map(
            &provider.body_overrides,
            &format!("providers.{name}.body_overrides"),
        )?;
        for (model, overrides) in &provider.model_overrides {
            check_json_map(
                overrides,
                &format!("providers.{name}.model_overrides.{model}"),
            )?;
        }
    }
    for (i, route) in config.routes.iter().enumerate() {
        for (j, target) in route.targets.iter().enumerate() {
            check_json_map(&target.extra, &format!("routes[{i}].targets[{j}].extra"))?;
        }
    }

    // Read the existing file (empty string if missing — toml_edit accepts
    // that as a blank document).
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .map_err(|e| sag_core::AppError::Config(format!("failed to parse existing config: {e}")))?;

    // ── [providers.*] — per-field update via upsert_table ──
    for (name, provider) in &config.providers {
        upsert_table(
            &mut doc,
            &["providers", name],
            || {
                // New provider: serialize entirely via to_document so
                // `skip_serializing_if` is honored.
                let pd = toml_edit::ser::to_document(provider).map_err(|e| {
                    sag_core::AppError::Config(format!(
                        "failed to serialize provider '{name}': {e}"
                    ))
                })?;
                let mut table = (*pd).clone();
                // Strip anthropic_cache_control unless this is an Anthropic provider
                // with an explicit opt-out (false). Same predicate as the update path.
                if !(matches!(provider.provider_type, ProviderType::Anthropic)
                    && !provider.anthropic_cache_control)
                {
                    table.remove("anthropic_cache_control");
                }
                Ok(toml_edit::Item::Table(table))
            },
            |item| update_existing_provider(item, provider),
        )?;
    }
    // Remove providers that no longer exist in memory.
    let provider_names: Vec<&str> = config.providers.keys().map(String::as_str).collect();
    prune_keys(&mut doc, &["providers"], &provider_names);

    // ── [backoff] — single table upsert ──
    upsert_table(
        &mut doc,
        &["backoff"],
        || {
            let pd = toml_edit::ser::to_document(&config.backoff).map_err(|e| {
                sag_core::AppError::Config(format!("failed to serialize backoff: {e}"))
            })?;
            Ok(toml_edit::Item::Table((*pd).clone()))
        },
        |item| {
            item["retries"] = toml_edit::value(config.backoff.retries as i64);
            item["base_ms"] = toml_edit::value(config.backoff.base_ms as i64);
            item["max_ms"] = toml_edit::value(config.backoff.max_ms as i64);
            item["jitter"] = toml_edit::value(config.backoff.jitter);
            item["retry_on_empty_response"] =
                toml_edit::value(config.backoff.retry_on_empty_response);
            Ok(())
        },
    )?;

    // ── [gateway_auth] — write or leave absent ──
    // The Settings page owns client API keys, so this section is written
    // back. Field-level update via upsert_table preserves comments and
    // any future sibling fields. Empty/`None` removes the section so a
    // cleared auth config leaves no stale `api_keys` behind (empty list
    // and missing section are equivalent: auth disabled).
    match &config.gateway_auth {
        Some(auth) if !auth.api_keys.is_empty() => {
            let keys = auth.api_keys.clone();
            upsert_table(
                &mut doc,
                &["gateway_auth"],
                || {
                    let mut table = toml_edit::Table::new();
                    let array = ensure_array_field(&mut table, "api_keys");
                    sync_string_array_in_place(array, &keys);
                    Ok(toml_edit::Item::Table(table))
                },
                |item| {
                    let table = item.as_table_mut().ok_or_else(|| {
                        sag_core::AppError::Config(
                            "gateway_auth is not a table in the config file".into(),
                        )
                    })?;
                    sync_string_array_in_place(ensure_array_field(table, "api_keys"), &keys);
                    Ok(())
                },
            )?;
        }
        _ => {
            if let Some(table) = doc.as_table_mut().get_mut("gateway_auth")
                && let Some(inner) = table.as_table_mut()
            {
                inner.remove("api_keys");
                // Drop the section entirely when it has no remaining fields
                // (avoid leaving a bare `[gateway_auth]` behind).
                if inner.is_empty() {
                    doc.as_table_mut().remove("gateway_auth");
                }
            }
        }
    }

    // ── [[routes]] — array of tables, key-matched on model_name ──
    sync_array_of_tables(
        &mut doc,
        &["routes"],
        "model_name",
        &config.routes,
        |r| r.model_name.as_str(),
        |r| {
            let rd = toml_edit::ser::to_document(r).map_err(|e| {
                sag_core::AppError::Config(format!("failed to serialize route: {e}"))
            })?;
            Ok((*rd).clone())
        },
        write_route_fields,
    )?;

    // ── top-level proxy_url — write or remove ──
    // The admin save path owns the top-level `proxy_url` scalar (unlike
    // `shuffle_cache_ttl_secs`, which is intentionally read-only).
    // `Some` writes the URL trimmed of surrounding whitespace (so a
    // `Some("  http://…  ")` value persists canonically); `None` — or a
    // value that is empty/whitespace-only after trimming — removes the
    // key so a cleared global proxy leaves no stale `proxy_url` behind
    // (and never an empty string). Everything else in the document —
    // comments, other top-level scalars and tables — is preserved.
    match config.proxy_url.as_deref().map(str::trim) {
        Some(url) if !url.is_empty() => {
            doc["proxy_url"] = toml_edit::value(url);
        }
        _ => {
            doc.as_table_mut().remove("proxy_url");
        }
    }

    // ── Write back (atomic: temp-file + rename) ──
    // Multi-line layout for inline-table-arrays (e.g. `targets`) is
    // handled natively by `toml_edit` 0.24.1 via per-item
    // `decor_mut().set_prefix()` set inside
    // `sync_inline_table_array_in_place`. No string-level
    // post-processing is needed - the rendered document already
    // contains multi-line arrays.
    let rendered = doc.to_string();
    atomic_write(path, &rendered)?;

    info!("config persisted to {}", path.display());
    Ok(())
}

/// Atomically write `content` to `path` via a temp-file in the same
/// directory followed by `rename(2)`.
///
/// On Unix, `rename` atomically overwrites the destination. On Windows,
/// `std::fs::rename` fails if the target exists, so this function is
/// currently Unix-only for the overwrite case. SAG targets Linux/macOS
/// deployments; Windows support would require a separate
/// `rename-with-replace` implementation.
///
/// Steps:
/// 1. Write `content` to a unique temp file in the same directory.
/// 2. `sync_all` the temp file so data reaches disk before the rename.
/// 3. `rename(temp, path)` – atomic on Unix.
/// 4. On any failure, clean up the temp file and return `Err` without
///    touching the destination.
fn atomic_write(path: &Path, content: &str) -> Result<(), sag_core::AppError> {
    let dir = path
        .parent()
        .ok_or_else(|| sag_core::AppError::Config("config path has no parent directory".into()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let temp_path = dir.join(format!(".{file_name}.tmp.{}", std::process::id()));

    // 1. Write to temp file.
    std::fs::write(&temp_path, content).map_err(|e| {
        sag_core::AppError::Config(format!("failed to write temp config file: {e}"))
    })?;

    // 2. Sync the temp file.
    let file = std::fs::File::open(&temp_path).map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        sag_core::AppError::Config(format!("failed to open temp config for sync: {e}"))
    })?;
    file.sync_all().map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        sag_core::AppError::Config(format!("failed to sync temp config file: {e}"))
    })?;
    drop(file);

    // 3. Atomic rename (overwrites destination on Unix).
    std::fs::rename(&temp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        sag_core::AppError::Config(format!("failed to rename temp config file: {e}"))
    })?;

    Ok(())
}

/// Update fields of an existing provider's TOML sub-table
/// (`[providers.<name>]`). Preserves comments and any field not explicitly
/// written here.
///
/// All HashMap- and Vec-backed fields (api_keys, enabled_models, headers,
/// body_overrides) use the precise-edit pattern: they preserve on-disk
/// order, remove stale entries, and append new entries in deterministic
/// order. This means the user's comments and ordering are kept across
/// saves — previously these were rebuilt from scratch via `HashMap`
/// iteration, which lost comments and produced non-deterministic output.
fn update_existing_provider(
    item: &mut toml_edit::Item,
    provider: &ProviderConfig,
) -> Result<(), sag_core::AppError> {
    // The `item` is a provider sub-table (a `&mut Item` whose value is a
    // `Table`). We resolve it to `&mut Table` once and use it
    // consistently — `toml_edit`'s `Table` type does not have an
    // `as_item_mut()` method, so we go the other way. Mixing `table[...]`
    // and `item[...]` indexing in the same scope would be a borrow
    // conflict (both are `&mut` of the same data), so everything below
    // mutates through `table`.
    let table = item.as_table_mut().expect("provider item must be a table");

    // api_keys — sync as a string array (no-op if list is empty and the
    // field doesn't exist; removes the field if the list is empty and
    // the field does exist).
    if provider.api_keys.is_empty() {
        table.remove("api_keys");
    } else {
        let arr = ensure_array_field(table, "api_keys");
        sync_string_array_in_place(arr, &provider.api_keys);
    }
    // enabled_models — same treatment
    if provider.enabled_models.is_empty() {
        table.remove("enabled_models");
    } else {
        let arr = ensure_array_field(table, "enabled_models");
        sync_string_array_in_place(arr, &provider.enabled_models);
    }
    // scalars
    table["base_url"] = toml_edit::value(provider.base_url.as_str());
    table["models_endpoint"] = toml_edit::value(provider.models_endpoint.as_str());
    table["timeout_secs"] = toml_edit::value(provider.timeout_secs as i64);
    // provider_type — skip if default (mirrors `skip_serializing_if =
    // "is_default_provider_type"` on `ProviderConfig::provider_type`). Without
    // this guard the update path would force-write `provider_type = "openai"`
    // for every minimal user-authored provider block that omits the field,
    // contradicting both the load-time default semantics and what the
    // create path (`to_document`) emits.
    if provider.provider_type != ProviderType::default() {
        table["provider_type"] = toml_edit::value(provider.provider_type.as_str());
    } else {
        table.remove("provider_type");
    }
    // key_strategy — skip if default (mirrors `skip_serializing_if =
    // "is_default_key_strategy"`). Same rationale as `provider_type` above.
    if provider.key_strategy != KeyStrategy::default() {
        table["key_strategy"] = toml_edit::value(match provider.key_strategy {
            KeyStrategy::FillFirst => "fill_first",
            KeyStrategy::RoundRobin => "round_robin",
        });
    } else {
        table.remove("key_strategy");
    }
    // proxy_mode — skip if default (mirrors `skip_serializing_if =
    // "is_default_proxy_mode"`). `inherit` is the load-time default, so
    // omitting the key on disk is both lossless and keeps user-authored
    // provider blocks minimal.
    if provider.proxy_mode != ProxyMode::default() {
        table["proxy_mode"] = toml_edit::value(provider.proxy_mode.as_str());
    } else {
        table.remove("proxy_mode");
    }
    table["enabled"] = toml_edit::value(provider.enabled);
    // headers — inline table; remove the key entirely if empty.
    if provider.headers.is_empty() {
        table.remove("headers");
    } else {
        let headers = ensure_inline_table_field(table, "headers");
        sync_inline_table_in_place(headers, &provider.headers, |v| {
            Ok(toml_edit::Value::from(v.as_str()))
        })?;
    }
    // model_overrides — inline table keyed by model name; remove
    // key entirely if empty. Saved as inline-table syntax (like
    // headers / body_overrides / role_mappings):
    //   model_overrides = { "model-a" = { k = "v" } }
    if provider.model_overrides.is_empty() {
        table.remove("model_overrides");
    } else {
        let mo_inline = ensure_inline_table_field(table, "model_overrides");

        // Remove stale model entries.
        let model_set: std::collections::HashSet<&str> = provider
            .model_overrides
            .keys()
            .map(String::as_str)
            .collect();
        let stale: Vec<String> = mo_inline
            .iter()
            .map(|(k, _)| k.to_string())
            .filter(|k| !model_set.contains(k.as_str()))
            .collect();
        for k in stale {
            mo_inline.remove(k.as_str());
        }
        // Create or update each model's overrides as a nested inline table.
        for (model, overrides) in &provider.model_overrides {
            if overrides.is_empty() {
                mo_inline.remove(model);
            } else {
                let model_table = ensure_nested_inline_table(mo_inline, model);
                sync_inline_table_in_place(model_table, overrides, json_to_toml_value)?;
            }
        }
    }
    if provider.body_overrides.is_empty() {
        table.remove("body_overrides");
    } else {
        let body_overrides = ensure_inline_table_field(table, "body_overrides");
        sync_inline_table_in_place(body_overrides, &provider.body_overrides, json_to_toml_value)?;
    }
    // strip_fields — string array; remove if empty
    if provider.strip_fields.is_empty() {
        table.remove("strip_fields");
    } else {
        let arr = ensure_array_field(table, "strip_fields");
        sync_string_array_in_place(arr, &provider.strip_fields);
    }
    // model_strip_fields — inline table keyed by model name -> string array; remove if empty
    if provider.model_strip_fields.is_empty() {
        table.remove("model_strip_fields");
    } else {
        let msf_inline = ensure_inline_table_field(table, "model_strip_fields");
        let model_set: std::collections::HashSet<&str> = provider
            .model_strip_fields
            .keys()
            .map(String::as_str)
            .collect();
        let stale: Vec<String> = msf_inline
            .iter()
            .map(|(k, _)| k.to_string())
            .filter(|k| !model_set.contains(k.as_str()))
            .collect();
        for k in stale {
            msf_inline.remove(k.as_str());
        }
        for (model, fields) in &provider.model_strip_fields {
            if fields.is_empty() {
                msf_inline.remove(model);
            } else {
                let array_val = toml_edit::Value::Array(
                    fields
                        .iter()
                        .map(|s| toml_edit::Value::from(s.as_str()))
                        .collect(),
                );
                msf_inline.insert(model, array_val);
            }
        }
    }
    // chunk_timeout_secs — skip if None (mirrors skip_serializing_if = "Option::is_none")
    if let Some(secs) = provider.chunk_timeout_secs {
        table["chunk_timeout_secs"] = toml_edit::value(secs as i64);
    } else {
        table.remove("chunk_timeout_secs");
    }
    // ttfb_timeout_secs — same treatment
    if let Some(secs) = provider.ttfb_timeout_secs {
        table["ttfb_timeout_secs"] = toml_edit::value(secs as i64);
    } else {
        table.remove("ttfb_timeout_secs");
    }
    // role_mappings — inline table, remove if empty
    if provider.role_mappings.is_empty() {
        table.remove("role_mappings");
    } else {
        let rm = ensure_inline_table_field(table, "role_mappings");
        sync_inline_table_in_place(rm, &provider.role_mappings, |v| {
            Ok(toml_edit::Value::from(v.as_str()))
        })?;
    }
    // anthropic_cache_control — only meaningful for Anthropic providers.
    // Keep only the explicit opt-out (false); default `true` and any value
    // on non-Anthropic providers are stripped so config files stay clean.
    if matches!(provider.provider_type, ProviderType::Anthropic)
        && !provider.anthropic_cache_control
    {
        table["anthropic_cache_control"] = toml_edit::value(false);
    } else {
        table.remove("anthropic_cache_control");
    }
    Ok(())
}

/// Update fields of an existing route's TOML sub-table (a single
/// `[[routes]]` element). Mirrors the `skip_serializing_if` rules on
/// `RouteConfig` / `RouteTarget` so untouched fields stay absent and
/// existing comments on present fields are preserved.
///
/// The `targets` array is synced with [`sync_inline_table_array_in_place`]
/// matched on the **composite** `(provider, real_model)` key (with
/// `real_model`'s `Option::None` for targets that omit the field) — each
/// target's inline table is updated in place, so user-authored comments,
/// decorator order, and the on-disk ordering of `extra` keys are all
/// preserved across saves. Composite keying is required because the same
/// `provider` may legitimately appear in multiple targets when their
/// `real_model`s differ (e.g. `[{ provider="openai", real_model="gpt-4"
/// }, { provider="openai", real_model="gpt-3.5-turbo" }]`); matching
/// on `provider` alone would silently confuse the two on save.
fn write_route_fields(
    existing: &mut toml_edit::Table,
    route: &RouteConfig,
) -> Result<(), sag_core::AppError> {
    // model_name — always written
    existing["model_name"] = toml_edit::value(route.model_name.as_str());

    // api_endpoint — skip if default (use the encapsulated predicate so
    // that "what counts as the default" stays a private detail of the
    // config module).
    if !route.uses_default_api_endpoint() {
        existing["api_endpoint"] = toml_edit::value(route.api_endpoint.as_str());
    } else {
        existing.remove("api_endpoint");
    }

    // strategy — skip if default
    if route.strategy != Strategy::default() {
        existing["strategy"] = toml_edit::value(match route.strategy {
            Strategy::StickyFailover => "sticky_failover",
            Strategy::FillFirst => "fill_first",
            Strategy::RoundRobin => "round_robin",
        });
    } else {
        existing.remove("strategy");
    }

    // retries — skip if None
    if let Some(r) = route.retries {
        existing["retries"] = toml_edit::value(r as i64);
    } else {
        existing.remove("retries");
    }

    // timeout_secs — skip if None (mirrors skip_serializing_if = "Option::is_none")
    if let Some(ts) = route.timeout_secs {
        existing["timeout_secs"] = toml_edit::value(ts as i64);
    } else {
        existing.remove("timeout_secs");
    }

    // ── targets — sync as inline table array, matched on ──
    // `(provider, real_model)` composite key. See doc on this function for
    // why composite keying is required; single-field `provider` matching
    // would silently overwrite the second target when two targets in the
    // same route share a `provider` but differ in `real_model`.
    //
    // Ensure the targets array exists and is the right type. If the user
    // (or an older version of the file) has `targets = something-else`,
    // replace it with an empty array.
    let arr = ensure_array_field(existing, "targets");

    sync_inline_table_array_in_place(
        arr,
        &route.targets,
        &["provider", "real_model"],
        |t| vec![Some(t.provider.clone()), t.real_model.clone()],
        build_target_table,
        write_target_fields,
    )?;

    // Multi-line layout (`[\n    { ... },\n    { ... }\n]`) for
    // inline-table arrays is handled natively by `toml_edit` 0.24.1
    // via per-item `decor_mut().set_prefix()` set inside
    // `sync_inline_table_array_in_place` — no string-level
    // post-processing is needed.
    // the payload and skips).
    Ok(())
}

/// Build a brand-new `InlineTable` for a route target. Used by
/// `write_route_fields` for the `on_create` closure of the inline-table
/// array sync. For the update path, [`write_target_fields`] is called
/// directly on the existing inline table.
fn build_target_table(target: &RouteTarget) -> Result<toml_edit::InlineTable, sag_core::AppError> {
    let mut new_table = toml_edit::InlineTable::new();
    write_target_fields(&mut new_table, target)?;
    Ok(new_table)
}

/// Update fields of an existing route target's inline table in place.
/// Mirrors the `skip_serializing_if` rules on `RouteTarget` so untouched
/// fields stay absent and existing comments / key order on present fields
/// are preserved. The `extra` map is synced via
/// [`toml_edit_util::sync_inline_table_in_place`] to preserve the user's
/// on-disk key ordering inside the nested `extra = { ... }` table.
fn write_target_fields(
    existing: &mut toml_edit::InlineTable,
    target: &RouteTarget,
) -> Result<(), sag_core::AppError> {
    // provider — always written (insert preserves position if the key
    // already exists, so the user's ordering is retained).
    existing.insert("provider", toml_edit::Value::from(target.provider.as_str()));

    // real_model — skip if None
    if let Some(ref rm) = target.real_model {
        existing.insert("real_model", toml_edit::Value::from(rm.as_str()));
    } else {
        existing.remove("real_model");
    }

    // priority — skip if default (0)
    if target.priority != 0 {
        existing.insert("priority", toml_edit::Value::from(target.priority as i64));
    } else {
        existing.remove("priority");
    }

    // retries — skip if None
    if let Some(r) = target.retries {
        existing.insert("retries", toml_edit::Value::from(r as i64));
    } else {
        existing.remove("retries");
    }

    // timeout_secs — skip if None (mirrors skip_serializing_if = "Option::is_none")
    if let Some(ts) = target.timeout_secs {
        existing.insert("timeout_secs", toml_edit::Value::from(ts as i64));
    } else {
        existing.remove("timeout_secs");
    }

    // extra — serialized as a single nested inline table
    // (`extra = { ... }`). If empty, drop the field entirely; otherwise
    // ensure the inline table exists and sync its keys in on-disk order.
    if target.extra.is_empty() {
        existing.remove("extra");
    } else {
        let extra_table = ensure_nested_inline_table(existing, "extra");
        sync_inline_table_in_place(extra_table, &target.extra, json_to_toml_value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use crate::config::*;
    use crate::loader::load::load_config;

    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_toml(content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let id = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let file_path = dir.join(format!("sag-save-test-{}-{}.toml", std::process::id(), id));
        let mut file = std::fs::File::create(&file_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file_path
    }

    fn minimal_provider() -> ProviderConfig {
        ProviderConfig {
            provider_type: ProviderType::OpenAI,
            api_keys: vec!["sk-test".into()],
            key_strategy: KeyStrategy::FillFirst,
            proxy_mode: ProxyMode::Inherit,
            base_url: "https://api.openai.com/v1".into(),
            models_endpoint: "models".into(),
            timeout_secs: 120,
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
            strip_fields: vec![],
            model_strip_fields: HashMap::new(),
            model_overrides: HashMap::new(),
            role_mappings: HashMap::new(),
            enabled: true,
            enabled_models: vec![],
            anthropic_cache_control: true,
            chunk_timeout_secs: None,
            ttfb_timeout_secs: None,
        }
    }

    fn openai_only_config() -> GatewayConfig {
        GatewayConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 9090,
            },
            gateway_auth: None,
            providers: HashMap::from([("openai".into(), minimal_provider())]),
            ..crate::gateway_config_base!()
        }
    }

    // ── basic save tests ──

    #[test]
    fn save_preserves_provider_subtable_format() {
        let content = r#"[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("[providers.openai]"),
            "sub-table format should be preserved, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_preserves_comments_in_providers() {
        let content = r#"[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
# This is a user comment
api_keys = ["sk-test"]
# Another comment
base_url = "https://custom.url"
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("# This is a user comment"),
            "comment should be preserved, got:\n{saved}"
        );
        assert!(
            saved.contains("# Another comment"),
            "second comment should be preserved, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_preserves_unrelated_provider() {
        let content = r#"[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]

[providers.anthropic]
provider_type = "anthropic"
api_keys = ["sk-ant"]
base_url = "https://api.anthropic.com"
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("[providers.openai]"),
            "openai should remain sub-table, got:\n{saved}"
        );
        assert!(
            saved.contains("[providers.anthropic]"),
            "anthropic should remain sub-table, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn none_options_not_written() {
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let config = GatewayConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 9090,
            },
            gateway_auth: None,
            providers: HashMap::from([(
                "openai".into(),
                ProviderConfig {
                    provider_type: ProviderType::OpenAI,
                    api_keys: vec!["sk-test".into()],
                    key_strategy: KeyStrategy::FillFirst,
                    proxy_mode: ProxyMode::Inherit,
                    base_url: "https://api.openai.com/v1".into(),
                    models_endpoint: "models".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
                    strip_fields: vec![],
                    model_strip_fields: HashMap::new(),
                    model_overrides: HashMap::new(),
                    role_mappings: HashMap::new(),
                    enabled: true,
                    enabled_models: vec![],
                    anthropic_cache_control: true,
                    chunk_timeout_secs: None,
                    ttfb_timeout_secs: None,
                },
            )]),
            routes: vec![RouteConfig {
                model_name: "gpt-4".into(),
                api_endpoint: "/chat/completions".into(),
                strategy: Strategy::FillFirst,
                retries: None,
                timeout_secs: None,
                targets: vec![RouteTarget {
                    provider: "openai".into(),
                    real_model: None,
                    priority: 0,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::new(),
                }],
            }],
            ..crate::gateway_config_base!()
        };

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        let doc: toml_edit::DocumentMut = saved
            .parse()
            .expect("saved file should still be valid TOML");
        let route = doc["routes"]
            .as_array_of_tables()
            .expect("routes must be an array of tables")
            .iter()
            .next()
            .expect("should have at least one route");
        assert!(
            !route.contains_key("retries"),
            "route-level retries=None should not be written, got:\n{saved}"
        );
        let target = route["targets"]
            .as_array()
            .expect("targets must be an array")
            .iter()
            .next()
            .expect("should have at least one target")
            .as_inline_table()
            .expect("target must be an inline table");
        assert!(
            !target.contains_key("real_model"),
            "target-level real_model=None should not be written, got:\n{saved}"
        );
        assert!(
            !target.contains_key("retries"),
            "target-level retries=None should not be written, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_skips_default_provider_type_and_key_strategy_on_roundtrip() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("provider_type"),
            "default provider_type should not be written on roundtrip, got:\n{saved}"
        );
        assert!(
            !saved.contains("key_strategy"),
            "default key_strategy should not be written on roundtrip, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_writes_non_default_provider_type_and_key_strategy() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.anthropic]
provider_type = "anthropic"
api_keys = ["sk-ant"]
base_url = "https://api.anthropic.com"
key_strategy = "round_robin"
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("provider_type = \"anthropic\""),
            "non-default provider_type should be written, got:\n{saved}"
        );
        assert!(
            saved.contains("key_strategy = \"round_robin\""),
            "non-default key_strategy should be written, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn default_fields_not_written() {
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let config = GatewayConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 9090,
            },
            gateway_auth: None,
            providers: HashMap::from([(
                "openai".into(),
                ProviderConfig {
                    provider_type: ProviderType::OpenAI,
                    api_keys: vec!["sk-test".into()],
                    key_strategy: KeyStrategy::FillFirst,
                    proxy_mode: ProxyMode::Inherit,
                    base_url: "https://api.openai.com/v1".into(),
                    models_endpoint: "models".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
                    strip_fields: vec![],
                    model_strip_fields: HashMap::new(),
                    model_overrides: HashMap::new(),
                    role_mappings: HashMap::new(),
                    enabled: true,
                    enabled_models: vec![],
                    anthropic_cache_control: true,
                    chunk_timeout_secs: None,
                    ttfb_timeout_secs: None,
                },
            )]),
            ..crate::gateway_config_base!()
        };

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("provider_type"),
            "default provider_type should not be written, got:\n{saved}"
        );
        assert!(
            !saved.contains("key_strategy"),
            "default key_strategy should not be written"
        );
        assert!(
            !saved.contains("headers"),
            "empty headers should not be written"
        );
        assert!(
            !saved.contains("body_overrides"),
            "empty body_overrides should not be written"
        );
        assert!(
            !saved.contains("enabled_models"),
            "empty enabled_models should not be written"
        );
        assert!(
            !saved.contains("anthropic_cache_control"),
            "default anthropic_cache_control (OpenAI + true) should not be written, got:\n{saved}"
        );
        assert!(
            saved.contains("models_endpoint = \"models\""),
            "models_endpoint should be written even when default, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let content = r#"[server]
host = "127.0.0.1"
port = 8080

[gateway_auth]
api_keys = ["key1"]

[providers.openai]
api_keys = ["sk-test"]
models_endpoint = "models"

[providers.anthropic]
provider_type = "anthropic"
api_keys = ["sk-ant"]
base_url = "https://api.anthropic.com"
models_endpoint = "v1/models"
timeout_secs = 60

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai", real_model = "gpt-4o" },
    { provider = "anthropic", priority = 5 },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        // Reload and verify
        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();

        assert_eq!(reloaded.providers.len(), config.providers.len());
        for (name, p1) in &config.providers {
            let p2 = reloaded.providers.get(name).unwrap();
            assert_eq!(p1.provider_type, p2.provider_type);
            assert_eq!(p1.api_keys, p2.api_keys);
            assert_eq!(p1.key_strategy, p2.key_strategy);
            assert_eq!(p1.base_url, p2.base_url);
            assert_eq!(p1.models_endpoint, p2.models_endpoint);
            assert_eq!(p1.timeout_secs, p2.timeout_secs);
            assert_eq!(p1.headers, p2.headers);
            assert_eq!(p1.body_overrides, p2.body_overrides);
            assert_eq!(p1.enabled, p2.enabled);
            assert_eq!(p1.enabled_models, p2.enabled_models);
        }

        assert_eq!(reloaded.routes.len(), config.routes.len());
        for (r1, r2) in config.routes.iter().zip(reloaded.routes.iter()) {
            assert_eq!(r1.model_name, r2.model_name);
            assert_eq!(r1.api_endpoint, r2.api_endpoint);
            assert_eq!(r1.strategy, r2.strategy);
            assert_eq!(r1.retries, r2.retries);
            assert_eq!(r1.targets.len(), r2.targets.len());
            for (t1, t2) in r1.targets.iter().zip(r2.targets.iter()) {
                assert_eq!(t1.provider, t2.provider);
                assert_eq!(t1.real_model, t2.real_model);
                assert_eq!(t1.priority, t2.priority);
                assert_eq!(t1.retries, t2.retries);
                assert_eq!(t1.extra, t2.extra);
            }
        }

        let _ = std::fs::remove_file(&path);
    }

    // ── null value tests ──

    #[test]
    fn extra_with_nested_null_in_object_returns_error() {
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let config = GatewayConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 9090,
            },
            gateway_auth: None,
            providers: HashMap::from([(
                "openai".into(),
                ProviderConfig {
                    provider_type: ProviderType::OpenAI,
                    api_keys: vec!["sk-test".into()],
                    key_strategy: KeyStrategy::FillFirst,
                    proxy_mode: ProxyMode::Inherit,
                    base_url: "https://api.openai.com/v1".into(),
                    models_endpoint: "models".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
                    strip_fields: vec![],
                    model_strip_fields: HashMap::new(),
                    model_overrides: HashMap::new(),
                    role_mappings: HashMap::new(),
                    enabled: true,
                    enabled_models: vec![],
                    anthropic_cache_control: true,
                    chunk_timeout_secs: None,
                    ttfb_timeout_secs: None,
                },
            )]),
            routes: vec![RouteConfig {
                model_name: "gpt-4".into(),
                api_endpoint: "/chat/completions".into(),
                strategy: Strategy::FillFirst,
                retries: None,
                timeout_secs: None,
                targets: vec![RouteTarget {
                    provider: "openai".into(),
                    real_model: None,
                    priority: 0,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::from([(
                        "nested".into(),
                        serde_json::json!({ "temperature": null }),
                    )]),
                }],
            }],
            ..crate::gateway_config_base!()
        };

        let result = save_config_to_file(&config, &path);
        assert!(
            result.is_err(),
            "nested null in object should be rejected, got Ok"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("null") && err_msg.contains("temperature"),
            "error should mention both 'null' and the nested field path; got: {err_msg}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn extra_with_null_in_nested_array_returns_error() {
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let config = GatewayConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 9090,
            },
            gateway_auth: None,
            providers: HashMap::from([(
                "openai".into(),
                ProviderConfig {
                    provider_type: ProviderType::OpenAI,
                    api_keys: vec!["sk-test".into()],
                    key_strategy: KeyStrategy::FillFirst,
                    proxy_mode: ProxyMode::Inherit,
                    base_url: "https://api.openai.com/v1".into(),
                    models_endpoint: "models".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
                    strip_fields: vec![],
                    model_strip_fields: HashMap::new(),
                    model_overrides: HashMap::new(),
                    role_mappings: HashMap::new(),
                    enabled: true,
                    enabled_models: vec![],
                    anthropic_cache_control: true,
                    chunk_timeout_secs: None,
                    ttfb_timeout_secs: None,
                },
            )]),
            routes: vec![RouteConfig {
                model_name: "gpt-4".into(),
                api_endpoint: "/chat/completions".into(),
                strategy: Strategy::FillFirst,
                retries: None,
                timeout_secs: None,
                targets: vec![RouteTarget {
                    provider: "openai".into(),
                    real_model: None,
                    priority: 0,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::from([("list".into(), serde_json::json!([1, null, 3]))]),
                }],
            }],
            ..crate::gateway_config_base!()
        };

        let result = save_config_to_file(&config, &path);
        assert!(result.is_err(), "null in nested array should be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("null") && err_msg.contains("[1]"),
            "error should report the array index path; got: {err_msg}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn extra_with_null_returns_error() {
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let config = GatewayConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 9090,
            },
            gateway_auth: None,
            providers: HashMap::from([(
                "openai".into(),
                ProviderConfig {
                    provider_type: ProviderType::OpenAI,
                    api_keys: vec!["sk-test".into()],
                    key_strategy: KeyStrategy::FillFirst,
                    proxy_mode: ProxyMode::Inherit,
                    base_url: "https://api.openai.com/v1".into(),
                    models_endpoint: "models".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
                    strip_fields: vec![],
                    model_strip_fields: HashMap::new(),
                    model_overrides: HashMap::new(),
                    role_mappings: HashMap::new(),
                    enabled: true,
                    enabled_models: vec![],
                    anthropic_cache_control: true,
                    chunk_timeout_secs: None,
                    ttfb_timeout_secs: None,
                },
            )]),
            routes: vec![RouteConfig {
                model_name: "gpt-4".into(),
                api_endpoint: "/chat/completions".into(),
                strategy: Strategy::FillFirst,
                retries: None,
                timeout_secs: None,
                targets: vec![RouteTarget {
                    provider: "openai".into(),
                    real_model: None,
                    priority: 0,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::from([("temperature".into(), serde_json::Value::Null)]),
                }],
            }],
            ..crate::gateway_config_base!()
        };

        let result = save_config_to_file(&config, &path);
        assert!(result.is_err(), "save with null in extra should fail");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("null"),
            "error should mention 'null': {err_msg}"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ── composite-key (provider, real_model) round-trip tests ──

    #[test]
    fn save_targets_with_same_provider_distinct_real_models_preserves_both() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]
[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4o"
strategy = "fill_first"
targets = [
    { provider = "openai", real_model = "gpt-4o", priority = 10 },
    { provider = "openai", real_model = "gpt-3.5-turbo", priority = 5 },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        let targets = &reloaded.routes[0].targets;
        assert_eq!(
            targets.len(),
            2,
            "both targets must survive round-trip, got: {:?}",
            targets
        );

        let gpt4 = targets
            .iter()
            .find(|t| t.real_model.as_deref() == Some("gpt-4o"))
            .expect("gpt-4o target missing");
        let gpt35 = targets
            .iter()
            .find(|t| t.real_model.as_deref() == Some("gpt-3.5-turbo"))
            .expect("gpt-3.5-turbo target missing");
        assert_eq!(gpt4.priority, 10, "gpt-4o priority lost");
        assert_eq!(gpt35.priority, 5, "gpt-3.5-turbo priority lost");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_targets_mutates_one_target_without_disturbing_its_sibling() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]
[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "multi"
targets = [
    { provider = "openai", real_model = "gpt-4", priority = 10 },
    { provider = "openai", real_model = "gpt-3.5", priority = 5, extra = { temperature = 0.3 } },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        let gpt35_idx = config.routes[0]
            .targets
            .iter()
            .position(|t| t.real_model.as_deref() == Some("gpt-3.5"))
            .expect("fixture must contain gpt-3.5 target");
        config.routes[0].targets[gpt35_idx].priority = 99;
        config.routes[0].targets[gpt35_idx]
            .extra
            .insert("top_p".into(), serde_json::json!(0.95));

        save_config_to_file(&config, &path).unwrap();

        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        let targets = &reloaded.routes[0].targets;
        assert_eq!(targets.len(), 2);

        let gpt4 = targets
            .iter()
            .find(|t| t.real_model.as_deref() == Some("gpt-4"))
            .expect("gpt-4 still present");
        assert_eq!(gpt4.priority, 10, "gpt-4 priority untouched");
        assert!(
            gpt4.extra.is_empty(),
            "gpt-4 extras untouched, got: {:?}",
            gpt4.extra
        );

        let gpt35 = targets
            .iter()
            .find(|t| t.real_model.as_deref() == Some("gpt-3.5"))
            .expect("gpt-3.5 still present");
        assert_eq!(gpt35.priority, 99, "gpt-3.5 priority updated");
        assert_eq!(
            gpt35.extra.get("temperature").and_then(|v| v.as_f64()),
            Some(0.3),
            "pre-existing extra preserved"
        );
        assert_eq!(
            gpt35.extra.get("top_p").and_then(|v| v.as_f64()),
            Some(0.95),
            "newly added extra persisted"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ── retries / [backoff] / routes comment-preservation round-trip tests ──

    #[test]
    fn save_persists_retries() {
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let mut config = openai_only_config();
        config.backoff.retries = 7;

        save_config_to_file(&config, &path).unwrap();
        let reloaded = load_config(&path).unwrap();
        let c = reloaded.inner.try_read().unwrap();
        assert_eq!(c.backoff.retries, 7, "retries should be persisted to disk");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_persists_backoff() {
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let mut config = openai_only_config();
        config.backoff = BackoffConfig {
            retries: 2,
            base_ms: 250,
            max_ms: 30_000,
            jitter: false,
            retry_on_empty_response: true,
        };

        save_config_to_file(&config, &path).unwrap();
        let reloaded = load_config(&path).unwrap();
        let c = reloaded.inner.try_read().unwrap();
        assert_eq!(c.backoff.base_ms, 250);
        assert_eq!(c.backoff.max_ms, 30_000);
        assert!(!c.backoff.jitter);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_preserves_existing_backoff_with_comments() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

# User-authored backoff config
[backoff]
base_ms = 200
max_ms = 10_000
jitter = false
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("# User-authored backoff config"),
            "backoff section comment should be preserved, got:\n{saved}"
        );
        assert!(saved.contains("base_ms = 200"));
        assert!(saved.contains("max_ms = 10000"));
        assert!(saved.contains("jitter = false"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_preserves_comments_in_routes() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

# GPT-4 route section
[[routes]]
# Inline comment 1
model_name = "gpt-4"
# Inline comment 2
strategy = "round_robin"
targets = [
    { provider = "openai" },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("# GPT-4 route section"),
            "route section comment should be preserved, got:\n{saved}"
        );
        assert!(saved.contains("# Inline comment 1"));
        assert!(saved.contains("# Inline comment 2"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_route_insert_then_update_preserves_other_route_comments() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

[[routes]]
# Keep me
model_name = "gpt-4"
strategy = "fill_first"
targets = [
    { provider = "openai" },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config.routes.push(RouteConfig {
            model_name: "claude".into(),
            api_endpoint: "/chat/completions".into(),
            strategy: Strategy::FillFirst,
            retries: None,
            timeout_secs: None,
            targets: vec![RouteTarget {
                provider: "openai".into(),
                real_model: None,
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
            }],
        });

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("# Keep me"),
            "unrelated route's comment should be preserved, got:\n{saved}"
        );
        assert!(saved.contains("model_name = \"claude\""));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_removes_stale_providers() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]

[providers.anthropic]
api_keys = ["sk-ant"]
base_url = "https://api.anthropic.com"
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config.providers.remove("anthropic");

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("[providers.openai]"),
            "openai provider section should remain, got:\n{saved}"
        );
        assert!(
            !saved.contains("[providers.anthropic]"),
            "anthropic provider section should be removed, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_empty_routes_removes_routes_section() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4"
targets = [
    { provider = "openai" },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config.routes.clear();

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("[[routes]]") && !saved.contains("routes = ["),
            "empty routes should not leave a routes key in the file, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ── route targets / extra precise-edit tests ──

    #[test]
    fn save_preserves_extra_key_order() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4"
targets = [
    { provider = "openai", extra = { temperature = 0.7, max_tokens = 100, top_p = 0.9 } },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        let temp_pos = saved.find("temperature").expect("temperature");
        let max_pos = saved.find("max_tokens").expect("max_tokens");
        let top_p_pos = saved.find("top_p").expect("top_p");
        assert!(
            temp_pos < max_pos && max_pos < top_p_pos,
            "extra key order should be preserved: got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_preserves_extra_inline_table_on_rewrite() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4"
targets = [
    { provider = "openai", extra = { temperature = 0.7, max_tokens = 100 } },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("temperature = 0.7"));
        assert!(saved.contains("max_tokens = 100"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_extra_remove_removes_key() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4"
targets = [
    { provider = "openai", extra = { temperature = 0.7, max_tokens = 100 } },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config.routes[0].targets[0].extra.remove("max_tokens");

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("temperature"));
        assert!(
            !saved.contains("max_tokens"),
            "removed extra key should not appear in saved file, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_extra_add_appends_new_key() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4"
targets = [
    { provider = "openai", extra = { temperature = 0.7 } },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config.routes[0].targets[0]
            .extra
            .insert("max_tokens".into(), serde_json::json!(100));

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("temperature"));
        assert!(saved.contains("max_tokens"));
        let temp_pos = saved.find("temperature").unwrap();
        let max_pos = saved.find("max_tokens").unwrap();
        assert!(
            temp_pos < max_pos,
            "existing key should come before appended key, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_targets_preserves_unrelated_target_comments() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

[providers.anthropic]
api_keys = ["sk-ant"]

[[routes]]
model_name = "gpt-4"
targets = [
    # OpenAI primary
    { provider = "openai" },
    { provider = "anthropic" },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("# OpenAI primary"),
            "target's comment should be preserved, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_targets_removes_stale_target() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

[providers.anthropic]
api_keys = ["sk-ant"]

[[routes]]
model_name = "gpt-4"
targets = [
    { provider = "openai" },
    { provider = "anthropic" },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config.routes[0]
            .targets
            .retain(|t| t.provider != "anthropic");

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        let after_routes = saved
            .split("[[routes]]")
            .nth(1)
            .expect("should have a routes section");
        let target_count = after_routes.matches("provider = ").count();
        assert_eq!(
            target_count, 1,
            "should have exactly one target after removing anthropic, got:\n{after_routes}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_targets_formatted_multiline_when_input_is_single_line() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

[providers.kimchi]
api_keys = ["sk-kimchi"]

[providers.jx]
api_keys = ["sk-jx"]

[[routes]]
model_name = "kimi-k2.6"
targets = [{ provider = "kimchi", real_model = "kimi-k2.6" }, { provider = "jx", real_model = "kimi-k2.6" }]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("\n    { provider = \"kimchi\""),
            "first target should be on its own line, got:\n{saved}"
        );
        assert!(
            saved.contains("\n    { provider = \"jx\""),
            "second target should be on its own line, got:\n{saved}"
        );
        assert!(
            saved.contains("\n]"),
            "closing bracket should be on its own line, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ── provider api_keys / enabled_models / headers / body_overrides
    //    precise-edit tests ──

    #[test]
    fn save_preserves_api_keys_order() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-3", "sk-1", "sk-2"]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        let sk3 = saved.find("sk-3").expect("sk-3");
        let sk1 = saved.find("sk-1").expect("sk-1");
        let sk2 = saved.find("sk-2").expect("sk-2");
        assert!(
            sk3 < sk1 && sk1 < sk2,
            "api_keys on-disk order should be preserved, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_api_keys_remove_removes_key() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-1", "sk-2", "sk-3"]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config.providers.get_mut("openai").unwrap().api_keys = vec!["sk-1".into(), "sk-3".into()];

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("sk-1"));
        assert!(
            !saved.contains("sk-2"),
            "removed key should be gone:\n{saved}"
        );
        assert!(saved.contains("sk-3"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_api_keys_add_appends_key() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-1"]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config.providers.get_mut("openai").unwrap().api_keys = vec!["sk-1".into(), "sk-2".into()];

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        let sk1 = saved.find("sk-1").expect("sk-1");
        let sk2 = saved.find("sk-2").expect("sk-2");
        assert!(sk1 < sk2, "appended key should come last:\n{saved}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_api_keys_empty_removes_field() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-1"]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config.providers.get_mut("openai").unwrap().api_keys.clear();

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("api_keys"),
            "empty api_keys should remove the field:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_preserves_enabled_models_order() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
enabled_models = ["gpt-4o", "gpt-4-turbo", "gpt-3.5-turbo"]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        let g4o = saved.find("gpt-4o").unwrap();
        let g4t = saved.find("gpt-4-turbo").unwrap();
        let g35 = saved.find("gpt-3.5-turbo").unwrap();
        assert!(
            g4o < g4t && g4t < g35,
            "enabled_models on-disk order should be preserved, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_preserves_headers_key_order() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
headers = { X-Trace = "on", X-Request-Id = "abc", Authorization = "Bearer t" }
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        let trace = saved.find("X-Trace").expect("X-Trace");
        let req = saved.find("X-Request-Id").expect("X-Request-Id");
        let auth = saved.find("Authorization").expect("Authorization");
        assert!(
            trace < req && req < auth,
            "headers on-disk order should be preserved, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_headers_remove_removes_key() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
headers = { X-Trace = "on", X-Request-Id = "abc" }
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config
            .providers
            .get_mut("openai")
            .unwrap()
            .headers
            .remove("X-Request-Id");

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("X-Trace"));
        assert!(
            !saved.contains("X-Request-Id"),
            "removed header should be gone:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ── model_overrides round-trip tests ──

    #[test]
    fn model_overrides_round_trip() {
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let mut config = openai_only_config();
        let mut gpt4o = HashMap::new();
        gpt4o.insert("max_tokens".into(), serde_json::json!(4096));
        gpt4o.insert("temperature".into(), serde_json::json!(0.8));
        let mut gpt35 = HashMap::new();
        gpt35.insert("max_tokens".into(), serde_json::json!(2048));
        config.providers.get_mut("openai").unwrap().model_overrides = HashMap::from([
            ("gpt-4o".into(), gpt4o.clone()),
            ("gpt-3.5-turbo".into(), gpt35.clone()),
        ]);

        save_config_to_file(&config, &path).unwrap();

        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        let reloaded_mo = &reloaded.providers.get("openai").unwrap().model_overrides;
        assert_eq!(
            reloaded_mo,
            &config.providers.get("openai").unwrap().model_overrides,
            "model_overrides should survive round-trip"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn model_overrides_save_removes_stale_model() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]

[providers.openai.model_overrides]
gpt-4o = { max_tokens = 4096 }
gpt-3.5-turbo = { max_tokens = 2048 }
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config
            .providers
            .get_mut("openai")
            .unwrap()
            .model_overrides
            .remove("gpt-3.5-turbo");

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("gpt-4o"),
            "gpt-4o model override should remain, got:\n{saved}"
        );
        assert!(
            !saved.contains("gpt-3.5-turbo"),
            "gpt-3.5-turbo model override should be removed, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn model_overrides_save_empty_removes_section() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]

[providers.openai.model_overrides]
gpt-4o = { max_tokens = 4096 }
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config
            .providers
            .get_mut("openai")
            .unwrap()
            .model_overrides
            .clear();

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("model_overrides"),
            "empty model_overrides section should be removed, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_preserves_body_overrides_key_order() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
body_overrides = { max_tokens = 4096, temperature = 0.8, top_p = 0.9 }
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        let max = saved.find("max_tokens").expect("max_tokens");
        let temp = saved.find("temperature").expect("temperature");
        let top = saved.find("top_p").expect("top_p");
        assert!(
            max < temp && temp < top,
            "body_overrides on-disk order should be preserved, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ── chunk_timeout_secs / ttfb_timeout_secs / role_mappings /
    //    anthropic_cache_control round-trip tests ──

    #[test]
    fn save_round_trips_chunk_and_ttfb_timeout_secs() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config
            .providers
            .get_mut("openai")
            .unwrap()
            .chunk_timeout_secs = Some(30);
        config
            .providers
            .get_mut("openai")
            .unwrap()
            .ttfb_timeout_secs = Some(45);

        save_config_to_file(&config, &path).unwrap();

        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        let p = reloaded.providers.get("openai").unwrap();
        assert_eq!(
            p.chunk_timeout_secs,
            Some(30),
            "chunk_timeout_secs round-trip"
        );
        assert_eq!(
            p.ttfb_timeout_secs,
            Some(45),
            "ttfb_timeout_secs round-trip"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_round_trips_chunk_and_ttfb_removed_when_none() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
chunk_timeout_secs = 30
ttfb_timeout_secs = 45
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config
            .providers
            .get_mut("openai")
            .unwrap()
            .chunk_timeout_secs = None;
        config
            .providers
            .get_mut("openai")
            .unwrap()
            .ttfb_timeout_secs = None;

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("chunk_timeout_secs"),
            "None chunk_timeout_secs should be removed from disk, got:\n{saved}"
        );
        assert!(
            !saved.contains("ttfb_timeout_secs"),
            "None ttfb_timeout_secs should be removed from disk, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_round_trips_role_mappings() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        let mut mappings = HashMap::new();
        mappings.insert("developer".into(), "system".into());
        mappings.insert("user".into(), "assistant".into());
        config.providers.get_mut("openai").unwrap().role_mappings = mappings;

        save_config_to_file(&config, &path).unwrap();

        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        let p = reloaded.providers.get("openai").unwrap();
        assert_eq!(p.role_mappings.get("developer").unwrap(), "system");
        assert_eq!(p.role_mappings.get("user").unwrap(), "assistant");
        assert_eq!(p.role_mappings.len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_removes_empty_role_mappings() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
role_mappings = { developer = "system" }
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config
            .providers
            .get_mut("openai")
            .unwrap()
            .role_mappings
            .clear();

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("role_mappings"),
            "empty role_mappings should be removed from disk, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ── route / target timeout_secs round-trip tests ──

    #[test]
    fn save_round_trips_route_timeout_secs() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4"
targets = [
    { provider = "openai" },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config.routes[0].timeout_secs = Some(300);

        save_config_to_file(&config, &path).unwrap();

        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        assert_eq!(
            reloaded.routes[0].timeout_secs,
            Some(300),
            "route timeout_secs should survive round-trip"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_removes_route_timeout_secs_when_none() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4"
timeout_secs = 300
targets = [
    { provider = "openai" },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config.routes[0].timeout_secs = None;

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        let after_routes = saved
            .split("[[routes]]")
            .nth(1)
            .expect("should have a routes section");
        assert!(
            !after_routes.contains("timeout_secs"),
            "None route timeout_secs should be removed from disk, got:\n{after_routes}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_round_trips_target_timeout_secs() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4"
targets = [
    { provider = "openai" },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config.routes[0].targets[0].timeout_secs = Some(180);

        save_config_to_file(&config, &path).unwrap();

        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        assert_eq!(
            reloaded.routes[0].targets[0].timeout_secs,
            Some(180),
            "target timeout_secs should survive round-trip"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_removes_target_timeout_secs_when_none() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4"
targets = [
    { provider = "openai", timeout_secs = 180 },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config.routes[0].targets[0].timeout_secs = None;

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        let after_routes = saved
            .split("[[routes]]")
            .nth(1)
            .expect("should have a routes section");
        assert!(
            !after_routes.contains("timeout_secs"),
            "None target timeout_secs should be removed from disk, got:\n{after_routes}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_round_trips_all_provider_fields_with_non_default_values() {
        // Table-driven round-trip test: every field of ProviderConfig is
        // set to a non-default value, saved through the update path
        // (provider already exists on disk), then reloaded. If any field
        // is missing from `update_existing_provider`, this test catches
        // it — the reloaded value will be the default, not our non-default.
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers]

[providers.openai]
api_keys = ["sk-test"]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        let mut model_overrides = HashMap::new();
        let mut inner = HashMap::new();
        inner.insert("max_tokens".into(), serde_json::json!(4096));
        model_overrides.insert("gpt-4o".into(), inner);

        let mut role_mappings = HashMap::new();
        role_mappings.insert("developer".into(), "system".into());

        let mut body_overrides = HashMap::new();
        body_overrides.insert("temperature".into(), serde_json::json!(0.3));

        let mut headers = HashMap::new();
        headers.insert("X-Test".into(), "1".into());

        config.providers.insert(
            "openai".into(),
            ProviderConfig {
                provider_type: ProviderType::Anthropic,
                api_keys: vec!["sk-a".into(), "sk-b".into()],
                key_strategy: KeyStrategy::RoundRobin,
                proxy_mode: ProxyMode::Disabled,
                base_url: "https://custom.example.com/v1".into(),
                models_endpoint: "v1/list".into(),
                timeout_secs: 300,
                chunk_timeout_secs: Some(30),
                ttfb_timeout_secs: Some(45),
                headers,
                body_overrides,
                strip_fields: vec![],
                model_strip_fields: HashMap::new(),
                model_overrides,
                role_mappings,
                enabled: false,
                enabled_models: vec!["gpt-4o".into()],
                anthropic_cache_control: false,
            },
        );

        save_config_to_file(&config, &path).unwrap();

        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        let p = reloaded.providers.get("openai").unwrap();

        assert_eq!(p.provider_type, ProviderType::Anthropic);
        assert_eq!(p.api_keys, vec!["sk-a", "sk-b"]);
        assert_eq!(p.key_strategy, KeyStrategy::RoundRobin);
        assert_eq!(p.proxy_mode, ProxyMode::Disabled);
        assert_eq!(p.base_url, "https://custom.example.com/v1");
        assert_eq!(p.models_endpoint, "v1/list");
        assert_eq!(p.timeout_secs, 300);
        assert_eq!(p.chunk_timeout_secs, Some(30));
        assert_eq!(p.ttfb_timeout_secs, Some(45));
        assert_eq!(p.headers.get("X-Test").map(String::as_str), Some("1"));
        assert_eq!(p.headers.len(), 1);
        assert_eq!(
            p.body_overrides.get("temperature").and_then(|v| v.as_f64()),
            Some(0.3)
        );
        assert_eq!(p.body_overrides.len(), 1);
        let mo = &p.model_overrides;
        assert_eq!(mo.len(), 1);
        let gpt4 = mo.get("gpt-4o").unwrap();
        assert_eq!(gpt4.get("max_tokens").and_then(|v| v.as_u64()), Some(4096));
        assert_eq!(
            p.role_mappings.get("developer").map(String::as_str),
            Some("system")
        );
        assert_eq!(p.role_mappings.len(), 1);
        assert!(!p.enabled);
        assert_eq!(p.enabled_models, vec!["gpt-4o"]);
        assert!(!p.anthropic_cache_control);

        // Content assertion alongside the round-trip assert: Anthropic +
        // false must actually be written to disk.
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("anthropic_cache_control = false"),
            "Anthropic + false should be written to TOML, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_round_trips_anthropic_cache_control_false() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.anthropic]
provider_type = "anthropic"
api_keys = ["sk-ant"]
base_url = "https://api.anthropic.com"
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        config
            .providers
            .get_mut("anthropic")
            .unwrap()
            .anthropic_cache_control = false;

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("anthropic_cache_control = false"),
            "Anthropic + false should be written to TOML, got:\n{saved}"
        );

        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        let p = reloaded.providers.get("anthropic").unwrap();
        assert!(
            !p.anthropic_cache_control,
            "anthropic_cache_control=false should survive round-trip"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn default_anthropic_cache_control_not_written() {
        // Anthropic + true (default): key must not be written; reload
        // falls back to the serde default (`true`).
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let mut config = openai_only_config();
        let mut p = minimal_provider();
        p.provider_type = ProviderType::Anthropic;
        p.base_url = "https://api.anthropic.com".into();
        p.anthropic_cache_control = true;
        config.providers.insert("anthropic".into(), p);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("anthropic_cache_control"),
            "Anthropic + true (default) should not write the key, got:\n{saved}"
        );

        let reloaded = load_config(&path).unwrap();
        let c = reloaded.inner.try_read().unwrap();
        assert!(
            c.providers
                .get("anthropic")
                .unwrap()
                .anthropic_cache_control,
            "reload without the key should default to true"
        );
        drop(c);
        drop(reloaded);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn openai_never_writes_anthropic_cache_control() {
        // OpenAI + false: key must still not be written (predicate
        // requires Anthropic type regardless of value).
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let mut config = openai_only_config();
        config
            .providers
            .get_mut("openai")
            .unwrap()
            .anthropic_cache_control = false;

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("anthropic_cache_control"),
            "non-Anthropic provider must never write the key, got:\n{saved}"
        );

        let reloaded = load_config(&path).unwrap();
        let c = reloaded.inner.try_read().unwrap();
        assert!(
            c.providers.get("openai").unwrap().anthropic_cache_control,
            "reload without the key should default to true"
        );
        drop(c);
        drop(reloaded);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn create_path_keeps_anthropic_cache_control_false() {
        // New provider (create path via to_document): Anthropic + false
        // must survive serialization and be written as `= false`.
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let mut config = openai_only_config();
        let mut p = minimal_provider();
        p.provider_type = ProviderType::Anthropic;
        p.base_url = "https://api.anthropic.com".into();
        p.anthropic_cache_control = false;
        config.providers.insert("anthropic".into(), p);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("anthropic_cache_control = false"),
            "create path should keep Anthropic + false, got:\n{saved}"
        );

        let reloaded = load_config(&path).unwrap();
        let c = reloaded.inner.try_read().unwrap();
        assert!(
            !c.providers
                .get("anthropic")
                .unwrap()
                .anthropic_cache_control,
            "Anthropic + false should reload as false"
        );
        drop(c);
        drop(reloaded);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_clears_anthropic_cache_control_on_true() {
        // Existing `= false` on disk, in-memory value flipped to `true`
        // → key removed on save; reload sees the default `true`.
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.anthropic]
provider_type = "anthropic"
api_keys = ["sk-ant"]
base_url = "https://api.anthropic.com"
anthropic_cache_control = false
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        assert!(
            !config.providers["anthropic"].anthropic_cache_control,
            "fixture should load as false"
        );
        config
            .providers
            .get_mut("anthropic")
            .unwrap()
            .anthropic_cache_control = true;

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("anthropic_cache_control"),
            "setting back to true should remove the key, got:\n{saved}"
        );

        let reloaded = load_config(&path).unwrap();
        let c = reloaded.inner.try_read().unwrap();
        assert!(
            c.providers
                .get("anthropic")
                .unwrap()
                .anthropic_cache_control,
            "reload without the key should default to true"
        );
        drop(c);
        drop(reloaded);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn provider_type_switch_to_openai_strips_cache_control() {
        // Anthropic + false persisted, then provider_type switched to
        // OpenAI in the same process → save strips the key (acceptable
        // edge: the false is lost after a restart in this state).
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.anthropic]
provider_type = "anthropic"
api_keys = ["sk-ant"]
base_url = "https://api.anthropic.com"
anthropic_cache_control = false
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        assert!(
            !config.providers["anthropic"].anthropic_cache_control,
            "fixture should load as false"
        );
        // Switch type to OpenAI while the in-memory value stays false.
        config.providers.get_mut("anthropic").unwrap().provider_type = ProviderType::OpenAI;

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("anthropic_cache_control"),
            "switching to OpenAI must strip the key even when value is false, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn create_then_update_model_overrides_does_not_panic() {
        // Regression test: when a provider with non-empty model_overrides is
        // first created (create path, saved as inline table via to_document),
        // then modified and saved again (update path), the second save must
        // NOT panic. The old code used `as_table_mut().expect(...)` on the
        // model_overrides field, but the create path produces an inline table
        // (not a sub-table), so `as_table_mut()` returned None -> panic.
        //
        // Setup: [providers] empty table, so the provider goes through the
        // create path on first save.
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let mut config = openai_only_config();

        // Add non-empty model_overrides.
        let mut gpt4o = HashMap::new();
        gpt4o.insert("max_tokens".into(), serde_json::json!(4096));
        gpt4o.insert("temperature".into(), serde_json::json!(0.8));
        config.providers.get_mut("openai").unwrap().model_overrides =
            HashMap::from([("gpt-4o".into(), gpt4o.clone())]);

        // First save: create path (provider doesn't exist on disk yet).
        save_config_to_file(&config, &path).unwrap();

        // Verify inline table format on disk.
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("model_overrides = {"),
            "model_overrides should be inline table format on first save, got:\n{saved}"
        );
        assert!(
            !saved.contains("[providers.openai.model_overrides]"),
            "model_overrides should NOT be sub-table format, got:\n{saved}"
        );

        // Modify any field (e.g. timeout_secs) and save again: update path.
        config.providers.get_mut("openai").unwrap().timeout_secs = 300;
        // This must not panic.
        save_config_to_file(&config, &path).unwrap();

        // Reload and verify model_overrides values are preserved.
        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        let reloaded_mo = &reloaded.providers.get("openai").unwrap().model_overrides;
        assert_eq!(
            reloaded_mo,
            &config.providers.get("openai").unwrap().model_overrides,
            "model_overrides should survive create-then-update round-trip"
        );

        // Verify inline table format persists after second save.
        let saved2 = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved2.contains("model_overrides = {"),
            "model_overrides should remain inline table format after update, got:\n{saved2}"
        );
        assert!(
            !saved2.contains("[providers.openai.model_overrides]"),
            "model_overrides should NOT become sub-table format after update, got:\n{saved2}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn model_strip_fields_round_trip() {
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let mut config = openai_only_config();
        config.providers.get_mut("openai").unwrap().strip_fields = vec!["global_bad".into()];
        config
            .providers
            .get_mut("openai")
            .unwrap()
            .model_strip_fields = HashMap::from([
            ("glm-5.2".into(), vec!["chat_template_kwargs".into()]),
            ("qwen-2.5".into(), vec!["extra_a".into(), "extra_b".into()]),
        ]);

        save_config_to_file(&config, &path).unwrap();

        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        let provider = reloaded.providers.get("openai").unwrap();

        assert_eq!(provider.strip_fields, vec!["global_bad"]);
        assert_eq!(
            provider.model_strip_fields.get("glm-5.2"),
            Some(&vec!["chat_template_kwargs".to_string()])
        );
        assert_eq!(
            provider.model_strip_fields.get("qwen-2.5"),
            Some(&vec!["extra_a".to_string(), "extra_b".to_string()])
        );

        // Test updating: remove one model mapping and save again
        drop(reloaded);
        drop(reloaded_handle);

        let mut config2 = load_config(&path)
            .unwrap()
            .inner
            .try_read()
            .unwrap()
            .clone();
        config2
            .providers
            .get_mut("openai")
            .unwrap()
            .model_strip_fields
            .remove("glm-5.2");
        save_config_to_file(&config2, &path).unwrap();

        let reloaded2 = load_config(&path)
            .unwrap()
            .inner
            .try_read()
            .unwrap()
            .clone();
        let provider2 = reloaded2.providers.get("openai").unwrap();
        assert!(!provider2.model_strip_fields.contains_key("glm-5.2"));
        assert_eq!(
            provider2.model_strip_fields.get("qwen-2.5"),
            Some(&vec!["extra_a".to_string(), "extra_b".to_string()])
        );

        // Test clearing all model_strip_fields removes section from file
        let config3 = provider2.clone();
        let mut full_config = openai_only_config();
        full_config
            .providers
            .insert("openai".into(), config3.clone());
        full_config
            .providers
            .get_mut("openai")
            .unwrap()
            .model_strip_fields
            .clear();
        save_config_to_file(&full_config, &path).unwrap();

        let saved_raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved_raw.contains("model_strip_fields"),
            "empty model_strip_fields section should be removed from saved file, got:\n{saved_raw}"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ── proxy_url / proxy_mode round-trip tests ──

    #[test]
    fn save_round_trips_proxy_url() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);
        assert_eq!(config.proxy_url, None);

        // Write a global proxy.
        config.proxy_url = Some("http://proxy.example.com:8080".into());
        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("proxy_url = \"http://proxy.example.com:8080\""),
            "proxy_url should be written, got:\n{saved}"
        );

        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        assert_eq!(
            reloaded.proxy_url.as_deref(),
            Some("http://proxy.example.com:8080"),
            "proxy_url should survive round-trip"
        );
        drop(reloaded);
        drop(reloaded_handle);

        // Clearing the global proxy removes the key from disk.
        let mut config2 = load_config(&path)
            .unwrap()
            .inner
            .try_read()
            .unwrap()
            .clone();
        config2.proxy_url = None;
        save_config_to_file(&config2, &path).unwrap();

        let saved2 = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved2.contains("proxy_url"),
            "cleared proxy_url should be removed from disk, got:\n{saved2}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_round_trips_gateway_auth_api_keys() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);
        assert!(config.gateway_auth.is_none());

        // Install gateway auth keys.
        config.gateway_auth = Some(crate::GatewayAuthConfig {
            api_keys: vec!["sk-client-a".into(), "sk-client-b".into()],
        });
        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("[gateway_auth]"),
            "gateway_auth section should be written, got:\n{saved}"
        );
        assert!(
            saved.contains("sk-client-a") && saved.contains("sk-client-b"),
            "api keys should be written, got:\n{saved}"
        );

        let reloaded = load_config(&path).unwrap();
        let cfg = reloaded.inner.try_read().unwrap();
        let auth = cfg.gateway_auth.as_ref().expect("gateway_auth must reload");
        assert_eq!(auth.api_keys, vec!["sk-client-a", "sk-client-b"]);
        drop(cfg);
        drop(reloaded);

        // Clearing auth removes the section from disk.
        let mut config2 = load_config(&path)
            .unwrap()
            .inner
            .try_read()
            .unwrap()
            .clone();
        config2.gateway_auth = None;
        save_config_to_file(&config2, &path).unwrap();

        let saved2 = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved2.contains("[gateway_auth]"),
            "cleared gateway_auth should be removed from disk, got:\n{saved2}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_removes_blank_proxy_url_and_trims_padded_url() {
        // `Some("")` / `Some("   ")` must delete the key (never write an
        // empty string), so a subsequent reload sees `None`. A
        // whitespace-padded URL must be persisted trimmed.
        let content = r#"
proxy_url = "http://proxy.example.com:8080"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
"#;
        let path = temp_toml(content);

        let mut config = load_config(&path)
            .unwrap()
            .inner
            .try_read()
            .unwrap()
            .clone();
        config.proxy_url = Some("   ".into());
        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("proxy_url"),
            "blank proxy_url should remove the key from disk, got:\n{saved}"
        );
        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        assert_eq!(
            reloaded.proxy_url, None,
            "reload after clearing a blank proxy_url must be None"
        );
        drop(reloaded);
        drop(reloaded_handle);

        // Whitespace-padded URL → written trimmed.
        let mut config2 = load_config(&path)
            .unwrap()
            .inner
            .try_read()
            .unwrap()
            .clone();
        config2.proxy_url = Some("  http://proxy2.example.com:9090  ".into());
        save_config_to_file(&config2, &path).unwrap();

        let saved2 = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved2.contains("proxy_url = \"http://proxy2.example.com:9090\""),
            "padded proxy_url should be written trimmed, got:\n{saved2}"
        );
        let reloaded2_handle = load_config(&path).unwrap();
        let reloaded2 = reloaded2_handle.inner.try_read().unwrap();
        assert_eq!(
            reloaded2.proxy_url.as_deref(),
            Some("http://proxy2.example.com:9090"),
            "trimmed proxy_url should survive round-trip"
        );
        drop(reloaded2);
        drop(reloaded2_handle);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_round_trips_proxy_mode() {
        let content = r#"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let mut config = handle.inner.try_read().unwrap().clone();
        drop(handle);
        assert_eq!(config.providers["openai"].proxy_mode, ProxyMode::Inherit);

        // Write a non-default mode (`disabled` needs no global proxy, so the
        // reload's validate() still passes).
        config.providers.get_mut("openai").unwrap().proxy_mode = ProxyMode::Disabled;
        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("proxy_mode = \"disabled\""),
            "non-default proxy_mode should be written, got:\n{saved}"
        );

        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        assert_eq!(
            reloaded.providers["openai"].proxy_mode,
            ProxyMode::Disabled,
            "proxy_mode should survive round-trip"
        );
        drop(reloaded);
        drop(reloaded_handle);

        // Back to the default `inherit`: the key must be removed from disk.
        let mut config2 = load_config(&path)
            .unwrap()
            .inner
            .try_read()
            .unwrap()
            .clone();
        config2.providers.get_mut("openai").unwrap().proxy_mode = ProxyMode::Inherit;
        save_config_to_file(&config2, &path).unwrap();

        let saved2 = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved2.contains("proxy_mode"),
            "default inherit proxy_mode should be removed from disk, got:\n{saved2}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_round_trips_proxy_url_and_proxy_mode_together() {
        // End-to-end: global proxy + provider forced through that proxy.
        let content = r#"
proxy_url = "http://proxy.example.com:8080"
[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
proxy_mode = "enabled"
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);
        assert_eq!(
            config.proxy_url.as_deref(),
            Some("http://proxy.example.com:8080")
        );
        assert_eq!(config.providers["openai"].proxy_mode, ProxyMode::Enabled);

        save_config_to_file(&config, &path).unwrap();

        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        assert_eq!(
            reloaded.proxy_url.as_deref(),
            Some("http://proxy.example.com:8080")
        );
        assert_eq!(reloaded.providers["openai"].proxy_mode, ProxyMode::Enabled);

        let _ = std::fs::remove_file(&path);
    }
}
