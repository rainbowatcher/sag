use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::config::{
    GatewayConfig, KeyStrategy, LogFormat, ProviderConfig, ProviderType, RouteConfig, RouteTarget,
    Strategy,
};
use crate::toml_edit_util::{
    ensure_array_field, ensure_inline_table_field, ensure_nested_inline_table, prune_keys,
    reformat_inline_table_arrays_multiline, sync_array_of_tables, sync_inline_table_array_in_place,
    sync_inline_table_in_place, sync_string_array_in_place, upsert_table,
};

/// A shared, hot-reloadable configuration handle.
#[derive(Debug, Clone)]
pub struct ConfigHandle {
    inner: Arc<RwLock<GatewayConfig>>,
    config_path: Option<Arc<PathBuf>>,
}

impl ConfigHandle {
    /// Build a `ConfigHandle` from an in-memory `GatewayConfig` (for testing / programmatic use).
    pub fn from_config(config: GatewayConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(config)),
            config_path: None,
        }
    }

    /// Read the current config (acquires a read lock).
    pub async fn read(&self) -> tokio::sync::RwLockReadGuard<'_, GatewayConfig> {
        self.inner.read().await
    }

    /// Acquire a write lock on the config (for runtime mutation via admin API).
    pub async fn write(&self) -> tokio::sync::RwLockWriteGuard<'_, GatewayConfig> {
        self.inner.write().await
    }

    /// Returns the path to the config file, or `None` if the config is in-memory.
    pub fn path(&self) -> Option<&Path> {
        self.config_path.as_ref().map(|p| p.as_path())
    }

    /// Returns `true` if the config was loaded from a real file (not in-memory).
    pub fn is_file_backed(&self) -> bool {
        self.config_path.is_some()
    }

    /// Persist the in-memory config back to disk if this handle is file-backed.
    ///
    /// Logs a warning on failure but never returns an error — persistence is
    /// best-effort so that an admin API mutation succeeds even if the disk
    /// write fails (the in-memory config is already updated).
    pub fn try_persist(&self, config: &GatewayConfig, action: &str) {
        if let Some(path) = self.path() {
            if let Err(e) = save_config_to_file(config, path) {
                warn!(?e, "failed to persist config after {action}");
            }
        }
    }
}

/// Quickly read `logging.level` and `logging.format` from a TOML file without
/// full config parsing. Returns `(level, format)` filled with defaults where
/// the file can't be read or the keys are missing.
///
/// This is used before tracing is initialized so that the log level and format
/// from the config file can be applied early, without waiting for the full
/// config load.
pub fn peek_logging_config(path: impl AsRef<Path>) -> (String, LogFormat) {
    let content = match std::fs::read_to_string(path.as_ref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[sag] failed to read config file '{}' for logging peek: {e}",
                path.as_ref().display()
            );
            return ("info".into(), LogFormat::default());
        }
    };
    #[derive(serde::Deserialize)]
    struct LoggingPeek {
        level: Option<String>,
        format: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct ConfigPeek {
        logging: Option<LoggingPeek>,
    }
    let config: ConfigPeek = match toml::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[sag] failed to parse config file '{}' as TOML for logging peek: {e}",
                path.as_ref().display()
            );
            return ("info".into(), LogFormat::default());
        }
    };
    let logging = match config.logging {
        Some(l) => l,
        None => {
            eprintln!(
                "[sag] no [logging] section found in config file '{}', using defaults",
                path.as_ref().display()
            );
            return ("info".into(), LogFormat::default());
        }
    };
    let level = logging.level.unwrap_or_else(|| "info".into());
    let format = match logging.format.as_deref() {
        Some("json") => LogFormat::Json,
        _ => LogFormat::Compact,
    };
    (level, format)
}

/// Warn if the config still uses the legacy top-level `retries` key.
///
/// `retries` now lives under `[backoff]`. serde silently ignores unknown
/// top-level fields, so without this check a user who kept `retries = N` at
/// the top level would silently fall back to the default (2) with no
/// indication. Parses the TOML a second time as a generic `Value` —
/// acceptable since config load/reload is not on the hot path.
fn warn_legacy_top_level_retries(content: &str) {
    if let Ok(value) = toml::from_str::<toml::Value>(content)
        && value.get("retries").is_some()
    {
        warn!(
            "top-level `retries` is no longer supported; move it under [backoff] \
             (e.g. `[backoff] retries = 2`). The top-level value is ignored — \
             falling back to [backoff].retries (default 2)."
        );
    }
}

