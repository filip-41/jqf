use std::{
    env,
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

const SOURCE_ROOTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "jqf-codec/core/Cargo.toml",
    "jqf-codec/core/src",
    "jqf-codec/json/Cargo.toml",
    "jqf-codec/json/src",
    "jqf-data/Cargo.toml",
    "jqf-data/src",
    "jqf-resource/Cargo.toml",
    "jqf-resource/src",
    "jqf-source/Cargo.toml",
    "jqf-source/README.md",
    "jqf-source/src",
    "tools/jqf-bench-build.rs",
    "tools/jqf-bench-build-manifest.rs",
    "tools/jqf-data-bench/Cargo.toml",
    "tools/jqf-data-bench/build.rs",
    "tools/jqf-data-bench/src",
];

fn main() {
    for variable in [
        "RUSTC",
        "TARGET",
        "PROFILE",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
        "CARGO_PROFILE_RELEASE_CODEGEN_UNITS",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let repository = manifest_dir.join("../..").canonicalize().expect("repository root");
    let mut files = Vec::new();
    for root in SOURCE_ROOTS {
        validate_source_root(root).unwrap_or_else(|error| panic!("{error}"));
        println!("cargo:rerun-if-changed={}", repository.join(root).display());
        collect_files(&repository.join(root), &mut files);
    }
    let mut relative: Vec<String> = files
        .iter()
        .map(|path| {
            println!("cargo:rerun-if-changed={}", path.display());
            path.strip_prefix(&repository)
                .expect("source under repository")
                .to_str()
                .expect("source path is UTF-8")
                .to_owned()
        })
        .collect();
    relative.sort();
    relative.dedup();
    let modes: Vec<u32> = relative
        .iter()
        .map(|path| source_file_mode(&repository.join(path)))
        .collect();
    let hashes = git_hash_paths(&repository, &relative);
    let mut manifest = String::new();
    for ((path, hash), mode) in relative.iter().zip(&hashes).zip(&modes) {
        writeln!(manifest, "{hash}\t{mode:04o}\t{path}").expect("write manifest entry");
    }
    let identity = git_hash_stdin(&repository, manifest.as_bytes());
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let compiler = command_text(&repository, &rustc, &["-Vv"]);
    let git_head = command_text(&repository, "git", &["rev-parse", "HEAD"]);

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("output directory")).join("source_manifest.rs");
    let mut generated = format!(
        "pub(crate) const BUILD_SOURCE_MANIFEST_ID: &str = {identity:?};\n\
         pub(crate) const BUILD_SOURCE_ROOTS: &[&str] = &[\n"
    );
    for root in SOURCE_ROOTS {
        writeln!(generated, "    {root:?},").expect("write generated source root");
    }
    generated.push_str(
        "];\n\
         pub(crate) const BUILD_SOURCE_MANIFEST: &[(&str, &str)] = &[\n",
    );
    for (path, hash) in relative.iter().zip(&hashes) {
        writeln!(generated, "    ({path:?}, {hash:?}),").expect("write generated manifest");
    }
    generated.push_str("];\npub(crate) const BUILD_SOURCE_MODES: &[u32] = &[\n");
    for mode in &modes {
        writeln!(generated, "    0o{mode:04o},").expect("write generated source mode");
    }
    generated.push_str("];\n");
    for (name, value) in [
        ("BUILD_COMPILER", compiler),
        ("BUILD_GIT_HEAD", git_head),
        ("BUILD_TARGET", env::var("TARGET").unwrap_or_default()),
        ("BUILD_PROFILE", env::var("PROFILE").unwrap_or_default()),
        ("BUILD_RUSTFLAGS", env::var("RUSTFLAGS").unwrap_or_default()),
        (
            "BUILD_ENCODED_RUSTFLAGS",
            env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default(),
        ),
        (
            "BUILD_CONFIGURED_TARGET",
            env::var("CARGO_BUILD_TARGET").unwrap_or_default(),
        ),
        (
            "BUILD_RELEASE_CODEGEN_UNITS",
            env::var("CARGO_PROFILE_RELEASE_CODEGEN_UNITS").unwrap_or_default(),
        ),
    ] {
        writeln!(generated, "pub(crate) const {name}: &str = {value:?};").expect("write build identity");
    }
    fs::write(output, generated).expect("write source manifest");
}

fn collect_files(path: &Path, output: &mut Vec<PathBuf>) {
    let metadata = fs::symlink_metadata(path).unwrap_or_else(|error| panic!("inspect {}: {error}", path.display()));
    assert!(
        !metadata.file_type().is_symlink(),
        "source manifest path must not be a symlink: {}",
        path.display()
    );
    if metadata.is_file() {
        output.push(path.to_owned());
        return;
    }
    assert!(
        metadata.is_dir(),
        "source manifest path is neither a file nor directory: {}",
        path.display()
    );
    let mut entries: Vec<_> = fs::read_dir(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for entry in entries {
        if entry.file_name().is_some_and(|name| name == "target") {
            let metadata =
                fs::symlink_metadata(&entry).unwrap_or_else(|error| panic!("inspect {}: {error}", entry.display()));
            assert!(
                !metadata.file_type().is_symlink(),
                "source manifest path must not be a symlink: {}",
                entry.display()
            );
            continue;
        }
        collect_files(&entry, output);
    }
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

fn source_file_mode(path: &Path) -> u32 {
    let metadata = fs::symlink_metadata(path).unwrap_or_else(|error| panic!("inspect {}: {error}", path.display()));
    assert!(
        !metadata.file_type().is_symlink(),
        "source manifest path must not be a symlink: {}",
        path.display()
    );
    assert!(
        metadata.is_file(),
        "source manifest entry is not a file: {}",
        path.display()
    );
    portable_mode(&metadata)
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

fn command_text(repository: &Path, program: &str, arguments: &[&str]) -> String {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(repository)
        .output()
        .unwrap_or_else(|error| panic!("run {program}: {error}"));
    assert!(output.status.success(), "{program} failed");
    String::from_utf8(output.stdout)
        .expect("command output is UTF-8")
        .trim()
        .to_owned()
}

fn git_hash_paths(repository: &Path, paths: &[String]) -> Vec<String> {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin-paths"])
        .current_dir(repository)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git hash-object");
    {
        let stdin = child.stdin.as_mut().expect("git stdin");
        for path in paths {
            writeln!(stdin, "{path}").expect("write git path");
        }
    }
    let output = child.wait_with_output().expect("wait for git hash-object");
    assert!(output.status.success(), "git hash-object failed");
    let hashes: Vec<_> = String::from_utf8(output.stdout)
        .expect("git hashes are UTF-8")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(hashes.len(), paths.len(), "one hash per source path");
    hashes
}

fn git_hash_stdin(repository: &Path, bytes: &[u8]) -> String {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .current_dir(repository)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git hash-object");
    child
        .stdin
        .as_mut()
        .expect("git stdin")
        .write_all(bytes)
        .expect("write manifest");
    let output = child.wait_with_output().expect("wait for manifest hash");
    assert!(output.status.success(), "git manifest hash failed");
    String::from_utf8(output.stdout)
        .expect("manifest hash is UTF-8")
        .trim()
        .to_owned()
}
