//! The one receipt harness for every codec battery (plan 124).
//!
//! `jqf-codec-receipts smoke <codec>` runs a codec's smoke battery;
//! `jqf-codec-receipts differential <codec>` runs its differential corpus.
//! The drive scaffold — source setup, resource context/limits, the poll
//! loop, pass/fail reporting — lives once in [`drive`]; each codec's law
//! pins live verbatim in `smoke/<codec>.rs` / `differential/<codec>.rs`.
//! Plan 123 X4 rewrites [`drive`] to straight-line calls and nothing else
//! here should need X4 edits.

mod differential;
mod drive;
mod smoke;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(sub) = args.next() else {
        usage();
    };
    let Some(codec) = args.next() else {
        usage();
    };
    // Anything after the codec is forwarded to the differential dispatch, so a
    // codec module's own flags (`differential json --dump-accepts`) survive
    // the migration into the harness.
    let extra: Vec<String> = args.collect();
    match sub.as_str() {
        "smoke" => smoke::dispatch(&codec),
        "differential" => differential::dispatch(&codec, &extra),
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!("usage: jqf-codec-receipts smoke <codec> | differential <codec>");
    std::process::exit(2);
}
