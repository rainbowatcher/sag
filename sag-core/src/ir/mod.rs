//! Protocol-neutral intermediate representation (IR).
//!
//! Pure type definitions for the three-API native/translated lane design.
//! Nothing here is wired into dispatch/providers yet (M1).

mod loss;
mod request;
mod response;
mod stream;

pub use loss::{LossEntry, LossInventory, LossReason, LossScope};
pub use request::{
    IrContentBlock, IrMessage, IrRequest, IrRole, IrTool, IrToolCall, Reasoning, Sampling,
};
pub use response::{IrFinishReason, IrOutputItem, IrResponse, IrResponseMeta};
pub use stream::IrStreamEvent;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Usage;

    #[test]
    fn top_level_ir_types_roundtrip_smoke() {
        let req = IrRequest::default();
        let json = serde_json::to_value(&req).unwrap();
        let back: IrRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, req);

        let resp = IrResponse {
            id: "x".into(),
            created: None,
            model: "m".into(),
            output: vec![IrOutputItem::Text { text: "t".into() }],
            finish_reason: Some(IrFinishReason::Stop),
            usage: Some(Usage::default()),
            raw: Default::default(),
            loss: LossInventory::default(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        let back: IrResponse = serde_json::from_value(json).unwrap();
        assert_eq!(back, resp);

        let mut inv = LossInventory::default();
        inv.push_loss(
            LossScope::Request,
            "field",
            LossReason::UnsupportedByIr,
            serde_json::json!(null),
        );
        let json = serde_json::to_value(&inv).unwrap();
        let back: LossInventory = serde_json::from_value(json).unwrap();
        assert_eq!(back, inv);
        assert!(!back.is_empty());
    }
}
