use std::collections::HashMap;

use sag_core::AppError;

/// Convert a `serde_json::Value` into a `toml_edit::Value`.
///
/// TOML has no `null`. `Null` is rejected (`AppError::Config`) rather
/// than coerced to the string `"null"` because silent coercion would
/// corrupt the config file: a `Null` written today would come back as
/// the string `"null"` on the next load and the user-visible behaviour
/// would drift silently.
///
/// Validation lives upstream in [`check_json_map`] (called from
/// [`save_config_to_file`](crate::loader::save_config_to_file)
/// for both `body_overrides` and target `extra`), which recursively
/// walks the JSON tree and rejects nulls at any depth. An `Err` from
/// this function therefore indicates one of two things: (a) a new call
/// site was added without going through validation, or (b)
/// [`check_json_map`]'s recursion has a gap. Either way, propagate the
/// error so the save fails before the atomic rename — never write a
/// broken file, never panic.
pub fn json_to_toml_value(v: &serde_json::Value) -> Result<toml_edit::Value, AppError> {
    match v {
        serde_json::Value::Null => Err(AppError::Config(format!(
            "json_to_toml_value called with Null at {v:?} — TOML has no null \
             representation and upstream validation (check_json_map) should \
             have rejected this value before reaching here. The error indicates \
             either a new write path that bypasses validation, or a recursion \
             gap in check_json_map itself; investigate before re-running."
        ))),
        serde_json::Value::Bool(b) => Ok(toml_edit::Value::from(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(toml_edit::Value::from(i))
            } else if let Some(f) = n.as_f64() {
                Ok(toml_edit::Value::from(f))
            } else {
                Ok(toml_edit::Value::from(n.to_string()))
            }
        }
        serde_json::Value::String(s) => Ok(toml_edit::Value::from(s.as_str())),
        serde_json::Value::Array(items) => {
            let mut arr = toml_edit::Array::new();
            for item in items {
                arr.push(json_to_toml_value(item)?);
            }
            Ok(toml_edit::Value::Array(arr))
        }
        serde_json::Value::Object(map) => {
            let mut table = toml_edit::InlineTable::new();
            for (k, val) in map {
                table.insert(k, json_to_toml_value(val)?);
            }
            Ok(toml_edit::Value::InlineTable(table))
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
pub fn check_json_value_path(v: &serde_json::Value, path: &str) -> Result<(), AppError> {
    match v {
        serde_json::Value::Null => Err(AppError::Config(format!(
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
) -> Result<(), AppError> {
    for (key, val) in map {
        check_json_value_path(val, &format!("{context}.{key}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_to_toml_value_errors_on_null() {
        // Defence-in-depth test: even if a future call site forgets to
        // route through check_json_map, the silent "write the string
        // \"null\"" coercion is gone — a Null reaching json_to_toml_value
        // returns an error that still diagnoses the validation gap, so
        // the bug surfaces instead of corrupting the config file. The
        // previous code mapped Null to the string "null" defensively,
        // which meant a regression in the upstream null check would
        // silently produce broken-but-parseable TOML.
        let err = json_to_toml_value(&serde_json::Value::Null).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("json_to_toml_value called with Null"),
            "error should keep the validation-gap diagnosis; got: {msg}"
        );
        assert!(
            msg.contains("check_json_map"),
            "error should point at the upstream validator; got: {msg}"
        );
        assert!(
            !msg.contains("\"null\""),
            "error must not suggest coercing Null to the string \"null\"; got: {msg}"
        );
    }

    #[test]
    fn json_to_toml_value_propagates_nested_null() {
        // A null nested inside an array must abort the whole conversion
        // (recursion propagates with `?`) rather than panic or skip.
        let v = serde_json::json!([1, null, 3]);
        let err = json_to_toml_value(&v).unwrap_err();
        assert!(
            err.to_string()
                .contains("json_to_toml_value called with Null"),
            "nested null should produce the same diagnosis; got: {err}"
        );

        let v = serde_json::json!({ "a": { "b": null } });
        assert!(json_to_toml_value(&v).is_err());
    }

    #[test]
    fn json_to_toml_value_converts_scalars_and_containers() {
        let v = serde_json::json!({
            "b": true,
            "i": 42,
            "f": 0.5,
            "s": "x",
            "arr": [1, "two", false],
            "obj": { "nested": 1 }
        });
        let out = json_to_toml_value(&v).expect("null-free JSON must convert");
        let table = out.as_inline_table().expect("object → inline table");
        assert_eq!(table["b"].as_bool(), Some(true));
        assert_eq!(table["i"].as_integer(), Some(42));
        assert_eq!(table["f"].as_float(), Some(0.5));
        assert_eq!(table["s"].as_str(), Some("x"));
        assert_eq!(table["arr"].as_array().map(|a| a.len()), Some(3));
        assert!(table["obj"].as_inline_table().is_some());
    }
}
