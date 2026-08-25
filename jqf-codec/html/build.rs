//! Declares the debug-tracing cfg so `cargo check`/`clippy` never warn about `#[cfg(jqf_trace)]` (the lint gate runs
//! `-D warnings`).

fn main() {
    println!("cargo::rustc-check-cfg=cfg(jqf_trace)");
}
