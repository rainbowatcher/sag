//! Typed serde DTOs for the TypeSafe "System One" decision API
//! (`POST /v1/systemone`).
//!
//! Coverage:
//!
//! * [`SystemOneRequest`] — request body (`model` / `state` / `questions` /
//!   optional `assets`).
//! * [`Question`] — one entry of the dynamic `questions` map.
//! * [`extract_usage`] — `usage.input_tokens` / `usage.output_tokens` for
//!   analytics.
//!
//! The response is deliberately **not** modelled: `answers` / `id` / the
//! per-question answer payloads (including the probability fields) are
//! forwarded verbatim as [`serde_json::Value`], so a decode → encode round-trip
//! can never lose or reorder them.
//!
//! Round-trip fidelity has exactly **one** deliberate loss vector: an explicit
//! `null` in an optional field (`instructions` / `criteria` / `assets`)
//! deserializes to `None` and is skipped by `skip_serializing_if`, so it comes
//! back as *absent* rather than `null`. The two spellings are treated as the
//! same thing on purpose — the upstream makes no distinction, and preserving it
//! would require a double `Option` for no functional gain. Every other field,
//! known or unknown, is preserved as given.
//!
//! **The DTOs are not on the wire path.** Production dispatch never decodes a
//! SystemOne body into [`SystemOneRequest`] / [`Question`]: the ingress handler
//! keeps the whole body as `Json<Value>` and hands it to the carrier's
//! `extra`, and `build_systemone_body` rebuilds the upstream body from that
//! map, rewriting only `model`. Only [`extract_usage`] is consumed by the
//! dispatch layer. These types therefore exist as a public (documented) API
//! surface and for tests; the explicit-`null`-becomes-absent loss vector above
//! **cannot** affect real traffic.
//!
//! Conventions (mirroring `crate::types` and the sibling modules):
//!
//! * No `deny_unknown_fields`: unknown fields survive the round-trip through
//!   the per-struct `#[serde(flatten)] extra` map.
//! * No semantic validation: `question.type` and the `criteria` shape are
//!   carried as-is. Unknown question kinds are tolerated (the caller's
//!   contract with the upstream decides), because "rejecting unknown values"
//!   is reserved for typed config enums, not request bodies.
//! * `state` is a [`serde_json::Value`] rather than an untagged enum: the
//!   protocol accepts a string, an object or an array, and any of them must
//!   round-trip unchanged.
//! * The dynamic maps (`questions`, `assets`) are [`BTreeMap`]s, not
//!   `serde_json::Map`: `serde_json::Map` implements `Serialize` /
//!   `Deserialize` / `Clone` / `Debug` / `PartialEq` for `Map<String, Value>`
//!   **only**, so it cannot hold typed values. `BTreeMap` is identical on the
//!   wire — serde_json's default map backend *is* a `BTreeMap`
//!   (`preserve_order` is off workspace-wide), so keys are emitted in the same
//!   sorted order either way.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Request body of `POST /v1/systemone`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemOneRequest {
    /// Target model id (routing key; also the caller's model selector).
    pub model: String,
    /// Arbitrary caller state: a string, an object or an array all round-trip
    /// verbatim (see the module docs).
    pub state: Value,
    /// Dynamic map of question id → question. Keys are caller-chosen and
    /// answered by the upstream under the same ids.
    pub questions: BTreeMap<String, Question>,
    /// Image dictionary (multi-modal `box` questions). Values are **objects**,
    /// not strings: the spec carries them either inline
    /// (`{"type": "image", "format": "png", "data_base64": "…"}`) or, for
    /// multipart uploads, as a reference to a form part. Typing the value as
    /// [`Value`] keeps both spellings (and anything the upstream adds later)
    /// decodable instead of rejecting legal requests with a 422.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assets: Option<BTreeMap<String, Value>>,
    /// Catch-all for non-standard / provider-specific fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One entry of `SystemOneRequest::questions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    /// Question kind, e.g. `"noul"` / `"choice"` / `"score"` / `"box"`.
    ///
    /// Never validated: unknown kinds are forwarded so future upstream
    /// variants keep working.
    #[serde(rename = "type")]
    pub kind: String,
    /// Free-form instructions. Deliberately a [`Value`]: the shape differs per
    /// question kind and is the upstream's contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Value>,
    /// Choice list / score legend / box criteria. Same freedom as
    /// `instructions` — never interpreted by the gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<Value>,
    /// Catch-all for non-standard / provider-specific fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Extract `(input_tokens, output_tokens)` from a SystemOne / any