/// Load the initial configuration from a TOML file and return a `ConfigHandle`.
pub fn load_config(path: impl AsRef<Path>) -> Result<ConfigHandle, sag_core::AppError> {
    let path = path.as_ref().to_path_buf();
    let content = std::fs::read_to_string(&path).map_err(|e| {
        sag_core::AppError::Config(format!("failed to read config {}: {e}", path.display()))
    })?;

    warn_legacy_top_level_retries(&content);
    let config: GatewayConfig = toml::from_str(&content)
        .map_err(|e| sag_core::AppError::Config(format!("failed to parse config: {e}")))?;

    config
        .validate()
        .map_err(|errors| sag_core::AppError::Config(errors.join("; ")))?;

    debug!(?config, "configuration loaded");
    Ok(ConfigHandle {
        inner: Arc::new(RwLock::new(config)),
        config_path: Some(Arc::new(path)),
    })
}

/// Handle for the hot-reload background task, including a shutdown signal.
pub struct HotReloadHandle {
    /// The spawned tokio task.
    pub task: tokio::task::JoinHandle<()>,
    /// Send `()` to request graceful shutdown of the watcher.
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl HotReloadHandle {
    /// Signal the watcher to shut down gracefully and wait for it to finish.
    ///
    /// This ensures the `notify::Watcher` is properly dropped, which stops
    /// OS-level file-watching threads (FSEvents on macOS, inotify on Linux).
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.task).await;
    }
}

/// Spawn a background task that watches the config file for changes and reloads it.
/// The returned handle will reflect the latest config. If `handle` is in-memory
/// (no associated file path), the watcher task exits immediately.
pub fn spawn_hot_reload(handle: ConfigHandle) -> HotReloadHandle {
    let inner = handle.inner.clone();
    let path = handle.config_path.clone();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let task = tokio::spawn(async move {
        // If there is no backing file, skip the watcher entirely.
        let Some(path) = path else {
            info!("hot-reload skipped for in-memory config");
            return;
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<notify::Event, notify::Error>>(16);

        let mut watcher = match RecommendedWatcher::new(
            move |res| {
                let _ = tx.blocking_send(res);
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                error!("failed to create file watcher: {e}");
                return;
            }
        };

        if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
            error!("failed to watch config file {}: {e}", path.display());
            return;
        }

        info!("hot-reload watcher started for {}", path.display());

        // Debounce: wait 500ms after last event before reloading
        let mut debounce = tokio::time::Instant::now();

        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    info!("hot-reload watcher shutting down");
                    // Drop the watcher explicitly — this stops OS file-watch threads.
                    drop(watcher);
                    return;
                }
                event_res = rx.recv() => {
                    match event_res {
                        Some(Ok(event)) => {
                            if is_modify_event(&event) {
                                let now = tokio::time::Instant::now();
                                if now.duration_since(debounce).as_millis() < 500 {
                                    continue; // debounce
                                }
                                debounce = now;

                                // Small delay to let the file system settle
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                                match reload_config(&path).await {
                                    Ok(new_config) => {
                                        let mut guard = inner.write().await;
                                        *guard = new_config;
                                        info!("configuration hot-reloaded");
                                    }
                                    Err(e) => {
                                        warn!("failed to reload config: {e}");
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            error!("file watcher error: {e}");
                        }
                        None => {
                            // All senders dropped — watcher was destroyed.
                            // This is a belt-and-suspenders safety net; in
                            // practice the shutdown signal arm handles exit.
                            info!("file watcher channel closed, exiting");
                            return;
                        }
                    }
                }
            }
        }
    });

    HotReloadHandle { task, shutdown_tx }
}

fn is_modify_event(event: &Event) -> bool {
    matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
}

async fn reload_config(path: &std::path::Path) -> Result<GatewayConfig, sag_core::AppError> {
    let content = tokio::fs::read_to_string(path).await.map_err(|e| {
        sag_core::AppError::Config(format!("failed to read config for reload: {e}"))
    })?;
    warn_legacy_top_level_retries(&content);
    let config: GatewayConfig = toml::from_str(&content)
        .map_err(|e| sag_core::AppError::Config(format!("failed to parse config: {e}")))?;

    config
        .validate()
        .map_err(|errors| sag_core::AppError::Config(errors.join("; ")))?;

    Ok(config)
}

