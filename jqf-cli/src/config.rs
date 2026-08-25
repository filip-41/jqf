//! The `.jqf.toml` config file: user preferences that default the CLI's own Tier-P flags, in the shape of `.gitconfig`
//! / `.rustfmt.toml`.
//!
//! # The tier split
//!
//! Every CLI flag carries a tier (see [`crate:args:FLAG_TIERS`]). Tier P (presentation/resource) may be defaulted from
//! a config file; Tier S (semantic) is argv-only and never config-readable. The config key table below contains ONLY
//! Tier P keys, and the classification test (`args:tests:every_flag_carries_a_tier`) fails the build when a flag is
//! added to the surface without a tier — an unclassified flag is never silently config-readable.
//!
//! # Precedence (highest wins, per key)
//!
//! 1. argv
//! 2. `--config PATH` — an explicit file, the only file read
//! 3. the nearest `.jqf.toml` walking up from the cwd (to the filesystem
//!    root — no VCS-root stopping; `--show-config` makes it visible)
//! 4. the global config: `~/Library/Application Support/jqf/.jqf.toml` on macOS (the platform-consistent location),
//!    `$XDG_CONFIG_HOME/jqf/.jqf.toml` (default `~/.config/jqf/`) elsewhere
//! 5. built-in defaults
//!
//! Only ONE discovery file is read — the nearest — and it is overlaid on the global file (the nearest wins per key).
//! `--config` replaces both.
//!
//! # Hermeticity
//!
//! `--no-config` and a non-empty `JQF_NO_CONFIG` disable config reading entirely. Every gate, harness, and differential
//! sets `JQF_NO_CONFIG`, so a developer's config cannot move a gate by construction (the same law colour got in 058
//! W2).
//!
//! # Sections
//!
//! `[defaults]` is this plan's section. `[query]` is RESERVED for the future project-artifact direction (PITCH's
//! portable query artifacts) and is never read. Neither section may read the other's keys. An unknown key or section
//! prints a warning on stderr and is ignored (visible, not fatal); a malformed file or a mistyped known key is a hard
//! usage error naming the file.

use std::path::{Path, PathBuf};

use crate::args::{CliFormat, FlagTier, parse_indent, parse_max_rss, parse_workers};
use crate::errors::CliFailure;

/// The config file name everywhere: discovery walking up, and the global file's name inside its directory. One
/// canonical name, one file (the `[defaults]`/`[query]` section split lives INSIDE it).
pub(crate) const CONFIG_FILE_NAME: &str = ".jqf.toml";

/// The hermeticity variable: present and non-empty disables config reading (the same "non-empty wins" law `NO_COLOR`
/// has).
const NO_CONFIG_VAR: &str = "JQF_NO_CONFIG";

/// Whether config reading is disabled by the environment.
pub(crate) fn disabled_by_env() -> bool {
    std::env::var_os(NO_CONFIG_VAR).is_some_and(|value| !value.is_empty())
}

/// The value type one config key accepts. Every key maps to an existing CLI flag row — no invented knobs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConfigValueKind {
    Bool,
    /// `indent = N` (−1..=7, the `--indent` range).
    Indent,
    /// `output-format = "json"|"ndjson"|"json-seq"` — the JSON family ONLY. A config naming any other output format is
    /// a config error: a non-JSON target changes the value model and stays argv-only (§2's border-case ruling).
    OutputFormat,
    /// A nonnegative byte count.
    U64,
    /// `max-rss = N|N%|0` (integer bytes or string, the `--max-rss` grammar).
    MaxRss,
    /// `workers = N|"auto"` (integer width or string).
    Workers,
    /// `mismatch-policy = "lenient"|"warn"|"strict"`.
    MismatchPolicy,
    /// `strictness = "error"|"warn"|"strict"|"lenient"` (; the lenient position is 's decode-leniency dial).
    Strictness,
}

/// The Tier P config keys. The test `every_config_key_is_tier_p` keeps this table and [`crate:args:FLAG_TIERS`] in
/// agreement, so a key can never silently become Tier S (or a new flag never appear here).
pub(crate) const CONFIG_KEYS: &[(&str, ConfigValueKind)] = &[
    // `color = true` records the preference but leaves the colour decision auto (`-C` is the only force-on); `false`
    // forces monochrome.
    ("color", ConfigValueKind::Bool),
    ("compact", ConfigValueKind::Bool), // alias of `compact-output` (the task's spelling)
    ("compact-output", ConfigValueKind::Bool),
    ("indent", ConfigValueKind::Indent),
    ("tab", ConfigValueKind::Bool),
    ("output-format", ConfigValueKind::OutputFormat),
    ("max-memory-bytes", ConfigValueKind::U64),
    ("max-rss", ConfigValueKind::MaxRss),
    ("max-spill-bytes", ConfigValueKind::U64),
    ("max-spill-disk-bytes", ConfigValueKind::U64),
    ("parallel", ConfigValueKind::Bool),
    ("workers", ConfigValueKind::Workers),
    ("diagnostics", ConfigValueKind::Bool),
    ("explain", ConfigValueKind::Bool),
    ("unbuffered", ConfigValueKind::Bool),
    ("mismatch-policy", ConfigValueKind::MismatchPolicy),
    ("strictness", ConfigValueKind::Strictness),
];

