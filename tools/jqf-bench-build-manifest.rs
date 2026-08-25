use std::{
    fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

include!(concat!(env!("OUT_DIR"), "/source_manifest.rs"));

pub(crate) fn verify() -> Result<(), String> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .map_err(|error| format!("canonicalize repository root: {error}"))?;
    verify_at(
        &repository,
        BUILD_SOURCE_ROOTS,
        BUILD_SOURCE_MANIFEST,
        BUILD_SOURCE_MODES,
    )
}

fn verify_at(
    repository: &Path,
    source_roots: &[&str],
    expected: &[(&str, &str)],
    expected_modes: &[u32],
) -> Result<(), String> {
    if expected_modes.len() != expected.len() {
        return Err(format!(
            "embedded source mode count {} does not match manifest file count {}",
            expected_modes.len(),
            expected.len()
        ));
    }
    let observed_paths = enumerate_source_paths(repository, source_roots)?;
    let expected_paths: Vec<_> = expected.iter().map(|(path, _)| *path).collect();
    if observed_paths != expected_paths {
        let mismatch = observed_paths
            .iter()
            .map(String::as_str)
            .zip(expected_paths.iter().copied())
            .position(|(observed, expected)| observed != expected)
            .unwrap_or_else(|| observed_paths.len().min(expected_paths.len()));
        return Err(format!(
            "source manifest path set changed after build at entry {mismatch}: expected {}, observed {}; rebuild and restart the complete baseline",
            expected_paths.get(mismatch).copied().unwrap_or("<end>"),
            observed_paths.get(mismatch).map_or("<end>", String::as_str),
        ));
    }

    let observed_modes: Vec<u32> = observed_paths
        .iter()
        .map(|path| source_file_mode(&repository.join(path)))
        .collect::<Result<_, _>>()?;
    for (index, (((path, _), expected_mode), observed_mode)) in
        expected.iter().zip(expected_modes).zip(&observed_modes).enumerate()
    {
        if expected_mode != observed_mode {
            return Err(format!(
                "source mode drift after build at manifest entry {index}: {path} expected {expected_mode:04o}, observed {observed_mode:04o}; rebuild and restart the complete baseline"
            ));
        }
    }

    let observed_hashes = git_hash_paths(repository, &observed_paths)?;
    for (index, ((path, expected_hash), observed_hash)) in expected.iter().zip(&observed_hashes).enumerate() {
        if expected_hash != observed_hash {
            return Err(format!(
                "source drift after build at manifest entry {index}: {path} expected {expected_hash}, observed {observed_hash}; rebuild and restart the complete baseline"
            ));
        }
    }
    Ok(())
}

fn enumerate_source_paths(repository: &Path, source_roots: &[&str]) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    for root in source_roots {
        validate_source_root(root)?;
        collect_files(&repository.join(root), &mut files)?;
    }
    let mut relative = files
        .into_iter()
        .map(|path| repository_relative_utf8(repository, &path))
        .collect::<Result<Vec<_>, _>>()?;
    relative.sort();
    relative.dedup();
    Ok(relative)
}

fn repository_relative_utf8(repository: &Path, path: &Path) -> Result<String, String> {
    path.strip_prefix(repository)
        .map_err(|error| format!("source path {} is outside repository: {error}", path.display()))?
        .to_str()
        .ok_or_else(|| format!("source path {} is not UTF-8", path.display()))
        .map(str::to_owned)
}

fn collect_files(path: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "source manifest path must not be a symlink: {}",
            path.display()
        ));
    }
    if metadata.is_file() {
        output.push(path.to_owned());
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(format!(
            "source manifest path is neither a file nor directory: {}",
            path.display()
        ));
    }
    let mut entries: Vec<_> = fs::read_dir(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| format!("read entry under {}: {error}", path.display()))
        })
        .collect::<Result<_, _>>()?;
    entries.sort();
    for entry in entries {
        if entry.file_name().is_some_and(|name| name == "target") {
            let metadata =
                fs::symlink_metadata(&entry).map_err(|error| format!("inspect {}: {error}", entry.display()))?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "source manifest path must not be a symlink: {}",
                    entry.display()
                ));
            }
            continue;
        }
        collect_files(&entry, output)?;
    }
    Ok(())
}

fn validate_source_root(root: &str) -> Result<(), String> {
    if root.is_empty()
        || root
            .split(['/', '\\'])
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
        || root
            .as_bytes()
            .get(1)
            .is_some_and(|byte| *byte == b':' && root.as_bytes()[0].is_ascii_alphabetic())
        || Path::new(root)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "source root must contain only strict relative normal components: {root:?}"
        ));
    }
    Ok(())
}

