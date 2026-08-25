//! Reading a container that is still a source span.
//!
//! A decoder may leave a subtree unbuilt and name the validated source text it occupies. Turning those bytes into a
//! value is a format question, so this crate only owns the seam ([`LazySpanMaterializer`]). The decoder that wrote the
//! span owns the read.
//!
//! Each materialize returns a fresh owned value. Nothing is written back onto the node, so a later update is not
//! sharing with a cache.

use jqf_resource::ResourceContext;

use super::{ContainerSpanKind, CountStep, CountVerdict, DataError, SliceRange};
use crate::Value;

/// Turn one validated container-span text into an owned value.
///
/// The decoder that wrote the span installs this. The text is the exact source extent, already proved well-formed.
/// Re-read it; do not re-decide validity.
pub trait LazySpanMaterializer: Sync {
    /// Read one complete container text as an owned value.
    ///
    /// # Errors
    ///
    /// Returns a [`DataError`] when the text cannot be read as one complete value of the document's format, or when the
    /// request ledger refuses the materialization.
    fn materialize_span(&self, text: &str, resources: &mut ResourceContext<'_>) -> Result<Value, DataError>;

    /// Read one complete container span as an owned value.
    ///
    /// Default treats the span as UTF-8 and calls [`materialize_span`](Self::materialize_span). Text formats implement
    /// that arm. A binary format implements this one.
    ///
    /// # Errors
    ///
    /// Returns a [`DataError`] when the span is not valid UTF-8 under the default, or when the span cannot be read as
    /// one complete value of the document's format.
    fn materialize_span_bytes(&self, bytes: &[u8], resources: &mut ResourceContext<'_>) -> Result<Value, DataError> {
        let text = core::str::from_utf8(bytes).map_err(|_| DataError::InvalidDocument)?;
        self.materialize_span(text, resources)
    }

    /// Counts one deferred container span's children under a count probe, without materializing leaves — the format's
    /// span-count leaf.
    ///
    /// The default FALLS BACK to materializing the span and counting the owned value's children — correct for every
    /// codec, and what codecs without a native span scan (or without a span skeleton at all) get for free. It goes
    /// through [`materialize_span_bytes`](Self::materialize_span_bytes), so a codec that implements only the byte arm
    /// gets the fallback too. When a strict-JSON codec override exists it replaces this path with a byte scan that
    /// never builds the leaves.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] only for a genuinely unreadable span or a refused materialization; a shape the count
    /// cannot prove is a [`CountVerdict::Decline`], never an error.
    fn count_span(
        &self,
        text: &str,
        container: ContainerSpanKind,
        range: Option<SliceRange>,
        probe: &[CountStep],
        resources: &mut ResourceContext<'_>,
    ) -> Result<CountVerdict, DataError> {
        let value = self.materialize_span_bytes(text.as_bytes(), resources)?;
        let _ = container;
        Ok(super::count::count_owned_container(&value, range, probe))
    }

    /// Counts one deferred container span under a collect-filter predicate — the filter-row twin of
    /// [`count_span`](Self::count_span): each item contributes 0 or 1 by the test [`CountTest::answer`] states over
    /// [`CountFilter::contributes`]' navigation law.
    ///
    /// The default FALLS BACK to materializing the span and evaluating the filter per owned element — correct for
    /// every codec. When a strict-JSON codec override exists it replaces this path with a byte scan that classifies
    /// only the tested member per element.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] only for a genuinely unreadable span or a refused materialization; a shape the leaf cannot
    /// prove is a [`CountVerdict::Decline`], never an error.
    fn count_span_filtered(
        &self,
        text: &str,
        container: ContainerSpanKind,
        range: Option<SliceRange>,
        filter: &super::count::CountFilter,
        resources: &mut ResourceContext<'_>,
    ) -> Result<CountVerdict, DataError> {
        let value = self.materialize_span_bytes(text.as_bytes(), resources)?;
        let _ = container;
        Ok(super::count::count_owned_container_filtered(&value, range, filter))
    }

    /// Visits one deferred container span's elements — the format's element-iteration leaf: for each element,
    /// navigates the demand's probe over a materialized owned element and hands the value to `visit` — the
    /// span-iteration half of the document-core [`crate::Document::visit_elements`] consumer.
    ///
    /// The default FALLS BACK to materializing the whole span and iterating the owned container's elements — correct
    /// for every codec, and what codecs without a native span scan (or without a span skeleton at all) get for free. It
    /// goes through [`materialize_span_bytes`](Self::materialize_span_bytes), so a codec that implements only the byte
    /// arm gets the fallback too. When a strict-JSON codec override exists it replaces this path with a batched element
    /// re-parse that never builds the whole container's tree.
    ///
    /// The contract mirrors [`crate::Document::visit_elements`]: for an [`crate::ElementRow::FanOut`] demand the
    /// visitor must run for EVERY element or for NONE (the leaf pre-passes provability), and a
    /// [`crate::ElementRow::ReduceFold`] decline may interrupt the iteration (nothing has been published by the
    /// caller).
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] only for a genuinely unreadable span, a refused materialization, or a visitor failure; a
    /// shape the leaf cannot prove is an [`crate::ElementVerdict::Decline`], never an error.
    fn visit_span_elements(
        &self,
        text: &str,
        container: ContainerSpanKind,
        demand: &crate::ElementDemand,
        resources: &mut ResourceContext<'_>,
        visit: &mut dyn FnMut(&Value, &mut ResourceContext<'_>) -> Result<(), DataError>,
    ) -> Result<crate::ElementVerdict, DataError> {
        let value = self.materialize_span_bytes(text.as_bytes(), resources)?;
        let _ = container;
        super::element::visit_owned_container(&value, demand, resources, visit)
    }
}