/// The merged configuration view: one field per configurable key, `None` when neither the config files nor argv spoke.
#[derive(Debug, Default)]
pub(crate) struct ConfigView {
    /// The files whose values were merged, lowest precedence first (global, then the nearest discovery file). `--config
    /// PATH` contributes exactly that one file.
    pub(crate) source_files: Vec<PathBuf>,
    /// `color = true` is recorded but leaves the colour decision AUTO — TTY detection and `NO_COLOR` still apply, and
    /// `-C` remains the only force-on. `color = false` forces it off (as `-M`).
    pub(crate) color: Option<bool>,
    /// The indent family (`indent`/`tab`/`compact`), resolved across both files and every key they carry. TOML tables
    /// iterate keys in ALPHABETICAL order here (no preserve-order feature), so when several family keys appear in one
    /// file, the alphabetically LAST one wins — `tab = true` beats `indent = 2`, and `indent = 3` beats `compact-output
    /// = true`. The overlay between files stays positional: the discovery file's value wins per key over the global
    /// file's. `tab = false` / `compact = false` clear the family, falling back to argv or the built-in default.
    pub(crate) indent: Option<jqf_codec_json::JsonIndent>,
    pub(crate) output_format: Option<CliFormat>,
    pub(crate) max_memory_bytes: Option<u64>,
    pub(crate) max_rss: Option<crate::rss::MaxRss>,
    pub(crate) max_spill_bytes: Option<u64>,
    pub(crate) max_spill_disk_bytes: Option<u64>,
    pub(crate) parallel: Option<bool>,
    pub(crate) workers: Option<jqf_runtime::records::WorkerRequest>,
    pub(crate) diagnostics: Option<bool>,
    pub(crate) explain: Option<bool>,
    pub(crate) unbuffered: Option<bool>,
    pub(crate) mismatch_policy: Option<jqf_resource::policy::MismatchPolicy>,
    pub(crate) strictness: Option<jqf_resource::policy::StrictnessPolicy>,
}

/// Resolves the merged configuration view. Errors (a missing explicit `--config` file, malformed TOML, a mistyped known
/// key) are hard usage failures; unknown keys and sections warn on stderr and are ignored.
///
/// argv is parsed TWICE per run (the catalog-less pre-pass, then the catalog pass for file-name detection), so this
/// function runs twice too; the warning latch below keeps its output once-per-process.
pub(crate) fn resolve(no_config: bool, explicit: Option<&Path>) -> Result<ConfigView, CliFailure> {
    let mut view = ConfigView::default();
    if no_config || disabled_by_env() {
        return Ok(view);
    }
    let mut warnings: Vec<String> = Vec::new();
    if let Some(path) = explicit {
        read_config_file(path, &mut view, &mut warnings)?;
        view.source_files.push(path.to_path_buf());
    } else {
        // The global file first (lowest precedence of the two files), then the nearest discovery file overlaid on it —
        // per-key, nearest wins.
        if let Some(global) = global_config_path()
            && global.is_file()
        {
            read_config_file(&global, &mut view, &mut warnings)?;
            view.source_files.push(global);
        }
        if let Some(nearest) = discover()? {
            read_config_file(&nearest, &mut view, &mut warnings)?;
            view.source_files.push(nearest);
        }
    }
    emit_warnings(&warnings);
    Ok(view)
}

/// Prints the config files' unknown-key/section warnings ONCE per process: both parse passes resolve the same files and
/// collect the same warnings, so printing inside [`resolve`] unguarded would double every line.
fn emit_warnings(warnings: &[String]) {
    static EMITTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if warnings.is_empty() || EMITTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    for warning in warnings {
        crate::eprint_line_buffered(&format!("jqf: warning: {warning}"));
    }
    crate::flush_stderr();
}

