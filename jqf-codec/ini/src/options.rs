//! Sealed flat-config dialect identities and the grammar selector.
//!
//! Three format ids (`properties`, `ini`, `dotenv`) and their input/output dialect pairs. The grammar is sealed per
//! registration, never an option.

/// Stable `.properties` format identity text.
pub const FORMAT_ID: &str = "properties";
/// Stable INI format identity text.
pub const INI_FORMAT_ID: &str = "ini";
/// Stable dotenv format identity text.
pub const DOTENV_FORMAT_ID: &str = "dotenv";

/// The properties input dialect: logical lines, `#`/`!` comments, `\` continuation, and cooked escapes. Clause list:
/// the crate `CONTRACTS.md`.
pub const PROPERTIES_JDK_DIALECT_ID: &str = "properties.jdk@1";
/// The crate-defined INI dialect: a conservative intersection, never "INI conformance". Clause list: the crate
/// `CONTRACTS.md`.
pub const INI_JQF_STRICT_DIALECT_ID: &str = "ini.jqf-strict@1";
/// The crate-defined dotenv dialect: `export ` prefixes accepted and stripped, single quotes literal, double quotes
/// escaped, no `$VAR` interpolation. Clause list: the crate `CONTRACTS.md`.
pub const DOTENV_JQF_STRICT_DIALECT_ID: &str = "dotenv.jqf-strict@1";

/// The deterministic `.properties` output profile (the `<fmt>.jqf-1.0@1` namespace spelling).
pub const PROPERTIES_JQF_1_0_DIALECT_ID: &str = "properties.jqf-1.0@1";
/// The deterministic INI output profile.
pub const INI_JQF_1_0_DIALECT_ID: &str = "ini.jqf-1.0@1";
/// The deterministic dotenv output profile.
pub const DOTENV_JQF_1_0_DIALECT_ID: &str = "dotenv.jqf-1.0@1";

/// The three grammars one crate owns. The grammar is part of the executable semantics — `;` is a comment in INI and a
/// value byte in `.properties` — so it is sealed per registration, never an option dial.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Grammar {
    /// The `properties.jdk@1` logical-line grammar.
    Properties,
    /// The written `ini.jqf-strict@1` clause list.
    Ini,
    /// The written `dotenv.jqf-strict@1` clause list.
    Dotenv,
}

impl Grammar {
    /// The format identity this grammar normalizes to.
    #[must_use]
    pub const fn format_id(self) -> &'static str {
        match self {
            Self::Properties => FORMAT_ID,
            Self::Ini => INI_FORMAT_ID,
            Self::Dotenv => DOTENV_FORMAT_ID,
        }
    }

    /// The sealed input dialect identity this grammar normalizes to.
    #[must_use]
    pub const fn input_dialect_id(self) -> &'static str {
        match self {
            Self::Properties => PROPERTIES_JDK_DIALECT_ID,
            Self::Ini => INI_JQF_STRICT_DIALECT_ID,
            Self::Dotenv => DOTENV_JQF_STRICT_DIALECT_ID,
        }
    }

    /// The comment-fact identity this grammar attaches (the per-codec convention, `properties.comment@1` /
    /// `ini.comment@1`; dotenv follows the same family rule).
    #[must_use]
    pub const fn comment_fact(self) -> &'static str {
        match self {
            Self::Properties => "properties.comment@1",
            Self::Ini => "ini.comment@1",
            Self::Dotenv => "dotenv.comment@1",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grammar_identities_are_the_written_names() {
        assert_eq!(Grammar::Properties.format_id(), FORMAT_ID);
        assert_eq!(Grammar::Properties.input_dialect_id(), PROPERTIES_JDK_DIALECT_ID);
        assert_eq!(PROPERTIES_JQF_1_0_DIALECT_ID, "properties.jqf-1.0@1");
        assert_eq!(Grammar::Ini.input_dialect_id(), INI_JQF_STRICT_DIALECT_ID);
        assert_eq!(Grammar::Dotenv.input_dialect_id(), DOTENV_JQF_STRICT_DIALECT_ID);
    }
}