/// `{"usage": {…}}` response body.
///
/// Returns `None` when `usage` is absent, or when either counter is missing or
/// not a non-negative integer. Callers must keep the analytics column `NULL`
/// in that case — a fabricated `0` would silently corrupt cost accounting.
pub fn extract_usage(body: &Value) -> Option<(u64, u64)> {
    let usage = body.get("usage")?;
    let input = usage.get("input_tokens")?.as_u64()?;
    let output = usage.get("output_tokens")?.as_u64()?;
    Some((input, output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decode(raw: &Value) -> SystemOneRequest {
        serde_json::from_value(raw.clone()).expect("request should deserialize")
    }

    /// Round-trip helper: `Value -> DTO -> Value` must be lossless.
    fn roundtrip(raw: &Value) -> Value {
        let req = decode(raw);
        serde_json::to_value(&req).expect("request should serialize")
    }

    #[test]
    fn request_roundtrip_preserves_unknown_fields() {
        let raw = json!({
            "model": "systemone-v1",
            "state": {"user": "u-1", "history": ["a", "b"]},
            "questions": {
                "sentiment": {
                    "type": "choice",
                    "instructions": "pick one",
                    "criteria": ["positive", "negative"],
                    "weight": 2,
                    "vendor_hint": {"nested": true}
                }
            },
            "assets": {
                "img_1": {"type": "image", "format": "png", "data_base64": "iVBORw0KGgo="}
            },
            "temperature": 0.2,
            "id": "client-side-id",
            "extra_block": {"deep": [1, 2, 3]}
        });

        assert_eq!(roundtrip(&raw), raw);

        let req = decode(&raw);
        assert_eq!(req.model, "systemone-v1");
        assert_eq!(req.questions.len(), 1);
        assert_eq!(req.assets.as_ref().map(BTreeMap::len), Some(1));
        // Unknown top-level fields land in `extra`, not dropped.
        assert!(req.extra.contains_key("temperature"));
        assert!(req.extra.contains_key("id"));
        assert!(req.extra.contains_key("extra_block"));
        // Unknown question fields land in the question's own `extra`.
        let q = req.questions.get("sentiment").expect("question present");
        assert_eq!(q.kind, "choice");
        assert!(q.extra.contains_key("weight"));
        assert!(q.extra.contains_key("vendor_hint"));
    }

    #[test]
    fn question_unknown_type_is_tolerated() {
        let raw = json!({
            "model": "m",
            "state": "",
            "questions": {
                "q1": {"type": "future_kind", "criteria": {"any": "shape"}},
                "q2": {"type": "", "extra_field": 1}
            }
        });

        let req = decode(&raw);
        let q1 = req.questions.get("q1").expect("q1 present");
        let q2 = req.questions.get("q2").expect("q2 present");
        assert_eq!(q1.kind, "future_kind");
        assert_eq!(q1.criteria, Some(json!({"any": "shape"})));
        assert_eq!(q1.instructions, None);
        assert_eq!(q2.kind, "");
        assert_eq!(q2.extra.get("extra_field"), Some(&json!(1)));
        // Nothing is invented for absent criteria/instructions on round-trip.
        assert_eq!(roundtrip(&raw), raw);
    }

    /// `state` accepts a string, an object or an array (three states) and each
    /// is preserved exactly.
    #[test]
    fn state_accepts_string_object_and_array() {
        for state in [
            json!("plain string state"),
            json!({"nested": {"list": [1, "two", null]}}),
            json!([{"turn": 1}, {"turn": 2}]),
            json!(null),
            json!(42),
        ] {
            let raw = json!({
                "model": "m",
                "state": state,
                "questions": {}
            });
            let req = decode(&raw);
            assert_eq!(req.state, state, "state must round-trip verbatim");
            assert_eq!(roundtrip(&raw), raw);
        }
    }

    /// `assets` values are objects per the spec; any JSON shape must survive
    /// untouched (the upstream owns the shape, the gateway only decodes it).
    #[test]
    fn assets_object_values_are_preserved() {
        let raw = json!({
            "model": "m",
            "state": "s",
            "questions": {"box_q": {"type": "box", "instructions": "find it"}},
            "assets": {
                "inline": {"type": "image", "format": "png", "data_base64": "iVBORw0KGgo="},
                "part_ref": {"type": "image", "part": "files[0]"},
                "odd_number": 7,
                "odd_null": null
            }
        });

        let req = decode(&raw);
        let assets = req.assets.as_ref().expect("assets present");
        assert_eq!(assets.len(), 4);
        assert_eq!(
            assets.get("inline"),
            Some(&json!({"type": "image", "format": "png", "data_base64": "iVBORw0KGgo="}))
        );
        assert_eq!(
            assets.get("part_ref"),
            Some(&json!({"type": "image", "part": "files[0]"}))
        );
        assert_eq!(assets.get("odd_number"), Some(&json!(7)));
        assert_eq!(assets.get("odd_null"), Some(&json!(null)));
        assert_eq!(roundtrip(&raw), raw);
    }

    /// The only deliberate loss vector: an explicit `null` optional field comes
    /// back absent. Documented in the module docs; asserted so the trade-off
    /// stays visible.
    #[test]
    fn explicit_null_optional_fields_become_absent() {
        let raw = json!({
            "model": "m",
            "state": "s",
            "questions": {
                "q1": {"type": "noul", "instructions": null, "criteria": null}
            },
            "assets": null
        });

        assert_eq!(
            roundtrip(&raw),
            json!({
                "model": "m",
                "state": "s",
                "questions": {"q1": {"type": "noul"}}
            })
        );
    }

    #[test]
    fn assets_omitted_when_absent() {
        let raw = json!({"model": "m", "state": "s", "questions": {}});
        let req = decode(&raw);
        assert_eq!(req.assets, None);
        let out = serde_json::to_value(&req).unwrap();
        assert!(
            out.get("assets").is_none(),
            "absent assets must stay absent"
        );
        assert_eq!(out, raw);
    }

    #[test]
    fn missing_required_fields_are_rejected() {
        for raw in [
            json!({"state": "s", "questions": {}}),
            json!({"model": "m", "questions": {}}),
            json!({"model": "m", "state": "s"}),
            // `type` is required on a question.
            json!({"model": "m", "state": "s", "questions": {"q": {"instructions": "x"}}}),
            // `questions` must be a map of objects.
            json!({"model": "m", "state": "s", "questions": []}),
        ] {
            assert!(
                serde_json::from_value::<SystemOneRequest>(raw.clone()).is_err(),
                "expected rejection: {raw}"
            );
        }
    }

    #[test]
    fn extract_usage_reads_both_counters() {
        assert_eq!(
            extract_usage(&json!({"usage": {"input_tokens": 12, "output_tokens": 34}})),
            Some((12, 34))
        );
        // Extra usage fields / extra body fields are ignored.
        assert_eq!(
            extract_usage(&json!({
                "id": "r1",
                "answers": {},
                "usage": {"input_tokens": 0, "output_tokens": 7, "cached_tokens": 3}
            })),
            Some((0, 7))
        );
        // Nested under an envelope still works (top-level `usage` lookup only).
        assert_eq!(
            extract_usage(&json!({"usage": {"input_tokens": 1, "output_tokens": 2}})),
            Some((1, 2))
        );
    }

    /// Missing / malformed counters yield `None` — never a fabricated `0`.
    #[test]
    fn extract_usage_never_fabricates_zero() {
        for body in [
            json!({}),
            json!({"answers": {}}),
            json!({"usage": null}),
            json!({"usage": {}}),
            json!({"usage": {"input_tokens": 10}}),
            json!({"usage": {"output_tokens": 10}}),
            json!({"usage": {"input_tokens": "10", "output_tokens": 10}}),
            json!({"usage": {"input_tokens": -1, "output_tokens": 10}}),
            json!({"usage": {"input_tokens": 1.5, "output_tokens": 10}}),
        ] {
            assert_eq!(extract_usage(&body), None, "expected None for {body}");
        }
    }
}
