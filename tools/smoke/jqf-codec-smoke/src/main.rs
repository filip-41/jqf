//! The one receipt harness for every codec battery.
//!
//! `jqf-codec-smoke smoke <codec>` runs a codec's smoke battery;
//! `jqf-codec-smoke differential <codec>` runs its differential corpus.
//! Source setup, resource context/limits, poll-resume, and pass/fail
//! reporting live once in [`drive`]; each codec's law pins live in
//! `smoke/<codec>.rs` / `differential/<codec>/`.

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
    let extra: Vec<String> = args.collect();
    match sub.as_str() {
        "smoke" => {
            if !extra.is_empty() {
                eprintln!("jqf-codec-smoke: smoke takes no extra arguments");
                std::process::exit(2);
            }
            smoke::dispatch(&codec);
        }
        "differential" => differential::dispatch(&codec, &extra),
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!("usage: jqf-codec-smoke smoke <codec> | differential <codec>");
    std::process::exit(2);
}
