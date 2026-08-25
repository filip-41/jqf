//! Source-independent codec descriptions: format id, dialects, CLI facts.
//!
//! Access slots live on the provider, not here. Sibling: [`crate::registration`].

use jqf_data::{DialectIdRef, FormatIdRef};

use crate::RouteCapability;

/// Who owns the byte between items in an output stream of one of a codec's dialects.
///
/// The facade suffix is the CLI's item terminator: it appends `\n` after every encoded item, except where the codec's
/// own item/document encoding already carries the byte (record formats, binary formats, and the document-shaped formats
/// whose edit renders retain their trailing byte). One value per dialect in [`CodecDescriptor::dialects`], aligned by
/// index; the CLI reads the value for the selected dialect's lane instead of re-declaring per-format match arms.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ItemByteOwner {
    /// The facade appends the inter-item byte after each encoded item.
    Facade,
    /// The codec's item/document encoding carries its own inter-item byte; the facade appends nothing.
    Codec,
}

/// Coarse operations which a registration can construct.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CodecOperations {
    decode: bool,
    encode: bool,
    validate_tags: bool,
}

impl CodecOperations {
    /// Creates an operation declaration.
    #[must_use]
    pub const fn new(decode: bool, encode: bool, validate_tags: bool) -> Self {
        Self {
            decode,
            encode,
            validate_tags,
        }
    }

    /// Whether decoder construction is available.
    #[must_use]
    pub const fn decode(self) -> bool {
        self.decode
    }

    /// Whether encoder construction is available.
    #[must_use]
    pub const fn encode(self) -> bool {
        self.encode
    }

    /// Whether target tag validation is available.
    #[must_use]
    pub const fn validate_tags(self) -> bool {
        self.validate_tags
    }
}

/// Allocation-free, source-independent description of one codec.
#[derive(Debug)]
pub struct CodecDescriptor<'registration> {
    format: FormatIdRef<'registration>,
    dialects: &'registration [DialectIdRef<'registration>],
    operations: CodecOperations,
    /// The CLI-facing routes this format's registrations can serve: a closed declaration the CLI consumes instead of
    /// re-declaring format route facts as `match` arms, so adding or retiring a lane is a change to the codec's own
    /// table rather than to every consumer of it.
    routes: &'registration [RouteCapability],
    /// Filename extensions this registration claims for implicit input-format detection. An extension selects a FORMAT,
    /// never a dialect: the resolving registration's own first (default input) dialect applies. Only the format's
    /// default-dialect registration declares extensions; a sibling registration declares the empty list, so two
    /// registrations claiming one extension is a registration bug the catalog's `detect_by_extension` surfaces as
    /// ambiguous. Filenames ([`Self::filenames`]) sit beside this list: a whole-file name with no extension (`.env`,
    /// `Makefile`) is claimed there, never here.
    extensions: &'registration [&'registration str],
    /// One [`ItemByteOwner`] per dialect in [`Self::dialects`], aligned by index. A registration whose input dialect
    /// and output profile share one descriptor declares both rows here: the INPUT dialect's document encoding retains
    /// its own trailing byte (the edit lane's empty suffix), while the OUTPUT profile's item stream has the facade
    /// supply the inter-item byte. The alignment is validated at registration construction.
    inter_item_byte: &'registration [ItemByteOwner],
    /// Exact filenames and filename globs this registration claims for implicit input-format detection, beside the
    /// extension list. A filename is the whole file name, not its extension: `.env` has no extension, so extension
    /// matching can never reach it. Two shapes share one list: an EXACT name (`".env"`, `"Makefile"`) and a GLOB ending
    /// in `*` whose star matches any (possibly empty) filename suffix — `.env.*` matches `.env.local` but not `.env`,
    /// so the three dotenv spellings are three exact registrations with one glob. A star elsewhere in a pattern is not
    /// a wildcard. Detection precedence is exact name, then glob, then extension, so `.env.local` resolves by glob
    /// before a `.local` extension claim is consulted. A filename selects a FORMAT, never a dialect, exactly as an
    /// extension does. The registration-ambiguity law is the same: two registrations claiming the same exact name, or
    /// globs that both match one filename, is a registration bug the catalog's `detect_by_filename` surfaces as
    /// ambiguous, never a silent winner.
    filenames: &'registration [&'registration str],
    /// The insignificant inter-value whitespace the sequence drives skip between this format's adjacent complete texts.
    /// Empty unless the format declares [`RouteCapability::AdjacentValues`] with trivia between texts. See
    /// [`Self::value_separators`].
    value_separators: &'registration [u8],
}

