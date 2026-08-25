//! HTML codec options.

/// The HTML output profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HtmlProfile {
    /// `html.source@1`: a byte-faithful echo of the sealed source.
    Source,
    /// `html.document-serialize@1`: the pinned HTML serialization algorithm with exactly one UTF-8 BOM.
    DocumentSerialize,
}
