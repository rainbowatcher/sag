//! Privacy primitives for SAG: placeholder generation and streaming restore.
//!
//! The crate is deliberately dependency-light (no config, no state) so it can
//! be reused by the dispatch path and tested in isolation:
//!
//! - [`placeholder`] — deterministic `[[{KIND}_{h12}]]` markers plus the
//!   reverse [`PlaceholderMap`].
//! - [`restore`] — [`RestoreWriter`], which puts the plaintext back into an
//!   SSE response body frame by frame.
//! - [`rules`] — compiled detectors (built-in regexes with adjacency and
//!   checksum guards).
//! - [`engine`] — [`PrivacyEngine`], which scans a chat request and applies
//!   the configured mask / warn / block policy.

pub mod engine;
pub mod placeholder;
pub mod restore;
pub mod rules;

pub use engine::{MaskReport, Outcome, PrivacyEngine};
pub use placeholder::{HASH_HEX_LEN, PlaceholderMap, make, placeholder_len};
pub use restore::RestoreWriter;
