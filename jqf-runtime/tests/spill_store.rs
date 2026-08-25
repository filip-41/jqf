use std::path::PathBuf;

use jqf_resource::SpillStore;
use jqf_runtime::spill::TempDirSpillStore;

#[test]
fn multi_entry_run_reads_back_exactly() {
    let store = TempDirSpillStore::try_new(None).expect("store");
    let id = store.create_run().expect("run");
    // Three entries: [u32 len][key][u64 pos], keys deliberately length-varied.
    let mut run = Vec::new();
    for (key, pos) in [(b"a".as_slice(), 7u64), (b"{\"id\":18824}", 18824), (b"zz", 3)] {
        run.extend_from_slice(&(u32::try_from(key.len()).expect("key length")).to_le_bytes());
        run.extend_from_slice(key);
        run.extend_from_slice(&pos.to_le_bytes());
    }
    store.write_run(id, &run).expect("write");
    let cursor = store.open_run(id).expect("open");
    let mut buf = Vec::new();
    let mut positions = Vec::new();
    while let Some(pos) = store.read_next(cursor, &mut buf).expect("read") {
        positions.push(pos);
        buf.clear();
    }
    assert_eq!(positions, vec![7, 18824, 3]);
}

/// A scratch base directory, removed on drop. The counter keeps parallel test threads from colliding on one
/// clock-tick's worth of nanosecond names.
struct TempBase(PathBuf);

impl TempBase {
    fn new() -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "jqf-spill-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        std::fs::create_dir(&path).expect("base");
        TempBase(path)
    }
}

impl Drop for TempBase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn refuses_a_pre_existing_directory_at_its_path() {
    // The store must refuse a path that already exists — the whole mkdtemp discipline is that a taken path is a
    // failure, never an adoption.
    let base = TempBase::new();
    let dir = base.0.join("occupied");
    std::fs::create_dir(&dir).expect("occupied");
    assert!(
        TempDirSpillStore::try_new_at(&dir).is_err(),
        "a pre-existing path at the store's name must be refused"
    );
}

#[test]
fn created_directory_is_0700_and_lazily_created() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let base = TempBase::new();
        let store = TempDirSpillStore::try_new_at(&base.0.join("store")).expect("store");
        // Lazy creation: the directory does not exist until a run is created — its existence is exactly the fact that
        // the spill path engaged.
        assert!(
            !store.temp_dir().exists(),
            "the directory must not exist before the first run"
        );
        store.create_run().expect("run");
        let mode = std::fs::metadata(store.temp_dir()).expect("dir").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "the spill directory must be 0700 at creation");
    }
}

#[test]
fn run_files_are_not_reachable_through_a_symlink() {
    #[cfg(unix)]
    {
        let base = TempBase::new();
        let store = TempDirSpillStore::try_new_at(&base.0.join("store")).expect("store");
        store.create_run().expect("run 0");
        // Run names are predictable ordinal files; plant a symlink at the next run's name pointing at a victim file.
        // create_new refuses the path (O_EXCL never follows a symlink), so the victim must stay untouched.
        let victim = base.0.join("victim");
        std::fs::write(&victim, b"do-not-truncate").expect("victim");
        let link = store.temp_dir().join("1.run");
        std::os::unix::fs::symlink(&victim, &link).expect("symlink");
        assert!(
            store.create_run().is_err(),
            "create_run must refuse a symlinked run path"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).expect("victim readable"),
            "do-not-truncate",
            "the symlink target must never be opened"
        );
    }
}

#[test]
fn drop_does_not_remove_a_path_the_store_never_created() {
    // Construction only records the path. A directory that appears at that name before the exclusive create is not ours
    // — Drop must leave it.
    let base = TempBase::new();
    let dir = base.0.join("planted");
    let store = TempDirSpillStore::try_new_at(&dir).expect("store");
    std::fs::create_dir(&dir).expect("plant");
    std::fs::write(dir.join("keep"), b"x").expect("marker");
    drop(store);
    assert!(
        dir.join("keep").exists(),
        "Drop must not remove a directory the store did not create"
    );
}