impl<'registration> CodecDescriptor<'registration> {
    /// Creates a descriptor over registration-owned identities.
    ///
    /// `inter_item_byte` carries one [`ItemByteOwner`] per dialect in `dialects`, aligned by index; the registration
    /// constructor validates the alignment.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "one flat declaration row: every field is an independent registration-owned fact, and a builder would hide the arity the alignment laws validate"
    )]
    pub const fn new(
        format: FormatIdRef<'registration>,
        dialects: &'registration [DialectIdRef<'registration>],
        operations: CodecOperations,
        routes: &'registration [RouteCapability],
        extensions: &'registration [&'registration str],
        inter_item_byte: &'registration [ItemByteOwner],
        filenames: &'registration [&'registration str],
        value_separators: &'registration [u8],
    ) -> Self {
        Self {
            format,
            dialects,
            operations,
            routes,
            extensions,
            inter_item_byte,
            filenames,
            value_separators,
        }
    }

    /// Codec format identity.
    #[must_use]
    pub const fn format(&self) -> FormatIdRef<'registration> {
        self.format
    }

    /// Supported dialect identities.
    #[must_use]
    pub const fn dialects(&self) -> &'registration [DialectIdRef<'registration>] {
        self.dialects
    }

    /// Coarse factory potential.
    #[must_use]
    pub const fn operations(&self) -> CodecOperations {
        self.operations
    }

    /// The CLI-facing routes this format can serve, from the same closed declaration the field above owns; consumers
    /// read this table instead of re-declaring per-format facts.
    #[must_use]
    pub const fn route_capabilities(&self) -> &'registration [RouteCapability] {
        self.routes
    }

    /// Filename extensions this registration claims for implicit input-format detection. The catalog resolves an
    /// extension to this registration's format; extensions never select a dialect. Exact filenames and filename globs
    /// with no extension are claimed through [`Self::filenames`] instead.
    #[must_use]
    pub const fn extensions(&self) -> &'registration [&'registration str] {
        self.extensions
    }

    /// The per-dialect inter-item byte ownership, aligned with [`Self::dialects`]. The CLI derives its facade suffix
    /// from the selected dialect's row instead of re-declaring a per-format match.
    #[must_use]
    pub const fn inter_item_byte(&self) -> &'registration [ItemByteOwner] {
        self.inter_item_byte
    }

    /// Exact filenames and filename globs this registration claims for implicit input-format detection. The catalog
    /// resolves a full filename to this registration's format by exact name, then glob, then extension; filenames never
    /// select a dialect. See the field's documentation for the two shapes and their precedence.
    #[must_use]
    pub const fn filenames(&self) -> &'registration [&'registration str] {
        self.filenames
    }

    /// The insignificant inter-value whitespace this format's adjacent-value stream may carry, skipped by the sequence
    /// drives between complete texts. Empty for formats whose grammar has none (every byte reaches the decoder). The
    /// CLI reads THIS declaration onto the request's [`crate::DecodeRequest::value_separator`] instead of re-declaring
    /// a per-format list: a codec whose adjacent-value contract assumes the drive skips its trivia (YAML's blank-line
    /// narrowing, jqft's `---`-separated documents) declares the set beside the [`RouteCapability::AdjacentValues`]
    /// capability that requires it.
    #[must_use]
    pub const fn value_separators(&self) -> &'registration [u8] {
        self.value_separators
    }
}
