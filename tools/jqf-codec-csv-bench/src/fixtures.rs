//! Deterministic CSV bench fixtures.
//!
//! The `EXPECTED` map pins each fixture's jqf record-route checksum so a
//! correctness drift is a hard preflight failure before timing. The checksums
//! are filled by the `pin_checksums` ignored test and then hard-pinned.

use std::fmt::Write as _;

pub(crate) struct Fixture {
    pub(crate) name: &'static str,
    pub(crate) source: &'static str,
}

fn row(width: usize, index: usize) -> String {
    let mut out = String::new();
    for column in 0..width {
        if column > 0 {
            out.push(',');
        }
        let _ = write!(out, "v{column}_{index}");
    }
    out.push('\n');
    out
}

fn header(width: usize) -> String {
    let mut out = String::new();
    for column in 0..width {
        if column > 0 {
            out.push(',');
        }
        let _ = write!(out, "col{column}");
    }
    out.push('\n');
    out
}

fn build(width: usize, rows: usize) -> String {
    let mut out = header(width);
    for index in 0..rows {
        out.push_str(&row(width, index));
    }
    out
}

pub(crate) fn fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            name: "small/3x4",
            source: "col0,col1,col2\na0,b0,c0\na1,b1,c1\na2,b2,c2\na3,b3,c3\n",
        },
        Fixture {
            name: "small/quoted",
            source: "col0,col1,col2\n\"a,0\",\"b 0\",c0\n\"say \"\"hi\"\"\",b1,c1\na2,\"multi\nline\",c2\n",
        },
        Fixture {
            name: "medium/10x500",
            source: Box::leak(build(10, 500).into_boxed_str()),
        },
        Fixture {
            name: "large/20x5000",
            source: Box::leak(build(20, 5000).into_boxed_str()),
        },
    ]
}

pub(crate) struct Expected {
    pub(crate) whole: u64,
    pub(crate) scoped: u64,
    pub(crate) shallow: u64,
    pub(crate) encoded: u64,
}

pub(crate) const EXPECTED: &[(&str, Expected)] = &[
    (
        "small/3x4",
        Expected {
            whole: 0x8f2c_b1d1_2cb1_0c46,
            scoped: 0xf40e_556a_0922_026c,
            shallow: 0x078d_d4fe_6ed6_362d,
            encoded: 0x1281_555b_2352_916e,
        },
    ),
    (
        "small/quoted",
        Expected {
            whole: 0xfebd_1e90_c211_113e,
            scoped: 0x0e04_aafa_193c_c665,
            shallow: 0x2a35_d0f5_8707_8dd6,
            encoded: 0xf836_5c48_fa87_0068,
        },
    ),
    (
        "medium/10x500",
        Expected {
            whole: 0x2a8c_d15f_6005_b683,
            scoped: 0x7053_1abd_ffd4_4a3f,
            shallow: 0x4712_cb61_a75d_ade3,
            encoded: 0xcae7_cd9b_9f5f_7204,
        },
    ),
    (
        "large/20x5000",
        Expected {
            whole: 0x596a_aa5b_9488_e773,
            scoped: 0x5edb_db5c_afdb_0a00,
            shallow: 0x6858_a20b_7d5d_51f1,
            encoded: 0x30c2_8e29_3588_da2a,
        },
    ),
];

#[must_use]
pub(crate) fn expected(name: &str) -> Expected {
    EXPECTED.iter().find(|(candidate, _)| *candidate == name).map_or(
        Expected {
            whole: 0,
            scoped: 0,
            shallow: 0,
            encoded: 0,
        },
        |(_, expected)| Expected {
            whole: expected.whole,
            scoped: expected.scoped,
            shallow: expected.shallow,
            encoded: expected.encoded,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "run with --ignored to regenerate the pinned checksums"]
    fn pin_checksums() {
        for fixture in fixtures() {
            let whole = crate::cases::pin_decode_checksum(fixture.source);
            let scoped = crate::cases::pin_scoped_checksum(fixture.source);
            let shallow = crate::cases::pin_shallow_checksum(fixture.source);
            let encoded = crate::cases::pin_encode_checksum(fixture.source);
            println!(
                "(\"{}\", {whole:#x}, {scoped:#x}, {shallow:#x}, {encoded:#x}),",
                fixture.name
            );
        }
    }
}
