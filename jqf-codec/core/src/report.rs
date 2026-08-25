//! Allocation-free successful access observations.
//!
//! [`AccessReport`] is sealed onto an opened session. Sibling: [`crate::access`].

use jqf_data::DiagnosticCoverage;

use crate::{AccessAdapter, PhysicalRouteReceipt};

/// Fixed successful access report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccessReport {
    diagnostics: DiagnosticCoverage,
    route: Option<PhysicalRouteReceipt>,
    adapter: AccessAdapter,
    consumed_offset: Option<u64>,
    open_ended: bool,
}

impl AccessReport {
    pub(crate) const fn new(diagnostics: DiagnosticCoverage, adapter: AccessAdapter) -> Self {
        Self {
            diagnostics,
            route: None,
            adapter,
            consumed_offset: None,
            open_ended: false,
        }
    }
    /// Records the exact source bytes the complete value consumed. Format- neutral: an offset into the retained source
    /// bytes, not a JSON-specific position. Set only by routes whose caller opted into partial-input consumption (e.g.
    /// decoding one of several adjacent texts); untouched by [`Self::seal`], so it survives session publication
    /// unchanged.
    #[must_use]
    pub(crate) const fn with_consumed_offset(mut self, consumed_offset: u64) -> Self {
        self.consumed_offset = Some(consumed_offset);
        self
    }
    /// Marks the value as one whose last token more input could EXTEND: see [`Self::open_ended`]. Untouched by
    /// [`Self::seal`], like the consumed offset it qualifies.
    #[must_use]
    pub(crate) const fn with_open_ended(mut self) -> Self {
        self.open_ended = true;
        self
    }
    pub(crate) fn seal(
        &mut self,
        diagnostics: DiagnosticCoverage,
        adapter: AccessAdapter,
        route: PhysicalRouteReceipt,
    ) {
        self.diagnostics = diagnostics;
        self.adapter = adapter;
        self.route = Some(route);
    }
    /// Successful diagnostic coverage retained by the product.
    #[must_use]
    pub const fn diagnostics(self) -> DiagnosticCoverage {
        self.diagnostics
    }
    /// Sealed physical route observation.
    #[must_use]
    pub const fn route(self) -> Option<PhysicalRouteReceipt> {
        self.route
    }
    /// Core adapter selected before opening.
    #[must_use]
    pub const fn adapter(self) -> AccessAdapter {
        self.adapter
    }
    /// Exact source bytes consumed by the complete value this report describes. Present exactly when the route that
    /// produced this report recorded one through `with_consumed_offset` — that setter owns when an offset exists, so
    /// this getter carries no route enumeration of its own. `None` otherwise.
    #[must_use]
    pub const fn consumed_offset(self) -> Option<u64> {
        self.consumed_offset
    }
    /// Whether the value's last token ends at the last byte the decoder was given and is not closed by a delimiter, so
    /// MORE INPUT COULD EXTEND IT into a different value (`1234` becomes `1234567`, `inf` becomes `infinity`). A
    /// decoder cannot tell "the input ended" from "this window's bytes ran out" — only the caller knows whether the
    /// source is still growing — so it reports the ambiguity here instead of resolving it. A caller over a
    /// still-growing source must discard such a value and re-decode once more bytes arrive; a caller at true end of
    /// input publishes it unchanged. False for a value a delimiter closed (`]`, `}`, a closing quote) and for every
    /// value ending before the last byte.
    #[must_use]
    pub const fn open_ended(self) -> bool {
        self.open_ended
    }
}
