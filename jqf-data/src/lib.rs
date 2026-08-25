//! Owned values and the immutable document they come from.
//!
//! [`Value`] is the owned semantic value. [`Document`] is one immutable document a decoder builds and a caller reads.
//! Building a value does not invent document topology, facts, or source authority.
//!
//! [`Document`] is cheap to share. Build one with [`AccountedDocumentBuilder`]; [`DocumentCapacity`] can reserve table
//! sizes first. Heap payloads on a [`Value`] are shared: `Clone` is a refcount bump, and a later write copies first.
//! Value constructors fail only if the allocator refuses; they do not take a resource context. Array and object writes
//! are the same. Materializing still needs a context for work credits and cancel.
//!
//! # Example
//!
//! ``` use jqf_data::{     AccountedDocumentBuilder, AccountedSemanticNode, FormatId, Value, };
//! use jqf_resource::{     ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter, };
//!
//! static CONTROL: ContinueControl = ContinueControl;
//! let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
//! let mut resources = ResourceContext::new(     RequestAccount::try_new(limits)?,
//!     &CONTROL,     WorkMeter::try_new_v1(1).ok_or("work meter")?, )?;
//!
//! let format = FormatId::try_new("example")?; let mut builder = AccountedDocumentBuilder::try_new("example", None)?;
//! let root = builder.add_node(     "example.bool",     AccountedSemanticNode::Bool(true),     None,
//!     &resources, )?; let document = builder.finish(root, &resources)?;
//!
//! assert!(matches!(document.materialize_root(&mut resources)?, Value::Bool(true)));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![no_std]
#![allow(
    clippy::missing_errors_doc,
    reason = "fallible APIs use the crate's closed typed errors documented in CONTRACTS.md"
)]

extern crate alloc;

mod document;
mod format;
mod identity;
mod index;
mod kind;
pub(crate) mod materialize;
mod number;
mod temporal;
mod value;
mod view;

pub use document::*;
pub use format::{DialectId, DialectIdRef, FormatId, FormatIdError, FormatIdRef};
pub use kind::ValueKind;
pub use materialize::MaterializeWorkspace;
pub use number::{
    BigInt, Decimal, DecimalText, Float, Integer, Number, NumberCategory, NumericError, decimal_parts_to_f64,
    format_binary64,
};
pub use temporal::{
    FractionalSecond, KnownUtcOffset, LocalDate, LocalDateTime, LocalTime, OffsetDateTime, TemporalError, UtcOffset,
    civil_from_days, civil_from_epoch, days_from_civil, epoch_seconds_from_civil_parts, parse_rfc3339,
    temporal_to_epoch, try_epoch_seconds_from_civil_parts, write_epoch_rfc3339,
};
pub use value::{
    Array, Object, ObjectBuilder, ObjectEntry, ObjectKey, Shared, TagError, TagId, Value, ValueAllocationError,
    resolve_index,
};
pub use view::{
    ArrayIter, ArrayView, LocalDateTimeView, LocalTimeView, NumberView, ObjectEntryView, ObjectIter, ObjectView,
    OffsetDateTimeView, ScalarView, ValueView,
};