fn source_file_mode(path: &Path) -> Result<u32, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "source manifest path must not be a symlink: {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!("source manifest entry is not a file: {}", path.display()));
    }
    Ok(portable_mode(&metadata))
}

#[cfg(unix)]
fn portable_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;

    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn portable_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

fn git_hash_paths(repository: &Path, paths: &[String]) -> Result<Vec<String>, String> {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin-paths"])
        .current_dir(repository)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("spawn git hash-object: {error}"))?;
    {
        let stdin = child.stdin.as_mut().ok_or("git stdin unavailable")?;
        for path in paths {
            writeln!(stdin, "{path}").map_err(|error| format!("write git path: {error}"))?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for git hash-object: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git hash-object failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let observed: Vec<_> = String::from_utf8(output.stdout)
        .map_err(|error| format!("git hashes are not UTF-8: {error}"))?
        .lines()
        .map(str::to_owned)
        .collect();
    if observed.len() != paths.len() {
        return Err(format!(
            "git hash-object returned {} hashes for {} source paths",
            observed.len(),
            paths.len()
        ));
    }
    Ok(observed)
}

// Only jqf-codec-json-bench calls provenance(); jqf-data-bench includes this
// file with zero callers, so dead_code fires in that unit without the
// attribute.
#[allow(dead_code)]
pub(crate) fn provenance() -> String {
    format!(
        "source_manifest={} manifest_files={} build_compiler_hash={} build_target={:?} build_profile={:?} build_rustflags={:?} build_encoded_rustflags={:?} build_configured_target={:?} build_release_codegen_units={:?} git_head={}",
        BUILD_SOURCE_MANIFEST_ID,
        BUILD_SOURCE_MANIFEST.len(),
        compiler_hash(),
        BUILD_TARGET,
        BUILD_PROFILE,
        BUILD_RUSTFLAGS,
        BUILD_ENCODED_RUSTFLAGS,
        BUILD_CONFIGURED_TARGET,
        BUILD_RELEASE_CODEGEN_UNITS,
        BUILD_GIT_HEAD,
    )
}

pub(crate) fn compiler_hash() -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in BUILD_COMPILER.as_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub(crate) const fn identity() -> &'static str {
    BUILD_SOURCE_MANIFEST_ID
}
pub(crate) const fn entries() -> &'static [(&'static str, &'static str)] {
    BUILD_SOURCE_MANIFEST
}
pub(crate) const fn roots() -> &'static [&'static str] {
    BUILD_SOURCE_ROOTS
}
pub(crate) const fn modes() -> &'static [u32] {
    BUILD_SOURCE_MODES
}
pub(crate) const fn compiler() -> &'static str {
    BUILD_COMPILER
}
pub(crate) const fn git_head() -> &'static str {
    BUILD_GIT_HEAD
}
pub(crate) const fn target() -> &'static str {
    BUILD_TARGET
}
pub(crate) const fn profile() -> &'static str {
    BUILD_PROFILE
}
pub(crate) const fn rustflags() -> &'static str {
    BUILD_RUSTFLAGS
}
pub(crate) const fn encoded_rustflags() -> &'static str {
    BUILD_ENCODED_RUSTFLAGS
}
pub(crate) const fn configured_target() -> &'static str {
    BUILD_CONFIGURED_TARGET
}
pub(crate) const fn release_codegen_units() -> &'static str {
    BUILD_RELEASE_CODEGEN_UNITS
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{enumerate_source_paths, git_hash_paths, source_file_mode, validate_source_root, verify_at};
    // Only the unix test below exercises the non-UTF-8 source-name law, so the
    // import is gated with it (a Tier-2 Windows build must stay warning-clean).
    #[cfg(unix)]
    use super::repository_relative_utf8;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempRepository(PathBuf);

    impl TempRepository {
        fn new() -> Self {
            let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("jqf-source-manifest-{}-{nonce}", std::process::id()));
            fs::create_dir_all(path.join("src")).expect("create temporary source tree");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRepository {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove temporary source tree");
        }
    }

    fn snapshot(repository: &Path, roots: &[&str]) -> (Vec<(String, String)>, Vec<u32>) {
        let paths = enumerate_source_paths(repository, roots).expect("enumerate source paths");
        let modes = paths
            .iter()
            .map(|path| source_file_mode(&repository.join(path)).expect("read source mode"))
            .collect();
        let hashes = git_hash_paths(repository, &paths).expect("hash source paths");
        (paths.into_iter().zip(hashes).collect(), modes)
    }

    fn borrowed(snapshot: &[(String, String)]) -> Vec<(&str, &str)> {
        snapshot
            .iter()
            .map(|(path, hash)| (path.as_str(), hash.as_str()))
            .collect()
    }

    #[test]
    fn verification_detects_path_set_and_content_drift() {
        let repository = TempRepository::new();
        let roots = &["src"];
        fs::write(repository.path().join("src/original.rs"), "original").expect("write original source");
        let (snapshot, modes) = snapshot(repository.path(), roots);
        let expected = borrowed(&snapshot);
        verify_at(repository.path(), roots, &expected, &modes).expect("unchanged source verifies");

        fs::write(repository.path().join("src/added.rs"), "added").expect("add source");
        let error = verify_at(repository.path(), roots, &expected, &modes).expect_err("addition must fail");
        assert!(error.contains("path set changed"), "{error}");
        fs::remove_file(repository.path().join("src/added.rs")).expect("remove added source");

        fs::rename(
            repository.path().join("src/original.rs"),
            repository.path().join("src/renamed.rs"),
        )
        .expect("rename source");
        let error = verify_at(repository.path(), roots, &expected, &modes).expect_err("rename must fail");
        assert!(error.contains("path set changed"), "{error}");
        fs::rename(
            repository.path().join("src/renamed.rs"),
            repository.path().join("src/original.rs"),
        )
        .expect("restore source name");

        fs::remove_file(repository.path().join("src/original.rs")).expect("delete source");
        let error = verify_at(repository.path(), roots, &expected, &modes).expect_err("deletion must fail");
        assert!(error.contains("path set changed"), "{error}");
        fs::write(repository.path().join("src/original.rs"), "changed").expect("restore changed source");

        let error = verify_at(repository.path(), roots, &expected, &modes).expect_err("change must fail");
        assert!(error.contains("source drift after build"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn verification_detects_mode_only_drift() {
        use std::os::unix::fs::PermissionsExt as _;

        let repository = TempRepository::new();
        let roots = &["src"];
        let source = repository.path().join("src/original.rs");
        fs::write(&source, "unchanged bytes").expect("write source");
        let (snapshot, modes) = snapshot(repository.path(), roots);
        let expected = borrowed(&snapshot);

        let changed_mode = modes[0] ^ 0o100;
        fs::set_permissions(&source, fs::Permissions::from_mode(changed_mode)).expect("change source mode");
        let paths = enumerate_source_paths(repository.path(), roots).expect("re-enumerate source");
        let hashes = git_hash_paths(repository.path(), &paths).expect("rehash source");
        assert_eq!(hashes[0], snapshot[0].1, "source bytes must be unchanged");
        let error = verify_at(repository.path(), roots, &expected, &modes).expect_err("mode-only drift must fail");
        assert!(error.contains("source mode drift after build"), "{error}");
    }

    #[test]
    fn source_roots_must_be_strict_relative_normal_components() {
        for invalid in [
            "",
            ".",
            "./src",
            "src/./nested",
            "../src",
            "src/../other",
            "/src",
            "src/",
            "src//nested",
            "C:\\src",
        ] {
            let error = validate_source_root(invalid).expect_err("invalid source root must fail");
            assert!(
                error.contains("strict relative normal components"),
                "{invalid:?}: {error}"
            );
        }
        validate_source_root("jqf-source/README.md").expect("normal relative root is valid");
    }

    #[cfg(unix)]
    #[test]
    fn traversal_rejects_file_and_escaping_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let repository = TempRepository::new();
        let outside = TempRepository::new();
        fs::write(repository.path().join("src/original.rs"), "original").expect("write source");
        symlink("original.rs", repository.path().join("src/alias.rs")).expect("create file symlink");
        let error = enumerate_source_paths(repository.path(), &["src"]).expect_err("file symlink must fail");
        assert!(error.contains("must not be a symlink"), "{error}");
        fs::remove_file(repository.path().join("src/alias.rs")).expect("remove file symlink");

        fs::write(outside.path().join("src/outside.rs"), "outside").expect("write outside source");
        symlink(outside.path().join("src"), repository.path().join("src/escape"))
            .expect("create escaping directory symlink");
        let error =
            enumerate_source_paths(repository.path(), &["src"]).expect_err("escaping directory symlink must fail");
        assert!(error.contains("must not be a symlink"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn relative_source_path_rejects_non_utf8_names() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let repository = TempRepository::new();
        let invalid = OsString::from_vec(vec![b'n', b'o', b'n', b'-', 0xff]);
        let path = repository.path().join("src").join(invalid);
        let error = repository_relative_utf8(repository.path(), &path).expect_err("non-UTF-8 source path must fail");
        assert!(error.contains("is not UTF-8"), "{error}");
    }
}
