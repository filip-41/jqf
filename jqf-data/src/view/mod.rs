//! Borrowed views over one document node.
//!
//! Read a node without copying it into an owned [`crate::Value`]. Copy happens only when you materialize.

mod array;
mod object;
mod scalar;
mod value;

pub use array::{ArrayIter, ArrayView};
pub use object::{ObjectEntryView, ObjectIter, ObjectView};
pub use scalar::{LocalDateTimeView, LocalTimeView, NumberView, OffsetDateTimeView, ScalarView};
pub use value::ValueView;
