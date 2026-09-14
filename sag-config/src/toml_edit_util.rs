//! Path-based precise-edit primitives for `toml_edit::DocumentMut`.
//!
//! Every operation takes a `path: &[&str]` and preserves comments, field
//! order, and formatting in fields the caller does not explicitly touch.
//!
//! # Primitives
//!
//! | Function                              | Structure                           | Notes                                |
//! |---------------------------------------|-------------------------------------|--------------------------------------|
//! | [`set_scalar`]                        | Any path → Value                    | Creates intermediate tables as needed |
//! | [`upsert_table`]                      | Path → single Table                 | `on_create` for new, `on_update` for existing |
//! | [`sync_array_of_tables`]              | Path → ArrayOfTables                | Key-matched; preserves untouched fields |
//! | [`sync_inline_table_array_in_place`]  | `&mut Array` → inline-table items   | Key-matched; operates on already-resolved array (for nested arrays) |
//! | [`remove_if_exists`]                  | Any path                            | No-op if missing                     |
//! | [`prune_keys`]                        | Path → Table, remove non-matching   | Bulk stale-key removal               |
//!
//! # Why closures, not a trait?
//!
//! The two write paths (single table vs. array-of-tables) are structurally
//! different — a `TomlSyncSection` trait would have to split into two traits
//! (or branch at runtime) and gains little for a single call site. Closure
//! callbacks keep the surface minimal: each call site writes the field-assign
//! logic inline and never touches `Table`/`Item` directly.
//!
//! # When callbacks can fail
//!
//! `on_create` returns `Result` because the most common case
//! (serializing a `serde::Serialize` value via `toml_edit::ser::to_document`)
//! can fail. `on_update` also returns `Result` so that fallible value
//! converters nested inside an update (e.g. `json_to_toml_value`, which
//! rejects JSON `null`) can abort the save before the atomic write —
//! infallible call sites simply return `Ok(())`.

use std::collections::{HashMap, HashSet};

use sag_core::AppError;
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

// ────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ────────────────────────────────────────────────────────────────────────────

/// Walk down `path` from `root`, creating intermediate tables as needed.
/// On the final key, call `on_create` to produce the new value if missing.
/// Returns a mutable reference to the final item.
fn ensure_path<'a, F>(root: &'a mut Item, path: &[&str], on_create: F) -> &'a mut Item
where
    F: FnOnce() -> Item,
{
    assert!(!path.is_empty(), "path must be non-empty");

    // First pass: ensure all intermediate (non-final) keys exist as tables.
    let mut current = root;
    if path.len() > 1 {
        for key in &path[..path.len() - 1] {
            let key_exists = current
                .as_table()
                .expect("path element must be a table")
                .contains_key(key);
            if !key_exists {
                current
                    .as_table_mut()
                    .expect("path element must be a table")
                    .insert(key, Item::Table(Table::new()));
            }
            current = current
                .as_table_mut()
                .expect("path element must be a table")
                .get_mut(key)
                .expect("just inserted or already existed");
        }
    }

    // Now `current` is the parent of the final key.
    let last = path[path.len() - 1];
    let final_exists = current
        .as_table()
        .expect("path element must be a table")
        .contains_key(last);

    if !final_exists {
        // Call `on_create` exactly once, after all intermediates are in place.
        let created = on_create();
        current
            .as_table_mut()
            .expect("path element must be a table")
            .insert(last, created);
    }

    current
        .as_table_mut()
        .expect("path element must be a table")
        .get_mut(last)
        .expect("just inserted or already existed")
}

/// Apply multi-line formatting to a `toml_edit` Array if warranted.
///
/// - Arrays whose single-line rendering is ≤80 chars are left untouched.
/// - Arrays already containing newlines (from a prior save) stay multi-line.
/// - Arrays exceeding 80 chars are expanded with per-item indentation,
///   trailing comma, and trailing newline.
///
/// Indentation is sniffed from existing multi-line items so new items
/// match the original file's style. Per-item leading comments are
/// preserved (items whose prefix already contains a newline are skipped).
fn format_array_multiline(array: &mut Array) {
    if array.is_empty() {
        return;
    }

    let rendered = array.to_string();
    let is_single_line = !rendered.contains('\n');
    let needs_multiline = !is_single_line || rendered.len() > 80;
    if !needs_multiline {
        return;
    }

    // Sniff the indent from existing multi-line items.
    let default_indent = "\n    ";
    let indent: String = array
        .iter()
        .find_map(|v| {
            v.decor()
                .prefix()
                .and_then(|r| r.as_str())
                .and_then(|s| s.rfind('\n').map(|i| s[i..].to_string()))
        })
        .unwrap_or_else(|| default_indent.to_string());

    let mut any_changed = false;
    for v in array.iter_mut() {
        let current = v.decor().prefix().and_then(|r| r.as_str()).unwrap_or("");
        if !current.contains('\n') {
            v.decor_mut().set_prefix(indent.as_str());
            any_changed = true;
        }
    }

    // Trailing comma (Taplo-style) via toml_edit's native API.
    array.set_trailing_comma(true);

    // Trailing newline only when items were modified (single-line →
    // multi-line). If all items were already multi-line, the array's
    // trailing is left untouched to avoid double-newline blank lines.
    if any_changed {
        array.set_trailing("\n");
    }
}

/// Walk down `path` from `root`, returning `None` if any segment is missing.
fn navigate_mut<'a>(root: &'a mut Item, path: &[&str]) -> Option<&'a mut Item> {
    let mut current = root;
    for key in path {
        current = current.as_table_mut()?.get_mut(key)?;
    }
    Some(current)
}

// ────────────────────────────────────────────────────────────────────────────
// Public API
// ────────────────────────────────────────────────────────────────────────────

/// Set a scalar value at `path`, creating intermediate tables as needed.
///
/// Replaces the value at the path. Other fields under the same parent table
/// are preserved with their comments and formatting.
pub fn set_scalar<V: Into<Value>>(doc: &mut DocumentMut, path: &[&str], value: V) {
    let v = value.into();
    let target = ensure_path(doc.as_item_mut(), path, || Item::Value(v.clone()));
    *target = Item::Value(v);
}

/// Create or update a single table at `path`.
///
/// - If `path` does not exist, `on_create` is called to produce the new
///   `Item` (typically via `toml_edit::ser::to_document`, which honors
///   `skip_serializing_if`). The item is inserted and **not** further
///   mutated — `on_update` is **not** called in this case.
/// - If `path` exists, `on_update` is called to mutate it in-place.
///   **Any field `on_update` does not touch is preserved** with its original
///   comments and formatting.
///
/// Calling both `on_create` and `on_update` on the same path would defeat
/// the purpose of `skip_serializing_if` (the create produces a lean table,
/// the update would re-add the skipped fields). This function makes the
/// create/update path explicit at the call site.
pub fn upsert_table<F, G>(
    doc: &mut DocumentMut,
    path: &[&str],
    on_create: F,
    on_update: G,
) -> Result<(), AppError>
where
    F: FnOnce() -> Result<Item, AppError>,
    G: FnOnce(&mut Item) -> Result<(), AppError>,
{
    match navigate_mut(doc.as_item_mut(), path) {
        Some(existing) => {
            on_update(existing)?;
            Ok(())
        }
        None => {
            let created = on_create()?;
            ensure_path(doc.as_item_mut(), path, || created);
            Ok(())
        }
    }
}

