//! Deterministic TOML bench fixtures.
//!
//! The `EXPECTED` map pins each fixture's jqf semantic checksum so a decode
//! correctness drift is a hard preflight failure before timing. The encode
//! fixtures are derived from the same sources: decode once (at inventory
//! construction, outside timing) and encode once to pin the expected bytes.

pub(crate) struct Fixture {
    pub(crate) name: &'static str,
    pub(crate) source: &'static str,
}

pub(crate) const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "small/scalars",
        source: "title = \"TOML Example\"\ncount = 42\nratio = 3.14\nok = true\n",
    },
    Fixture {
        name: "small/nested-tables",
        source: "[server]\nhost = \"example.org\"\nport = 8080\n[server.tls]\nenabled = true\n",
    },
    Fixture {
        name: "small/arrays",
        source: "ports = [8000, 8001, 8002]\nnested = [[1, 2], [3, 4]]\n",
    },
    Fixture {
        name: "small/array-of-tables",
        source: "[[product]]\nname = \"Hammer\"\nsku = 738594937\n[[product]]\nname = \"Nail\"\nsku = 284758393\n",
    },
    Fixture {
        name: "small/inline-tables",
        source: "point = { x = 1, y = 2 }\nname = { first = \"Tom\", last = \"P\" }\n",
    },
    Fixture {
        name: "small/dotted-keys",
        source: "a.b.c = 1\nx.y.z.w = \"deep\"\n",
    },
    Fixture {
        name: "small/numbers",
        source: "hex = 0x1F\noct = 0o17\nbin = 0b101\nbig = 1_000_000\nneg = -17\nfloat = 1.5e2\ninf = inf\n",
    },
    Fixture {
        name: "small/temporals",
        source: "d1 = 1979-05-27\nt1 = 07:32:00\nodt = 1979-05-27T07:32:00Z\nldt = 1979-05-27T00:32:00.999999\n",
    },
    Fixture {
        name: "medium/mixed",
        source: "title = \"Config\"\n\n[owner]\nname = \"Tom\"\ndob = 1979-05-27\n\n[database]\nserver = \"192.168.1.1\"\nports = [8001, 8001, 8002]\nconnection_max = 5000\nenabled = true\n\n[servers.alpha]\nip = \"10.0.0.1\"\nrole = \"frontend\"\n\n[servers.beta]\nip = \"10.0.0.2\"\nrole = \"backend\"\n\n[[clients]]\nname = \"Hammer\"\n[[clients]]\nname = \"Nail\"\n",
    },
];

/// The large-catalog lane name and source.
pub(crate) const LARGE_CATALOG_NAME: &str = "large/catalog";

/// A >=1 MB TOML array-of-tables document, built deterministically at first
/// use and leaked (the CSV bench's fixture pattern).
///
/// The bench's largest fixture used to be ~380 B (`medium/mixed`), so a
/// decode-side lever had nothing to bite on — the fixed per-invocation
/// overhead dominated every lane. This is the same record shape the e2e
/// generator's
/// `toml-catalog-10mb.toml` uses (one `[[catalog]]` table per record, quoted
/// strings + plain scalars), sized so 20,000 records land at ~1 MB.
pub(crate) fn large_catalog_source() -> &'static str {
    use std::fmt::Write as _;
    let mut out = String::from("title = \"catalog\"\n\n");
    for index in 0..20_000 {
        out.push_str("[[catalog]]\n");
        let _ = write!(out, "id = {index}");
        out.push('\n');
        let _ = write!(out, "name = \"item-{index:06}\"");
        out.push('\n');
        let _ = write!(out, "stock = {}", index % 5000);
        out.push_str("\n\n");
    }
    Box::leak(out.into_boxed_str())
}

/// Pins each fixture's jqf semantic checksum. These are filled by the
/// `pin_checksums` ignored test; they are then hard-pinned and a drift is a
/// preflight failure.
pub(crate) const EXPECTED: &[(&str, u64)] = &[
    ("small/scalars", 0x7e5b_c71a_22dd_941c),
    ("small/nested-tables", 0x7ee3_70ca_beb7_043b),
    ("small/arrays", 0x7e53_9cd2_9242_b14b),
    ("small/array-of-tables", 0x7e53_9cdc_7d87_d342),
    ("small/inline-tables", 0x7e53_94c5_eab2_c00b),
    ("small/dotted-keys", 0x7e53_9c90_84b0_8121),
    ("small/numbers", 0x3821_7f4e_263e_090a),
    ("small/temporals", 0x7e53_98e4_164b_4f00),
    ("medium/mixed", 0xe711_184c_245d_661c),
    ("large/catalog", 0x877d_bd43_4028_4b11),
    // Capability-roadmap route lanes over the medium fixture: the shallow
    // root stand-in, the scoped `.owner.name`, and the clients count.
    ("medium/mixed/shallow-root", 0x797c_b249_ccb0_950a),
    ("medium/mixed/scoped-owner-name", 0x7e53_9c90_8444_d8ac),
    ("medium/mixed/count-clients", 0x2),
    ("medium/mixed/stream-clients", 0x7e53_9c92_843a_2803),
    ("medium/mixed/projected-clients", 0x7e53_9c92_843a_2803),
];

/// Looks up a fixture's pinned expected checksum.
#[must_use]
pub(crate) fn expected_checksum(name: &str) -> u64 {
    EXPECTED
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map_or(0, |(_, checksum)| *checksum)
}

/// Regenerates the pinned checksums. Run with `cargo test -- --ignored` after
/// changing a fixture, then paste the printed values into `EXPECTED`.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "run with --ignored to regenerate the pinned checksums"]
    fn pin_checksums() {
        for fixture in FIXTURES {
            // Recompute via the crate's decode lane so the pins always agree
            // with the measured path.
            let checksum = crate::cases::pin_decode_checksum(fixture.source);
            println!("(\"{}\", {checksum:#x}),", fixture.name);
        }
        let large = large_catalog_source();
        let checksum = crate::cases::pin_decode_checksum(large);
        println!("(\"{LARGE_CATALOG_NAME}\", {checksum:#x}),");
    }
}
