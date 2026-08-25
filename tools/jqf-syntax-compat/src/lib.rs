//! jq 1.8.2 syntax compatibility harness.
//!
//! MANUAL-RUN, deliberately outside the standing gate battery: the oracle
//! must be exactly `jq-1.8.2` on this machine, which no gate tier can assume.
//! Run it explicitly with `cargo run -p jqf-syntax-compat -- --jq <path>`
//! (or `JQF_JQ_ORACLE`); registering it as a lane means first committing the
//! pinned oracle binary or a fetch step for one.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use jqf_source::{SourceId, SourceKind, SourceRef};
use jqf_syntax::{BinaryOp, InfixOperation, OperatorSpec, parse_query};

/// Exact jq release accepted as the syntax oracle.
pub const EXPECTED_JQ_VERSION: &str = "jq-1.8.2";

/// Repository-relative fallback oracle path.
pub const DEFAULT_JQ_ORACLE: &str = "tools/jq-1.8.2";

/// One syntax-only compatibility fixture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyntaxCase {
    /// Stable fixture name.
    pub name: &'static str,
    /// Query source.
    pub query: &'static str,
    /// Whether jq 1.8.2 accepts the query grammar.
    pub accepted: bool,
}

/// Generated syntax fixture whose identity and query are owned by the harness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedSyntaxCase {
    /// Stable fixture name.
    pub name: String,
    /// Query source.
    pub query: String,
    /// Whether jq 1.8.2 accepts the query grammar.
    pub accepted: bool,
}

/// Oracle result after excluding execution semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OracleAcceptance {
    /// jq compiled the query.
    Accepted,
    /// jq rejected the query during compilation.
    Rejected,
}

/// One parity mismatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mismatch {
    /// Stable case name.
    pub name: String,
    /// Query source.
    pub query: String,
    /// jq's syntax decision, or `None` for a jqf-only extension that was not
    /// sent to the oracle.
    pub jq_accepted: Option<bool>,
    /// jqf's syntax decision.
    pub jqf_accepted: bool,
}

/// Summary returned by one compatibility run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatibilityReport {
    /// Number of jq-shared cases checked.
    pub shared_cases: usize,
    /// Number of jqf-only extension cases checked locally.
    pub extension_cases: usize,
    /// Any parity or fixture mismatches.
    pub mismatches: Vec<Mismatch>,
}

/// Resolve the oracle path with CLI, environment, then repository fallback precedence.
#[must_use]
pub fn resolve_oracle_path(cli: Option<&Path>, env: Option<&str>) -> PathBuf {
    cli.map(Path::to_path_buf)
        .or_else(|| env.map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_JQ_ORACLE))
}

/// Convert jq's process status into a syntax-only decision.
///
/// # Errors
///
/// Returns an error for statuses other than jq compile rejection 3, successful
/// execution 0, or runtime failure 5.
pub fn classify_oracle_exit(status: i32) -> Result<OracleAcceptance, String> {
    match status {
        0 | 5 => Ok(OracleAcceptance::Accepted),
        3 => Ok(OracleAcceptance::Rejected),
        status => Err(format!(
            "jq oracle exited {status}; only compile rejection 3 and syntax acceptance 0 or 5 are classified"
        )),
    }
}

/// Require the exact jq syntax baseline.
///
/// # Errors
///
/// Returns an error unless `version` is exactly [`EXPECTED_JQ_VERSION`].
pub fn verify_version(version: &str) -> Result<(), String> {
    if version == EXPECTED_JQ_VERSION {
        Ok(())
    } else {
        Err(format!(
            "expected jq oracle version {EXPECTED_JQ_VERSION}, got {version:?}"
        ))
    }
}

/// Generate jq-shared operator cases from the public operator authority.
#[must_use]
pub fn shared_operator_cases() -> Vec<GeneratedSyntaxCase> {
    let shared = OperatorSpec::ALL.to_vec();
    let mut cases = Vec::with_capacity(shared.len() * shared.len());
    for outer in &shared {
        for inner in &shared {
            let outer_lexeme = operator_lexeme(*outer);
            let inner_lexeme = operator_lexeme(*inner);
            cases.push(GeneratedSyntaxCase {
                name: format!("operator-pair-{}-{}", operator_name(*outer), operator_name(*inner)),
                query: format!(".a {outer_lexeme} (.b {inner_lexeme} .c)"),
                accepted: true,
            });
        }
    }
    cases
}

/// Run all shared and extension syntax cases.
///
/// # Errors
///
/// Returns an error when the oracle cannot run, reports a different version,
/// exits outside the syntax-only status contract, or contradicts a fixture.
pub fn run_compatibility(oracle: &Path) -> Result<CompatibilityReport, String> {
    let version = run_oracle(oracle, &["--version"])?;
    if version.status != 0 {
        return Err(format!("failed to read jq oracle version: exit {}", version.status));
    }
    let version =
        String::from_utf8(version.stdout).map_err(|error| format!("jq oracle version was not UTF-8: {error}"))?;
    verify_version(version.trim_end_matches(['\r', '\n']))?;

    let generated = shared_operator_cases();
    let shared_cases = SHARED_FIXTURES.len() + generated.len();
    let mut mismatches = Vec::new();
    for case in SHARED_FIXTURES {
        compare_shared_case(oracle, case.name, case.query, case.accepted, &mut mismatches)?;
    }
    for case in &generated {
        compare_shared_case(oracle, &case.name, &case.query, case.accepted, &mut mismatches)?;
    }
    for case in EXTENSION_FIXTURES {
        compare_extension_case(case, &mut mismatches)?;
    }

    Ok(CompatibilityReport {
        shared_cases,
        extension_cases: EXTENSION_FIXTURES.len(),
        mismatches,
    })
}

