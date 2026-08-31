//! Engine embedder re-exports must not grow a unused dual of builtins.

#[test]
fn unused_raw_text_reexport_cannot_return() {
    let surface = std::fs::read_to_string("src/lib.rs").expect("lib.rs");
    assert!(
        !surface.contains("is_raw_text"),
        "jqf-engine must not re-export unused is_raw_text; callers use publication_facts"
    );
}

#[test]
fn compiled_program_does_not_reexport_flatmap_probe() {
    let surface = std::fs::read_to_string("src/compile/program.rs").expect("program.rs");
    assert!(
        !surface.contains("fn has_reachable_flatmap"),
        "CompiledProgram must not wrap Program::has_reachable_flatmap"
    );
}