/// The nearest `.jqf.toml` walking up from the cwd to the filesystem root. Only the nearest file is read; nothing below
/// it in the tree contributes.
fn discover() -> Result<Option<PathBuf>, CliFailure> {
    let cwd = std::env::current_dir()
        .map_err(|error| CliFailure::from(format!("cannot determine the current directory: {error}")))?;
    let mut dir = cwd.as_path();
    loop {
        let candidate = dir.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return Ok(None),
        }
    }
}

/// The global config path. macOS uses `~/Library/Application Support` (the platform-consistent location, 's Tier-1
/// platform); everywhere else the XDG base directory (`$XDG_CONFIG_HOME/jqf`, default `~/.config/jqf`).
fn global_config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let home = PathBuf::from(home);
    #[cfg(target_os = "macos")]
    let base = home.join("Library").join("Application Support");
    #[cfg(not(target_os = "macos"))]
    let base = std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
    Some(base.join("jqf").join(CONFIG_FILE_NAME))
}

/// Reads and applies one config file: the `[defaults]` section feeds the view, `[query]` and unknown sections warn, and
/// every other top-level key is a warning too (the section split law: nothing may live outside a section).
fn read_config_file(path: &Path, view: &mut ConfigView, warnings: &mut Vec<String>) -> Result<(), CliFailure> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| CliFailure::from(format!("cannot read config file {}: {error}", path.display())))?;
    let value: toml::Value = toml::from_str(&text)
        .map_err(|error| CliFailure::from(format!("config file {}: invalid TOML: {error}", path.display())))?;
    let toml::Value::Table(table) = value else {
        return Err(CliFailure::from(format!(
            "config file {}: the top level must be a table",
            path.display()
        )));
    };
    for (section, section_value) in table {
        match section.as_str() {
            "defaults" => {
                let toml::Value::Table(defaults) = section_value else {
                    return Err(CliFailure::from(format!(
                        "config file {}: [defaults] must be a table",
                        path.display()
                    )));
                };
                for (key, key_value) in defaults {
                    apply_key(&key, &key_value, path, view, warnings)?;
                }
            }
            "query" => warnings.push(format!(
                "config file {}: [query] is reserved for the query-artifact \
                 direction and is ignored",
                path.display()
            )),
            other => {
                if section_value.is_table() {
                    warnings.push(format!(
                        "config file {}: unknown section [{other}] is ignored",
                        path.display()
                    ));
                } else {
                    warnings.push(format!(
                        "config file {}: key {other:?} is outside a section; \
                         put it in [defaults]",
                        path.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Applies one `[defaults]` key: a Tier P key is coerced into the view, a known Tier S flag warns and is ignored (never
/// takes effect), and an unknown key warns and is ignored.
fn apply_key(
    key: &str,
    value: &toml::Value,
    path: &Path,
    view: &mut ConfigView,
    warnings: &mut Vec<String>,
) -> Result<(), CliFailure> {
    if let Some((_, kind)) = CONFIG_KEYS.iter().find(|(name, _)| *name == key) {
        return apply_value(key, *kind, value, path, view);
    }
    if let Some((_, tier)) = crate::args::FLAG_TIERS.iter().find(|(name, _)| *name == key)
        && *tier == FlagTier::Semantic
    {
        warnings.push(format!(
            "config file {}: {key} is a semantic (argv-only) flag and is \
             ignored in [defaults]",
            path.display()
        ));
        return Ok(());
    }
    warnings.push(format!(
        "config file {}: unknown key {key:?} in [defaults] is ignored",
        path.display()
    ));
    Ok(())
}

/// Coerces one Tier P key's TOML value. A mistyped value is a hard error naming the file and key — a type error in the
/// user's own file is better loud than silently dropped.
#[allow(
    clippy::too_many_lines,
    reason = "one flat key table; every arm is a few lines of the same shape, and splitting it \
              would thread a dozen out-parameters through helpers"
)]
fn apply_value(
    key: &str,
    kind: ConfigValueKind,
    value: &toml::Value,
    path: &Path,
    view: &mut ConfigView,
) -> Result<(), CliFailure> {
    let bad = |expected: &str| {
        CliFailure::from(format!(
            "config file {}: {key} must be {expected}, got {}",
            path.display(),
            toml_type_name(value)
        ))
    };
    let wrap = |message: &str| CliFailure::from(format!("config file {}: {key}: {message}", path.display()));
    match kind {
        ConfigValueKind::Bool => {
            let toml::Value::Boolean(b) = value else {
                return Err(bad("a boolean"));
            };
            match key {
                "color" => view.color = Some(*b),
                // The indent family's OFF spellings clear the family — a project file can then undo the global file's
                // `tab = true`, falling back to argv or the built-in default. Within one file the alphabetical key
                // order decides which of several family keys speaks last (see `ConfigView:indent`).
                "tab" => {
                    view.indent = (*b).then_some(jqf_codec_json::JsonIndent::Tabs);
                }
                "compact" | "compact-output" => {
                    view.indent = (*b).then_some(jqf_codec_json::JsonIndent::Compact);
                }
                "parallel" => view.parallel = Some(*b),
                "diagnostics" => view.diagnostics = Some(*b),
                "explain" => view.explain = Some(*b),
                "unbuffered" => view.unbuffered = Some(*b),
                _ => {
                    // A Bool-kind key the arm table does not handle is a table/arm drift; never panic the process over
                    // a user config file.
                    return Err(wrap("unsupported key: no handler for a boolean"));
                }
            }
        }
        ConfigValueKind::Indent => {
            let toml::Value::Integer(n) = value else {
                return Err(bad("an integer between -1 and 7"));
            };
            view.indent = Some(parse_indent(&n.to_string()).map_err(|_| wrap("expected an integer between -1 and 7"))?);
        }
        ConfigValueKind::OutputFormat => {
            let toml::Value::String(spelling) = value else {
                return Err(bad("a string"));
            };
            // The §2 border-case ruling: output-format is presentation for JSON-family targets and semantic when it
            // changes the value model, so a config may name only the JSON family.
            view.output_format = Some(match spelling.as_str() {
                "json" => CliFormat::Json,
                "ndjson" => CliFormat::Ndjson,
                "json-seq" => CliFormat::JsonSeq,
                other => {
                    return Err(CliFailure::from(format!(
                        "config file {}: output-format may name only the JSON \
                         family (json, ndjson, json-seq); {other:?} changes \
                         the value model and must be given per invocation",
                        path.display()
                    )));
                }
            });
        }
        ConfigValueKind::U64 => {
            let toml::Value::Integer(n) = value else {
                return Err(bad("a nonnegative integer"));
            };
            let n = u64::try_from(*n).map_err(|_| wrap("expected a nonnegative integer"))?;
            match key {
                "max-memory-bytes" => view.max_memory_bytes = Some(n),
                "max-spill-bytes" => view.max_spill_bytes = Some(n),
                "max-spill-disk-bytes" => view.max_spill_disk_bytes = Some(n),
                _ => {
                    // A U64-kind key the arm table does not handle is a table/arm drift; never panic the process over a
                    // user config file.
                    return Err(wrap("unsupported key: no handler for a byte count"));
                }
            }
        }
        ConfigValueKind::MaxRss => {
            let text = match value {
                toml::Value::Integer(n) => n.to_string(),
                toml::Value::String(s) => s.clone(),
                _ => return Err(bad("a byte count, N%, or 0")),
            };
            view.max_rss = Some(parse_max_rss(&text).map_err(|_| wrap("expected N bytes, N%, or 0"))?);
        }
        ConfigValueKind::Workers => {
            let text = match value {
                toml::Value::Integer(n) => n.to_string(),
                toml::Value::String(s) => s.clone(),
                _ => return Err(bad("a worker count or \"auto\"")),
            };
            view.workers = Some(parse_workers(&text).map_err(|_| wrap("expected a worker count or \"auto\""))?);
        }
        ConfigValueKind::MismatchPolicy => {
            let toml::Value::String(policy) = value else {
                return Err(bad("lenient, warn, or strict"));
            };
            view.mismatch_policy = Some(match policy.as_str() {
                "lenient" => jqf_resource::policy::MismatchPolicy::Lenient,
                "warn" => jqf_resource::policy::MismatchPolicy::Warn,
                "strict" => jqf_resource::policy::MismatchPolicy::Strict,
                _ => return Err(wrap("expected lenient, warn, or strict")),
            });
        }
        ConfigValueKind::Strictness => {
            let toml::Value::String(policy) = value else {
                return Err(bad("error, warn, strict, or lenient"));
            };
            view.strictness = Some(match policy.as_str() {
                "error" => jqf_resource::policy::StrictnessPolicy::Error,
                "warn" => jqf_resource::policy::StrictnessPolicy::Warn,
                "strict" => jqf_resource::policy::StrictnessPolicy::Strict,
                "lenient" => jqf_resource::policy::StrictnessPolicy::Lenient,
                _ => return Err(wrap("expected error, warn, strict, or lenient")),
            });
        }
    }
    Ok(())
}

fn toml_type_name(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "a string",
        toml::Value::Integer(_) => "an integer",
        toml::Value::Float(_) => "a float",
        toml::Value::Boolean(_) => "a boolean",
        toml::Value::Datetime(_) => "a datetime",
        toml::Value::Array(_) => "an array",
        toml::Value::Table(_) => "a table",
    }
}

/// The origin of one reported configuration value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConfigOrigin {
    Argv,
    ConfigFile,
    BuiltIn,
}

/// One `--show-config` report row.
pub(crate) struct ConfigEntry {
    pub(crate) key: &'static str,
    pub(crate) value: String,
    pub(crate) origin: ConfigOrigin,
}

/// Renders the `--show-config` report: a `[defaults]` table, one line per configurable key with its origin, then the
/// files that were read. The lines are valid TOML (string values quoted; the origin rides a `#` comment), so the report
/// doubles as a config file.
pub(crate) fn render_report(entries: &[ConfigEntry], source_files: &[PathBuf]) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("# effective .jqf.toml configuration\n[defaults]\n");
    for entry in entries {
        let origin = match entry.origin {
            ConfigOrigin::Argv => "argv",
            ConfigOrigin::ConfigFile => "config file",
            ConfigOrigin::BuiltIn => "built-in default",
        };
        match toml_assignable(entry.key, &entry.value) {
            Some(value) => {
                let _ = writeln!(out, "{} = {value}  # {origin}", entry.key);
            }
            None => {
                // Effective defaults the parser has no spelling for (color auto, unlimited memory) stay visible as
                // comments so the report still feeds back as a config file.
                let _ = writeln!(out, "# {} = {}  # {origin}", entry.key, entry.value);
            }
        }
    }
    for path in source_files {
        let _ = writeln!(out, "# read from: {}", path.display());
    }
    out
}

/// Values the config parser cannot spell (the built-in color-auto and unlimited memory ceiling) are comments, not
/// assignments.
fn toml_assignable(key: &str, raw: &str) -> Option<String> {
    match (key, raw) {
        ("color", "auto") | ("max-memory-bytes", "unlimited") => None,
        _ => Some(toml_literal(raw)),
    }
}

/// A value that is already a TOML bool or integer stays bare; everything else is a quoted string so the report is valid
/// TOML.
fn toml_literal(raw: &str) -> String {
    if raw == "true" || raw == "false" || raw.parse::<i64>().is_ok() {
        return raw.to_owned();
    }
    let mut out = String::from("\"");
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classification law's config half: every config key is a Tier P flag (or a documented alias of one). A key
    /// that drifted to Tier S, or a new Tier P flag that never joined the config table, fails here.
    #[test]
    fn every_config_key_is_tier_p() {
        for (name, _) in CONFIG_KEYS {
            let tier = crate::args::FLAG_TIERS
                .iter()
                .find(|(flag, _)| *flag == *name)
                .map(|(_, tier)| *tier);
            match *name {
                // Documented aliases: `color` covers the -C/-M pair and `compact` covers --compact-output; their
                // targets are asserted below.
                "color" | "compact" => {}
                _ => assert_eq!(
                    tier,
                    Some(FlagTier::Presentation),
                    "config key {name} must classify as a Tier P flag"
                ),
            }
        }
        for target in ["color-output", "monochrome-output", "compact-output"] {
            assert_eq!(
                crate::args::FLAG_TIERS
                    .iter()
                    .find(|(flag, _)| *flag == target)
                    .map(|(_, tier)| *tier),
                Some(FlagTier::Presentation),
                "the {target} flag must be Tier P for its config alias to be valid"
            );
        }
    }

    /// The `--show-config` report lines are valid TOML (the report doubles as a config file), and the origin comment
    /// survives a round trip.
    #[test]
    fn report_lines_are_valid_toml() {
        let entries = [
            ConfigEntry {
                key: "color",
                value: "false".to_owned(),
                origin: ConfigOrigin::Argv,
            },
            ConfigEntry {
                key: "indent",
                value: "2".to_owned(),
                origin: ConfigOrigin::BuiltIn,
            },
            ConfigEntry {
                key: "workers",
                value: "auto".to_owned(),
                origin: ConfigOrigin::BuiltIn,
            },
        ];
        let report = render_report(&entries, &[PathBuf::from("/tmp/.jqf.toml")]);
        assert!(
            report.starts_with("# effective .jqf.toml configuration\n[defaults]\n"),
            "the report must open with a [defaults] table: {report}"
        );
        let parsed: toml::Value = toml::from_str(&report).expect("the report is valid TOML");
        let table = parsed
            .get("defaults")
            .and_then(toml::Value::as_table)
            .expect("a [defaults] table");
        assert_eq!(table["color"].as_bool(), Some(false));
        assert_eq!(table["indent"].as_integer(), Some(2));
        assert_eq!(table["workers"].as_str(), Some("auto"));
        assert!(report.contains("# read from: /tmp/.jqf.toml"));
    }
}