/// Sync an `ArrayOfTables` at `path` with `items`.
///
/// For each item:
/// - If an entry with the same `key_fn(item)` already exists, `on_update` is
///   called to mutate it in-place (preserving untouched fields/comments).
/// - Otherwise, `on_create` is called to build a new entry, which is
///   appended to the array.
///
/// Entries that exist on disk but not in `items` are removed. If `items` is
/// empty, the entire path is removed (no empty `path = []` left behind).
///
/// `key_fn` must read a stable identifier from `T` (e.g. `model_name` for
/// routes). `key_field` is the name of that same identifier as a field in
/// the on-disk TOML table — the same field is consulted to find and prune
/// existing entries.
pub fn sync_array_of_tables<T, K, FC, FU>(
    doc: &mut DocumentMut,
    path: &[&str],
    key_field: &str,
    items: &[T],
    key_fn: K,
    on_create: FC,
    on_update: FU,
) -> Result<(), AppError>
where
    K: Fn(&T) -> &str,
    FC: Fn(&T) -> Result<Table, AppError>,
    FU: Fn(&mut Table, &T) -> Result<(), AppError>,
{
    assert!(!path.is_empty(), "path must be non-empty");

    // Empty items → remove the section entirely (no empty array left behind).
    if items.is_empty() {
        remove_if_exists(doc, path);
        return Ok(());
    }

    let last = path[path.len() - 1];
    let parent_path = &path[..path.len() - 1];

    // Ensure intermediate parent tables exist (no-op for a 1-segment path).
    if !parent_path.is_empty() {
        ensure_path(doc.as_item_mut(), parent_path, || Item::Table(Table::new()));
    }

    // Resolve the parent item (root or last intermediate table).
    let parent = if parent_path.is_empty() {
        doc.as_item_mut()
    } else {
        navigate_mut(doc.as_item_mut(), parent_path).expect("parent path was just ensured")
    };

    // Initialize the array-of-tables slot if missing or wrong type.
    let needs_init = {
        let parent_table = parent.as_table().expect("parent must be a table");
        parent_table
            .get(last)
            .and_then(|i| i.as_array_of_tables())
            .is_none()
    };
    if needs_init {
        parent
            .as_table_mut()
            .expect("parent must be a table")
            .insert(last, Item::ArrayOfTables(ArrayOfTables::new()));
    }

    // Take a mutable reference to the array.
    let arr = parent
        .as_table_mut()
        .expect("parent must be a table")
        .get_mut(last)
        .expect("just initialized")
        .as_array_of_tables_mut()
        .expect("just initialized as array of tables");

    // Snapshot the desired key set for the "remove stale" pass. OWNED
    // `String` keys so the set lives across the array-mutating loop below
    // without borrow conflicts.
    let desired_keys: HashSet<String> = items.iter().map(|item| key_fn(item).to_string()).collect();

    // Build a position index of existing entries matched on `key_field`.
    // O(m) one-time scan — lets the per-item update loop below drop from
    // O(n*m) (linear scan per item) to O(n+m) (O(1) lookup per item).
    //
    // `entry().or_insert()` preserves the previous "first match wins"
    // behaviour: when two on-disk rows share the same `key_field` value
    // (which is itself an invariant violation), the earlier position is
    // kept — matching the old `iter().position()` semantics.
    //
    // Invariant: `on_update` must NOT write a `key_field` value different
    // from `key_fn(item)`. The position map captures indices by the
    // in-memory key value at build time; the retain pass reads the
    // post-update on-disk value. A mismatch would cause ghost retention
    // (an "updated" entry that the retain pass keeps, then a stale entry
    // in the same position that `on_update` missed). Today's call sites
    // only touch non-key fields — keep it that way.
    let mut position_by_key: HashMap<String, usize> = HashMap::with_capacity(arr.len());
    for (pos, t) in arr.iter().enumerate() {
        if let Some(key) = t.get(key_field).and_then(|v| v.as_str()) {
            position_by_key.entry(key.to_string()).or_insert(pos);
        }
    }

    // Update or create each entry. O(n) total (HashMap::get is O(1)
    // amortized). Note: `on_update` does not push new entries into `arr`,
    // so the captured `pos` indices remain stable throughout the loop.
    for item in items {
        let key = key_fn(item);
        match position_by_key.get(key).copied() {
            Some(pos) => {
                let existing = arr.get_mut(pos).expect("position was just indexed");
                on_update(existing, item)?;
            }
            None => {
                let new_table = on_create(item)?;
                arr.push(new_table);
            }
        }
    }

    // Drop entries whose keys are no longer in `items`. O(m) retain
    // traversal; each check is O(1) amortized via the desired set.
    arr.retain(|t| {
        t.get(key_field)
            .and_then(|v| v.as_str())
            .map(|s| desired_keys.contains(s))
            .unwrap_or(false)
    });

    Ok(())
}

