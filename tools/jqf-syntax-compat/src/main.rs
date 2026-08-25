use std::{env, path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let mut cli_oracle = None;
    while let Some(arg) = args.next() {
        if arg == "--jq" {
            let Some(path) = args.next() else {
                eprintln!("error: --jq requires a path");
                return ExitCode::from(2);
            };
            cli_oracle = Some(PathBuf::from(path));
        } else {
            eprintln!("error: unsupported argument {}", arg.to_string_lossy());
            return ExitCode::from(2);
        }
    }
    let env_oracle = env::var("JQF_JQ_ORACLE").ok();
    let oracle = jqf_syntax_compat::resolve_oracle_path(cli_oracle.as_deref(), env_oracle.as_deref());
    match jqf_syntax_compat::run_compatibility(&oracle) {
        Ok(report) if report.mismatches.is_empty() => {
            println!(
                "ok: {} jq-shared cases and {} jqf extension cases",
                report.shared_cases, report.extension_cases
            );
            ExitCode::SUCCESS
        }
        Ok(report) => {
            for mismatch in &report.mismatches {
                match mismatch.jq_accepted {
                    Some(jq_accepted) => eprintln!(
                        "{}: jq={} jqf={} query={:?}",
                        mismatch.name, jq_accepted, mismatch.jqf_accepted, mismatch.query
                    ),
                    None => eprintln!(
                        "{}: jq=not-run (jqf extension) jqf={} query={:?}",
                        mismatch.name, mismatch.jqf_accepted, mismatch.query
                    ),
                }
            }
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}
