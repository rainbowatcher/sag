use std::collections::HashMap;

use reqwest::RequestBuilder;

/// Apply custom headers to a reqwest request builder.
/// Custom headers override any previously set headers since they are
/// applied last.
pub fn apply_headers(req: RequestBuilder, headers: &HashMap<String, String>) -> RequestBuilder {
    let mut req = req;
    for (key, value) in headers {
        req = req.header(key.as_str(), value.as_str());
    }
    req
}

/// Merge body overrides into a serialized JSON value at the top level.
///
/// Each key in `overrides` is inserted into (or overwritten in) the body's
/// top-level object. Fields with nested objects are replaced wholesale.
pub fn apply_body_overrides(
    mut body: serde_json::Value,
    overrides: &HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    if overrides.is_empty() {
        return body;
    }
    if let serde_json::Value::Object(map) = &mut body {
        for (key, value) in overrides {
            map.insert(key.clone(), value.clone());
        }
    }
    body
}
