//! `messagepack.key-equivalence@1`: the native map-key equivalence law, bound as a duplicate-rejecting input dialect (a
//! registered identity that resolves to no behaviour is a maturity claim with no code behind it).
//!
//! The law compares NATIVE keys — the scan skeleton, before timestamp/ extension or object projection. Integers
//! compare by mathematical value ACROSS marker widths (uint64 `5` == positive fixint `5` — ONE numeric group, where
//! CBOR has six); floats only with floats, signed zeros equal and ALL NaNs equal (CBOR requires significand match);
//! `str` by raw payload bytes, DISTINCT from a byte-equal `bin`; arrays in order; maps as unordered multisets of
//! recursively equal (key, value) pairs; extensions by signed type code plus exact raw payload; projected timestamps by
//! `(seconds, nanoseconds)`; `null`/`bool`/`bin` by kind and value; integer `1` distinct from float `1.0`.
//!
//! The skeleton is a TREE — no shared references, no cycles — so the comparator needs no coinduction stack (the
//! YAML law's in-progress stack exists for that graph's aliases; `MessagePack` has none). Recursion depth is the
//! container nesting the scan already bounded with its governed ceiling, and each level re-enters the nesting guard
//! like YAML's does.

use alloc::vec;
use alloc::vec::Vec;

use jqf_codec_core::CodecError;
use jqf_resource::{MemoryCategory, ResourceContext};
use jqf_source::ResolvedSource;

use crate::error;
use crate::scan::{ItemKind, Skeleton};

