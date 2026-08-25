//! JSONC codec options: the output profile (the input side's grammar is a request fact — the dialect the provider was
//! opened with — plus the resource leniency dial, so no decode options struct is needed).

use crate::encode::JsonEncodeOptions;

/// The JSONC output profile: which dialect of JSONC the encoder writes.
///
/// The two profiles differ in exactly the trailing-comma bit — the input side's dialect symmetry carried to the
/// output side. Both emit comment facts as `//` lines when the encoded document carries them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JsoncEncodeProfile {
    /// `jsonc.trailing-jqf@1`: a trailing comma before every closing delimiter. The CLI's default `--output-format
    /// jsonc` profile, matching the default input dialect's acceptance.
    #[default]
    Trailing,
    /// `jsonc.default-jqf@1`: strict JSON's comma law, with comment trivia. The edit lane's whole-document floor
    /// renders through this profile too.
    Default,
}

/// JSONC encoder options: the strict JSON style options plus the output profile, read out of the request by the JSONC
/// encoder factory.
#[derive(Clone, Copy, Debug, Default)]
pub struct JsoncEncodeOptions {
    /// The strict JSON render style (indent, `-r`, `-S`, `-a`, NUL guard).
    pub style: JsonEncodeOptions,
    /// Which JSONC output profile to write.
    pub profile: JsoncEncodeProfile,
}

impl JsoncEncodeOptions {
    /// Whether this profile writes trailing commas.
    #[must_use]
    pub const fn trailing_commas(self) -> bool {
        matches!(self.profile, JsoncEncodeProfile::Trailing)
    }
}
