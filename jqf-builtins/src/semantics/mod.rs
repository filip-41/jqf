//! The one jqf semantic value model, centralized away from the executor.
//!
//! One job: own the single authoritative jqf value laws — truth, and (in later verticals) equality, ordering,
//! hashing, arithmetic, indexing, and path update — so no two callers can grow a second, divergent definition. Every
//! law here reads borrowed [`crate::codec_result::EngineResult`] inputs (a located document view or an owned value) and
//! returns a plain semantic answer; it drives no cardinality, allocates no owned result, and holds no state.
//!
//! This module owns [`truth`]; the sibling modules own the remaining laws — ordering (`order`), arithmetic (`arith`,
//! `binary`), containment (`contain`), indexing and path update (`path`, `keyed`), and the stream/codec laws
//! (`stream_events`, `decode`, `scan`).

pub mod arith;

pub mod binary;
pub mod contain;
pub mod decode;
pub mod depth;
pub mod facts;
pub mod generate;
pub mod json_escape;
pub mod keyed;
pub mod order;
pub mod owned;
pub mod path;
pub mod pathset;
pub mod rand;
pub mod rawtext;
pub mod render;
pub mod scan;
pub mod spill;
pub mod stream_events;
pub mod text;
pub mod truth;

pub use facts::{accessor_matches_fact, materialize_fact_payload};
pub use owned::{
    DynAccess, OwnedNav, clone_owned, dyn_index, index_owned, navigate_owned_index, navigate_owned_key, owned_kind,
    resolve_slice_bound, unrepresentable_index,
};
pub use rand::{Prng, rand_float, with_prng};
