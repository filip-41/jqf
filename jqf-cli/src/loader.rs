//! The host-state snapshot and module search chain: the environment, working directory, and the adopted `-L` loader
//! with its default library dirs. Everything here turns process state into what the engine's builtins and
//! `import`/`include` see; no route decision happens in this module.

use std::env;
use std::fs;

use jqf_resource::EnvironmentSnapshot;

use crate::errors::CliFailure;

/// The host-state snapshot the process builtins read: the environment, the working directory, and the module search
/// list.
///
/// the adopted `env` passes raw bytes through; jqf strings are UTF-8, so a non-UTF-8 variable name or value is dropped
/// here (catalogued divergence in the parity memo). The search list starts at the adopted literal defaults; the module
/// campaign's `-L` flag extends it.
pub(crate) fn host_environment() -> Result<EnvironmentSnapshot, CliFailure> {
    // The environment is host state, but its collection is an allocation that must refuse cleanly under a tight ceiling
    // (125 H2: the request infrastructure is not exempt from the typed-refusal law). std's own `env:vars_os` builds its
    // Vec with infallible pushes, so the environment is read here directly from `environ` with a fallible reserve per
    // entry; a refused reserve means the allocator tripped the ceiling, and the typed ceiling refusal (with the live
    // ceiling/current numbers) surfaces rather than a bare allocation failure.
    let mut vars = Vec::new();
    collect_environment(&mut vars)?;
    let cwd = env::current_dir().ok().map(|path| path.to_string_lossy().into_owned());
    let jq_origin = env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.to_string_lossy().into_owned()));
    let search_list = ["~/.jq", "$ORIGIN/../lib/jq", "$ORIGIN/../lib"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    Ok(EnvironmentSnapshot::new(vars, cwd, search_list, jq_origin))
}

/// The environment-collection refusal: a refused reserve means the allocator tripped the ceiling, so the typed ceiling
/// refusal (with the live ceiling/current numbers) surfaces rather than a bare allocation failure. The unused parameter
/// keeps the signature uniform across the two reserve error shapes this file maps.
fn environment_refusal(_: impl core::fmt::Debug) -> CliFailure {
    let error = jqf_resource::cooperative_refusal()
        .err()
        .unwrap_or(jqf_resource::ResourceError::AllocationFailed);
    CliFailure::Codec {
        kind: jqf_codec_core::CodecFailureKind::Resource(error),
        diagnostic: None,
    }
}

/// Reads the process environment into `vars` with a fallible reserve per entry, so a tight ceiling refuses at the
/// collection instead of aborting.
///
/// Unix reads the `environ` symbol directly — std's `env:vars_os` builds its own Vec with infallible pushes, which is
/// exactly the abort this path must not take. Non-UTF-8 names and values are dropped (the existing law). On non-Unix
/// platforms the std iterator stands in; the ceiling there is a best-effort (Tier 2).
#[cfg(unix)]
fn collect_environment(vars: &mut Vec<(String, String)>) -> Result<(), CliFailure> {
    use std::os::raw::c_char;
    unsafe extern "C" {
        /// The process environment: a NUL-terminated array of NUL-terminated `KEY=VALUE` strings, stable for the
        /// process lifetime.
        static environ: *const *const c_char;
    }
    // SAFETY: `environ` is the process environment, read exactly as std's `env:vars_os` reads it (the same symbol, the
    // same iteration shape). Each entry is a NUL-terminated string; the cursor walks the array until the terminal null
    // pointer, which the OS guarantees.
    unsafe {
        if !environ.is_null() {
            let mut cursor = environ;
            while !(*cursor).is_null() {
                let entry = std::ffi::CStr::from_ptr(*cursor).to_bytes();
                if let Some((key, value)) = parse_env_entry(entry)? {
                    vars.try_reserve(1).map_err(environment_refusal)?;
                    vars.push((key, value));
                }
                cursor = cursor.add(1);
            }
        }
    }
    Ok(())
}

/// Splits one `KEY=VALUE` environment entry, dropping malformed and non-UTF-8 entries. The equals-sign search starts
/// after the first byte, exactly as std's own parse does (a variable name must not be empty). The per-entry `String`
/// copies reserve fallibly so a single entry past the remaining budget refuses instead of aborting (125 H2: a single
/// allocation larger than the remaining budget is the audited `try_reserve` class).
#[cfg(unix)]
fn parse_env_entry(input: &[u8]) -> Result<Option<(String, String)>, CliFailure> {
    if input.is_empty() {
        return Ok(None);
    }
    let Some(pos) = input[1..].iter().position(|&byte| byte == b'=').map(|p| p + 1) else {
        return Ok(None);
    };
    let (Ok(key_text), Ok(value_text)) = (
        std::str::from_utf8(&input[..pos]),
        std::str::from_utf8(&input[pos + 1..]),
    ) else {
        // the adopted `env` passes raw bytes through; jqf strings are UTF-8, so a non-UTF-8 name or value is dropped
        // here (catalogued divergence in the parity memo).
        return Ok(None);
    };
    let mut key = String::new();
    key.try_reserve(key_text.len()).map_err(environment_refusal)?;
    key.push_str(key_text);
    let mut value = String::new();
    value.try_reserve(value_text.len()).map_err(environment_refusal)?;
    value.push_str(value_text);
    Ok(Some((key, value)))
}

