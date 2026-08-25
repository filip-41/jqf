//! Semantic-checksum witness shared by the jqf and serde decode oracles.
//!
//! The canonical copy lives in `tools/jqf-codec-fuzz/src/semantic.rs` (the
//! json codec owns the collapsed fuzz harness's crate root, plan 124 Y3);
//! this file includes it (065 D28 clause 3) and adds the `SCHEMA` const the
//! differential's report needs.

include!("../../../../jqf-codec-fuzz/src/semantic.rs");

/// Identifies the exact checksum scheme so reports are self-describing.
pub(crate) const SCHEMA: &str = "json-semantic-fnv1a64-v2";
