//! The per-document anchor index.
//!
//! §4.8: anchor names are not unique identities. Within one document, an alias resolves to the most recent preceding
//! anchor with the same name; a forward alias is invalid. The anchor index therefore maintains source-ordered binding
//! history and is reset at each document boundary rather than using one global-last map.
//!
//! The index lives IN the graph (per document); this module owns the resolution law and the error construction.

use jqf_codec_core::CodecError;
use jqf_source::ResolvedSource;

use crate::error;
use crate::graph::{NodeId, YamlGraph};

/// Resolves an alias name to its target node, raising the typed forward/undefined-alias error when the anchor has not
/// been seen yet in this document.
pub(crate) fn resolve(
    graph: &YamlGraph,
    name: u32,
    source: ResolvedSource<'_>,
    offset: usize,
) -> Result<NodeId, CodecError> {
    match graph.resolve_alias(name) {
        Some(node) => Ok(node),
        None => Err(error::invalid(source, offset, "alias", "found undefined alias")),
    }
}
