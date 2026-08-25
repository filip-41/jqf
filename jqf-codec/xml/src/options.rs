//! XML codec options.

/// The XML output profile.
///
/// The two profiles answer different consumers: the source profile preserves
/// the authored byte stream, the deterministic profile answers a machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum XmlProfile {
    /// `xml.source@1`: echoes the retained source of an unchanged whole
    /// document. Interior bytes are reproduced exactly; trailing whitespace
    /// outside the document element is normalized to one newline. An edited
    /// document is serialized from the value projection.
    Source,
    /// `xml.jqf-deterministic@1`: a byte-deterministic, namespace-correct
    /// canonical rewrite (the §4.9 law), not W3C C-XML.
    Deterministic,
}