/// Sync a `toml_edit::Array` of `Value::InlineTable` items in place,
/// matching by a **composite key** built from one or more on-disk
/// fields. The inline-table analogue of [`sync_array_of_tables`].
/// Example: `targets = [{ provider = "...", real_model = "..." }]`
/// where the natural composite key is `(provider, real_model)` so two
/// targets sharing `provider="openai"` but with different
/// `real_model`s are correctly distinguished.
///
/// `key_fields[i]` is the name of the i-th TOML field to read for
/// matching, and `key_fn(item)[i]` is the i-th value of the in-memory
/// composite key. The two slices must have the same length. An
/// `Option::None` in the i-th slot — from `key_fn` returning None or
/// from the field being absent on disk — distinguishes a target whose
/// i-th field is deliberately omitted from one whose field is present.
///
/// # Why composite keying
///
/// Single-field matching (`provider` alone) is unsafe when more than
/// one target may legitimately reference the same `provider` with
/// different `real_model`s, e.g. `[{ provider="openai",
/// real_model="gpt-4" }, { provider="openai", real_model="gpt-3.5" }]`.
/// Single-field would conflate them and the second `on_update` would
/// silently overwrite the first target's `priority`/`extra` with the
/// second's values. Composite `(provider, real_model)` matching
/// distinguishes them; combined with `RouteConfig::validate()` rejecting
/// duplicates within a route, save+load is now lossless.
///
/// # Behaviour
///
/// For each in-memory item:
/// - If an existing entry's composite key equals `key_fn(item)`, call
///   `on_update` to mutate that inline table in-place (preserving
///   untouched fields, comments, and key order).
/// - Otherwise, call `on_create` to build a new `InlineTable`, which is
///   appended.
///
/// Stale entries (no matching in-memory item) are removed from the array.
/// Non-inline-table items are also removed defensively.
///
/// This function takes a `&mut Array` directly (not a path) because the
/// caller has typically already resolved the parent path — the array is
/// usually a field of a parent table being updated in place. The caller
/// is responsible for ensuring the array exists and is the right type;
/// this function does not handle path resolution.
///
/// # Example
///
/// ```ignore
/// // `existing` is `&mut toml_edit::Table` for a route's sub-table
/// if !existing.contains_key("targets") {
///     existing["targets"] = toml_edit::value(toml_edit::Array::new());
/// }
/// let arr = existing["targets"].as_array_mut().expect("targets must be an array");
/// sync_inline_table_array_in_place(
///     arr,
///     &route.targets,
///     &["provider", "real_model"],                                 // composite key
///     |t| vec![Some(t.provider.clone()), t.real_model.clone()],    // in-memory key
///     |t| build_new_target(t),         // returns Result<InlineTable, AppError>
///     |existing, t| write_target_fields(existing, t), // returns Result<(), AppError>
/// )?;
/// ```
pub fn sync_inline_table_array_in_place<T, K, FC, FU>(
    array: &mut Array,
    items: &[T],
    key_fields: &[&str],
    key_fn: K,
    on_create: FC,
    on_update: FU,
) -> Result<(), AppError>
where
    K: Fn(&T) -> Vec<Option<String>>,
    FC: Fn(&T) -> Result<InlineTable, AppError>,
    FU: Fn(&mut InlineTable, &T) -> Result<(), AppError>,
{
    assert!(
        !key_fields.is_empty(),
        "key_fields must be non-empty (e.g. [\"provider\"] or [\"provider\", \"real_model\"])"
    );

    // Snapshot desired composite keys for the "remove stale" pass. OWNED
    // `Vec<Option<String>>` so the set lives across the array-mutating
    // loop below without borrow conflicts. Use an inline closure
    // (`|item| key_fn(item)`) — passing `key_fn` to `.map()` directly
    // would move the closure into the Map adapter, and we re-invoke
    // `key_fn` in the per-item update loop below.
    let desired_keys: HashSet<Vec<Option<String>>> = items.iter().map(&key_fn).collect();

    // Build a position index of existing inline-table entries matched
    // against the composite key built from `key_fields`. O(m) one-time
    // scan — lets the per-item update loop below run in O(n+m) instead
    // of O(n*m). `entry().or_insert()` preserves "first match wins"
    // semantics: if a legacy config accidentally contains two entries
    // with the same composite key (validate() should forbid this for
    // route targets), the earlier position is kept — save() updates the
    // first match, and the duplicate's fate is decided by whether its
    // composite key is in the desired set.
    //
    // Invariant: `on_update` must NOT write any `key_fields[i]` to a
    // value different from `key_fn(item)[i]`. The position map captures
    // indices by the in-memory composite key at build time; the retain
    // pass reads the post-update on-disk composite key. A mismatch
    // would cause ghost retention. Today's call sites only touch
    // non-key fields — keep it that way.
    let mut position_by_key: HashMap<Vec<Option<String>>, usize> =
        HashMap::with_capacity(array.len());
    for (pos, v) in array.iter().enumerate() {
        if let Some(table) = v.as_inline_table() {
            let key = read_composite_key(table, key_fields);
            position_by_key.entry(key).or_insert(pos);
        }
    }

    // Update or create each entry. O(n) total (HashMap::get is O(1)
    // amortized). Note: `on_update` does not push new entries into the
    // outer array, so the captured `pos` indices remain stable
    // throughout the loop.
    for item in items {
        let key = key_fn(item);
        match position_by_key.get(&key).copied() {
            Some(pos) => {
                if let Some(existing) = array.get_mut(pos).and_then(|v| v.as_inline_table_mut()) {
                    on_update(existing, item)?;
                }
                // If the indexed entry is somehow no longer an inline
                // table, silently skip the update — the retain pass
                // below will drop it as a defensive measure.
            }
            None => {
                let new_table = on_create(item)?;
                array.push(Value::InlineTable(new_table));
            }
        }
    }

    // Drop entries whose composite key is no longer in `items`.
    // Non-inline-table items are also removed (defensive — they
    // shouldn't be here). O(m) retain traversal; each check is O(1)
    // amortized via the desired set.
    array.retain(|v| {
        v.as_inline_table()
            .map(|t| read_composite_key(t, key_fields))
            .map(|key| desired_keys.contains(&key))
            .unwrap_or(false)
    });

    format_array_multiline(array);
    Ok(())
}

/// Read a composite key from an inline-table entry: for each name in
/// `key_fields`, read the corresponding field as a string, or `None`
/// if the field is absent or not a string. The returned
/// `Vec<Option<String>>` is positional — index `i` corresponds to
/// `key_fields[i]`. Two entries match iff their composite keys are
/// exactly equal (Vec equality is structural).
///
/// `Option::None` makes a deliberate distinction between "field
/// absent" and "field present-but-empty/empty-string". `real_model =
/// ""` is not the same as `real_model` being omitted — both are
/// caller-visible meanings and the validator catches invalid empties
/// separately.
fn read_composite_key(table: &InlineTable, key_fields: &[&str]) -> Vec<Option<String>> {
    key_fields
        .iter()
        .map(|kf| table.get(kf).and_then(|v| v.as_str()).map(String::from))
        .collect()
}

/// Remove the key at `path` if it exists. No-op if any segment of the path
/// is missing.
pub fn remove_if_exists(doc: &mut DocumentMut, path: &[&str]) {
    if path.is_empty() {
        return;
    }
    let last = path[path.len() - 1];
    let parent_path = &path[..path.len() - 1];

    let parent = if parent_path.is_empty() {
        doc.as_item_mut()
    } else {
        match navigate_mut(doc.as_item_mut(), parent_path) {
            Some(p) => p,
            None => return,
        }
    };

    if let Some(table) = parent.as_table_mut() {
        table.remove(last);
    }
}

/// In the table at `path`, remove every key that is not in `keep`.
/// No-op if the path doesn't exist.
pub fn prune_keys(doc: &mut DocumentMut, path: &[&str], keep: &[&str]) {
    let Some(item) = navigate_mut(doc.as_item_mut(), path) else {
        return;
    };
    let Some(table) = item.as_table_mut() else {
        return;
    };
    let to_remove: Vec<String> = table
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| !keep.contains(&k.as_str()))
        .collect();
    for key in to_remove {
        table.remove(&key);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Field ensure / sync helpers
// ────────────────────────────────────────────────────────────────────────────

/// Ensure `table` contains an `array` field at `key`. If the field is
/// missing or not an array, initialize it to an empty array. Returns a
/// mutable reference to the array.
pub fn ensure_array_field<'a>(table: &'a mut Table, key: &str) -> &'a mut Array {
    let needs_init = table.get(key).map(|v| !v.is_array()).unwrap_or(true);
    if needs_init {
        table.insert(key, Item::Value(Value::Array(Array::new())));
    }
    table
        .get_mut(key)
        .and_then(|i| i.as_array_mut())
        .expect("just initialized as array")
}

/// Ensure `table` contains an `inline_table` field at `key`. If the
/// field is missing or not an inline table, initialize it to an empty
/// inline table. Returns a mutable reference to the inline table.
pub fn ensure_inline_table_field<'a>(table: &'a mut Table, key: &str) -> &'a mut InlineTable {
    let needs_init = table.get(key).map(|v| !v.is_inline_table()).unwrap_or(true);
    if needs_init {
        table.insert(key, Item::Value(Value::InlineTable(InlineTable::new())));
    }
    table
        .get_mut(key)
        .and_then(|i| i.as_inline_table_mut())
        .expect("just initialized as inline table")
}

