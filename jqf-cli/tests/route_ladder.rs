//! The route-selection ladder, end-to-end: every rung must actually fire for the shape it exists to serve, and its
//! bytes must be the floor's bytes.
//!
//! This is the CI tripwire the echo route needed and lacked: a rung that silently declines to the floor (correct bytes,
//! dead fast lane) is invisible to byte-identity checks alone, so each row here pins the ROUTE name a canonical
//! program×document pair takes AND its byte identity against the forced whole-document floor (`[.][0] | (P)`).

use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs jqf with stdin redirected from a regular file (the seekable shape the whole-document rungs require) and returns
/// the output.
fn run_file(args: &[&str], input: &[u8]) -> Output {
    let path = std::env::temp_dir().join(format!(
        "jqf-route-ladder-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, input).expect("input file");
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .stdin(Stdio::from(std::fs::File::open(&path).expect("open input file")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("jqf runs to completion");
    let _ = std::fs::remove_file(&path);
    output
}

/// The `--explain` route line of one run.
fn route_of(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("jqf: explain: route: "))
        .unwrap_or_else(|| panic!("no route line in: {stderr}"))
        .to_owned()
}

/// One ladder row: the program, its document, and the rung that must serve it. The expected route is the whole pin — a
/// rung that stopped firing for its canonical shape fails here even though its bytes stayed correct.
struct Row {
    program: &'static str,
    document: &'static [u8],
    route: &'static str,
}

const LADDER: &[Row] = &[
    // The canonical-identity echo.
    Row {
        program: ".",
        document: b"[1.5]",
        route: "roundtrip",
    },
    // The W3-T4 collapse: the structure-count, projected and element-stream rungs are gone, and their canonical shapes
    // now take the whole-document sequence drive (the lazy document subsumes them). These rows pin the NEW home of each
    // shape so a re-introduction of a rung fails here.
    Row {
        program: "length",
        document: b"[1,2,3]",
        route: "sequence",
    },
    Row {
        program: ".[1:2] | length",
        document: b"[1,2,3]",
        route: "sequence",
    },
    Row {
        program: ".[] | empty",
        document: b"[1,2,3]",
        route: "sequence",
    },
    // The count-sum shape: a collect of a multi-member choice feeding length.
    Row {
        program: "[.catalog[].id, .catalog[].name] | length",
        document: b"{\"catalog\":[{\"id\":1,\"name\":\"a\"},{\"id\":2,\"name\":\"b\"}]}",
        route: "sequence",
    },
    // The projected shape: the collect reads only a bounded member set.
    Row {
        program: "[.a[] | .b]",
        document: b"{\"a\":[{\"b\":1},{\"b\":2}]}",
        route: "sequence",
    },
    // The element-iteration shape: the whole-document drive serves the collect and the select fan-out.
    Row {
        program: "[.a[]]",
        document: b"{\"a\":[1,2,3]}",
        route: "sequence",
    },
    Row {
        program: ".[] | select(. > 1)",
        document: b"[1,2,3]",
        route: "sequence",
    },
    // The range-locate rung: the bare-slice publish.
    Row {
        program: ".[1:2]",
        document: b"[1,2,3]",
        route: "range-locate",
    },
    // The shallow shape: kind and member identities alone (the shallow rung is deleted; the whole document serves it).
    Row {
        program: "keys",
        document: b"{\"a\":1}",
        route: "sequence",
    },
];

#[test]
fn each_rung_fires_for_its_canonical_shape() {
    for row in LADDER {
        let output = run_file(&["--no-parallel", "--explain", "-c", row.program], row.document);
        assert_eq!(
            output.status.code(),
            Some(0),
            "program {:?} over {:?} exited {:?}: {}",
            row.program,
            row.document,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            route_of(&output),
            row.route,
            "program {:?} over {:?} must take the {} rung",
            row.program,
            row.document,
            row.route
        );
    }
}

#[test]
fn every_rungs_bytes_are_the_floors_bytes() {
    // The decline law's obligation on every row: whatever the rung serves, its bytes are what the forced whole-document
    // floor renders.
    for row in LADDER {
        let served = run_file(&["--no-parallel", "-c", row.program], row.document);
        let floor = run_file(
            &["--no-parallel", "-c", &format!("[.][0] | ({})", row.program)],
            row.document,
        );
        assert_eq!(
            served.stdout, floor.stdout,
            "{} bytes differ from the floor for {:?}",
            row.route, row.document
        );
        assert_eq!(served.status.code(), floor.status.code());
    }
}
