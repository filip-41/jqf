//! Encode-side YAML options: the target schema bound in a request.

use jqf_codec_core::{CodecError, CodecFailureKind};

/// The target schema a YAML encode binds (§4.8): the target failsafe/JSON/core schema is bound in normalized options.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum YamlTargetSchema {
    /// Failsafe schema: String/sequence/mapping core nodes only.
    Failsafe,
    /// JSON schema: adds the recognized Null/Bool/Integer/Float set.
    Json,
    /// Core schema: the same seven tags with core resolution.
    #[default]
    Core,
}

impl YamlTargetSchema {
    /// Reads the target schema from the request's normalized options.
    pub(crate) fn from_request_options(
        options: Option<&(dyn core::any::Any + Send + Sync)>,
    ) -> Result<Self, CodecError> {
        match options {
            None => Ok(Self::default()),
            Some(payload) => payload
                .downcast_ref::<Self>()
                .copied()
                .ok_or_else(|| CodecError::new(CodecFailureKind::RequirementMismatch)),
        }
    }
}

/// The YAML output profile.
///
/// The two canonical profiles answer a machine and are byte-frozen; the block profile answers a person and is what
/// `--output-format yaml` selects when no dialect is named.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum YamlProfile {
    /// `yaml.stream-canonical@1`: one `---\n` + `render_document` + `\n...\n` per item, empty byte stream for zero
    /// items.
    StreamCanonical,
    /// `yaml.single-document@1`: exactly one item, `render_document(root)` + `\n`, no markers.
    SingleDocument,
    /// `yaml.block@1`: block collections and plain scalars, `---` BETWEEN documents and no `...` terminator.
    Block,
}
