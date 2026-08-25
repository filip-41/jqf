//! Records, at compile time, whether this binary was built against a PGO profile — and which one.
//!
//! A profile-guided binary is 20 % faster than a plain one and otherwise indistinguishable from it: same version, same
//! flags, same bytes on stdout. That is exactly the situation in which a benchmark number gets attributed to the wrong
//! build. The allocator already rides along on `--diagnostics` for this reason (`src/allocator.rs`); the build path
//! joins it here.
//!
//! The single fact this script exports is `JQF_PGO_PROFILE_ID`: empty for an ordinary `cargo build --release`, and
//! otherwise the identity string that `tools/jqf-pgo-build.sh` stamped on the profile it merged. Emptiness is the
//! authority for `build=plain`, so there is no second flag to keep consistent.
//!
//! The PGO fact is read from `CARGO_ENCODED_RUSTFLAGS` rather than from an environment variable of our own, because
//! `-Cprofile-use` is what actually makes the binary profile-guided. A caller who sets the flag by hand and forgets
//! `JQF_PGO_PROFILE` still gets `build=pgo`, with the profile's own file name standing in for the identity the script
//! would have supplied.

use std::env;
use std::path::Path;

/// `CARGO_ENCODED_RUSTFLAGS` separates flags with an ASCII unit separator, so a flag containing spaces (a profile path,
/// here) survives the round trip.
const FLAG_SEPARATOR: char = '\u{1f}';

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    println!("cargo::rerun-if-env-changed=JQF_PGO_PROFILE");

    let profile_id = match profile_use_path() {
        None => String::new(),
        Some(path) => env::var("JQF_PGO_PROFILE")
            .ok()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| profile_file_name(&path)),
    };
    println!("cargo::rustc-env=JQF_PGO_PROFILE_ID={profile_id}");
}

/// The `-Cprofile-use=` argument, if this compilation has one. Both spellings rustc accepts are handled: one joined
/// flag, and `-C` followed by its value.
fn profile_use_path() -> Option<String> {
    let encoded = env::var("CARGO_ENCODED_RUSTFLAGS").ok()?;
    let mut expecting_value = false;
    for flag in encoded.split(FLAG_SEPARATOR) {
        if expecting_value {
            expecting_value = false;
            if let Some(path) = flag.strip_prefix("profile-use=") {
                return Some(path.to_owned());
            }
            continue;
        }
        if flag == "-C" {
            expecting_value = true;
            continue;
        }
        if let Some(path) = flag.strip_prefix("-Cprofile-use=") {
            return Some(path.to_owned());
        }
    }
    None
}

/// The fallback identity for a hand-driven PGO build: the profile's file name, which at least distinguishes two
/// profiles from each other.
fn profile_file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| "unnamed".to_owned(), |name| name.to_string_lossy().into_owned())
}
