#[cfg(test)]
use std::collections::HashMap;
use std::path::Path;

use tracing::info;

use crate::config::{
    GatewayConfig, KeyStrategy, ProviderConfig, ProviderType, RouteConfig, RouteTarget, Strategy,
};
use crate::loader::json_util::{check_json_map, json_to_toml_value};
use crate::toml_edit_util::{
    ensure_array_field, ensure_inline_table_field, ensure_nested_inline_table, prune_keys,
    reformat_inline_table_arrays_multiline, sync_array_of_tables, sync_inline_table_array_in_place,
    sync_inline_table_in_place, sync_string_array_in_place, upsert_table,
};

/// Persist the in-memory `GatewayConfig` back to the TOML file.
///
/// Uses `toml_edit` to parse the existing file and performs fine-grained
/// in-place updates. **All mutable sections are written back precisely:**
/// `[providers.*]`, `[[routes]]`, and `[backoff]` are all updated in
/// place, preserving comments and field order in any field/table the
/// in-memory config does not explicitly touch.
///
/// All other sections — `[server]`, `[analytics]`, `[gateway_auth]`,
/// `[circuit_breaker]`, `[logging]`, and the top-level scalar
/// `shuffle_cache_ttl_secs` — are intentionally **not** written back.
/// The admin save path only mutates the three sections above, so any
/// user-authored values (and comments) in these untouched sections are
/// preserved verbatim across saves. This also means admin-initiated
/// saves won't accidentally revert hand-edited tuning of these
/// read-only fields.
///
/// # Concurrency and crash-safety
///
/// This function performs a **read-then-write** (parse existing file →
/// mutate in-memory → `std::fs::write`). It does **not** use atomic
/// `tmp + rename` or file locking.
///
/// **Process-level serialization**: the admin API acquires the config
/// `RwLock` write guard (`state.config.write().await`) across config
/// mutation and `try_persist`. Concurrent admin requests within the same
/// process are serialized — the second request re-reads the file the
/// first just wrote, so no lost-update occurs within a single process.
///
/// **Error-path behaviour**: if `existing.parse()` or `config.validate()`
/// fails, the function returns `Err` without touching the file on disk —
/// the original config file is preserved untouched (all-or-nothing).
///
/// **Crash-safety gap**: a crash during `std::fs::write` would leave a
/// truncated or partially-written file. In practice, admin API mutations
/// are infrequent, and the config file can be restored from version
/// control or the `gateway.example.toml` template. If crash-safety
/// becomes important, replace `std::fs::write` with `write(tmp) →
/// fs::rename(tmp, path)`.
///
/// **Cross-process gap**: an external editor saving concurrently with an
/// admin API mutation would race the read and write. The hot-reload
/// watcher is read-only (it never writes the config file), so it does
/// not participate in the race. If cross-process mutation is a concern,
/// add OS-level file locking (`flock` / `LockFile`).
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
                Ok(toml_edit::Item::Table((*pd).clone()))
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
        },
    )?;

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

    // ── Write back ──
    // Render the document, then post-process inline-table-arrays to
    // a multi-line layout for hand-editing readability. We do this as
    // a string-level pass because `toml_edit` renders single-line
    // arrays on one line (e.g. `targets = [{...}, {...}]`).
    // `toml_edit` **does** preserve per-item prefix decor (comments)
    // and multi-line formatting when it already existed on input, so
    // the reformat is a no-op for already-multi-line arrays — it only
    // converts single-line arrays to multi-line. See
    // `reformat_inline_table_arrays_multiline` for the rationale.
    let mut rendered = doc.to_string();
    reformat_inline_table_arrays_multiline(&mut rendered, "targets", "    ");
    std::fs::write(path, rendered)
        .map_err(|e| sag_core::AppError::Config(format!("failed to write config file: {e}")))?;

    info!("config persisted to {}", path.display());
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
fn update_existing_provider(item: &mut toml_edit::Item, provider: &ProviderConfig) {
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
    table["enabled"] = toml_edit::value(provider.enabled);
    // headers — inline table; remove the key entirely if empty.
    if provider.headers.is_empty() {
        table.remove("headers");
    } else {
        let headers = ensure_inline_table_field(table, "headers");
        sync_inline_table_in_place(headers, &provider.headers, |v| {
            toml_edit::Value::from(v.as_str())
        });
    }
    // model_overrides — regular table keyed by model name; remove
    // key entirely if empty.
    if provider.model_overrides.is_empty() {
        table.remove("model_overrides");
    } else {
        let mo_item = table
            .entry("model_overrides")
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        let mo_table = mo_item
            .as_table_mut()
            .expect("model_overrides must be a table");
        // Remove stale model entries.
        let model_set: std::collections::HashSet<&str> = provider
            .model_overrides
            .keys()
            .map(String::as_str)
            .collect();
        let stale: Vec<String> = mo_table
            .iter()
            .map(|(k, _)| k.to_string())
            .filter(|k| !model_set.contains(k.as_str()))
            .collect();
        for k in stale {
            mo_table.remove(k.as_str());
        }
        // Create or update each model's overrides as an inline table.
        for (model, overrides) in &provider.model_overrides {
            if overrides.is_empty() {
                mo_table.remove(model);
            } else {
                let model_item = mo_table.entry(model).or_insert_with(|| {
                    toml_edit::Item::Value(toml_edit::Value::InlineTable(
                        toml_edit::InlineTable::new(),
                    ))
                });
                let model_table = match model_item {
                    toml_edit::Item::Value(v) if v.is_inline_table() => {
                        v.as_inline_table_mut().unwrap()
                    }
                    _ => {
                        *model_item = toml_edit::Item::Value(toml_edit::Value::InlineTable(
                            toml_edit::InlineTable::new(),
                        ));
                        model_item
                            .as_value_mut()
                            .unwrap()
                            .as_inline_table_mut()
                            .unwrap()
                    }
                };
                sync_inline_table_in_place(model_table, overrides, json_to_toml_value);
            }
        }
    }
    if provider.body_overrides.is_empty() {
        table.remove("body_overrides");
    } else {
        let body_overrides = ensure_inline_table_field(table, "body_overrides");
        sync_inline_table_in_place(body_overrides, &provider.body_overrides, json_to_toml_value);
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
            toml_edit::Value::from(v.as_str())
        });
    }
    // anthropic_cache_control — always write (no skip_serializing_if)
    table["anthropic_cache_control"] = toml_edit::value(provider.anthropic_cache_control);
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
fn write_route_fields(existing: &mut toml_edit::Table, route: &RouteConfig) {
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
    );

    // Force multi-line layout (`[\n    { ... },\n    { ... }\n]`) is
    // applied as a string-level post-process on the rendered doc in
    // `save_config_to_file` — see `reformat_inline_table_arrays_multiline`.
    // `toml_edit` preserves per-item prefix decor (comments) and any
    // existing multi-line formatting natively; the reformat step only
    // converts arrays that were originally single-line. Already-multi-line
    // arrays are left untouched by the reformatter (it detects `\n` in
    // the payload and skips).
}