struct CommandOutput {
    status: i32,
    stdout: Vec<u8>,
}

fn run_oracle(oracle: &Path, args: &[&str]) -> Result<CommandOutput, String> {
    let output = Command::new(oracle)
        .args(args)
        .output()
        .map_err(|error| format!("failed to run {}: {error}", oracle.display()))?;
    let status = output
        .status
        .code()
        .ok_or_else(|| format!("jq oracle {} terminated by signal", oracle.display()))?;
    Ok(CommandOutput {
        status,
        stdout: output.stdout,
    })
}

fn compare_shared_case(
    oracle: &Path,
    name: &str,
    query: &str,
    expected: bool,
    mismatches: &mut Vec<Mismatch>,
) -> Result<(), String> {
    let result = run_oracle(oracle, &["-n", query])?;
    let jq_accepted = matches!(classify_oracle_exit(result.status)?, OracleAcceptance::Accepted);
    if jq_accepted != expected {
        return Err(format!(
            "fixture {name} expectation disagrees with jq-1.8.2: expected {expected}, got {jq_accepted}"
        ));
    }
    let parser_accepted = jqf_accepts(query)?;
    if jq_accepted != parser_accepted {
        mismatches.push(Mismatch {
            name: name.into(),
            query: query.into(),
            jq_accepted: Some(jq_accepted),
            jqf_accepted: parser_accepted,
        });
    }
    Ok(())
}

fn compare_extension_case(case: &SyntaxCase, mismatches: &mut Vec<Mismatch>) -> Result<(), String> {
    let jqf_accepted = jqf_accepts(case.query)?;
    if jqf_accepted != case.accepted {
        mismatches.push(Mismatch {
            name: case.name.into(),
            query: case.query.into(),
            jq_accepted: None,
            jqf_accepted,
        });
    }
    Ok(())
}

fn jqf_accepts(query: &str) -> Result<bool, String> {
    let source = SourceRef::new(SourceId::new(1), SourceKind::Query);
    parse_query(source, query)
        .map(|parsed| parsed.into_valid_syntax().is_ok())
        .map_err(|error| format!("jqf rejected compatibility fixture input: {error}"))
}

fn operator_lexeme(spec: OperatorSpec) -> &'static str {
    spec.token.fixed_lexeme().unwrap_or_else(|| match spec.token {
        jqf_syntax::TokenKind::And => "and",
        jqf_syntax::TokenKind::Or => "or",
        _ => unreachable!("all operator tokens have one source spelling"),
    })
}

fn operator_name(spec: OperatorSpec) -> &'static str {
    match spec.operation {
        InfixOperation::Binary(operation) => match operation {
            BinaryOp::Add => "add",
            BinaryOp::Subtract => "subtract",
            BinaryOp::Multiply => "multiply",
            BinaryOp::Divide => "divide",
            BinaryOp::Remainder => "remainder",
            BinaryOp::Equal => "equal",
            BinaryOp::NotEqual => "not-equal",
            BinaryOp::Less => "less",
            BinaryOp::LessEqual => "less-equal",
            BinaryOp::Greater => "greater",
            BinaryOp::GreaterEqual => "greater-equal",
            BinaryOp::And => "and",
            BinaryOp::Or => "or",
            BinaryOp::Alternative => "alternative",
            BinaryOp::Pipe => "pipe",
            BinaryOp::Comma => "comma",
            _ => spec.token.description(),
        },
        InfixOperation::Assignment(operation) => match operation {
            jqf_syntax::AssignmentOp::Assign => "assign",
            jqf_syntax::AssignmentOp::Update => "update",
            jqf_syntax::AssignmentOp::Add => "add-assign",
            jqf_syntax::AssignmentOp::Subtract => "subtract-assign",
            jqf_syntax::AssignmentOp::Multiply => "multiply-assign",
            jqf_syntax::AssignmentOp::Divide => "divide-assign",
            jqf_syntax::AssignmentOp::Remainder => "remainder-assign",
            jqf_syntax::AssignmentOp::Alternative => "alternative-assign",
            _ => spec.token.description(),
        },
    }
}

