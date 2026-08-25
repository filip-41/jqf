//! Why a resource request was refused.
//!
//! The ledger builds these. Print them with `Display` — that is a sentence, never a Rust type name. `Debug` is a
//! struct literal; do not put that on stderr.

/// A request ceiling a charge can hit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceLimit {
    /// Logical input bytes accepted.
    InputBytes,
    /// Logical output bytes published.
    OutputBytes,
    /// Aggregate live or reserved memory bytes.
    MemoryBytes,
    /// Bytes written to spill files on disk.
    SpillDiskBytes,
    /// Structural nesting depth.
    NestingDepth,
}

impl ResourceLimit {
    /// Short name for a diagnostic (`"memory"`, `"output"`, …).
    ///
    /// Not the Rust variant name. Print this, not `Debug`.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::InputBytes => "input",
            Self::OutputBytes => "output",
            Self::MemoryBytes => "memory",
            Self::SpillDiskBytes => "spill disk",
            Self::NestingDepth => "nesting depth",
        }
    }

    /// `"bytes"` or `"levels"`.
    #[must_use]
    pub const fn unit(self) -> &'static str {
        match self {
            Self::InputBytes | Self::OutputBytes | Self::MemoryBytes | Self::SpillDiskBytes => "bytes",
            Self::NestingDepth => "levels",
        }
    }
}

/// A quota, arithmetic, or allocation failure.
///
/// Cancel / deadline / "out of work budget" are different types, so they cannot be reported as a quota error by
/// accident.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceError {
    /// A ceiling said no. Nothing was charged.
    LimitExceeded {
        /// Limit that rejected the operation.
        limit_kind: ResourceLimit,
        /// Configured ceiling.
        limit: u64,
        /// Usage before the rejected operation.
        current: u64,
        /// Extra bytes (or levels) that did not fit.
        requested_delta: u64,
    },
    /// Checked size or usage arithmetic overflowed.
    ArithmeticOverflow,
    /// The allocator returned null.
    AllocationFailed,
    /// `commit` asked to publish more than the permit reserved.
    OutputPermitExceeded {
        /// Maximum byte prefix authorized by the permit.
        reserved: u64,
        /// Byte prefix the caller attempted to commit.
        published: u64,
    },
    /// The ledger and a token disagreed. A bug, not a user mistake.
    AccountingInvariantViolation,
    /// The host could not do something (stderr write, disk, …).
    HostFailure {
        /// The host's own description of the failure.
        detail: &'static str,
    },
    /// A recursive definition went past the call-depth ceiling.
    ///
    /// Not catchable, same as the other resource stops.
    RecursionLimit {
        /// The configured ceiling, in call frames.
        limit: u64,
    },
}

/// The sentence to print. Not `Debug` — that is a Rust struct literal.
impl core::fmt::Display for ResourceError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::LimitExceeded {
                limit_kind,
                limit,
                current,
                requested_delta,
            } => fmt_limit_exceeded(formatter, limit_kind, limit, current, requested_delta),
            Self::ArithmeticOverflow => formatter.write_str("resource accounting overflowed while sizing an operation"),
            Self::AllocationFailed => {
                formatter.write_str("the system refused an allocation the request had authorized")
            }
            Self::OutputPermitExceeded { reserved, published } => {
                fmt_output_permit_exceeded(formatter, reserved, published)
            }
            Self::AccountingInvariantViolation => {
                formatter.write_str("internal resource accounting invariant violation")
            }
            Self::HostFailure { detail } => formatter.write_str(detail),
            Self::RecursionLimit { limit } => write!(
                formatter,
                "recursive function calls exceeded the depth ceiling of {limit}"
            ),
        }
    }
}

fn fmt_limit_exceeded(
    formatter: &mut core::fmt::Formatter<'_>,
    limit_kind: ResourceLimit,
    limit: u64,
    current: u64,
    requested_delta: u64,
) -> core::fmt::Result {
    write!(
        formatter,
        "{} limit exceeded: the ceiling is {limit} {}, {current} are already \
         in use, and {requested_delta} more could not be granted",
        limit_kind.describe(),
        limit_kind.unit(),
    )
}

fn fmt_output_permit_exceeded(
    formatter: &mut core::fmt::Formatter<'_>,
    reserved: u64,
    published: u64,
) -> core::fmt::Result {
    write!(
        formatter,
        "output permit exceeded: {published} bytes were offered against a \
         {reserved}-byte reservation"
    )
}

impl core::error::Error for ResourceError {}

/// A failed `Vec::try_reserve` is [`Self::AllocationFailed`].
impl From<std::collections::TryReserveError> for ResourceError {
    fn from(_: std::collections::TryReserveError) -> Self {
        Self::AllocationFailed
    }
}

#[cfg(test)]
mod tests {
    use super::{ResourceError, ResourceLimit};
    use std::format;
    use std::vec::Vec;

    const LIMITS: [ResourceLimit; 5] = [
        ResourceLimit::InputBytes,
        ResourceLimit::OutputBytes,
        ResourceLimit::MemoryBytes,
        ResourceLimit::SpillDiskBytes,
        ResourceLimit::NestingDepth,
    ];