/// Build a brand-new `InlineTable` for a route target. Used by
/// `write_route_fields` for the `on_create` closure of the inline-table
/// array sync. For the update path, [`write_target_fields`] is called
/// directly on the existing inline table.
fn build_target_table(target: &RouteTarget) -> toml_edit::InlineTable {
    let mut new_table = toml_edit::InlineTable::new();
    write_target_fields(&mut new_table, target);
    new_table
}

/// Update fields of an existing route target's inline table in place.
/// Mirrors the `skip_serializing_if` rules on `RouteTarget` so untouched
/// fields stay absent and existing comments / key order on present fields
/// are preserved. The `extra` map is synced via
/// [`toml_edit_util::sync_inline_table_in_place`] to preserve the user's
/// on-disk key ordering inside the nested `extra = { ... }` table.
fn write_target_fields(existing: &mut toml_edit::InlineTable, target: &RouteTarget) {
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
        sync_inline_table_in_place(extra_table, &target.extra, json_to_toml_value);
    }
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
            base_url: "https://api.openai.com/v1".into(),
            models_endpoint: "models".into(),
            timeout_secs: 120,
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
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
                    base_url: "https://api.openai.com/v1".into(),
                    models_endpoint: "models".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
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
                    base_url: "https://api.openai.com/v1".into(),
                    models_endpoint: "models".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
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
                    base_url: "https://api.openai.com/v1".into(),
                    models_endpoint: "models".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
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
                    base_url: "https://api.openai.com/v1".into(),
                    models_endpoint: "models".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
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
                    base_url: "https://api.openai.com/v1".into(),
                    models_endpoint: "models".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
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
                base_url: "https://custom.example.com/v1".into(),
                models_endpoint: "v1/list".into(),
                timeout_secs: 300,
                chunk_timeout_secs: Some(30),
                ttfb_timeout_secs: Some(45),
                headers,
                body_overrides,
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

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_round_trips_anthropic_cache_control_false() {
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
            .anthropic_cache_control = false;

        save_config_to_file(&config, &path).unwrap();

        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();
        let p = reloaded.providers.get("openai").unwrap();
        assert!(
            !p.anthropic_cache_control,
            "anthropic_cache_control=false should survive round-trip"
        );

        let _ = std::fs::remove_file(&path);
    }
}
