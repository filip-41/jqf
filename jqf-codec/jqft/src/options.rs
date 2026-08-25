//! Codec options for the jqft family (the `jqft` text profile, the `jqfjson` JSON envelope, and the `jqfb` machine
//! image).
//!
//! Decode owns no grammar knobs: profile choice is the output dialect. Encode is parameterized by its profile.
//! `with_source` requests the retained-source emission surface; a run that cannot supply it is a typed error, never a
//! silently thinner file.

/// jqft encoder options.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct JqftEncodeOptions {
    /// Emit the retained source instead of the canonical form.
    pub with_source: bool,
}

/// jqfb encoder options: the retained-source chunk (SOUR).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct JqfbEncodeOptions {
    /// Request the retained-source chunk.
    pub with_source: bool,
}