/// jq-shared regression fixtures that avoid name resolution and target validation.
pub const SHARED_FIXTURES: &[SyntaxCase] = &[
    SyntaxCase {
        name: "trailing-dot-field",
        query: "1..field",
        accepted: true,
    },
    SyntaxCase {
        name: "chained-comparison",
        query: ".a < .b < .c",
        accepted: false,
    },
    SyntaxCase {
        name: "chained-assignment",
        query: ".a = .b = .c",
        accepted: false,
    },
    SyntaxCase {
        name: "remainder-multiplicative-precedence",
        query: ".a + .b % .c",
        accepted: true,
    },
    SyntaxCase {
        name: "runtime-error-is-syntax",
        query: "error(\"runtime\")",
        accepted: true,
    },
    SyntaxCase {
        name: "slice-both-bounds-absent",
        query: ".[:]",
        accepted: false,
    },
    SyntaxCase {
        name: "slice-both-bounds-null",
        query: ".[null:null]",
        accepted: true,
    },
    SyntaxCase {
        name: "slice-open-start",
        query: ".[:2]",
        accepted: true,
    },
    SyntaxCase {
        name: "slice-open-end",
        query: ".[1:]",
        accepted: true,
    },
    SyntaxCase {
        name: "recursive-descent-postfix-chain",
        query: "..[0]?",
        accepted: true,
    },
];

/// jqf-only syntax additions, intentionally excluded from jq parity.
pub const EXTENSION_FIXTURES: &[SyntaxCase] = &[
    SyntaxCase {
        name: "let-binding",
        query: "let $x = .a | $x",
        accepted: true,
    },
    SyntaxCase {
        name: "node-accessor",
        query: ".a.@tag",
        accepted: true,
    },
    SyntaxCase {
        name: "attribute-accessor",
        query: ".a.&href",
        accepted: true,
    },
];

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    #[cfg(unix)]
    use std::{fs, os::unix::fs::PermissionsExt};

    use jqf_syntax::OperatorSpec;

    use super::{
        DEFAULT_JQ_ORACLE, EXPECTED_JQ_VERSION, OracleAcceptance, SyntaxCase, classify_oracle_exit,
        compare_extension_case, resolve_oracle_path, shared_operator_cases, verify_version,
    };
    // `run_compatibility` is exercised only by the unix fake-oracle test below.
    #[cfg(unix)]
    use super::run_compatibility;

    #[test]
    fn oracle_path_uses_cli_then_environment_then_repository_default() {
        assert_eq!(
            resolve_oracle_path(Some(Path::new("/cli/jq")), Some("/env/jq")),
            PathBuf::from("/cli/jq")
        );
        assert_eq!(resolve_oracle_path(None, Some("/env/jq")), PathBuf::from("/env/jq"));
        assert_eq!(resolve_oracle_path(None, None), PathBuf::from(DEFAULT_JQ_ORACLE));
    }

    #[test]
    fn exact_version_and_syntax_only_exit_contract_are_enforced() {
        assert_eq!(verify_version(EXPECTED_JQ_VERSION), Ok(()));
        assert!(verify_version("jq-1.8.1").is_err());
        assert_eq!(classify_oracle_exit(0), Ok(OracleAcceptance::Accepted));
        assert_eq!(classify_oracle_exit(3), Ok(OracleAcceptance::Rejected));
        assert_eq!(classify_oracle_exit(5), Ok(OracleAcceptance::Accepted));
        assert!(classify_oracle_exit(2).is_err());
    }

    #[test]
    fn shared_operator_pairs_come_from_operator_spec() {
        let shared_specs = OperatorSpec::ALL.len();
        let cases = shared_operator_cases();

        assert_eq!(cases.len(), shared_specs * shared_specs);
        assert!(cases.iter().all(|case| case.accepted));
        assert!(cases.iter().all(|case| {
            !case.query.contains("===")
                && !case.query.contains("!==")
                && !case.query.contains(".@")
                && !case.query.contains(".&")
                && !case.query.contains("let ")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn fake_oracle_proves_version_exit_and_full_harness_mechanics() {
        let directory = std::env::temp_dir().join(format!("jqf-syntax-compat-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let oracle = directory.join("jq");
        let invocation_log = oracle.with_extension("invocations");
        fs::write(
            &oracle,
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf 'jq-1.8.2\n'
  exit 0
fi
printf '%s\n' "$2" >> "${0}.invocations"
case "$2" in
  '.a < .b < .c'|'.a = .b = .c'|'.[:]') exit 3 ;;
  'error("runtime")') exit 5 ;;
  *) exit 0 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&oracle, fs::Permissions::from_mode(0o755)).unwrap();

        let report = run_compatibility(&oracle).unwrap();
        assert!(report.shared_cases > super::SHARED_FIXTURES.len());
        assert_eq!(report.extension_cases, super::EXTENSION_FIXTURES.len());
        assert!(report.mismatches.is_empty());
        let invocations = fs::read_to_string(invocation_log).unwrap();
        assert_eq!(invocations.lines().count(), report.shared_cases);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn extension_mismatches_do_not_claim_an_oracle_decision() {
        let mut mismatches = Vec::new();
        compare_extension_case(
            &SyntaxCase {
                name: "deliberately-wrong-extension-expectation",
                query: ".@tag",
                accepted: false,
            },
            &mut mismatches,
        )
        .unwrap();

        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].jq_accepted, None);
        assert!(mismatches[0].jqf_accepted);
    }
}
