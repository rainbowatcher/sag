use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 显式 loss 记录条目作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LossScope {
    Request,
    Message,
    ContentBlock,
    Response,
    StreamEvent,
}

/// 结构性丢弃原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LossReason {
    UnsupportedByIr,
    UnsupportedByTarget,
    DroppedByTargetFeature,
}

/// 单条 loss：记录被丢弃字段的作用域、路径、原因与原值快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LossEntry {
    pub scope: LossScope,
    /// 字段路径，如 `"messages[2].content[3]"`、`"choices[0].finish_reason"`。
    pub field: String,
    pub reason: LossReason,
    /// 原值快照
    pub value: Value,
}

/// 显式 loss 记录：禁止静默丢失。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LossInventory {
    pub entries: Vec<LossEntry>,
}

impl LossInventory {
    pub fn push(&mut self, entry: LossEntry) {
        self.entries.push(entry);
    }

    pub fn push_loss(
        &mut self,
        scope: LossScope,
        field: impl Into<String>,
        reason: LossReason,
        value: Value,
    ) {
        self.entries.push(LossEntry {
            scope,
            field: field.into(),
            reason,
            value,
        });
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Compact single-line summary for `CallLog.loss_summary`:
    /// `"{len} entries; field:reason; field:reason; …"` with at most
    /// `max` detail entries. Returns `None` when empty.
    pub fn summarize(&self, max: usize) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let shown = self.entries.len().min(max.max(1));
        let mut parts: Vec<String> = Vec::with_capacity(shown + 1);
        for e in self.entries.iter().take(shown) {
            let reason = match e.reason {
                LossReason::UnsupportedByIr => "unsupported_by_ir",
                LossReason::UnsupportedByTarget => "unsupported_by_target",
                LossReason::DroppedByTargetFeature => "dropped_by_target_feature",
            };
            parts.push(format!("{}:{}", e.field, reason));
        }
        let mut out = format!("{} entries", self.entries.len());
        if !parts.is_empty() {
            out.push_str("; ");
            out.push_str(&parts.join("; "));
            if self.entries.len() > shown {
                out.push_str("; …");
            }
        }
        Some(out)
    }
}

