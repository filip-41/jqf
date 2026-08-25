use std::fmt::Write as _;

use crate::build_manifest;

/// Frozen identity of a generated benchmark fixture.
#[derive(Clone, Copy)]
pub(crate) struct FixtureEvidence {
    /// Stable catalog identifier.
    pub(crate) id: &'static str,
    /// Revision of the fixture's generation law.
    pub(crate) revision: u32,
    /// FNV-1a hash of the exact source bytes.
    pub(crate) hash: u64,
}

pub(crate) const NESTED: FixtureEvidence = FixtureEvidence {
    id: "nested-catalog-v1",
    revision: 1,
    hash: 0x8cf9_8f28_763c_a366,
};

pub(crate) const ESCAPES: FixtureEvidence = FixtureEvidence {
    id: "escape-array-v1",
    revision: 1,
    hash: 0xc4a5_51ec_7bce_827d,
};

pub(crate) const WIDE_DUPLICATES: FixtureEvidence = FixtureEvidence {
    id: "wide-duplicate-object-v1",
    revision: 1,
    hash: 0x074f_256c_7835_ad53,
};

pub(crate) const ENCODE_NESTED: FixtureEvidence = FixtureEvidence {
    id: "owned-encode-nested-v1",
    revision: 1,
    hash: 0xbc26_18ba_06ef_7cb8,
};

pub(crate) const ENCODE_DEEP_64: FixtureEvidence = FixtureEvidence {
    id: "owned-encode-deep-64-v1",
    revision: 1,
    hash: 0xf7c6_e441_458b_cc64,
};

pub(crate) const ENCODE_DEEP_256: FixtureEvidence = FixtureEvidence {
    id: "owned-encode-deep-256-v1",
    revision: 1,
    hash: 0x54e6_9b7c_c0a6_14e4,
};

pub(crate) const ENCODE_ESCAPE_DENSE: FixtureEvidence = FixtureEvidence {
    id: "owned-encode-escape-dense-v1",
    revision: 1,
    hash: 0x00fe_7f13_9a49_b97c,
};

pub(crate) const ENCODE_ESCAPE_SPARSE: FixtureEvidence = FixtureEvidence {
    id: "owned-encode-escape-sparse-v1",
    revision: 1,
    hash: 0xc77c_f59c_1e34_261a,
};

pub(crate) fn nested() -> String {
    let mut value = String::from("{\"meta\":{\"version\":1},\"catalog\":[");
    for index in 0..512 {
        if index != 0 {
            value.push(',');
        }
        let _ = write!(
            value,
            "{{\"id\":{index},\"name\":\"item-{index}\",\"price\":{}.25,\"active\":true}}",
            index + 10
        );
    }
    value.push_str("]}");
    value
}

pub(crate) fn escapes() -> String {
    let mut value = String::from("[");
    for index in 0..1024 {
        if index != 0 {
            value.push(',');
        }
        value.push_str("\"line\\nquote\\\"slash\\\\music\\uD834\\uDD1E\"");
    }
    value.push(']');
    value
}

pub(crate) fn wide_duplicate_object() -> String {
    const UNIQUE: usize = 2_048;
    let mut value = String::from("{");
    for pass in 0..2 {
        for index in (0..UNIQUE).rev() {
            if pass != 0 || index != UNIQUE - 1 {
                value.push(',');
            }
            let _ = write!(value, "\"key-{index:04}\":{}", index + pass * UNIQUE);
        }
    }
    value.push('}');
    value
}

pub(crate) fn provenance() -> String {
    format!(
        "case_revision=1 {} allocation_stats={} command={}",
        build_manifest::provenance(),
        cfg!(feature = "allocation-stats"),
        std::env::args().collect::<Vec<_>>().join(" "),
    )
}

/// Returns the catalog hash and rejects accidental fixture drift before timing.
pub(crate) fn verify_fixture(evidence: FixtureEvidence, bytes: &[u8]) -> u64 {
    let actual = fnv1a64(bytes);
    assert_eq!(
        actual, evidence.hash,
        "fixture {} revision {} drifted; update its catalog revision rather than mutating evidence",
        evidence.id, evidence.revision
    );
    actual
}

/// Stable fixture hash used by receipts and the versioned catalog.
#[must_use]
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::{ESCAPES, NESTED, WIDE_DUPLICATES, escapes, nested, verify_fixture, wide_duplicate_object};

    #[test]
    fn frozen_fixture_catalog_matches_every_generator() {
        verify_fixture(NESTED, nested().as_bytes());
        verify_fixture(ESCAPES, escapes().as_bytes());
        verify_fixture(WIDE_DUPLICATES, wide_duplicate_object().as_bytes());
    }
}
