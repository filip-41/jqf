//! Encode projection for scalars a target does not spell natively.
//!
//! A codec declares [`NativeSpellings`]. Everything it does not name is projected here, and the write records an event
//! so projection is never silent. Events are not attributed to a source format.
//!
//! - Temporals render as RFC 3339 text through `jqf-data`.
//! - Tagged values publish their payload as-is (RFC 8949).
//! - Byte strings render as unpadded base64url (RFC 8949 §6.5).

use crate::{CodecError, CodecFailureKind, EncodeItem};
use alloc::string::String;
use alloc::vec::Vec;
use jqf_data::{ScalarView, TagId, Value};
use jqf_resource::ResourceContext;
use jqf_resource::policy::ProjectionKind;

/// One scalar kind a target may or may not spell natively.
///
/// Deliberately finer than [`ProjectionKind`]: `jqf-codec-delimited` spells a bare date natively but not a time, and
/// `jqf-codec-cbor` spells an offset date-time as tag 0 but has no spelling for a local one. A per-kind declaration
/// lets each of those keep exactly the native path it has.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ProjectableScalar {
    /// A bare calendar date.
    LocalDate,
    /// A bare wall-clock time.
    LocalTime,
    /// A date and time with no offset.
    LocalDateTime,
    /// A date and time with a UTC offset.
    OffsetDateTime,
    /// A byte string.
    Bytes,
}

impl ProjectableScalar {
    /// The event kind a projection of this scalar records.
    #[must_use]
    pub const fn projection_kind(self) -> ProjectionKind {
        match self {
            Self::LocalDate | Self::LocalTime | Self::LocalDateTime | Self::OffsetDateTime => ProjectionKind::Temporal,
            Self::Bytes => ProjectionKind::Bytes,
        }
    }

    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// A target's declaration of which projectable scalars it spells NATIVELY.
///
/// Everything a target does not name here is projected. The declaration is the codec's only say in the policy, and it
/// is a statement about the FORMAT ("TOML has a date-time literal"), never about preference.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativeSpellings(u8);

impl NativeSpellings {
    /// A target with no native spelling for any projectable scalar (JSON).
    pub const NONE: Self = Self(0);

    /// Adds one natively spelled scalar.
    #[must_use]
    pub const fn with(self, scalar: ProjectableScalar) -> Self {
        Self(self.0 | scalar.bit())
    }