/// The non-Unix (Tier 2) environment read: the std iterator, whose own Vec is infallible. The ceiling is best-effort
/// there (the counting allocator still aborts on a crossing inside std's collection).
#[cfg(not(unix))]
fn collect_environment(vars: &mut Vec<(String, String)>) -> Result<(), CliFailure> {
    for (name, value) in env::vars_os() {
        let Ok(name) = name.into_string() else {
            continue;
        };
        let Ok(value) = value.into_string() else {
            continue;
        };
        vars.try_reserve(1)
            .map_err(|_| CliFailure::from("cannot collect the environment for the request"))?;
        vars.push((name, value));
    }
    Ok(())
}

/// The module loader the adopted `-L` search chain implements: authored `search` metadata (expanded relative to the
/// importing module's directory), the default chain (`"."` + the library dirs), and the `.jq`/`.json` suffix law.
pub(crate) struct CliModuleLoader {
    /// The `-L`/`--library-path` directories plus the adopted default library dirs.
    lib_dirs: Vec<String>,
    cwd: String,
    jq_origin: String,
}

impl CliModuleLoader {
    pub(crate) fn new(library_paths: Vec<std::path::PathBuf>) -> Self {
        let cwd = env::current_dir()
            .ok()
            .map_or_else(|| String::from("."), |path| path.to_string_lossy().into_owned());
        let jq_origin = env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|parent| parent.to_string_lossy().into_owned()))
            .unwrap_or_else(|| String::from("."));
        let home = env::var_os("HOME").map_or_else(|| String::from("~"), |home| home.to_string_lossy().into_owned());
        // `-L` dirs must be ABSOLUTE: the adopted search chain prefixes every relative entry with the importing
        // module's origin, so a relative `-L` dir would silently resolve inside every module directory.
        let cwd_for_paths = cwd.clone();
        let mut lib_dirs = library_paths
            .into_iter()
            .map(|path| {
                if path.is_absolute() {
                    path.to_string_lossy().into_owned()
                } else {
                    format!("{cwd_for_paths}/{}", path.to_string_lossy())
                }
            })
            .collect::<Vec<_>>();
        lib_dirs.push(format!("{home}/.jq"));
        lib_dirs.push(format!("{jq_origin}/../lib/jq"));
        lib_dirs.push(format!("{jq_origin}/../lib"));
        Self {
            lib_dirs,
            cwd,
            jq_origin,
        }
    }

    /// One search-chain entry, expanded: `.` → cwd, `$ORIGIN/…` → the binary's directory, and a relative entry → the
    /// importing module's directory.
    fn expand(&self, entry: &str, lib_origin: Option<&str>) -> String {
        if entry == "." {
            return self.cwd.clone();
        }
        if let Some(rest) = entry.strip_prefix("$ORIGIN/") {
            return format!("{}/{}", self.jq_origin, rest);
        }
        if !entry.starts_with('/')
            && let Some(origin) = lib_origin
        {
            return format!("{origin}/{entry}");
        }
        entry.to_owned()
    }
}

impl jqf_engine::ModuleLoader for CliModuleLoader {
    fn resolve(
        &self,
        relpath: &str,
        search: Option<&[String]>,
        lib_origin: Option<&str>,
        is_data: bool,
    ) -> Option<jqf_engine::LoadedModule> {
        let suffix = if is_data { ".json" } else { ".jq" };
        let entries: Vec<String> = if let Some(authored) = search {
            authored.to_vec()
        } else {
            let mut defaults = vec![String::from(".")];
            defaults.extend(self.lib_dirs.iter().cloned());
            defaults
        };
        let basename = relpath.rsplit('/').next().unwrap_or(relpath);
        for entry in entries {
            let dir = self.expand(&entry, lib_origin);
            let candidates = [
                format!("{dir}/{relpath}{suffix}"),
                format!("{dir}/{relpath}/jq/main{suffix}"),
                format!("{dir}/{relpath}/{basename}{suffix}"),
            ];
            for candidate in candidates {
                if let Ok(text) = fs::read_to_string(&candidate) {
                    // jq realpath-resolves the module it opened; so do we
                    //. Fall back to the search-chain spelling if
                    // canonicalize fails — a candidate that just read has almost certainly vanished, and the fallback
                    // keeps the request alive (the same graceful degradation as `resolve_destination` in output.rs).
                    // BOTH fields derive from the canonical path: `label` is `$__loc__`'s file and the cycle identity,
                    // and `dir` is the origin nested imports resolve relative search paths against.
                    let canonical = fs::canonicalize(&candidate)
                        .map_or_else(|_| candidate.clone(), |path| path.to_string_lossy().into_owned());
                    let parent = canonical
                        .rsplit_once('/')
                        .map_or_else(|| dir.clone(), |(dir, _)| dir.to_owned());
                    return Some(jqf_engine::LoadedModule {
                        text,
                        label: canonical,
                        dir: parent,
                    });
                }
            }
        }
        None
    }
}