/// Convert a `serde_json::Value` into a `toml_edit::Value`.
///
/// TOML has no `null`. `Null` is rejected (panic) rather than coerced to
/// the string `"null"` because silent coercion would corrupt the
/// config file: a `Null` written today would come back as the string
/// `"null"` on the next load and the user-visible behaviour would
/// drift silently.
///
/// Validation lives upstream in [`check_json_map`] (called from
/// [`save_config_to_file`] for both `body_overrides` and target
/// `extra`), which recursively walks the JSON tree and rejects nulls
/// at any depth. If this panic ever fires, one of two things has
/// happened: (a) a new call site was added without going through
/// validation, or (b) `check_json_map`'s recursion has a gap. Either
/// way, fail loudly rather than write a broken file.
fn json_to_toml_value(v: &serde_json::Value) -> toml_edit::Value {
    match v {
        serde_json::Value::Null => panic!(
            "json_to_toml_value called with Null at {v:?} — TOML has no null \
             representation and upstream validation (check_json_map) should \
             have rejected this value before reaching here. The panic indicates \
             either a new write path that bypasses validation, or a recursion \
             gap in check_json_map itself; investigate before re-running."
        ),
        serde_json::Value::Bool(b) => toml_edit::value(*b).into_value().unwrap(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml_edit::value(i).into_value().unwrap()
            } else if let Some(f) = n.as_f64() {
                toml_edit::value(f).into_value().unwrap()
            } else {
                toml_edit::value(n.to_string()).into_value().unwrap()
            }
        }
        serde_json::Value::String(s) => toml_edit::value(s.as_str()).into_value().unwrap(),
        serde_json::Value::Array(items) => {
            let mut arr = toml_edit::Array::new();
            for item in items {
                arr.push(json_to_toml_value(item));
            }
            toml_edit::Value::Array(arr)
        }
        serde_json::Value::Object(map) => {
            let mut table = toml_edit::InlineTable::new();
            for (k, val) in map {
                table.insert(k, json_to_toml_value(val));
            }
            toml_edit::Value::InlineTable(table)
        }
    }
}

/// Recursively validate a JSON value (or anything reachable through it)
/// for null at any depth. A null anywhere — top-level, inside an
/// array, or inside a nested object — is rejected because TOML has
/// no representation for it and any newline-coerced value would
/// silently drift on round-trip.
///
/// Returns the dotted-path index of the FIRST null encountered
/// (matches the previous top-level-only behaviour: first-null-wins
/// with a clear location). The path uses `[i]` for array indices and
/// `.field` for object keys, e.g. `extra.foo.bar` or `extra.list[0]`.
fn check_json_value_path(v: &serde_json::Value, path: &str) -> Result<(), sag_core::AppError> {
    match v {
        serde_json::Value::Null => Err(sag_core::AppError::Config(format!(
            "{path}: null value cannot be stored as TOML, remove the key or set an explicit value"
        ))),
        serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => Ok(()),
        serde_json::Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                check_json_value_path(item, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for (k, val) in map {
                check_json_value_path(val, &format!("{path}.{k}"))?;
            }
            Ok(())
        }
    }
}

/// Validate that a JSON map (top-level or nested somewhere under it)
/// doesn't contain null values. Walks recursively so that
/// `body_overrides = { foo: { bar: null } }` or `extra = { x: [null] }`
/// are also rejected — previously only top-level nulls were caught,
/// which let nested nulls reach [`json_to_toml_value`] and corrupt the
/// saved file. See [`check_json_value_path`] for the recursive scanner.
fn check_json_map(
    map: &HashMap<String, serde_json::Value>,
    context: &str,
) -> Result<(), sag_core::AppError> {
    for (key, val) in map {
        check_json_value_path(val, &format!("{context}.{key}"))?;
    }
    Ok(())
}