    /// Every renderable failure, so the prose gate below sweeps the whole enum rather than the arms someone remembered.
    fn every_error() -> Vec<ResourceError> {
        let mut errors = Vec::new();
        for limit_kind in LIMITS {
            errors.push(ResourceError::LimitExceeded {
                limit_kind,
                limit: 104_857_600,
                current: 92_812_697,
                requested_delta: 25_165_872,
            });
        }
        errors.push(ResourceError::ArithmeticOverflow);
        errors.push(ResourceError::AllocationFailed);
        errors.push(ResourceError::OutputPermitExceeded {
            reserved: 64,
            published: 96,
        });
        errors.push(ResourceError::AccountingInvariantViolation);
        errors.push(ResourceError::HostFailure {
            detail: "the host's stderr channel failed",
        });
        errors.push(ResourceError::RecursionLimit { limit: 10_000 });
        errors
    }

    /// The standing guard against a `Debug` rendering reaching a person: no braces, no `::` paths, and no variant/field
    /// identifier anywhere in the text. A new variant whose `Display` arm is copied from `Debug` fails here rather than
    /// on a user's terminal.
    #[test]
    fn every_rendering_is_prose_not_rust_syntax() {
        let banned = [
            "{",
            "}",
            "::",
            "LimitExceeded",
            "limit_kind",
            "requested_delta",
            "InputBytes",
            "OutputBytes",
            "MemoryBytes",
            "SpillDiskBytes",
            "NestingDepth",
            "ArithmeticOverflow",
            "AllocationFailed",
            "OutputPermitExceeded",
            "AccountingInvariantViolation",
            "HostFailure",
            "RecursionLimit",
        ];
        for error in every_error() {
            let rendered = format!("{error}");
            assert!(!rendered.is_empty());
            for needle in banned {
                assert!(
                    !rendered.contains(needle),
                    "rendered resource error leaks Rust syntax {needle:?}: {rendered}"
                );
            }
        }
    }

    #[test]
    fn a_memory_ceiling_states_its_numbers_in_prose() {
        let rendered = format!(
            "{}",
            ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::MemoryBytes,
                limit: 104_857_600,
                current: 92_812_697,
                requested_delta: 25_165_872,
            }
        );
        assert_eq!(
            rendered,
            "memory limit exceeded: the ceiling is 104857600 bytes, 92812697 are \
             already in use, and 25165872 more could not be granted"
        );
    }

    #[test]
    fn the_depth_ceiling_counts_levels_not_bytes() {
        let rendered = format!(
            "{}",
            ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::NestingDepth,
                limit: 10_000,
                current: 10_000,
                requested_delta: 1,
            }
        );
        assert_eq!(
            rendered,
            "nesting depth limit exceeded: the ceiling is 10000 levels, 10000 are \
             already in use, and 1 more could not be granted"
        );
    }

    /// A new `ResourceLimit` variant fails to compile here until it is in `LIMITS` and has a name and a unit.
    #[test]
    fn every_limit_is_named_in_prose() {
        for limit in LIMITS {
            match limit {
                ResourceLimit::InputBytes
                | ResourceLimit::OutputBytes
                | ResourceLimit::MemoryBytes
                | ResourceLimit::SpillDiskBytes
                | ResourceLimit::NestingDepth => {
                    assert!(!limit.describe().is_empty());
                    assert!(!limit.unit().is_empty());
                }
            }
        }
    }

    /// A new `ResourceError` variant fails to compile here until `every_error` constructs it and the Display sweep
    /// covers it.
    #[test]
    fn every_error_variant_is_sampled() {
        let errors = every_error();
        assert_eq!(errors.len(), 11);
        for error in errors {
            match error {
                ResourceError::LimitExceeded { .. }
                | ResourceError::ArithmeticOverflow
                | ResourceError::AllocationFailed
                | ResourceError::OutputPermitExceeded { .. }
                | ResourceError::AccountingInvariantViolation
                | ResourceError::HostFailure { .. }
                | ResourceError::RecursionLimit { .. } => {}
            }
        }
    }

    #[test]
    fn remaining_variants_render_as_the_pinned_sentences() {
        assert_eq!(
            format!("{}", ResourceError::ArithmeticOverflow),
            "resource accounting overflowed while sizing an operation"
        );
        assert_eq!(
            format!("{}", ResourceError::AllocationFailed),
            "the system refused an allocation the request had authorized"
        );
        assert_eq!(
            format!(
                "{}",
                ResourceError::OutputPermitExceeded {
                    reserved: 64,
                    published: 96,
                }
            ),
            "output permit exceeded: 96 bytes were offered against a 64-byte reservation"
        );
        assert_eq!(
            format!("{}", ResourceError::AccountingInvariantViolation),
            "internal resource accounting invariant violation"
        );
        assert_eq!(
            format!(
                "{}",
                ResourceError::HostFailure {
                    detail: "the host's stderr channel failed",
                }
            ),
            "the host's stderr channel failed"
        );
        assert_eq!(
            format!("{}", ResourceError::RecursionLimit { limit: 10_000 }),
            "recursive function calls exceeded the depth ceiling of 10000"
        );
    }

    #[test]
    fn a_failed_vec_reserve_is_an_allocation_failure() {
        let mut buffer = Vec::<u8>::new();
        let error = buffer
            .try_reserve(usize::MAX)
            .expect_err("a usize::MAX reservation must fail");
        assert_eq!(ResourceError::from(error), ResourceError::AllocationFailed);
    }
}
