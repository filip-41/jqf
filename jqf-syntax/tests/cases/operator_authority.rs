use jqf_source::{SourceId, SourceKind, SourceRef};
use jqf_syntax::{Associativity, Expr, ExprKind, InfixOperation, OperatorSpec, TokenKind, parse_query};

fn source() -> SourceRef {
    SourceRef::new(SourceId::new(73), SourceKind::Query)
}

fn spelling(token: TokenKind) -> &'static str {
    match token {
        TokenKind::And => "and",
        TokenKind::Or => "or",
        _ => token.fixed_lexeme().expect("infix token has source spelling"),
    }
}

fn operation(expr: &Expr) -> Option<InfixOperation> {
    match expr.kind() {
        ExprKind::Binary(binary) => Some(InfixOperation::Binary(binary.op)),
        ExprKind::Assignment(assignment) => Some(InfixOperation::Assignment(assignment.op)),
        _ => None,
    }
}

fn operands(expr: &Expr) -> Option<(&Expr, &Expr)> {
    match expr.kind() {
        ExprKind::Binary(binary) => Some((&binary.left, &binary.right)),
        ExprKind::Assignment(assignment) => Some((&assignment.target, &assignment.value)),
        _ => None,
    }
}

#[test]
fn operator_authority_is_complete_unique_and_self_consistent() {
    assert_eq!(OperatorSpec::ALL.len(), 24);
    for (index, spec) in OperatorSpec::ALL.iter().enumerate() {
        assert_eq!(OperatorSpec::for_token(spec.token), Some(*spec));
        assert!(
            !OperatorSpec::ALL[..index].iter().any(|prior| prior.token == spec.token),
            "duplicate operator token {:?}",
            spec.token
        );
    }
}

/// The token lookup answers for the token it was asked about, and only ever with a spec the public table registers.
#[test]
fn every_token_for_token_dispatches_to_is_registered_in_all() {
    for kind in TokenKind::ALL {
        let Some(spec) = OperatorSpec::for_token(*kind) else {
            continue;
        };
        assert_eq!(
            spec.token, *kind,
            "for_token({kind:?}) answered with another token's operator"
        );
        assert!(
            OperatorSpec::ALL.contains(&spec),
            "for_token dispatches {kind:?} but ALL registers no such operator"
        );
    }
}

#[test]
fn every_infix_operator_pair_groups_or_rejects_from_the_public_table() {
    for first in OperatorSpec::ALL {
        for second in OperatorSpec::ALL {
            let query = format!("a {} b {} c", spelling(first.token), spelling(second.token));
            let parsed = parse_query(source(), &query).unwrap();
            if first.precedence == second.precedence && first.associativity == Associativity::NonAssociative {
                assert!(
                    !parsed.diagnostics().is_empty(),
                    "{query:?} must reject non-associative chaining"
                );
                continue;
            }
            let root = parsed
                .into_valid_syntax()
                .unwrap_or_else(|diagnostics| panic!("{query:?} unexpectedly rejected: {diagnostics:?}"));
            match first.precedence.cmp(&second.precedence) {
                core::cmp::Ordering::Less => {
                    assert_eq!(operation(&root), Some(first.operation), "{query:?}");
                    let (_, right) = operands(&root).expect("infix root");
                    assert_eq!(operation(right), Some(second.operation), "{query:?}");
                }
                core::cmp::Ordering::Greater => {
                    assert_eq!(operation(&root), Some(second.operation), "{query:?}");
                    let (left, _) = operands(&root).expect("infix root");
                    assert_eq!(operation(left), Some(first.operation), "{query:?}");
                }
                core::cmp::Ordering::Equal => match first.associativity {
                    Associativity::Left => {
                        assert_eq!(operation(&root), Some(second.operation), "{query:?}");
                        let (left, _) = operands(&root).expect("left-associative infix root");
                        assert_eq!(operation(left), Some(first.operation), "{query:?}");
                    }
                    Associativity::Right => {
                        assert_eq!(operation(&root), Some(first.operation), "{query:?}");
                        let (_, right) = operands(&root).expect("right-associative infix root");
                        assert_eq!(operation(right), Some(second.operation), "{query:?}");
                    }
                    Associativity::NonAssociative => unreachable!("handled above"),
                },
            }
        }
    }
}