    /// Whether the target spells `scalar` itself.
    #[must_use]
    pub const fn spells(self, scalar: ProjectableScalar) -> bool {
        self.0 & scalar.bit() != 0
    }
}

/// Where a projection's canonical text is written.
///
/// Implemented over the encoder's OWN accounted output buffer, which is what keeps the layer allocation-free for byte
/// strings: base64url is streamed in small groups rather than materialized whole.
pub trait ProjectionSink {
    /// Appends projected text to the target's output.
    ///
    /// The text is always escape-free in every target this tree encodes: RFC 3339 text draws digits plus `:.+-` (its
    /// `T`/`Z` markers are plain letters), unpadded base64url draws `[0-9A-Za-z_-]`, and none of those characters is
    /// escaped inside a string by any of these formats. `projected_text_is_escape_free` pins that.
    fn push_projected(&mut self, text: &str, resources: &ResourceContext<'_>) -> Result<(), CodecError>;
}

/// A [`ProjectionSink`] over an encoder's own output buffer.
///
/// Most encoders stage their bytes in a `Vec<u8>`, so this adapter is what they use: projected text lands in the same
/// buffer as the bytes around it. An encoder whose target needs projected text QUOTED first writes its own
/// [`ProjectionSink`] over private scratch instead — yaml's block encoder stages into a `String` (`ScratchSink`,
/// `jqf-codec-yaml/src/block.rs`) so its own quoting runs over the result afterward.
pub struct TrackedProjectionSink<'buffer>(&'buffer mut Vec<u8>);

impl<'buffer> TrackedProjectionSink<'buffer> {
    /// Wraps one output buffer for the duration of a projection.
    #[must_use]
    pub fn new(buffer: &'buffer mut Vec<u8>) -> Self {
        Self(buffer)
    }
}

impl ProjectionSink for TrackedProjectionSink<'_> {
    fn push_projected(&mut self, text: &str, _resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        // Keep the growth on the fallible path: projected text is unbounded in the document's own size, so a refused
        // reservation must surface as a codec failure rather than abort the process.
        self.0
            .try_reserve(text.len())
            .map_err(jqf_resource::ResourceError::from)?;
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

/// One scalar the target has no native spelling for, ready to be written as ordinary target TEXT.
///
/// Classification is separate from writing because an encoder has to open its own string quoting before the text and
/// close it after.
#[derive(Clone, Copy, Debug)]
pub struct ScalarProjection<'value> {
    source: ProjectedSource<'value>,
}

#[derive(Clone, Copy, Debug)]
enum ProjectedSource<'value> {
    LocalDate(&'value jqf_data::LocalDate),
    LocalTime(jqf_data::LocalTimeView<'value>),
    LocalDateTime(jqf_data::LocalDateTimeView<'value>),
    OffsetDateTime(jqf_data::OffsetDateTimeView<'value>),
    Bytes(&'value [u8]),
}

impl ScalarProjection<'_> {
    /// Which scalar this projection renders.
    #[must_use]
    pub const fn scalar(&self) -> ProjectableScalar {
        match self.source {
            ProjectedSource::LocalDate(_) => ProjectableScalar::LocalDate,
            ProjectedSource::LocalTime(_) => ProjectableScalar::LocalTime,
            ProjectedSource::LocalDateTime(_) => ProjectableScalar::LocalDateTime,
            ProjectedSource::OffsetDateTime(_) => ProjectableScalar::OffsetDateTime,
            ProjectedSource::Bytes(_) => ProjectableScalar::Bytes,
        }
    }

    /// The event kind writing this projection records.
    #[must_use]
    pub const fn projection_kind(&self) -> ProjectionKind {
        self.scalar().projection_kind()
    }

    /// Writes the canonical projected text and RECORDS the run's event.
    ///
    /// The two are one call on purpose: a codec cannot project without the run learning that it did.
    pub fn write<S: ProjectionSink>(&self, sink: &mut S, resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        match self.source {
            ProjectedSource::LocalDate(date) => {
                let mut text = String::new();
                date.write_text(&mut text).map_err(|_| allocation())?;
                sink.push_projected(&text, resources)?;
            }
            ProjectedSource::LocalTime(time) => {
                let mut text = String::new();
                time.write_text(&mut text).map_err(|_| allocation())?;
                sink.push_projected(&text, resources)?;
            }
            ProjectedSource::LocalDateTime(datetime) => {
                let mut text = String::new();
                datetime.write_text(&mut text).map_err(|_| allocation())?;
                sink.push_projected(&text, resources)?;
            }
            ProjectedSource::OffsetDateTime(datetime) => {
                let mut text = String::new();
                datetime.write_text(&mut text).map_err(|_| allocation())?;
                sink.push_projected(&text, resources)?;
            }
            ProjectedSource::Bytes(bytes) => write_base64url(bytes, sink, resources)?,
        }
        // Under `--types-as-strings` the cast is the request's explicit intent, so the projection records no event —
        // no warning, no diag.
        if !resources.types_as_strings() {
            resources.note_projection(self.projection_kind());
        }
        Ok(())
    }
}

/// Classifies one scalar against a target's native spellings.
///
/// `None` means the target encodes the scalar itself — either an ordinary null/bool/number/string, or a kind the
/// target named in `native`.
#[must_use]
pub fn classify_scalar<'value>(
    scalar: &ScalarView<'value>,
    native: NativeSpellings,
    resources: &ResourceContext<'_>,
) -> Option<ScalarProjection<'value>> {
    let source = match *scalar {
        ScalarView::LocalDate(date) => ProjectedSource::LocalDate(date),
        ScalarView::LocalTime(time) => ProjectedSource::LocalTime(time),
        ScalarView::LocalDateTime(datetime) => ProjectedSource::LocalDateTime(datetime),
        ScalarView::OffsetDateTime(datetime) => ProjectedSource::OffsetDateTime(datetime),
        ScalarView::Bytes(bytes) => ProjectedSource::Bytes(bytes),
        ScalarView::Null | ScalarView::Bool(_) | ScalarView::Number(_) | ScalarView::String(_) => {
            return None;
        }
    };
    let projection = ScalarProjection { source };
    // Under `--types-as-strings` a temporal ALWAYS reads as its plain text, so it projects even where the target spells
    // the kind natively. Bytes are deliberately untouched: no agreed text encoding exists for them, so they keep their
    // native/projection law as authored.
    if native.spells(projection.scalar())
        && !(resources.types_as_strings() && projection.scalar() != ProjectableScalar::Bytes)
    {
        return None;
    }
    Some(projection)
}

/// The tag layer over one item about to be encoded.
#[derive(Clone, Copy, Debug)]
pub enum TagLayer<'item> {
    /// No intrinsic tag at all.
    Untagged,
    /// A resolved CORE tag (`!!str`, `!!map`): presentation over an ordinary value every target can already spell, so
    /// it is never projected.
    Core(&'item TagId),
    /// A non-core `Value::Tagged` layer. A target that spells tags emits it; a target that does not calls
    /// [`project_tag`] and encodes the payload.
    Tagged(&'item TagId),
}

/// Reads the tag layer over one encode item.
pub fn tag_layer<'item>(item: EncodeItem<'item, '_>) -> Result<TagLayer<'item>, CodecError> {
    match item {
        EncodeItem::Owned(value) => Ok(value_tag_layer(value)),
        EncodeItem::Located { product, node } => {
            let view = product.document().value_view(node).map_err(|_| representation())?;
            view_tag_layer(view)
        }
    }
}

/// Reads the tag layer over one OWNED value.
#[must_use]
pub fn value_tag_layer(value: &Value) -> TagLayer<'_> {
    match value {
        Value::Tagged { tag, .. } => TagLayer::Tagged(tag),
        _ => TagLayer::Untagged,
    }
}

/// Reads the tag layer over one LOCATED value view.
///
/// The located half of [`tag_layer`], exposed on its own because several encoders recurse over views rather than over
/// cursor items.
pub fn view_tag_layer<'document>(view: jqf_data::ValueView<'document, '_>) -> Result<TagLayer<'document>, CodecError> {
    let Some(semantics) = view.tag_semantics().map_err(|_| representation())? else {
        return Ok(TagLayer::Untagged);
    };
    let tag = view.tag().map_err(|_| representation())?.ok_or_else(representation)?;
    Ok(match semantics {
        jqf_data::IntrinsicTagSemantics::Tagged => TagLayer::Tagged(tag),
        jqf_data::IntrinsicTagSemantics::Core => TagLayer::Core(tag),
    })
}

/// Records that a target with no native tag spelling published a tagged value's payload as-is.
///
/// Navigation is already payload-transparent, so the caller's only duty after this is to encode the value it was going
/// to encode anyway. The event is kind-only (`ProjectionKind::Tag`): the specific identity is not recorded, so the
/// function takes no tag argument.
pub fn project_tag(resources: &ResourceContext<'_>) {
    resources.note_projection(ProjectionKind::Tag);
}

/// The base64url alphabet (RFC 4648 §5), used WITHOUT padding per RFC 8949 §6.5.
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Streams `bytes` as unpadded base64url text in bounded groups.
///
/// Chunked rather than materialized so that projecting a large byte string costs a fixed stack buffer instead of a
/// 4/3-sized untracked allocation.
fn write_base64url<S: ProjectionSink>(
    bytes: &[u8],
    sink: &mut S,
    resources: &ResourceContext<'_>,
) -> Result<(), CodecError> {
    /// Whole 3-byte groups per flush; 48 input bytes render as 64 output bytes.
    const GROUP_BYTES: usize = 48;
    let mut encoded = [0_u8; GROUP_BYTES / 3 * 4];
    for chunk in bytes.chunks(GROUP_BYTES) {
        let mut written = 0;
        for triple in chunk.chunks(3) {
            let first = u32::from(triple[0]);
            let second = triple.get(1).copied().map_or(0, u32::from);
            let third = triple.get(2).copied().map_or(0, u32::from);
            let packed = first << 16 | second << 8 | third;
            // A trailing 1- or 2-byte group emits 2 or 3 characters and no padding, which is RFC 8949 §6.5's spelling.
            let characters = triple.len() + 1;
            for index in 0..characters {
                let sextet = packed >> (18 - 6 * index) & 0x3f;
                encoded[written] = BASE64URL[sextet as usize];
                written += 1;
            }
        }
        let text = core::str::from_utf8(&encoded[..written]).map_err(|_| representation())?;
        sink.push_projected(text, resources)?;
    }
    Ok(())
}

fn allocation() -> CodecError {
    CodecError::new(CodecFailureKind::AllocationFailure)
}

fn representation() -> CodecError {
    CodecError::new(CodecFailureKind::UnsupportedRepresentation)
}

#[cfg(test)]
mod tests {
    use super::{BASE64URL, NativeSpellings, ProjectableScalar, ProjectionSink, classify_scalar, write_base64url};
    use crate::CodecError;
    use alloc::string::String;
    use jqf_data::ScalarView;
    use jqf_resource::ResourceContext;

    use crate::test_support::resources as context;

    #[derive(Default)]
    struct TextSink(String);

    impl ProjectionSink for TextSink {
        fn push_projected(&mut self, text: &str, _resources: &ResourceContext<'_>) -> Result<(), CodecError> {
            self.0.push_str(text);
            Ok(())
        }
    }

    fn base64url(bytes: &[u8]) -> String {
        let resources = context();
        let mut sink = TextSink::default();
        write_base64url(bytes, &mut sink, &resources).expect("base64url");
        sink.0
    }

    #[test]
    fn base64url_matches_rfc_4648_vectors_without_padding() {
        // RFC 4648 §10's vectors, with §5's alphabet and RFC 8949 §6.5's "no padding" rule: the `=` runs are absent,
        // nothing else moves.
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64url_uses_the_url_safe_alphabet() {
        // The two characters that separate base64url from base64: index 62 is `-` and index 63 is `_`, never `+` and
        // `/`.
        assert_eq!(base64url(&[0xff, 0xef, 0xbe]), "_---");
    }

    #[test]
    fn base64url_streams_across_its_group_boundary() {
        // Longer than one flush group, and not a multiple of it, so the group seam and the ragged tail are both
        // exercised.
        let bytes: alloc::vec::Vec<u8> = (0..=200_u8).collect();
        let streamed = base64url(&bytes);
        assert_eq!(streamed.len(), (bytes.len() * 8).div_ceil(6));
        assert!(streamed.starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4v"));
    }

    #[test]
    fn projected_text_is_escape_free() {
        // The sink contract: no target escapes any character a projection can produce, so an encoder may write
        // projected text between its own quotes with no escape scan. base64url's alphabet plus RFC 3339's punctuation
        // is the complete set.
        let alphabet = BASE64URL.iter().copied().chain(b"-:.TZ+".iter().copied());
        for character in alphabet {
            assert!(
                character.is_ascii_alphanumeric() || b"-_:.+TZ".contains(&character),
                "projected text may not contain {character:#x}"
            );
        }
    }

    #[test]
    fn a_target_that_spells_a_scalar_natively_declines_the_projection() {
        let native = NativeSpellings::NONE.with(ProjectableScalar::LocalDate);
        let date = jqf_data::LocalDate::new(1979, 5, 27).expect("valid date");
        let resources = context();
        assert!(classify_scalar(&ScalarView::LocalDate(&date), native, &resources).is_none());
        assert!(classify_scalar(&ScalarView::LocalDate(&date), NativeSpellings::NONE, &resources).is_some());
        // An ordinary scalar is never a projection, whatever the declaration.
        assert!(classify_scalar(&ScalarView::Bool(true), NativeSpellings::NONE, &resources).is_none());
        // Under the flag a temporal projects even when spelled natively.
        let flagged = context().with_types_as_strings(true);
        assert!(classify_scalar(&ScalarView::LocalDate(&date), native, &flagged).is_some());
    }
}