/// Ensure a nested `inline_table` field inside an `InlineTable` parent
/// (e.g. `extra = { ... }` inside a route target). If the field is
/// missing or not an inline table, initialize it to an empty inline
/// table. Returns a mutable reference to the inline table.
pub fn ensure_nested_inline_table<'a>(
    parent: &'a mut InlineTable,
    key: &str,
) -> &'a mut InlineTable {
    let needs_init = parent
        .get(key)
        .map(|v| !v.is_inline_table())
        .unwrap_or(true);
    if needs_init {
        parent.insert(key, Value::InlineTable(InlineTable::new()));
    }
    parent
        .get_mut(key)
        .and_then(|v| v.as_inline_table_mut())
        .expect("just initialized as inline table")
}

/// Sync an array of `Value::String` items in place against an in-memory
/// `&[String]`. The string-array analogue of
/// [`sync_inline_table_array_in_place`] — for nested arrays like
/// `api_keys = ["sk-1", "sk-2", ...]`.
///
/// For each in-memory value:
/// - If the value is already present in the array, its position is kept
///   (and any leading comment is preserved).
/// - Otherwise it is appended to the end.
///
/// Items in the array that are not in `in_memory` are removed. Non-string
/// items are also removed defensively.
///
/// Unlike the inline-table-array primitive, this function does not need a
/// `key_field` parameter — the "key" is the value itself.
pub fn sync_string_array_in_place(array: &mut Array, in_memory: &[String]) {
    // Snapshot desired values for the "remove stale" pass.
    let desired: Vec<&str> = in_memory.iter().map(String::as_str).collect();

    // Update or create each entry.
    for val in in_memory {
        let mut found = false;
        for v in array.iter_mut() {
            if v.as_str() == Some(val.as_str()) {
                found = true;
                break;
            }
        }
        if !found {
            array.push(val.as_str());
        }
    }

    // Drop entries whose values are no longer in `in_memory`. Non-string
    // items are also removed (defensive — they shouldn't be here).
    array.retain(|v| v.as_str().map(|s| desired.contains(&s)).unwrap_or(false));

    format_array_multiline(array);
}

