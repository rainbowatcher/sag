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
//! # When `on_create` can fail
//!
//! `on_create` returns `Result<Item, AppError>` because the most common case
//! (serializing a `serde::Serialize` value via `toml_edit::ser::to_document`)
//! can fail. `on_update` returns `()` because it does in-place mutation
//! (typically a series of `item["key"] = value;` assignments) which cannot
//! fail in practice.

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
    G: FnOnce(&mut Item),
{
    match navigate_mut(doc.as_item_mut(), path) {
        Some(existing) => {
            on_update(existing);
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
    FU: Fn(&mut Table, &T),
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
                on_update(existing, item);
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
///     |t| build_new_target(t),
///     |existing, t| write_target_fields(existing, t),
/// );
/// ```
pub fn sync_inline_table_array_in_place<T, K, FC, FU>(
    array: &mut Array,
    items: &[T],
    key_fields: &[&str],
    key_fn: K,
    on_create: FC,
    on_update: FU,
) where
    K: Fn(&T) -> Vec<Option<String>>,
    FC: Fn(&T) -> InlineTable,
    FU: Fn(&mut InlineTable, &T),
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
                    on_update(existing, item);
                }
                // If the indexed entry is somehow no longer an inline
                // table, silently skip the update — the retain pass
                // below will drop it as a defensive measure.
            }
            None => {
                let new_table = on_create(item);
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

/// Reformat a single-line `key = [ ... ]` array-of-inline-tables inside
/// a rendered TOML `String` into a multi-line layout.
///
/// This is a string-level post-process, applied after
/// `toml_edit`'s `DocumentMut::to_string()` to the buffer that will be
/// written to disk. We scan the bytes looking for the literal `key = [`
/// pattern whose closing `]` is on the same line; for each match we
/// split the payload on top-level `}, {` boundaries (tracked with a
/// brace-depth counter that ignores characters inside string literals)
/// and rejoin with `\n{indent}` between every item plus a final `\n]`
/// before the closing bracket.
///
/// # Why this lives here and not in `toml_edit`'s formatter
///
/// In the `toml_edit` version pinned by this project, the
/// array-of-inline-table formatter emits the array on one line
/// when it was originally single-line (e.g.
/// `targets = [{...}, {...}]`). `toml_edit` **does** preserve
/// per-item prefix decor (comments) and any existing multi-line
/// formatting — already-multi-line arrays round-trip correctly.
/// The only stable way to convert single-line arrays to multi-line
/// is to post-process the rendered `String`.
///
/// # Inputs we leave alone
///
/// - **Already multi-line** — if the input has a `\n` between `[` and
///   the matching `]`, the array is left untouched.
/// - **Empty payloads** — `key = []` is returned unchanged; reshaping
///   it would change `[]` into `[\n]` for no benefit.
/// - **No match** — if the literal `key` isn't found (or its preceding
///   byte is an identifier character, so it's a partial-key match like
///   `something_targets = [...]`), the input is returned unchanged.
/// - **Literal on a `#` line comment** — a match whose containing
///   line begins (after spaces/tabs) with `#` is plain text in a
///   comment, not a real array. The partial-key check leaves this gap
///   because `#` isn't an identifier character; without this guard
///   the helper would treat `# example: targets = [{ a = 1 }]` as a
///   real array and reshape it, damaging the comment text.
///   *Known limitation*: a trailing on-line comment — e.g.
///   `key = "x"  # example: targets = [...]` — is still at risk
///   because line-start isn't `#`; the brace-and-quote walker doesn't
///   track whether we've crossed into such a comment after `[`. Not
///   seen in real configs but documented so future callers consider
///   whether the scope needs to widen.
/// - **Nested braces inside strings** — double-quote tracking across
///   `\` escapes ensures that string contents (e.g.
///   `foo = "a{b}c"`) don't perturb the brace counter.
///
/// # Known unsupported scenarios
///
/// This function is a pragmatic workaround for a specific `toml_edit`
/// formatting gap, not a general-purpose TOML reformatter. The
/// following patterns are **not** handled and may produce incorrect
/// output or silently corrupt the document:
///
/// - **Nested array-of-inline-tables**: if an inline-table item
///   contains a nested `array_of_nested = [{ ... }, { ... }]` (e.g.
///   `extra` that itself contains a targets-style array), the
///   brace-depth tracker sees the nested `}` as the end of the top-level
///   item, causing premature item splitting. Today's call sites only
///   pass `targets` arrays whose items contain scalar fields and
///   `extra = { ... }` (an inline table, not an array-of-inline-tables),
///   so this is not exercised but is structurally fragile.
///
/// - **`toml_edit` version upgrades**: if a future `toml_edit` release
///   changes the default rendering of `Array`→`String` (e.g. flips to
///   multi-line by default or introduces a formatter API), this
///   post-process may double-format (adding duplicate `\n    ` prefixes
///   on already-multi-line output) or silently become a no-op while the
///   output format changes. The multi-line detection guards against
///   double-formatting but cannot predict format changes.
///
/// - **Trailing on-line comments after `[`**: as noted above in "Inputs
///   we leave alone", a comment like `targets = [{ a = 1 }]  # note`
///   is not guarded against — the comment text after `]` is preserved
///   verbatim but the inside-payload reshaping still runs, which is
///   safe as long as the comment doesn't contain brace-like patterns.
///
/// - **Non-ASCII or multi-byte codepoints adjacent to braces**: the
///   brace walker operates on byte indices and only tracks `{`, `}`,
///   `[`, `]`, `"`, `\\`, and `#`. Non-ASCII brace lookalikes (e.g.
///   CJK quotation marks, fullwidth brackets) are not handled — they
///   will either be misinterpreted or silently ignored. Real-world TOML
///   configs use ASCII-only keys and values, so this is not a practical
///   concern but is a correctness boundary.
///
/// # Behaviour on reshaped output
///
/// ```text
/// // before
/// key = [{ a = 1 }, { b = 2 }]
/// // after, with indent = "    "
/// key = [
///     { a = 1 },
///     { b = 2 },
/// ]
/// ```
pub fn reformat_inline_table_arrays_multiline(text: &mut String, key: &str, indent: &str) {
    let needle = format!("{key} = [");
    let mut search_start = 0;
    while let Some(rel) = text[search_start..].find(&needle) {
        let abs = search_start + rel;
        let after_eq = abs + needle.len();
        let bytes = text.as_bytes();

        // Reject partial-key matches: 'something_targets = [' must NOT
        // reformat. We only proceed if the byte before `abs` is the
        // start of a line, whitespace, or non-identifier punctuation.
        if abs > 0 {
            let prev = bytes[abs - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                search_start = after_eq;
                continue;
            }
        }

        // Reject occurrences that fall inside a `#` line comment. The
        // partial-key check above only blocks identifier characters
        // (alnum + `_`) immediately before `abs`; `#` and arbitrary
        // whitespace pass through, so an input like
        // `# example: targets = [{ a = 1 }]`
        // would otherwise be reshaped. Scan backward to find the start
        // of the line containing `abs`, then walk forward through
        // spaces/tabs — if the first non-whitespace byte on the line is
        // `#`, this match is inside a comment and must be skipped. The
        // check is per-line: `# leading\ntargets = [...]` is OK because
        // the second line's `t` is not `#`.
        if abs > 0 {
            let line_start = bytes[..abs]
                .iter()
                .rposition(|&b| b == b'\n')
                .map(|p| p + 1)
                .unwrap_or(0);
            let mut j = line_start;
            while j < abs && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j < abs && bytes[j] == b'#' {
                search_start = after_eq;
                continue;
            }
        }

        // Walk to the closing ']' while tracking nested bracket depth
        // and string state. Not full tokenizer correctness — just
        // enough to step past matched brackets and not be fooled by
        // brace characters inside string literals or end-of-line
        // comments.
        let mut bracket_depth: i32 = 1;
        let mut in_string = false;
        let mut escape = false;
        let mut close_pos: Option<usize> = None;
        let mut i = after_eq;
        while i < bytes.len() {
            let c = bytes[i];
            if in_string {
                if escape {
                    escape = false;
                } else if c == b'\\' {
                    escape = true;
                } else if c == b'"' {
                    in_string = false;
                }
                i += 1;
                continue;
            }
            match c {
                b'"' => in_string = true,
                b'#' => {
                    // Comment runs to end of line; safe to skip.
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                b'[' => bracket_depth += 1,
                b']' => {
                    bracket_depth -= 1;
                    if bracket_depth == 0 {
                        close_pos = Some(i);
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        let Some(end_excl) = close_pos else {
            search_start = abs + 1;
            continue;
        };

        // Already multi-line? leave alone.
        if text[after_eq..end_excl].contains('\n') {
            search_start = end_excl + 1;
            continue;
        };

        // Empty payload? leave alone.
        if end_excl == after_eq {
            search_start = end_excl + 1;
            continue;
        }

        // Split the payload on top-level '{'…'}' boundaries, tracking
        // each item's EXACT byte range as `[open_start, close_end)`.
        // Storing exact ranges avoids the previous reconstruction
        // bug: the old code ripped a fixed `, ` (TWO bytes) out of
        // the slice, which silently deleted the prior item's closing
        // `}` when the in-place separator was just `,` (ONE byte) —
        // e.g. `[{a=1},{b=2},{c=3}]` lost two `}` braces from the
        // rebuilt output, and `[{a=1},  {b=2}]` (extra spaces) also
        // went wrong. With exact item ranges the separator is dropped
        // entirely (we re-emit `,\n{indent}` ourselves), so any
        // separator width round-trips correctly.
        let inside_bytes = &bytes[after_eq..end_excl];
        let mut bd: i32 = 0;
        let mut in_string = false;
        let mut escape = false;
        let mut open_pos: Option<usize> = None;
        let mut items: Vec<(usize, usize)> = Vec::new();
        let mut k = 0;
        while k < inside_bytes.len() {
            let c = inside_bytes[k];
            if in_string {
                if escape {
                    escape = false;
                } else if c == b'\\' {
                    escape = true;
                } else if c == b'"' {
                    in_string = false;
                }
                k += 1;
                continue;
            }
            match c {
                b'"' => in_string = true,
                b'{' => {
                    if bd == 0 && open_pos.is_none() {
                        open_pos = Some(k);
                    }
                    bd += 1;
                }
                b'}' => {
                    bd -= 1;
                    if bd == 0
                        && let Some(start) = open_pos.take()
                    {
                        items.push((start, k + 1));
                        // Skip the inter-item separator if one
                        // follows: optional spaces/tabs, optional
                        // comma, optional spaces/tabs. The outer
                        // `k += 1` lands on the byte AFTER the
                        // separator (i.e. the start of the next item)
                        // when one exists; if no separator is
                        // present, `k` simply advances past `}` and
                        // the loop reaches its natural end.
                        let mut j = k + 1;
                        while j < inside_bytes.len()
                            && (inside_bytes[j] == b' ' || inside_bytes[j] == b'\t')
                        {
                            j += 1;
                        }
                        if j < inside_bytes.len() && inside_bytes[j] == b',' {
                            j += 1;
                            while j < inside_bytes.len()
                                && (inside_bytes[j] == b' ' || inside_bytes[j] == b'\t')
                            {
                                j += 1;
                            }
                            // Next item starts at `j`, or end of
                            // payload. Advance `k` so outer loop
                            // doesn't reprocess.
                            if j < inside_bytes.len() {
                                k = j - 1;
                            }
                        }
                    }
                }
                _ => {}
            }
            k += 1;
        }

        // Reassemble using exact item ranges. Each item body is
        // preserved verbatim; the original separator between items
        // is dropped in favour of our canonical `,\n{indent}` (so
        // `,` and `, ` and `,  ` all produce the same output).
        // Last item never has a trailing comma.
        let inside_str = &text[after_eq..end_excl];
        let mut rebuilt = String::with_capacity(inside_str.len() * 2);
        let last_idx = items.len().checked_sub(1);
        for (i, &(start, end)) in items.iter().enumerate() {
            rebuilt.push('\n');
            rebuilt.push_str(indent);
            rebuilt.push_str(&inside_str[start..end]);
            if Some(i) != last_idx {
                rebuilt.push(',');
            }
        }

        // Replace the original `key = [ ... ]` slice in place.
        let replacement = format!("{key} = [{rebuilt}\n]");
        text.replace_range(abs..=end_excl, &replacement);
        search_start = abs + replacement.len();
    }
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
}

/// Sync the keys of an existing inline table against an in-memory map,
/// preserving the on-disk key order, removing stale keys, and appending
/// new keys in sorted order. The value-conversion closure lets this
/// function work with any value type (e.g. `serde_json::Value`,
/// `String`).
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
pub fn sync_inline_table_in_place<V, F>(
    existing: &mut InlineTable,
    in_memory: &std::collections::HashMap<String, V>,
    to_toml_value: F,
) where
    F: Fn(&V) -> Value,
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
            existing.insert(key, to_toml_value(value));
        }
    }

    // Step 4: append new keys (sorted for determinism).
    let mut new_keys: Vec<&String> = in_memory
        .keys()
        .filter(|k| !existing.contains_key(k))
        .collect();
    new_keys.sort();
    for key in new_keys {
        existing.insert(key, to_toml_value(&in_memory[key]));
    }
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
            |_, _| {},
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
            |_, _| {},
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
            |_, _| {},
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

    fn build_target_table((_, v): &(String, String)) -> InlineTable {
        let mut t = InlineTable::new();
        t.insert("provider", Value::from(v.as_str()));
        t
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
            |_, _| {},
        );
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
            },
        );
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
            },
        );
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
            |_, _| {},
        );
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
            },
        );
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
            },
        );
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
            |_, _| {},
        );
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
        sync_inline_table_array_in_place(
            &mut arr,
            &items,
            &[],
            |_| unreachable!(),
            |_| unreachable!(),
            |_, _| {},
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
            },
        );

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
            },
        );
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
            },
        );
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

    // ── reformat_inline_table_arrays_multiline ──

    #[test]
    fn reformat_inline_table_arrays_multiline_converts_single_line() {
        let mut s =
            String::from(r#"targets = [{ provider = "openai" }, { provider = "anthropic" }]"#);
        reformat_inline_table_arrays_multiline(&mut s, "targets", "    ");
        // First item must end with inter-item comma + indent for the next
        // item; last item must NOT have a trailing comma — the closing
        // bracket sits alone on its own line.
        assert!(
            s.contains("\n    { provider = \"openai\" },\n"),
            "first item should end with comma + newline: {s}",
        );
        assert!(
            s.contains("\n    { provider = \"anthropic\" }\n]"),
            "last item should have NO trailing comma, bracket on its own line: {s}",
        );
        // Original substrings must survive verbatim.
        assert!(s.contains("provider = \"openai\""));
        assert!(s.contains("provider = \"anthropic\""));
        // Output must end with `]` (no appended suffix from the slice).
        assert!(
            s.ends_with(']'),
            "should end with closing bracket, got tail: {:?}",
            &s[s.len().saturating_sub(20)..],
        );
    }

    #[test]
    fn reformat_inline_table_arrays_multiline_leaves_multi_line_alone() {
        let original = String::from(
            r#"targets = [
    { provider = "openai" },
    { provider = "anthropic" },
]"#,
        );
        let mut s = original.clone();
        reformat_inline_table_arrays_multiline(&mut s, "targets", "    ");
        assert_eq!(s, original, "already multi-line should be untouched: {s}");
    }

    #[test]
    fn reformat_inline_table_arrays_multiline_keeps_empty_array_compact() {
        let mut s = String::from("targets = []");
        reformat_inline_table_arrays_multiline(&mut s, "targets", "    ");
        assert_eq!(s, "targets = []", "empty array must stay compact: {s}");
    }

    #[test]
    fn reformat_inline_table_arrays_multiline_handles_nested_inline_tables() {
        // The inner `extra = { ... }` and `extra_parameters = { ... }`
        // braces must NOT trigger spurious splits — bd tracking keeps
        // track of brace depth and only the top-level `}` ends an item.
        let mut s = String::from(
            r#"targets = [{ provider = "kimchi", real_model = "kimi-k2.6", priority = 1 }, { provider = "jx", real_model = "kimi-k2.6", extra = { extra_parameters = { model_id = 39 } } }]"#,
        );
        reformat_inline_table_arrays_multiline(&mut s, "targets", "    ");
        assert!(
            s.contains(
                "\n    { provider = \"kimchi\", real_model = \"kimi-k2.6\", priority = 1 },"
            ),
            "first item body must be unmodified: {s}",
        );
        assert!(
            s.contains(
                "\n    { provider = \"jx\", real_model = \"kimi-k2.6\", extra = { extra_parameters = { model_id = 39 } } }\n]",
            ),
            "second item body must be unmodified and ending bracket on its own line: {s}",
        );
    }

    #[test]
    fn reformat_inline_table_arrays_multiline_ignores_braces_in_strings() {
        // String content contains `{`, `}`, and `,` which must NOT
        // perturb the brace counter.
        let mut s = String::from(r#"targets = [{ provider = "a{b}c, d{e}f" }]"#);
        reformat_inline_table_arrays_multiline(&mut s, "targets", "    ");
        assert!(
            s.contains("\n    { provider = \"a{b}c, d{e}f\" }"),
            "string contents must survive verbatim: {s}",
        );
    }

    #[test]
    fn reformat_inline_table_arrays_multiline_rejects_partial_key_match() {
        // `something_targets = [...]` must NOT be reformatted because
        // the key argument is the literal `targets`. The preceding
        // identifier character `_` disqualifies the match.
        let mut s = String::from(r#"something_targets = [{ a = 1 }, { b = 2 }]"#);
        reformat_inline_table_arrays_multiline(&mut s, "targets", "    ");
        assert_eq!(
            s, r#"something_targets = [{ a = 1 }, { b = 2 }]"#,
            "partial-key match must be left alone: {s}",
        );
    }

    #[test]
    fn reformat_inline_table_arrays_multiline_reformats_only_matching_key() {
        // Two single-line arrays in the same buffer where only `targets`
        // matches the key argument. `unrelated = [...]` should stay as-is.
        let mut s = String::from(
            r#"unrelated = [{ x = 1 }, { y = 2 }]
targets = [{ provider = "a" }, { provider = "b" }]"#,
        );
        reformat_inline_table_arrays_multiline(&mut s, "targets", "    ");
        // `unrelated` line must be unchanged.
        assert!(
            s.contains("unrelated = [{ x = 1 }, { y = 2 }]"),
            "non-matching key line must be untouched: {s}",
        );
        // `targets` line must be reformatted.
        assert!(
            s.contains("\n    { provider = \"a\" },"),
            "matching key line must be reformatted: {s}",
        );
    }

    #[test]
    fn reformat_inline_table_arrays_multiline_handles_escaped_quote_in_string() {
        // Within a string, `\"` must not close the string prematurely —
        // the second quote must be treated as part of the string contents.
        // Use `assert_eq!` against the exact expected rendering so any
        // regression in the string-state machine fails loudly, not just
        // as a substring mismatch.
        let mut s = String::from(r#"targets = [{ provider = "a\"b" }]"#);
        reformat_inline_table_arrays_multiline(&mut s, "targets", "    ");
        assert_eq!(
            s, "targets = [\n    { provider = \"a\\\"b\" }\n]",
            "escaped quote must be preserved: {s}",
        );
    }

    #[test]
    fn reformat_inline_table_arrays_multiline_skips_inline_comment() {
        // A `#` comment between `[` and `]` extends to EOL; the helper
        // must respect that boundary. We only have an opportunity to
        // exercise this on an insertion near a comment — the original
        // multi-line detection already short-circuits for `\n`.
        // Here we test the input that would trip the brace walker if
        // `#` weren't skipped: `[ { a = 1 # inline\n}, { b = 2 }]` is
        // single-line EMITTED but contains `\n`, so we test a
        // single-line no-newline case where there is no `#` at all
        // (handled by other tests).
        let mut s = String::from(r#"targets = [{ a = 1 }, { b = 2 }]"#);
        reformat_inline_table_arrays_multiline(&mut s, "targets", "    ");
        // Count of `\n    { ` occurrences must equal 2 (two items).
        let count = s.matches("\n    { ").count();
        assert_eq!(count, 2, "both items should be on their own line: {s}");
    }

    #[test]
    fn reformat_inline_table_arrays_multiline_repeated_calls_are_idempotent() {
        let mut s =
            String::from(r#"targets = [{ provider = "openai" }, { provider = "anthropic" }]"#);
        reformat_inline_table_arrays_multiline(&mut s, "targets", "    ");
        let once = s.clone();
        reformat_inline_table_arrays_multiline(&mut s, "targets", "    ");
        assert_eq!(s, once, "calling twice must not change the output: {s}");
    }

    #[test]
    fn reformat_inline_table_arrays_multiline_skips_literal_in_comment_line() {
        // The literal `targets = [{...}]` inside a `#` line comment is
        // plain text and must not be reshaped. The previous partial-key
        // check only blocked identifier characters before `abs`; `#`
        // and the surrounding whitespace pass through, so the input
        // would otherwise be damaged. This test pins the comment-line
        // detection rule.
        let mut s = String::from(
            r#"# Example: targets = [{ provider = "openai" }, { provider = "anthropic" }]
targets = [{ provider = "openai" }, { provider = "anthropic" }]"#,
        );
        reformat_inline_table_arrays_multiline(&mut s, "targets", "    ");
        // The comment line must survive verbatim — byte-for-byte.
        assert!(
            s.starts_with(
                "# Example: targets = [{ provider = \"openai\" }, { provider = \"anthropic\" }]\n"
            ),
            "comment line must be untouched: {s:?}",
        );
        // The real targets line (on the second physical line) must be
        // reshaped into multi-line.
        assert!(
            s.contains("\n    { provider = \"openai\" },\n"),
            "real targets line must be reshaped: {s}",
        );
        assert!(
            s.contains("\n    { provider = \"anthropic\" }\n]"),
            "real targets line must end on its own line: {s}",
        );
    }

    #[test]
    fn reformat_inline_table_arrays_multiline_accepts_keyword_after_comment_line() {
        // Sanity check that the comment guard is per-line: a `# ...`
        // comment on a previous line must NOT block the next line's
        // `targets = [...]` from being reshaped.
        let mut s = String::from(
            r#"# Comment above
targets = [{ a = 1 }, { b = 2 }]"#,
        );
        reformat_inline_table_arrays_multiline(&mut s, "targets", "    ");
        // Comment preserved
        assert!(
            s.starts_with("# Comment above\n"),
            "comment must survive verbatim: {s:?}",
        );
        // Real targets reshaped
        assert!(
            s.contains("\n    { a = 1 },\n"),
            "real line must be reshaped despite prior comment: {s}",
        );
    }
    #[test]
    fn reformat_inline_table_arrays_multiline_handles_variable_separator_widths() {
        // Regression for Problem 3: the old reconstruction used
        // `inside_str[last_pos..sp - 2]` to strip the assumed-2-byte
        // separator `, `, but legal TOML also accepts `,{` and
        // `,  ...` (arbitrary spacing inside inline tables). With a
        // 1-byte separator, `sp - 2` landed one byte too early and
        // silently dropped the prior item's closing `}` — the output
        // was unparseable as TOML (matched `{` count exceeded `}`).
        //
        // Pin all three separator widths in one test: each fixture
        // has 3 inline-tables with the given separator, and after
        // reformat the output must have balanced braces AND parse
        // back into a Document with 3 entries.
        //
        // `sep` is the literal separator between items — JUST the
        // comma + surrounding whitespace, no braces. The format!
        // template glues these directly between item bodies.
        for (label, sep) in &[
            (", ", ", "),   // canonical: comma + single space
            (",", ","),     // no whitespace at all
            (",  ", ",  "), // extra spaces (still legal TOML)
        ] {
            let mut s = String::from(&format!(
                "targets = [{{ a = 1 }}{sep}{{ b = 2 }}{sep}{{ c = 3 }}]"
            ));
            let original_open = s.matches('{').count();
            let original_close = s.matches('}').count();
            assert_eq!(
                original_open, original_close,
                "fixture `{label}` should be balanced: {s}",
            );
            reformat_inline_table_arrays_multiline(&mut s, "targets", "    ");
            let open_count = s.matches('{').count();
            let close_count = s.matches('}').count();
            assert_eq!(
                open_count, close_count,
                "separator `{label}` must produce balanced braces — bug was: \
                 prior `}}` got eaten by fixed-width strip. got: {s}",
            );
            // Output must still be valid TOML.
            let _: DocumentMut = s.parse().unwrap_or_else(|e| {
                panic!("separator `{label}` output should still parse as TOML: {e}\n---\n{s}\n---")
            });
            // Output must have 3 separate item lines.
            let line_count = s.matches("\n    { ").count();
            assert_eq!(
                line_count, 3,
                "separator `{label}` should produce 3 item lines, got: {s}",
            );
        }
    }

    // ── remove_if_exists ──

    #[test]
    fn remove_if_exists_removes_existing_key() {
        let mut doc = parse(
            r#"
[backoff]
base_ms = 100
"#,
        );
        remove_if_exists(&mut doc, &["backoff"]);
        let s = doc.to_string();
        assert!(!s.contains("backoff"));
        assert!(!s.contains("base_ms"));
    }

    #[test]
    fn remove_if_exists_noop_when_missing() {
        let mut doc = parse("");
        remove_if_exists(&mut doc, &["backoff"]);
        let s = doc.to_string();
        assert_eq!(s.trim(), "");
    }

    #[test]
    fn remove_if_exists_noop_when_parent_missing() {
        let mut doc = parse("");
        remove_if_exists(&mut doc, &["backoff", "base_ms"]);
        let s = doc.to_string();
        assert_eq!(s.trim(), "");
    }

    // ── prune_keys ──

    #[test]
    fn prune_keys_keeps_listed_and_removes_others() {
        let mut doc = parse(
            r#"
[providers.openai]
api_keys = ["k1"]

[providers.anthropic]
api_keys = ["k2"]

[providers.gemini]
api_keys = ["k3"]
"#,
        );
        prune_keys(&mut doc, &["providers"], &["openai", "anthropic"]);
        let s = doc.to_string();
        assert!(s.contains("openai"));
        assert!(s.contains("anthropic"));
        assert!(!s.contains("gemini"));
    }

    #[test]
    fn prune_keys_noop_when_path_missing() {
        let mut doc = parse("");
        prune_keys(&mut doc, &["providers"], &["openai"]);
        let s = doc.to_string();
        assert_eq!(s.trim(), "");
    }

    // ── ensure_array_field ──

    #[test]
    fn ensure_array_field_creates_when_missing() {
        let mut doc = parse("");
        let arr = ensure_array_field(doc.as_table_mut(), "api_keys");
        assert!(arr.is_empty());
        arr.push("sk-1");
        assert!(doc["api_keys"].is_array());
        // toml_edit's default formatting is compact (`["sk-1"]` not `[ "sk-1" ]`),
        // so just check that the key and value are present.
        let s = doc.to_string();
        assert!(s.contains("api_keys"), "should contain key: {s}");
        assert!(s.contains("\"sk-1\""), "should contain value: {s}");
    }

    #[test]
    fn ensure_array_field_keeps_existing_array() {
        let mut doc = parse(r#"api_keys = ["sk-existing"]"#);
        let arr = ensure_array_field(doc.as_table_mut(), "api_keys");
        // existing value should be preserved
        assert_eq!(arr.len(), 1);
        assert_eq!(
            arr.iter().next().and_then(|v| v.as_str()),
            Some("sk-existing")
        );
    }

    #[test]
    fn ensure_array_field_replaces_non_array_value() {
        // If the field exists but is the wrong type, replace it.
        let mut doc = parse(r#"api_keys = "not an array""#);
        let arr = ensure_array_field(doc.as_table_mut(), "api_keys");
        assert!(arr.is_empty());
    }

    // ── ensure_inline_table_field ──

    #[test]
    fn ensure_inline_table_field_creates_when_missing() {
        let mut doc = parse("");
        let t = ensure_inline_table_field(doc.as_table_mut(), "headers");
        assert!(t.is_empty());
        t.insert("X-Trace", Value::from("on"));
        assert!(doc["headers"].is_inline_table());
        assert!(doc.to_string().contains("headers = { X-Trace = \"on\" }"));
    }

    #[test]
    fn ensure_inline_table_field_keeps_existing_inline_table() {
        let mut doc = parse(r#"headers = { X-Existing = "on" }"#);
        let t = ensure_inline_table_field(doc.as_table_mut(), "headers");
        assert!(t.contains_key("X-Existing"));
    }

    #[test]
    fn ensure_inline_table_field_replaces_non_inline_table_value() {
        let mut doc = parse(r#"headers = "not a table""#);
        let t = ensure_inline_table_field(doc.as_table_mut(), "headers");
        assert!(t.is_empty());
    }

    // ── sync_string_array_in_place ──

    fn string_array(values: &[&str]) -> Array {
        // Build an `Array` directly (the `toml_edit::Array` type does not
        // implement `FromStr`).
        let mut arr = Array::new();
        for v in values {
            arr.push(*v);
        }
        arr
    }

    #[test]
    fn sync_string_array_appends_new_value() {
        let mut arr = string_array(&["sk-1"]);
        let in_memory = vec!["sk-1".to_string(), "sk-2".to_string()];
        sync_string_array_in_place(&mut arr, &in_memory);
        let s = arr.to_string();
        assert!(s.contains("\"sk-1\""));
        assert!(s.contains("\"sk-2\""));
    }

    #[test]
    fn sync_string_array_preserves_existing_order() {
        // On-disk order must be preserved for keys that are still in
        // in_memory; the appended key comes last.
        let mut arr = string_array(&["sk-3", "sk-1"]);
        let in_memory = vec!["sk-1".to_string(), "sk-3".to_string(), "sk-2".to_string()];
        sync_string_array_in_place(&mut arr, &in_memory);
        let s = arr.to_string();
        // Existing keys keep their on-disk order: sk-3, sk-1
        let sk3 = s.find("sk-3").unwrap();
        let sk1 = s.find("sk-1").unwrap();
        assert!(sk3 < sk1, "on-disk order should be preserved, got: {s}");
        // New key appended at the end
        let sk2 = s.find("sk-2").unwrap();
        assert!(sk1 < sk2, "appended key should come last, got: {s}");
    }

    #[test]
    fn sync_string_array_removes_stale_value() {
        let mut arr = string_array(&["sk-1", "sk-2", "sk-3"]);
        let in_memory = vec!["sk-1".to_string(), "sk-3".to_string()];
        sync_string_array_in_place(&mut arr, &in_memory);
        let s = arr.to_string();
        assert!(s.contains("\"sk-1\""));
        assert!(s.contains("\"sk-3\""));
        assert!(!s.contains("\"sk-2\""));
    }

    #[test]
    fn sync_string_array_empty_in_memory_clears_array() {
        let mut arr = string_array(&["sk-1", "sk-2"]);
        let in_memory: Vec<String> = vec![];
        sync_string_array_in_place(&mut arr, &in_memory);
        assert!(arr.is_empty());
    }

    #[test]
    fn sync_string_array_defensively_removes_non_string_items() {
        let mut arr = Array::new();
        arr.push(42_i64);
        arr.push("sk-1");
        let in_memory = vec!["sk-1".to_string()];
        sync_string_array_in_place(&mut arr, &in_memory);
        let s = arr.to_string();
        assert!(!s.contains("42"), "non-string item should be removed");
        assert!(s.contains("sk-1"));
    }

    // ── sync_inline_table_in_place ──

    fn json_value(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("test fixture must be valid JSON")
    }

    #[test]
    fn sync_inline_table_preserves_existing_order() {
        let mut doc = parse(r#"extra = { temperature = 0.7, max_tokens = 100 }"#);
        let t = doc["extra"].as_inline_table_mut().unwrap();
        let in_memory: std::collections::HashMap<String, serde_json::Value> = [
            ("temperature".into(), json_value("0.5")),
            ("max_tokens".into(), json_value("200")),
        ]
        .into_iter()
        .collect();
        sync_inline_table_in_place(&mut *t, &in_memory, |v| match v {
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::from(i)
                } else if let Some(f) = n.as_f64() {
                    Value::from(f)
                } else {
                    Value::from(n.to_string())
                }
            }
            serde_json::Value::String(s) => Value::from(s.as_str()),
            _ => Value::from(""),
        });
        let s = doc.to_string();
        let temp_pos = s.find("temperature").unwrap();
        let max_pos = s.find("max_tokens").unwrap();
        assert!(
            temp_pos < max_pos,
            "on-disk order should be preserved, got: {s}"
        );
    }

    #[test]
    fn sync_inline_table_appends_new_key() {
        let mut doc = parse(r#"extra = { temperature = 0.7 }"#);
        let t = doc["extra"].as_inline_table_mut().unwrap();
        let in_memory: std::collections::HashMap<String, serde_json::Value> = [
            ("temperature".into(), json_value("0.7")),
            ("max_tokens".into(), json_value("100")),
        ]
        .into_iter()
        .collect();
        sync_inline_table_in_place(&mut *t, &in_memory, |v| match v {
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::from(i)
                } else {
                    Value::from(n.to_string())
                }
            }
            _ => Value::from(""),
        });
        let s = doc.to_string();
        let temp_pos = s.find("temperature").unwrap();
        let max_pos = s.find("max_tokens").unwrap();
        assert!(temp_pos < max_pos, "appended key should come last: {s}");
    }

    #[test]
    fn sync_inline_table_removes_stale_key() {
        let mut doc = parse(r#"extra = { temperature = 0.7, max_tokens = 100 }"#);
        let t = doc["extra"].as_inline_table_mut().unwrap();
        let in_memory: std::collections::HashMap<String, serde_json::Value> =
            [("temperature".into(), json_value("0.7"))]
                .into_iter()
                .collect();
        sync_inline_table_in_place(&mut *t, &in_memory, |v| match v {
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::from(i)
                } else {
                    Value::from(n.to_string())
                }
            }
            _ => Value::from(""),
        });
        let s = doc.to_string();
        assert!(s.contains("temperature"));
        assert!(!s.contains("max_tokens"));
    }
}