/// Merge several inventories (request decode + encode + response) into one
/// summary string for `CallLog.loss_summary`.
pub fn summarize_losses(inventories: &[&LossInventory], max: usize) -> Option<String> {
    let total: usize = inventories.iter().map(|i| i.len()).sum();
    if total == 0 {
        return None;
    }
    // Flatten the first `max` entries across inventories in order.
    let mut parts: Vec<String> = Vec::new();
    let mut remaining = max.max(1);
    for inv in inventories {
        for e in inv.entries.iter() {
            if remaining == 0 {
                break;
            }
            let reason = match e.reason {
                LossReason::UnsupportedByIr => "unsupported_by_ir",
                LossReason::UnsupportedByTarget => "unsupported_by_target",
                LossReason::DroppedByTargetFeature => "dropped_by_target_feature",
            };
            parts.push(format!("{}:{}", e.field, reason));
            remaining -= 1;
        }
        if remaining == 0 {
            break;
        }
    }
    let mut out = format!("{total} entries");
    if !parts.is_empty() {
        out.push_str("; ");
        out.push_str(&parts.join("; "));
        if total > parts.len() {
            out.push_str("; …");
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_scope_snake_case_roundtrip() {
        assert_eq!(
            serde_json::to_value(LossScope::ContentBlock).unwrap(),
            serde_json::json!("content_block")
        );
        assert_eq!(
            serde_json::to_value(LossScope::StreamEvent).unwrap(),
            serde_json::json!("stream_event")
        );
        for scope in [
            LossScope::Request,
            LossScope::Message,
            LossScope::ContentBlock,
            LossScope::Response,
            LossScope::StreamEvent,
        ] {
            let json = serde_json::to_value(scope).unwrap();
            let back: LossScope = serde_json::from_value(json).unwrap();
            assert_eq!(back, scope);
        }
        assert!(serde_json::from_str::<LossScope>("\"nope\"").is_err());
    }

    #[test]
    fn loss_reason_snake_case_roundtrip() {
        assert_eq!(
            serde_json::to_value(LossReason::UnsupportedByIr).unwrap(),
            serde_json::json!("unsupported_by_ir")
        );
        assert_eq!(
            serde_json::to_value(LossReason::UnsupportedByTarget).unwrap(),
            serde_json::json!("unsupported_by_target")
        );
        assert_eq!(
            serde_json::to_value(LossReason::DroppedByTargetFeature).unwrap(),
            serde_json::json!("dropped_by_target_feature")
        );
        for reason in [
            LossReason::UnsupportedByIr,
            LossReason::UnsupportedByTarget,
            LossReason::DroppedByTargetFeature,
        ] {
            let json = serde_json::to_value(reason).unwrap();
            let back: LossReason = serde_json::from_value(json).unwrap();
            assert_eq!(back, reason);
        }
        assert!(serde_json::from_str::<LossReason>("\"nope\"").is_err());
    }

    #[test]
    fn push_and_is_empty() {
        let mut inv = LossInventory::default();
        assert!(inv.is_empty());
        assert_eq!(inv.len(), 0);

        inv.push(LossEntry {
            scope: LossScope::Request,
            field: "n".into(),
            reason: LossReason::UnsupportedByIr,
            value: serde_json::json!(2),
        });
        assert!(!inv.is_empty());
        assert_eq!(inv.len(), 1);

        inv.push_loss(
            LossScope::Message,
            "messages[0].name",
            LossReason::UnsupportedByTarget,
            serde_json::json!("sys"),
        );
        assert_eq!(inv.len(), 2);
        assert_eq!(inv.entries[1].field, "messages[0].name");
    }

    #[test]
    fn loss_inventory_roundtrip() {
        let mut inv = LossInventory::default();
        inv.push_loss(
            LossScope::ContentBlock,
            "messages[2].content[3]",
            LossReason::UnsupportedByTarget,
            serde_json::json!({"type": "refusal"}),
        );
        let json = serde_json::to_value(&inv).unwrap();
        let back: LossInventory = serde_json::from_value(json).unwrap();
        assert_eq!(back, inv);
    }

    #[test]
    fn summarize_empty_is_none() {
        let inv = LossInventory::default();
        assert_eq!(inv.summarize(5), None);
        assert_eq!(summarize_losses(&[&inv], 5), None);
    }

    #[test]
    fn summarize_format_count_and_field_reason() {
        let mut inv = LossInventory::default();
        inv.push_loss(
            LossScope::Request,
            "tools[0]",
            LossReason::UnsupportedByIr,
            serde_json::json!({"type": "custom"}),
        );
        inv.push_loss(
            LossScope::ContentBlock,
            "messages[1].content[2]",
            LossReason::UnsupportedByTarget,
            serde_json::json!({"type": "input_audio"}),
        );
        let s = inv.summarize(5).expect("summary");
        assert!(s.starts_with("2 entries"), "got: {s}");
        assert!(s.contains("tools[0]:unsupported_by_ir"), "got: {s}");
        assert!(
            s.contains("messages[1].content[2]:unsupported_by_target"),
            "got: {s}"
        );
    }

    #[test]
    fn summarize_truncates_to_max() {
        let mut inv = LossInventory::default();
        for i in 0..10 {
            inv.push_loss(
                LossScope::Request,
                format!("f{i}"),
                LossReason::UnsupportedByIr,
                serde_json::json!(i),
            );
        }
        let s = inv.summarize(3).expect("summary");
        assert!(s.starts_with("10 entries"), "got: {s}");
        assert!(s.contains("f0:"), "got: {s}");
        assert!(s.contains("f2:"), "got: {s}");
        assert!(!s.contains("f3:"), "got: {s}");
        assert!(s.contains("…"), "got: {s}");
    }

    #[test]
    fn summarize_losses_merges_inventories() {
        let mut a = LossInventory::default();
        a.push_loss(
            LossScope::Request,
            "a",
            LossReason::UnsupportedByIr,
            serde_json::json!(1),
        );
        let mut b = LossInventory::default();
        b.push_loss(
            LossScope::Response,
            "b",
            LossReason::UnsupportedByTarget,
            serde_json::json!(2),
        );
        let s = summarize_losses(&[&a, &b], 5).expect("summary");
        assert!(s.starts_with("2 entries"), "got: {s}");
        assert!(s.contains("a:unsupported_by_ir"), "got: {s}");
        assert!(s.contains("b:unsupported_by_target"), "got: {s}");
    }
}
