//! Typed serde DTOs for the upstream/downstream wire APIs the gateway speaks.
//!
//! Each submodule models one protocol body as plain data (no mapping logic):
//! every struct carries a flattened `extra` catch-all, so fields the gateway
//! does not model survive a deserialize → serialize round-trip untouched.

pub mod anthropic;
pub mod responses;
pub mod systemone;

pub use systemone::{Question, SystemOneRequest, extract_usage};