/// Persist the in-memory `GatewayConfig` back to the TOML file.
///
/// Uses `toml_edit` to parse the existing file and performs fine-grained
/// in-place updates. **All mutable sections are written back precisely:**
/// `[providers.*]`, `[[routes]]`, `[backoff]`, and the top-level `retries`
/// scalar are all updated in place, preserving comments and field order in
/// any field/table the in-memory config does not explicitly touch.
///
/// The remaining top-level sections (`[server]`, `[analytics]`,
/// `[gateway_auth]`) are intentionally **not** written back — the admin
/// save path only mutates the four sections above, and any user-authored
/// versions of those untouched sections are preserved verbatim.
pub fn save_config_to_file(
    config: &GatewayConfig,
    path: &std::path::Path,
) -> Result<(), sag_core::AppError> {
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
    // a string-level pass because `toml_edit`'s formatter ignores
    // per-item prefix decor in the version this project uses and
    // emits single-line arrays unconditionally otherwise — see
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
    // We don't try to coax the multi-line format out of `toml_edit`'s
    // array-of-inline-table formatter here; in this `toml_edit` version
    // it ignores per-item prefix decor and emits the array on one line
    // regardless.
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

    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Create a temp TOML file with the given content and return its path.
    fn temp_toml(content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let id = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let file_path = dir.join(format!("sag-test-{}-{}.toml", std::process::id(), id));
        let mut file = std::fs::File::create(&file_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file_path
    }

    fn valid_toml() -> &'static str {
        r#"[server]
host = "127.0.0.1"
port = 9090

[gateway_auth]
api_keys = ["test-key"]

[providers.openai]
api_keys = ["sk-test"]
"#
    }

    #[test]
    fn load_config_success() {
        let path = temp_toml(valid_toml());
        let handle = load_config(&path).unwrap();
        assert_eq!(handle.path(), Some(path.as_path()));

        // Can read the config synchronously via try_read
        let config = handle.inner.try_read().unwrap();
        assert_eq!(config.server.port, 9090);
        assert_eq!(
            config.gateway_auth.as_ref().unwrap().api_keys,
            vec!["test-key"]
        );
    }

    #[test]
    fn load_config_missing_file() {
        let result = load_config("/tmp/does-not-exist-sag-test.toml");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("failed to read config"));
    }

    #[test]
    fn load_config_invalid_toml() {
        let path = temp_toml("this is not valid toml {{{ ");
        let result = load_config(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("failed to parse config"));
    }

    #[test]
    fn load_config_missing_providers() {
        let path = temp_toml("[server]\n");
        let result = load_config(&path);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("failed to parse config")
        );
    }

    #[test]
    fn config_handle_clone_shares_state() {
        let path = temp_toml(valid_toml());
        let h1 = load_config(&path).unwrap();
        let h2 = h1.clone();

        let c1 = h1.inner.try_read().unwrap();
        let c2 = h2.inner.try_read().unwrap();
        assert_eq!(c1.server.port, c2.server.port);
        // They point to the same Arc — same address
        let ptr1 = &*c1 as *const GatewayConfig;
        let ptr2 = &*c2 as *const GatewayConfig;
        assert_eq!(ptr1, ptr2, "Clone should share the same inner Arc");
    }

    #[test]
    fn is_modify_event_detects_modify() {
        let event = Event::new(EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        )));
        assert!(is_modify_event(&event));
    }

    #[test]
    fn is_modify_event_detects_create() {
        let event = Event::new(EventKind::Create(notify::event::CreateKind::File));
        assert!(is_modify_event(&event));
    }

    #[test]
    fn is_modify_event_ignores_access() {
        let event = Event::new(EventKind::Access(notify::event::AccessKind::Close(
            notify::event::AccessMode::Write,
        )));
        assert!(!is_modify_event(&event));
    }

    #[test]
    fn is_modify_event_ignores_remove() {
        let event = Event::new(EventKind::Remove(notify::event::RemoveKind::File));
        assert!(!is_modify_event(&event));
    }

    // ── save_config_to_file tests ──

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
        // Tighten the assertion: route-level retries=None and target-level
        // retries=None / real_model=None must all be absent from the saved
        // file. The previous `!saved.contains("retries")` was too coarse —
        // it also matched the top-level `retries = 2` line, so the test
        // would have passed even if route-level retries were being written.
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
        // Regression: `update_existing_provider` used to unconditionally
        // write both `provider_type` and `key_strategy`, polluting the
        // file with defaults that a user's minimum-viable [providers.X]
        // block intentionally omitted. With the fix the update path
        // mirrors `skip_serializing_if`: default values are dropped,
        // non-default values are written. This test goes load→save
        // (the path the bug exercised); `default_fields_not_written`
        // already covers the create-from-scratch path which was always
        // correct.
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
        // Symmetry check: the same predicate that drops defaults must
        // also write non-defaults when the in-memory value differs from
        // the declared default. Without this, an admin-API mutation
        // that flips `key_strategy` to `round_robin` would silently
        // not be persisted.
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

    #[test]
    #[should_panic(expected = "json_to_toml_value called with Null")]
    fn json_to_toml_value_panics_on_null() {
        // Defence-in-depth test: even if a future call site forgets to
        // route through check_json_map, the silent "write the string
        // \"null\"" coercion is gone — a Null reaching json_to_toml_value
        // panics loudly so the bug surfaces instead of corrupting the
        // config file. The previous code mapped Null to the string
        // "null" defensively, which meant a regression in the upstream
        // null check would silently produce broken-but-parseable TOML.
        let _ = json_to_toml_value(&serde_json::Value::Null);
    }

    #[test]
    fn extra_with_nested_null_in_object_returns_error() {
        // Regression: `check_json_map` previously only caught top-level
        // nulls (and nulls in top-level arrays). A null nested inside a
        // map value — e.g. `extra = { nested: { temperature: null } }` —
        // would slip past validation and reach json_to_toml_value, which
        // then silently wrote the string "null" into the saved file.
        // After the recursive refactor, ALL nulls in the JSON tree are
        // caught.
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
                    // Nested null — top-level value is an Object whose
                    // interior contains a Null. The previous flat check
                    // would have missed this.
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
        // Same regression as `extra_with_nested_null_in_object_...` but
        // along the Array branch: a null inside an Array nested under a
        // top-level object also used to slip past the flat check.
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
                    // Null buried inside a top-level array value
                    // (parallel to the object case in the other test).
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
        // Regression for Problem 1: with single-field `provider`
        // keying, two targets sharing `provider="openai"` but differing
        // in `real_model` would silently overwrite each other on save
        // (the position-by-key map only had one slot for "openai"). The
        // composite `(provider, real_model)` key gives each a distinct
        // slot, so load → save → reload preserves both entries with
        // their distinct priorities and extras.
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

        // Both target entries reachable, with distinct real_models AND
        // distinct priorities (i.e. neither got overwritten by the
        // other — the original bug).
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
        // Update only one of the two same-provider targets and verify
        // the sibling's other fields (priority, extras) are preserved
        // verbatim. This is the precise-edit promise of the composite
        // key — the position-by-key map anchors `on_update` to the
        // exact entry, not the "first one with this provider" entry.
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

        // Bump the gpt-3.5 entry's priority and add an extra —
        // gpt-4 entry must NOT be touched.
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
        // After inserting a new route, comments on the OTHER (pre-existing)
        // route should survive — this is the precise-edit promise of
        // sync_array_of_tables (only the matched entry is mutated).
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

        // Add a second route via the in-memory config
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
        // The pre-existing route's comment should still be there
        assert!(
            saved.contains("# Keep me"),
            "unrelated route's comment should be preserved, got:\n{saved}"
        );
        // The new route should be appended
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

        // Remove anthropic from in-memory config
        config.providers.remove("anthropic");

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("openai"), "openai should remain");
        assert!(
            !saved.contains("anthropic"),
            "anthropic should be removed, got:\n{saved}"
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

        // Empty out routes
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
        // The on-disk order of `extra` keys should be preserved on save.
        // Previously, `extra` was iterated as a `HashMap` so the order was
        // non-deterministic across saves — this test pins the order.
        //
        // `RouteTarget::extra` is serialized as a nested inline table
        // (`extra = { ... }`). The `toml` crate (used for parsing) only
        // supports single-line inline tables, so we cannot put comments
        // between `extra` keys; we rely on the per-key order check below
        // to verify the precise-edit logic.
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
        // The `extra = { ... }` inline table itself is preserved across a
        // round-trip. We can't put comments between its fields (the `toml`
        // crate only supports single-line inline tables), but the keys
        // and values must survive verbatim.
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
        // Removing an `extra` key from the in-memory config should drop
        // it from the file too.
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
        // Adding an `extra` key should append it at the end (sorted
        // alongside any other new keys) without disturbing the existing
        // keys' positions.
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
        // New key should come after the existing one (preserved order)
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
        // A comment on one target should be preserved even when the
        // sibling targets are resaved — the precise-edit promise for
        // `targets` is the same as for the rest of the file.
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
        // Removing a target from in-memory should remove it from the
        // saved file.
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
        // The provider block still exists (we only removed the *target*),
        // but only one `provider = "..."` should appear inside the
        // targets array.
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
        // When the on-disk config has `targets = [{...}, {...}]` (single
        // line, user-authored or written by an older save), the saver
        // must convert to multi-line format on the next save so the
        // targets are readable / hand-editable.
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
        // On-disk order of api_keys should be preserved; previously these
        // were rebuilt from Vec iteration which lost user comments but
        // happened to be deterministic. Now `sync_string_array_in_place`
        // preserves position for keys that are still present.
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
        // When api_keys becomes empty, the field should be removed
        // (matching skip_serializing_if).
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
        // Previously, `headers` was rebuilt from a HashMap so the order
        // was non-deterministic across saves. Now it uses
        // `sync_inline_table_in_place` which preserves on-disk order.
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
        // Test that model_overrides survive a save→load round-trip.
        // Build the config programmatically (to avoid TOML parsing
        // edge cases with nested inline tables under sub-headers),
        // save it, then reload and compare.
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
        // When a model override entry is removed from the in-memory
        // config, the save must drop it from the file too.
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

        // Remove gpt-3.5-turbo from model overrides
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
        // When all model_overrides are cleared, the
        // [providers.X.model_overrides] section should disappear.
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
}