/// Sync the keys of an existing inline table against an in-memory map,
/// preserving the on-disk key order, removing stale keys, and appending
/// new keys in sorted order. The value-conversion closure lets this
/// function work with any value type (e.g. `serde_json::Value`,
/// `String`).
///
/// The converter is fallible so that validation-gap errors (e.g. a
/// JSON `null` reaching `json_to_toml_value` despite upstream checks)
/// abort the save instead of panicking or writing a corrupted file.
/// Infallible converters wrap their result in `Ok`.
///
/// # Behavior
///
/// - Existing keys are iterated in their on-disk order. Each is either
///   updated in place (preserving position and any leading comment) or
///   removed if no longer present in `in_memory`.
/// - New keys (present in `in_memory` but not on disk) are appended at
///   the end in **sorted** order for determinism. (Sorted, not in-memory
///   order, because `HashMap` iteration is non-deterministic; the user
///   can't rely on insertion order at this layer.)
/// - On converter error, the function returns `Err` immediately. The
///   partially-mutated table is discarded by the caller's error path —
///   the save function never renders/renames after an error.
pub fn sync_inline_table_in_place<V, F>(
    existing: &mut InlineTable,
    in_memory: &std::collections::HashMap<String, V>,
    to_toml_value: F,
) -> Result<(), AppError>
where
    F: Fn(&V) -> Result<Value, AppError>,
{
    // Step 1: snapshot existing keys (in on-disk order) before mutating.
    let existing_keys: Vec<String> = existing.iter().map(|(k, _)| k.to_string()).collect();

    // Step 2: remove existing keys not in `in_memory`.
    for key in &existing_keys {
        if !in_memory.contains_key(key) {
            existing.remove(key);
        }
    }

    // Step 3: update existing keys' values in on-disk order (re-insert
    // preserves position).
    for key in &existing_keys {
        if let Some(value) = in_memory.get(key) {
            existing.insert(key, to_toml_value(value)?);
        }
    }

    // Step 4: append new keys (sorted for determinism).
    let mut new_keys: Vec<&String> = in_memory
        .keys()
        .filter(|k| !existing.contains_key(k))
        .collect();
    new_keys.sort();
    for key in new_keys {
        existing.insert(key, to_toml_value(&in_memory[key])?);
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> DocumentMut {
        s.parse().expect("test fixture must be valid TOML")
    }

    const MODEL_NAME_KEY: &str = "model_name";

    // ── set_scalar ──

    #[test]
    fn set_scalar_creates_path_when_missing() {
        let mut doc = parse("");
        set_scalar(&mut doc, &["retries"], 5i64);
        assert_eq!(
            doc["retries"].as_value().and_then(|v| v.as_integer()),
            Some(5)
        );
    }

    #[test]
    fn set_scalar_creates_intermediate_tables() {
        let mut doc = parse("");
        set_scalar(&mut doc, &["nested", "deep", "value"], 42i64);
        assert_eq!(
            doc["nested"]["deep"]["value"]
                .as_value()
                .and_then(|v| v.as_integer()),
            Some(42)
        );
    }

    #[test]
    fn set_scalar_replaces_existing_value() {
        let mut doc = parse("retries = 3\n");
        set_scalar(&mut doc, &["retries"], 5i64);
        assert_eq!(
            doc["retries"].as_value().and_then(|v| v.as_integer()),
            Some(5)
        );
    }

    #[test]
    fn set_scalar_preserves_sibling_fields() {
        let mut doc = parse(
            r#"
[backoff]
base_ms = 100
max_ms = 5000
jitter = true
"#,
        );
        set_scalar(&mut doc, &["backoff", "base_ms"], 200i64);
        // base_ms updated
        assert_eq!(
            doc["backoff"]["base_ms"]
                .as_value()
                .and_then(|v| v.as_integer()),
            Some(200)
        );
        // siblings preserved
        assert_eq!(
            doc["backoff"]["max_ms"]
                .as_value()
                .and_then(|v| v.as_integer()),
            Some(5000)
        );
        assert_eq!(
            doc["backoff"]["jitter"]
                .as_value()
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    // ── upsert_table ──

    #[test]
    fn upsert_table_creates_path_when_missing() {
        let mut doc = parse("");
        let mut create_called = false;
        let mut update_called = false;
        upsert_table(
            &mut doc,
            &["backoff"],
            || {
                create_called = true;
                let mut t = Table::new();
                t.insert("base_ms", toml_edit::value(100i64));
                Ok(Item::Table(t))
            },
            |_| {
                update_called = true;
                Ok(())
            },
        )
        .unwrap();
        assert!(create_called, "on_create should fire for missing path");
        assert!(!update_called, "on_update must not fire for missing path");
    }

    #[test]
    fn upsert_table_calls_update_when_path_exists() {
        let mut doc = parse(
            r#"
[backoff]
base_ms = 100
"#,
        );
        let mut create_called = false;
        let mut update_called = false;
        upsert_table(
            &mut doc,
            &["backoff"],
            || {
                create_called = true;
                unreachable!("on_create must not fire when path exists")
            },
            |item| {
                update_called = true;
                item["base_ms"] = toml_edit::value(200i64);
                Ok(())
            },
        )
        .unwrap();
        assert!(!create_called);
        assert!(update_called);
        assert_eq!(
            doc["backoff"]["base_ms"]
                .as_value()
                .and_then(|v| v.as_integer()),
            Some(200)
        );
    }

    #[test]
    fn upsert_table_preserves_untouched_fields_and_comments() {
        let mut doc = parse(
            r#"
# Top-level comment about backoff
[backoff]
# Inline comment
base_ms = 100
max_ms = 5000
jitter = true
extra_field = "untouched"
"#,
        );
        upsert_table(
            &mut doc,
            &["backoff"],
            || unreachable!(),
            |item| {
                item["base_ms"] = toml_edit::value(200i64);
                Ok(())
            },
        )
        .unwrap();

        let s = doc.to_string();
        // Comments survive
        assert!(s.contains("# Top-level comment about backoff"));
        assert!(s.contains("# Inline comment"));
        // Updated field
        assert!(s.contains("base_ms = 200"));
        // Siblings preserved (with values)
        assert!(s.contains("max_ms = 5000"));
        assert!(s.contains("jitter = true"));
        assert!(s.contains("extra_field = \"untouched\""));
    }

    #[test]
    fn upsert_table_propagates_on_create_error() {
        let mut doc = parse("");
        let result = upsert_table(
            &mut doc,
            &["bad"],
            || Err::<Item, _>(sag_core::AppError::Config("boom".into())),
            |_| unreachable!(),
        );
        assert!(result.is_err());
    }

    // ── sync_array_of_tables ──

    fn route(name: &str) -> (String, String) {
        (name.to_string(), name.to_string())
    }

    fn make_table(model_name: &str) -> Result<Table, AppError> {
        let mut t = Table::new();
        t.insert("model_name", toml_edit::value(model_name));
        Ok(t)
    }

    #[test]
    fn sync_array_of_tables_creates_section_when_missing() {
        let mut doc = parse("");
        let items = vec![route("gpt-4"), route("claude")];
        sync_array_of_tables(
            &mut doc,
            &["routes"],
            MODEL_NAME_KEY,
            &items,
            |(k, _)| k.as_str(),
            |(_, v)| make_table(v),
            |_, _| Ok(()),
        )
        .unwrap();
        let s = doc.to_string();
        assert!(s.contains("[[routes]]"));
        assert!(s.contains("model_name = \"gpt-4\""));
        assert!(s.contains("model_name = \"claude\""));
    }

    #[test]
    fn sync_array_of_tables_empty_items_removes_section() {
        let mut doc = parse(
            r#"
[[routes]]
model_name = "gpt-4"
"#,
        );
        let items: Vec<(String, String)> = vec![];
        sync_array_of_tables(
            &mut doc,
            &["routes"],
            MODEL_NAME_KEY,
            &items,
            |(k, _)| k.as_str(),
            |_| unreachable!(),
            |_, _| unreachable!(),
        )
        .unwrap();
        let s = doc.to_string();
        assert!(!s.contains("routes"));
        assert!(!s.contains("model_name"));
    }

    #[test]
    fn sync_array_of_tables_updates_existing_entry() {
        let mut doc = parse(
            r#"
[[routes]]
model_name = "gpt-4"
strategy = "fill_first"
"#,
        );
        let items = vec![route("gpt-4")];
        sync_array_of_tables(
            &mut doc,
            &["routes"],
            MODEL_NAME_KEY,
            &items,
            |(k, _)| k.as_str(),
            |_| unreachable!("on_create should not fire for existing entry"),
            |existing, (_, _)| {
                existing["strategy"] = toml_edit::value("round_robin");
                Ok(())
            },
        )
        .unwrap();
        let s = doc.to_string();
        assert!(s.contains("model_name = \"gpt-4\""));
        assert!(s.contains("strategy = \"round_robin\""));
        assert!(!s.contains("fill_first"));
    }

    #[test]
    fn sync_array_of_tables_inserts_new_entries() {
        let mut doc = parse(
            r#"
[[routes]]
model_name = "gpt-4"
"#,
        );
        let items = vec![route("gpt-4"), route("claude")];
        sync_array_of_tables(
            &mut doc,
            &["routes"],
            MODEL_NAME_KEY,
            &items,
            |(k, _)| k.as_str(),
            |(_, v)| make_table(v),
            |_, _| Ok(()),
        )
        .unwrap();
        let s = doc.to_string();
        assert!(s.contains("model_name = \"gpt-4\""));
        assert!(s.contains("model_name = \"claude\""));
    }

    #[test]
    fn sync_array_of_tables_removes_stale_entries() {
        let mut doc = parse(
            r#"
[[routes]]
model_name = "gpt-4"

[[routes]]
model_name = "claude"

[[routes]]
model_name = "gemini"
"#,
        );
        let items = vec![route("gpt-4")];
        sync_array_of_tables(
            &mut doc,
            &["routes"],
            MODEL_NAME_KEY,
            &items,
            |(k, _)| k.as_str(),
            |_| unreachable!(),
            |_, _| Ok(()),
        )
        .unwrap();
        let s = doc.to_string();
        assert!(s.contains("model_name = \"gpt-4\""));
        assert!(!s.contains("\"claude\""));
        assert!(!s.contains("\"gemini\""));
    }

    #[test]
    fn sync_array_of_tables_preserves_comments_in_existing_entries() {
        let mut doc = parse(
            r#"
# Section header comment
[[routes]]
# Inline comment 1
model_name = "gpt-4"
# Inline comment 2
strategy = "fill_first"
"#,
        );
        let items = vec![route("gpt-4")];
        sync_array_of_tables(
            &mut doc,
            &["routes"],
            MODEL_NAME_KEY,
            &items,
            |(k, _)| k.as_str(),
            |_| unreachable!(),
            |existing, (_, _)| {
                existing["strategy"] = toml_edit::value("round_robin");
                Ok(())
            },
        )
        .unwrap();
        let s = doc.to_string();
        assert!(s.contains("# Section header comment"));
        assert!(s.contains("# Inline comment 1"));
        assert!(s.contains("# Inline comment 2"));
        assert!(s.contains("strategy = \"round_robin\""));
    }

    #[test]
    fn sync_array_of_tables_propagates_on_create_error() {
        let mut doc = parse("");
        let items = vec![route("gpt-4")];
        let result = sync_array_of_tables(
            &mut doc,
            &["routes"],
            MODEL_NAME_KEY,
            &items,
            |(k, _)| k.as_str(),
            |_| Err::<Table, _>(sag_core::AppError::Config("boom".into())),
            |_, _| unreachable!(),
        );
        assert!(result.is_err());
    }
    #[test]
    fn sync_array_of_tables_scales_to_1000_entries() {
        // Parallel stress test to
        // `sync_inline_table_array_scales_to_1000_entries` — exercises the
        // O(n+m) refactor on the ArrayOfTables variant which has
        // additional setup (parent-navigation, array-of-tables
        // initialisation).
        const N: usize = 1000;
        let sources: Vec<String> = (0..N).map(|i| format!("model-{i}")).collect();
        let items: Vec<(String, String)> = sources.iter().map(|s| (s.clone(), s.clone())).collect();

        // Build the entire ArrayOfTables FIRST (REVERSE order — exercises
        // the "update in place, preserve position" path for every entry),
        // THEN insert it into the document once. Building it inside the
        // loop and inserting per-iteration would `insert()`-overwrite
        // prior values and silently leave only one entry — an easy
        // mistake that would make the stress test degenerate to N=1.
        let mut aot = ArrayOfTables::new();
        for s in sources.iter().rev() {
            let mut t = Table::new();
            t.insert("model_name", toml_edit::value(s.as_str()));
            t.insert("strategy", toml_edit::value("fill_first"));
            aot.push(t);
        }
        assert_eq!(aot.len(), N, "fixture should have N entries");
        let mut doc = parse("");
        doc.as_item_mut()
            .as_table_mut()
            .unwrap()
            .insert("routes", Item::ArrayOfTables(aot));

        sync_array_of_tables(
            &mut doc,
            &["routes"],
            MODEL_NAME_KEY,
            &items,
            |(k, _)| k.as_str(),
            |(_, v)| make_table(v),
            |existing, (_, _)| {
                existing.insert("strategy", toml_edit::value("round_robin"));
                Ok(())
            },
        )
        .unwrap();

        let s = doc.to_string();
        assert!(
            s.find("model-999").unwrap() < s.find("model-0").unwrap(),
            "reverse order should be preserved"
        );
        assert!(s.contains("strategy = \"round_robin\""));
        assert!(!s.contains("fill_first"));
    }

    // ── sync_inline_table_array_in_place ──

    fn empty_inline_array() -> Array {
        Array::new()
    }

    fn target(provider: &str) -> (String, String) {
        (provider.to_string(), provider.to_string())
    }

    fn build_target_table((_, v): &(String, String)) -> Result<InlineTable, AppError> {
        let mut t = InlineTable::new();
        t.insert("provider", Value::from(v.as_str()));
        Ok(t)
    }

    #[test]
    fn sync_inline_table_array_creates_from_empty() {
        let mut arr = empty_inline_array();
        let items = vec![target("openai"), target("anthropic")];
        sync_inline_table_array_in_place(
            &mut arr,
            &items,
            &["provider"],
            |(k, _)| vec![Some(k.clone())],
            build_target_table,
            |_, _| Ok(()),
        )
        .unwrap();
        let s = arr.to_string();
        assert!(s.contains("provider = \"openai\""));
        assert!(s.contains("provider = \"anthropic\""));
    }

    #[test]
    fn sync_inline_table_array_updates_existing_entry() {
        let mut doc = parse(
            r#"
targets = [
    { provider = "openai", priority = 5 },
]
"#,
        );
        let arr = doc["targets"].as_array_mut().unwrap();
        let items: Vec<(String, i64)> = vec![("openai".into(), 10)];
        sync_inline_table_array_in_place(
            arr,
            &items,
            &["provider"],
            |(k, _)| vec![Some(k.clone())],
            |_| unreachable!("on_create should not fire for existing entry"),
            |existing, (_, priority)| {
                existing.insert("priority", Value::from(*priority));
                Ok(())
            },
        )
        .unwrap();
        let s = doc.to_string();
        assert!(s.contains("priority = 10"));
        assert!(!s.contains("priority = 5"));
    }

    #[test]
    fn sync_inline_table_array_inserts_new_entries() {
        let mut doc = parse(
            r#"
targets = [
    { provider = "openai" },
]
"#,
        );
        let arr = doc["targets"].as_array_mut().unwrap();
        let items = vec![target("openai"), target("anthropic")];
        sync_inline_table_array_in_place(
            arr,
            &items,
            &["provider"],
            |(k, _)| vec![Some(k.clone())],
            build_target_table,
            |existing, (provider, _)| {
                existing.insert("provider", Value::from(provider.as_str()));
                Ok(())
            },
        )
        .unwrap();
        let s = doc.to_string();
        assert!(s.contains("provider = \"openai\""));
        assert!(s.contains("provider = \"anthropic\""));
    }

    #[test]
    fn sync_inline_table_array_removes_stale_entries() {
        let mut doc = parse(
            r#"
targets = [
    { provider = "openai" },
    { provider = "anthropic" },
    { provider = "gemini" },
]
"#,
        );
        let arr = doc["targets"].as_array_mut().unwrap();
        let items = vec![target("openai")];
        sync_inline_table_array_in_place(
            arr,
            &items,
            &["provider"],
            |(k, _)| vec![Some(k.clone())],
            |_| unreachable!(),
            |_, _| Ok(()),
        )
        .unwrap();
        let s = doc.to_string();
        assert!(s.contains("provider = \"openai\""));
        assert!(!s.contains("anthropic"));
        assert!(!s.contains("gemini"));
    }

    #[test]
    fn sync_inline_table_array_preserves_comments_in_existing_entries() {
        let mut doc = parse(
            r#"
targets = [
    # Comment for openai target
    { provider = "openai", priority = 5 },
]
"#,
        );
        let arr = doc["targets"].as_array_mut().unwrap();
        let items: Vec<(String, i64)> = vec![("openai".into(), 10)];
        sync_inline_table_array_in_place(
            arr,
            &items,
            &["provider"],
            |(k, _)| vec![Some(k.clone())],
            |_| unreachable!(),
            |existing, (_, priority)| {
                existing.insert("priority", Value::from(*priority));
                Ok(())
            },
        )
        .unwrap();
        let s = doc.to_string();
        assert!(s.contains("# Comment for openai target"));
        assert!(s.contains("priority = 10"));
    }

    #[test]
    fn sync_inline_table_array_preserves_untouched_fields() {
        let mut doc = parse(
            r#"
targets = [
    { provider = "openai", priority = 5, temperature = 0.7 },
]
"#,
        );
        let arr = doc["targets"].as_array_mut().unwrap();
        let items: Vec<(String, i64)> = vec![("openai".into(), 10)];
        sync_inline_table_array_in_place(
            arr,
            &items,
            &["provider"],
            |(k, _)| vec![Some(k.clone())],
            |_| unreachable!(),
            |existing, (_, priority)| {
                // Only update priority; temperature and other fields should
                // be preserved.
                existing.insert("priority", Value::from(*priority));
                Ok(())
            },
        )
        .unwrap();
        let s = doc.to_string();
        assert!(s.contains("priority = 10"));
        assert!(s.contains("temperature = 0.7"));
    }

    #[test]
    fn sync_inline_table_array_defensively_removes_non_inline_table_items() {
        // If the array somehow contains a non-inline-table item, it should
        // be removed. This shouldn't happen in practice because we
        // initialize the array ourselves, but it's a defensive check.
        let mut arr = Array::new();
        arr.push(Value::from(42i64)); // not an inline table
        arr.push(Value::InlineTable({
            let mut t = InlineTable::new();
            t.insert("provider", Value::from("openai"));
            t
        }));
        let items = vec![target("openai")];
        sync_inline_table_array_in_place(
            &mut arr,
            &items,
            &["provider"],
            |(k, _)| vec![Some(k.clone())],
            build_target_table,
            |_, _| Ok(()),
        )
        .unwrap();
        let s = arr.to_string();
        assert!(!s.contains("42"), "non-inline-table item should be removed");
        assert!(s.contains("openai"));
    }

    #[test]
    #[should_panic(expected = "key_fields must be non-empty")]
    fn sync_inline_table_array_panics_on_empty_key_fields() {
        let mut arr = empty_inline_array();
        // Explicit type on the empty slice so the generic parameter `T` can
        // be inferred — the closures are never called (the function panics
        // before reaching them) but Rust still needs to resolve them.
        let items: Vec<(String, String)> = vec![];
        // `let _ =` silences `unused_must_use`: the function panics on the
        // empty `key_fields` assert before any Result is produced.
        let _ = sync_inline_table_array_in_place(
            &mut arr,
            &items,
            &[],
            |_| unreachable!(),
            |_| unreachable!(),
            |_, _| Ok(()),
        );
    }

    #[test]
    fn sync_inline_table_array_scales_to_1000_entries() {
        // Stress test for the O(n+m) refactor: sync a large inline-table
        // array against a similarly large in-memory slice. The previous
        // O(n*m) implementation would have been visibly slow on this
        // fixture (linear scan per item); the position-by-key map makes
        // the update loop O(n).
        const N: usize = 1000;
        let sources: Vec<String> = (0..N).map(|i| format!("src-{i}")).collect();
        let items: Vec<(String, String)> = sources.iter().map(|s| (s.clone(), s.clone())).collect();

        // Build a starting array with the same keys but in REVERSE order
        // — exercises the "update in place, preserve position" path for
        // every entry.
        let mut arr = Array::new();
        for s in sources.iter().rev() {
            let mut t = InlineTable::new();
            t.insert("provider", Value::from(s.as_str()));
            t.insert("priority", Value::from(0i64));
            arr.push(Value::InlineTable(t));
        }
        assert_eq!(arr.len(), N, "fixture should have N entries");

        sync_inline_table_array_in_place(
            &mut arr,
            &items,
            &["provider"],
            |(k, _)| vec![Some(k.clone())],
            |_| unreachable!(),
            |existing, (_, _)| {
                existing.insert("priority", Value::from(7i64));
                Ok(())
            },
        )
        .unwrap();

        // After sync, every entry should still be present, in REVERSE
        // order (preserved by the position map), with priority updated
        // to 7.
        let s = arr.to_string();
        assert!(
            s.find("src-999").unwrap() < s.find("src-0").unwrap(),
            "reverse order should be preserved"
        );
        assert!(s.contains("priority = 7"), "all entries should be updated");
        assert!(
            !s.contains("priority = 0"),
            "no entry should keep old value"
        );
        assert_eq!(arr.len(), N, "no spurious entries");
    }

    #[test]
    fn sync_inline_table_array_matches_composite_key() {
        // Regression for Problem 1: with single-field keying on
        // `provider`, two on-disk entries sharing the same provider but
        // with different real_models would be matched to the SAME
        // position — the second `on_update` would silently overwrite
        // the first's `priority`/`extra`. With composite-key matching
        // `(provider, real_model)`, each distinct real_model maps to a
        // distinct position, and round-trip preserves both targets'
        // per-entry state.
        //
        // Fixture: two on-disk entries with same provider, distinct
        // real_models (gpt-4 vs gpt-3.5), each with its own priority.
        // In-memory items match in REVERSE order (gpt-3.5 first,
        // gpt-4 second) — exercises "update at any matched position".
        let mut doc = parse(
            r#"
targets = [
    { provider = "openai", real_model = "gpt-4", priority = 10 },
    { provider = "openai", real_model = "gpt-3.5-turbo", priority = 5 },
]
"#,
        );
        let arr = doc["targets"].as_array_mut().unwrap();
        let items: Vec<(String, String, i64)> = vec![
            // In-memory order: gpt-3.5-turbo first.
            ("openai".into(), "gpt-3.5-turbo".into(), 99),
            ("openai".into(), "gpt-4".into(), 1),
        ];
        sync_inline_table_array_in_place(
            arr,
            &items,
            &["provider", "real_model"],
            |(provider, real_model, _)| vec![Some(provider.clone()), Some(real_model.clone())],
            |_| unreachable!("on_create should not fire — all keys exist on disk"),
            |existing, (_, _, priority)| {
                existing.insert("priority", Value::from(*priority));
                Ok(())
            },
        )
        .unwrap();
        // Drop the mutable borrow on `doc` before re-borrowing it for
        // `doc.to_string()`. Use the array length now (last use of `arr`)
        // so the borrow ends cleanly here.
        assert_eq!(
            arr.len(),
            2,
            "both targets must survive roundtrip — composite key distinguishes them"
        );
        let s = doc.to_string();
        // gpt-4 entry got priority=1 (was 10).
        assert!(
            s.contains("real_model = \"gpt-4\"") && s.contains("priority = 1"),
            "gpt-4 priority should have been updated to 1, got: {s}"
        );
        // gpt-3.5-turbo entry got priority=99 (was 5).
        assert!(
            s.contains("real_model = \"gpt-3.5-turbo\"") && s.contains("priority = 99"),
            "gpt-3.5 priority should have been updated to 99, got: {s}"
        );
    }

    #[test]
    fn sync_inline_table_array_distinguishes_present_vs_absent_optional_key() {
        // Two targets sharing provider but with one having
        // `real_model = Some("gpt-4")` and the other having
        // `real_model = None` should be distinguished by the composite
        // key (Option::None vs Some("gpt-4")). If `key_fn` collapses
        // both to the same string, the second update would overwrite
        // the first; the test pins the structural distinction.
        let mut doc = parse(
            r#"
targets = [
    { provider = "openai", real_model = "gpt-4", priority = 10 },
    { provider = "openai", priority = 5 },
]
"#,
        );
        let arr = doc["targets"].as_array_mut().unwrap();
        // In-memory items: one with real_model=None, one with real_model=gpt-4.
        // The order is reversed to exercise "update at any position".
        let items: Vec<(String, Option<String>, i64)> = vec![
            ("openai".into(), None, 77),
            ("openai".into(), Some("gpt-4".into()), 22),
        ];
        sync_inline_table_array_in_place(
            arr,
            &items,
            &["provider", "real_model"],
            |(provider, real_model, _)| vec![Some(provider.clone()), real_model.clone()],
            |_| unreachable!("on_create should not fire — both keys distinguishable on disk"),
            |existing, (_, _, priority)| {
                existing.insert("priority", Value::from(*priority));
                Ok(())
            },
        )
        .unwrap();
        // Drop the mutable borrow on `doc` from `arr` before re-borrowing
        // it for `doc.to_string()`. Use the array length now (last use)
        // so the borrow ends cleanly here.
        assert_eq!(
            arr.len(),
            2,
            "both targets must survive — None vs Some distinguished"
        );
        let s = doc.to_string();
        // The real_model=gpt-4 entry got priority=22.
        assert!(
            s.contains("real_model = \"gpt-4\"") && s.contains("priority = 22"),
            "gpt-4 entry priority should be 22, got: {s}"
        );
        // The real_model-absent entry got priority=77 (no real_model
        // emitted, since skip_serializing_if for None real_model).
        assert!(
            s.contains("priority = 77"),
            "real_model=None entry priority should be 77, got: {s}"
        );
    }

    // ── sync_inline_table_array_in_place formatting (80-char threshold) ──

    #[test]
    fn inline_table_array_stays_single_line_when_short() {
        // Two minimal targets — single-line rendering well under 80 chars.
        // (Only `provider` field; adding `real_model` would push it past
        // the threshold.)
        let mut arr = Array::new();
        for provider in ["openai", "anthropic"] {
            let mut t = InlineTable::new();
            t.insert("provider", Value::from(provider));
            arr.push(Value::InlineTable(t));
        }
        let items: Vec<(String, String)> = vec![
            ("openai".into(), "openai".into()),
            ("anthropic".into(), "anthropic".into()),
        ];
        sync_inline_table_array_in_place(
            &mut arr,
            &items,
            &["provider"],
            |(k, _)| vec![Some(k.clone())],
            |_| unreachable!(),
            |_, _| Ok(()),
        )
        .unwrap();
        let s = arr.to_string();
        assert!(
            !s.contains('\n'),
            "short inline-table array should stay single-line, got: {s}"
        );
        assert!(s.starts_with('[') && s.ends_with(']'));
    }

    #[test]
    fn inline_table_array_expands_to_multiline_when_long() {
        // Four targets with long provider names — single-line exceeds 80 chars.
        let mut arr = Array::new();
        let providers = [
            "openai-with-suffix-x",
            "anthropic-with-suffix-y",
            "google-gemini-pro-v2",
            "meta-llama-4-maverick",
        ];
        for p in &providers {
            let mut t = InlineTable::new();
            t.insert("provider", Value::from(*p));
            arr.push(Value::InlineTable(t));
        }
        let items: Vec<(String, String)> = providers
            .iter()
            .map(|p| (p.to_string(), p.to_string()))
            .collect();
        sync_inline_table_array_in_place(
            &mut arr,
            &items,
            &["provider"],
            |(k, _)| vec![Some(k.clone())],
            |_| unreachable!(),
            |_, _| Ok(()),
        )
        .unwrap();
        let s = arr.to_string();
        assert!(
            s.contains('\n'),
            "long inline-table array should be multi-line, got: {s}"
        );
        assert!(s.contains("\n    {"), "should have indented items");
    }

    #[test]
    fn inline_table_array_has_trailing_comma_when_multiline() {
        // Array long enough to trigger multi-line (exceeds 80 chars).
        let mut arr = Array::new();
        for (i, p) in ["prov-x-a", "prov-y-b", "prov-z-c"].iter().enumerate() {
            let mut t = InlineTable::new();
            t.insert("provider", Value::from(*p));
            t.insert("priority", Value::from(i as i64));
            arr.push(Value::InlineTable(t));
        }
        let items: Vec<(String, i64)> = vec![
            ("prov-x-a".into(), 0),
            ("prov-y-b".into(), 1),
            ("prov-z-c".into(), 2),
        ];
        sync_inline_table_array_in_place(
            &mut arr,
            &items,
            &["provider"],
            |(k, _)| vec![Some(k.clone())],
            |_| unreachable!(),
            |existing, (_, priority)| {
                existing.insert("priority", Value::from(*priority));
                Ok(())
            },
        )
        .unwrap();
        let s = arr.to_string();
        assert!(s.contains('\n'), "should be multi-line");
        // Trailing comma: last item should have `},` before the closing
        // newline + bracket.
        assert!(
            s.contains("},\n]"),
            "should have trailing comma before closing bracket, got: {s}"
        );
    }

    #[test]
    fn inline_table_array_already_multiline_keeps_format() {
        // Already multi-line array (from a previous save) should stay
        // multi-line even if its content is short.
        let mut doc = parse(
            r#"
targets = [
    { provider = "openai" },
]
"#,
        );
        let arr = doc["targets"].as_array_mut().unwrap();
        let items: Vec<(String, String)> = vec![("openai".into(), "openai".into())];
        sync_inline_table_array_in_place(
            arr,
            &items,
            &["provider"],
            |(k, _)| vec![Some(k.clone())],
            |_| unreachable!(),
            |_, _| Ok(()),
        )
        .unwrap();
        let s = doc.to_string();
        assert!(
            s.contains('\n'),
            "already multi-line should stay multi-line, got: {s}"
        );
        // Trailing comma should be added even to already-multi-line arrays.
        assert!(s.contains("},\n]"), "should have trailing comma, got: {s}");
    }

    // ── sync_string_array_in_place formatting (80-char threshold) ──

    #[test]
    fn string_array_stays_single_line_when_short() {
        let mut arr = Array::new();
        arr.push("key-a");
        arr.push("key-b");
        sync_string_array_in_place(&mut arr, &["key-a".into(), "key-b".into()]);
        let s = arr.to_string();
        assert!(
            !s.contains('\n'),
            "short string array should stay single-line, got: {s}"
        );
        assert!(s.starts_with('[') && s.ends_with(']'));
    }

    #[test]
    fn string_array_expands_to_multiline_when_long() {
        let mut arr = Array::new();
        // 6 long keys to exceed 80 chars single-line.
        let keys: Vec<String> = (0..6)
            .map(|i| format!("sk-very-long-key-name-{:02}", i))
            .collect();
        for k in &keys {
            arr.push(k.as_str());
        }
        sync_string_array_in_place(&mut arr, &keys);
        let s = arr.to_string();
        assert!(
            s.contains('\n'),
            "long string array should be multi-line, got: {s}"
        );
        assert!(s.contains("\n    \""), "should have indented items");
    }

    #[test]
    fn string_array_has_trailing_comma_when_multiline() {
        let mut arr = Array::new();
        let keys: Vec<String> = (0..5).map(|i| format!("sk-long-key-{:02}", i)).collect();
        for k in &keys {
            arr.push(k.as_str());
        }
        sync_string_array_in_place(&mut arr, &keys);
        let s = arr.to_string();
        assert!(s.contains('\n'), "should be multi-line");
        // Trailing comma: last item `,\n]`.
        assert!(
            s.contains("\",\n]"),
            "should have trailing comma before closing bracket, got: {s}"
        );
    }

    #[test]
    fn string_array_already_multiline_keeps_format() {
        // Parsed multi-line string array — should stay multi-line.
        let mut doc = parse(
            r#"
api_keys = [
    "sk-a",
    "sk-b",
]
"#,
        );
        let arr = doc["api_keys"].as_array_mut().unwrap();
        sync_string_array_in_place(arr, &["sk-a".into(), "sk-b".into()]);
        let s = doc.to_string();
        assert!(
            s.contains('\n'),
            "already multi-line should stay multi-line, got: {s}"
        );
        assert!(s.contains("\",\n]"), "should have trailing comma, got: {s}");
    }
}
