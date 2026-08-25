//! Render request options and profile identities.
//!
//! One render request binds a base renderer (the selected `render.*@1` dialect), a width profile, and normalized
//! options. This module owns the OPTION surface — the composition law's named parts — not the renderers themselves.

/// The `render` format identity.
pub const FORMAT_ID: &str = "render";

/// Plain text renderer: one frame per untagged core scalar item.
pub const PLAIN_DIALECT_ID: &str = "render.plain@1";
/// GitHub-flavored Markdown table renderer.
pub const GFM_TABLE_DIALECT_ID: &str = "render.gfm-table@1";
/// HTML table fragment renderer.
pub const HTML_TABLE_DIALECT_ID: &str = "render.html-table@1";
/// ASCII grid table renderer.
pub const GRID_TABLE_DIALECT_ID: &str = "render.grid-table@1";
/// Tree renderer over any owned semantic value.
pub const TREE_DIALECT_ID: &str = "render.tree@1";
/// Terminal styled-span renderer (bytes = the control-safe text shape).
pub const TERMINAL_DIALECT_ID: &str = "render.terminal@1";
/// POSIX `sh` assignment renderer: one flattened `name=value` line per leaf.
pub const SHELL_DIALECT_ID: &str = "render.shell@1";
/// Plain-ASCII frequency-histogram renderer over arrays of numbers.
pub const HIST_DIALECT_ID: &str = "render.hist@1";

/// The default sample row cap for the sampled layout profile.
const DEFAULT_SAMPLE_ROWS: usize = 256;
/// The default rendered-cell-byte cap for the sampled layout profile.
pub(crate) const DEFAULT_SAMPLE_BYTES: usize = 1 << 20;

/// Which display-width profile the request binds for the ambiguous class.
///
/// Both profiles share ONE grapheme-aware width law; only the ambiguous-class answer differs. The pinned
/// `render.unicode17@1` table is a documented follow-on; these v1 profiles name the practical law without claiming
/// Unicode-17 exactness.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WidthProfile {
    /// Ambiguous-width characters display as width 1 (western terminals).
    #[default]
    Western,
    /// Ambiguous-width characters display as width 2 (CJK terminals).
    Cjk,
}

/// Whether a table renderer emits a header row.
///
/// GFM tables require `Present` (GFM has no headless table). HTML and grid accept either. `Present` consumes the
/// complete ordered label vector; the label vector is the table extraction's column identities, never inferred from a
/// first data row.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HeaderPolicy {
    /// Emit and sample a header row.
    #[default]
    Present,
    /// Emit no header row.
    Absent,
}

/// Which extraction shape a `render.terminal@1` request binds.
///
/// The terminal renderer shares its frame boundary with its selected Plain/Table/Tree text shape; the shape is explicit
/// per request, never inferred from the item.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalShape {
    /// One untagged core scalar with terminal-safe text.
    Plain,
    /// The grid table shape.
    Table,
    /// The tree shape.
    #[default]
    Tree,
}

/// One normalized render request's options.
///
/// The base renderer is the request's DIALECT; these options carry the rest of the composition law. `max_width` is the
/// sampled layout's positive maximum display-cell width per column; `0` disables wrapping entirely (cells keep their
/// natural width and never wrap). `sample_rows` bounds the sampled layout's prepass row count; rendered-cell bytes are
/// capped by the crate's default sampled-layout byte ceiling. Exceeding either is a typed cap failure that emits no
/// frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderEncodeOptions {
    /// The bound width profile.
    pub width: WidthProfile,
    /// Whether table renderers emit a header row.
    pub header: HeaderPolicy,
    /// The terminal renderer's extraction shape.
    pub terminal_shape: TerminalShape,
    /// Maximum display-cell width per column; `0` disables wrapping.
    pub max_width: usize,
    /// Sampled layout row cap.
    pub sample_rows: usize,
    /// The path separator of the shell renderer's flattening (`render.shell@1`): nested paths join with this text,
    /// default `_`. A typed option on the request, never a CLI dial.
    pub shell_separator: &'static str,
}

impl Default for RenderEncodeOptions {
    fn default() -> Self {
        Self {
            width: WidthProfile::Western,
            header: HeaderPolicy::Present,
            terminal_shape: TerminalShape::Tree,
            max_width: 0,
            sample_rows: DEFAULT_SAMPLE_ROWS,
            shell_separator: "_",
        }
    }
}