/// Validates every map in the skeleton under the law: two keys equal under `messagepack.key-equivalence@1` are a
/// duplicate and reject the document (the dialect's one observable law — it never fires unless the dialect is
/// explicitly bound).
///
/// # Errors
///
/// Returns `InvalidInput` naming the LATER key's span when a map repeats a key under the law.
pub(crate) fn validate_duplicate_keys(
    skeleton: &Skeleton,
    source: ResolvedSource<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    let comparator = KeyEquality {
        skeleton,
        bytes: source.bytes(),
    };
    for item in &skeleton.items {
        let ItemKind::Map(children) = &item.kind else {
            continue;
        };
        for (i, key) in children.iter().step_by(2).enumerate() {
            // A flat map validates every key pair: quadratic over the map's entries. Admit one transition per
            // comparison so a large map surfaces host cancellation instead of running to completion.
            for later in children.iter().skip(2 * i + 2).step_by(2) {
                resources.admit_work_transition().map_err(CodecError::from)?;
                if comparator.item_equal(*key, *later, resources)? {
                    return Err(error::invalid(
                        source,
                        skeleton.items[*later].span.start() as usize,
                        "duplicate-key",
                        "a map key duplicates an earlier key under messagepack.key-equivalence@1",
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The native key-equivalence comparator over skeleton item indices.
struct KeyEquality<'a> {
    skeleton: &'a Skeleton,
    bytes: &'a [u8],
}

impl KeyEquality<'_> {
    fn item_equal(&self, left: usize, right: usize, resources: &ResourceContext<'_>) -> Result<bool, CodecError> {
        // The nesting guard: the comparator recurses once per container level (the 10000-level ceiling — the
        // stack-depth gate's law).
        let _depth = resources.enter_nesting().map_err(CodecError::from)?;
        let (l, r) = (&self.skeleton.items[left].kind, &self.skeleton.items[right].kind);
        match (l, r) {
            (ItemKind::Null, ItemKind::Null) => Ok(true),
            (ItemKind::Bool(a), ItemKind::Bool(b)) => Ok(a == b),
            // One numeric group: `(negative, magnitude)` is the canonical form of the signed value (`-1 - magnitude`
            // when negative), so mathematical equality across marker widths is exact equality of the pair — uint64
            // `5` == positive fixint `5`.
            (ItemKind::Integer(a), ItemKind::Integer(b)) => Ok(a.negative == b.negative && a.magnitude == b.magnitude),
            // Floats only with floats; signed zeros equal under IEEE `==`, and ALL NaNs equal (the law's NaN rule,
            // distinct from CBOR's significand match).
            (ItemKind::Float(a), ItemKind::Float(b)) => Ok(float_key_equal(*a, *b)),
            // `str` by raw payload bytes and `bin` by bytes — ONE arm, but the discriminant check runs first, so a
            // byte-equal `str`/`bin` pair stays distinct (different kinds).
            (ItemKind::Str(a) | ItemKind::Bin(a), ItemKind::Str(b) | ItemKind::Bin(b)) => {
                Ok(core::mem::discriminant(l) == core::mem::discriminant(r) && self.slice(*a) == self.slice(*b))
            }
            (ItemKind::Array(a), ItemKind::Array(b)) => self.array_equal(a, b, resources),
            (ItemKind::Map(a), ItemKind::Map(b)) => self.map_equal(a, b, resources),
            // Extensions by signed type code plus exact raw payload.
            (ItemKind::Ext { ty: a, payload: ap }, ItemKind::Ext { ty: b, payload: bp }) => {
                Ok(a == b && self.slice(*ap) == self.slice(*bp))
            }
            // Projected timestamps compare by their instant (the 32/64/96 encodings of one instant are the same key).
            (
                ItemKind::Timestamp {
                    seconds: a,
                    nanoseconds: an,
                },
                ItemKind::Timestamp {
                    seconds: b,
                    nanoseconds: bn,
                },
            ) => Ok(a == b && an == bn),
            // Every other kind pair — `str` vs a byte-equal `bin`, integer vs float — is distinct.
            _ => Ok(false),
        }
    }

    fn slice(&self, payload: jqf_source::Span) -> &[u8] {
        &self.bytes[payload.start() as usize..payload.end() as usize]
    }

    fn array_equal(
        &self,
        left: &[usize],
        right: &[usize],
        resources: &ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        if left.len() != right.len() {
            return Ok(false);
        }
        for (a, b) in left.iter().zip(right) {
            if !self.item_equal(*a, *b, resources)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Unordered multiset equality of a map's (key, value) pairs: every left pair must match a DISTINCT right pair. The
    /// pair condition is precomputed into an L×R adjacency matrix, then a perfect matching is found with Kuhn's
    /// augmenting-path algorithm (the YAML law's shape — a greedy resume scan would miss a permutation).
    fn map_equal(&self, left: &[usize], right: &[usize], resources: &ResourceContext<'_>) -> Result<bool, CodecError> {
        if left.len() != right.len() {
            return Ok(false);
        }
        let left_pairs: Vec<(usize, usize)> = left.chunks_exact(2).map(|p| (p[0], p[1])).collect();
        let right_pairs: Vec<(usize, usize)> = right.chunks_exact(2).map(|p| (p[0], p[1])).collect();
        // The adjacency matrix is L×R bytes of working state; charge it BEFORE allocating so a map whose pair count
        // squares past the memory ceiling refuses at the ledger instead of attempting the allocation.
        let matrix_bytes = (left_pairs.len() as u64).saturating_mul(right_pairs.len() as u64);
        resources
            .account()
            .charge_residency(matrix_bytes, MemoryCategory::Working)
            .map_err(CodecError::from)?;
        let matched = self.match_multiset(&left_pairs, &right_pairs, resources);
        resources
            .account()
            .release_residency(MemoryCategory::Working, matrix_bytes);
        matched
    }

    fn match_multiset(
        &self,
        left_pairs: &[(usize, usize)],
        right_pairs: &[(usize, usize)],
        resources: &ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let mut adjacency = vec![vec![false; right_pairs.len()]; left_pairs.len()];
        for (l, (lk, lv)) in left_pairs.iter().enumerate() {
            for (r, (rk, rv)) in right_pairs.iter().enumerate() {
                adjacency[l][r] = self.item_equal(*lk, *rk, resources)? && self.item_equal(*lv, *rv, resources)?;
            }
        }
        let mut matched: Vec<Option<usize>> = vec![None; right_pairs.len()];
        for l in 0..left_pairs.len() {
            let mut visited = vec![false; right_pairs.len()];
            if !try_augment(l, &adjacency, &mut visited, &mut matched, resources)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Float equality under the law: `-0.0 == +0.0` (IEEE `==`) and every NaN equal to every other NaN.
fn float_key_equal(left: f64, right: f64) -> bool {
    if left.is_nan() || right.is_nan() {
        return left.is_nan() && right.is_nan();
    }
    // Finite: IEEE `==` (signed zeros equal); expressed through `partial_cmp` because the lint forbids bare float
    // equality.
    left.partial_cmp(&right) == Some(core::cmp::Ordering::Equal)
}

/// One DFS step of Kuhn's algorithm: find an augmenting path from left pair `left`, reassigning matched right pairs
/// along the way. `visited` is reset per augment attempt; `matched[r]` names the left pair matched to right pair `r`.
/// The recursion re-enters the governed nesting guard at every level: its depth scales with the match size, which is
/// input size, not container depth.
fn try_augment(
    left: usize,
    adjacency: &[Vec<bool>],
    visited: &mut [bool],
    matched: &mut [Option<usize>],
    resources: &ResourceContext<'_>,
) -> Result<bool, CodecError> {
    let _depth = resources.enter_nesting().map_err(CodecError::from)?;
    for (r, &edge) in adjacency[left].iter().enumerate() {
        if !edge || visited[r] {
            continue;
        }
        visited[r] = true;
        if let Some(previous) = matched[r] {
            if try_augment(previous, adjacency, visited, matched, resources)? {
                matched[r] = Some(left);
                return Ok(true);
            }
        } else {
            matched[r] = Some(left);
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::format;
    use std::string::String;
    use std::vec;

    use super::{KeyEquality, validate_duplicate_keys};
    use crate::options::Dialect;
    use crate::scan::{self, ItemKind, Skeleton};
    use jqf_codec_core::CodecRunContext;

    fn scan_bytes(bytes: &[u8]) -> Skeleton {
        let mut resources = crate::test_support::resources();
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(98), jqf_source::SourceKind::Input),
            "keys.test",
            bytes,
            0,
        );
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        scan::scan(source, Dialect::Utf8, &mut run).expect("scan")
    }

    fn validate(bytes: &[u8]) -> Result<(), String> {
        let mut resources = crate::test_support::resources();
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(98), jqf_source::SourceKind::Input),
            "keys.test",
            bytes,
            0,
        );
        let skeleton = scan_bytes(bytes);
        validate_duplicate_keys(&skeleton, source, &mut resources).map_err(|error| format!("{error:?}"))
    }

    /// A map `{K: 1, L: 2}` for two key encodings; returns whether the two keys are equal under the law. `k1`/`k2` are
    /// the key byte sequences.
    fn map_keys_equal(k1: &[u8], k2: &[u8]) -> bool {
        let mut bytes = vec![0x82u8];
        bytes.extend_from_slice(k1);
        bytes.push(0x01);
        bytes.extend_from_slice(k2);
        bytes.push(0x02);
        let resources = crate::test_support::resources();
        let skeleton = scan_bytes(&bytes);
        let ItemKind::Map(children) = &skeleton.items[0].kind else {
            panic!("a map");
        };
        KeyEquality {
            skeleton: &skeleton,
            bytes: &bytes,
        }
        .item_equal(children[0], children[2], &resources)
        .expect("compare")
    }

    /// Integers compare by mathematical value ACROSS marker widths: uint8 `5` and positive fixint `5` are ONE key.
    #[test]
    fn integers_compare_across_marker_widths() {
        assert!(map_keys_equal(&[0xcc, 0x05], &[0x05]));
        assert!(map_keys_equal(&[0xcd, 0x00, 0x05], &[0xcc, 0x05]));
        assert!(map_keys_equal(&[0xcf, 0, 0, 0, 0, 0, 0, 0, 5], &[0x05]));
        // Distinct values stay distinct.
        assert!(!map_keys_equal(&[0x05], &[0x06]));
        // Negative forms: -1 == -1, but -1 != 1.
        assert!(map_keys_equal(&[0xff], &[0xd0, 0xff]));
        assert!(!map_keys_equal(&[0xff], &[0x01]));
    }

    /// Floats compare only with floats: float32 `1.0` == float64 `1.0`, `-0.0 == +0.0`, all NaNs equal — and integer
    /// `1` stays distinct from float `1.0`.
    #[test]
    fn float_keys_and_the_integer_float_boundary() {
        assert!(map_keys_equal(
            &[0xca, 0x3f, 0x80, 0x00, 0x00],
            &[0xcb, 0x3f, 0xf0, 0, 0, 0, 0, 0, 0]
        ));
        assert!(map_keys_equal(
            &[0xcb, 0x80, 0, 0, 0, 0, 0, 0, 0],
            &[0xcb, 0, 0, 0, 0, 0, 0, 0, 0]
        ));
        assert!(map_keys_equal(
            &[0xca, 0x7f, 0xc0, 0x00, 0x00],
            &[0xcb, 0x7f, 0xf8, 0, 0, 0, 0, 0, 0]
        ));
        assert!(!map_keys_equal(&[0x01], &[0xca, 0x3f, 0x80, 0x00, 0x00]));
    }

    /// `str` by raw payload bytes, distinct from a byte-equal `bin`.
    #[test]
    fn str_is_distinct_from_a_byte_equal_bin() {
        assert!(map_keys_equal(&[0xa1, b'a'], &[0xa1, b'a']));
        assert!(!map_keys_equal(&[0xa1, b'a'], &[0xa1, b'b']));
        assert!(!map_keys_equal(&[0xa1, b'a'], &[0xc4, 0x01, b'a']));
        assert!(map_keys_equal(&[0xc4, 0x01, b'a'], &[0xc5, 0x00, 0x01, b'a']));
    }

    /// Extensions by signed type code plus exact raw payload; timestamps by their instant across encodings.
    #[test]
    fn extensions_and_timestamps() {
        assert!(map_keys_equal(&[0xd4, 0x01, 0xaa], &[0xc7, 0x01, 0x01, 0xaa]));
        assert!(!map_keys_equal(&[0xd4, 0x01, 0xaa], &[0xd4, 0x02, 0xaa]));
        assert!(!map_keys_equal(&[0xd4, 0x01, 0xaa], &[0xd4, 0x01, 0xab]));
        assert!(map_keys_equal(
            &[0xd6, 0xff, 0, 0, 0, 1],
            &[0xc7, 0x0c, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
        ));
        assert!(!map_keys_equal(&[0xd6, 0xff, 0, 0, 0, 1], &[0xd6, 0xff, 0, 0, 0, 2]));
    }

    /// Arrays compare in order; a map key that is a map compares as an unordered multiset of pairs.
    #[test]
    fn container_keys() {
        assert!(map_keys_equal(&[0x91, 0x01], &[0x91, 0x01]));
        assert!(!map_keys_equal(&[0x91, 0x01], &[0x91, 0x02]));
        assert!(!map_keys_equal(&[0x91, 0x01], &[0x92, 0x01, 0x02]));
        // `{a:1}` == `{a:1}` as a key regardless of member order.
        assert!(map_keys_equal(&[0x81, 0xa1, b'a', 0x01], &[0x81, 0xa1, b'a', 0x01]));
        assert!(!map_keys_equal(&[0x81, 0xa1, b'a', 0x01], &[0x81, 0xa1, b'a', 0x02]));
    }

    /// The whole-document law: under the dialect, a duplicate map key rejects; the base dialect preserves it (jqf's own
    /// duplicate law).
    #[test]
    fn the_dialect_rejects_a_duplicate_the_base_preserves() {
        // `{uint8 5: 1, fixint 5: 2}` — the same key under the law.
        let error = validate(&[0x82, 0xcc, 0x05, 0x01, 0x05, 0x02]).expect_err("duplicate integer key rejects");
        assert!(error.contains("duplicate-key"), "{error}");
        assert!(error.contains("key-equivalence"), "{error}");
        // A map with distinct keys passes.
        validate(&[0x82, 0xcc, 0x05, 0x01, 0x06, 0x02]).expect("distinct keys pass");
        validate(&[0x81, 0xa1, b'a', 0x01]).expect("one key passes");
        // Nested maps are validated too.
        let error = validate(&[0x81, 0xa1, b'm', 0x82, 0x01, 0x01, 0x01, 0x02]).expect_err("nested duplicate rejects");
        assert!(error.contains("duplicate-key"), "{error}");
    }
}
