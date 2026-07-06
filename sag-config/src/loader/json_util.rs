use std::collections::HashMap;

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
pub fn json_to_toml_value(v: &serde_json::Value) -> toml_edit::Value {
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
pub fn check_json_value_path(v: &serde_json::Value, path: &str) -> Result<(), sag_core::AppError> {
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
pub fn check_json_map(
    map: &HashMap<String, serde_json::Value>,
    context: &str,
) -> Result<(), sag_core::AppError> {
    for (key, val) in map {
        check_json_value_path(val, &format!("{context}.{key}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
