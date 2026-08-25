//! The jqf IP/CIDR family.
//!
//! Five pure value laws over the piped string: `ip_valid/0`, `ip_version/0`, `ip_class/0`, `ip_canonical/0`, and the
//! argument-taking `ip_in_cidr/1`.
//! An address is parsed with `core::net`'s own `FromStr` (available in `no_std`), so trailing whitespace, bracketed
//! `[::1]` forms, and embedded zone indices are all rejected exactly the way `core` rejects them.
//!
//! Classification checks the address types' convenience predicates in the family's precedence order —
//! `is_unspecified`/`is_loopback`/`is_link_local`/ `is_private`/`is_multicast`/`is_broadcast` — plus two hand-rolled
//! segment masks: IPv6 has no broadcast slot, and its link-local, private, and multicast ranges fall back to the
//! segment arms (`fc00::/7`, `fe80::/10`, `ff00::/8`) beside the two predicates it does use. The precedence law
//! survives calling `is_private` directly because std's predicate is strictly RFC 1918 / RFC 4193 and never overlaps
//! the documented ranges checked afterwards. Every range is documented at its rule.
//!
//! Negative space: no host dependencies, no `std`. The only `core` surfaces used are `core::net::{IpAddr, Ipv4Addr,
//! Ipv6Addr}`, `FromStr`, and the two `to_bits` extractors.

use alloc::format;
use alloc::string::ToString;

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use jqf_data::{Number, Value};
use jqf_resource::ResourceContext;

use super::id;
use crate::error::EngineRunError;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::path::raise;

/// The ip-law discriminants, one per evaluator shape.
#[derive(Clone, Copy, Debug)]
pub enum NetLaw {
    /// `ip_valid/0` — `true` iff the string parses as an IPv4 or IPv6 address.
    Valid,
    /// `ip_version/0` — `4` or `6`; invalid input raises.
    Version,
    /// `ip_class/0` — the classification string checked in precedence order.
    Class,
    /// `ip_canonical/0` — the canonical text form (RFC 5952 for IPv6).
    Canonical,
    /// `ip_in_cidr/1` — whether the piped address is inside the CIDR argument.
    InCidr,
}

/// Parses `text` as an IP address, or `None` when it is not one.
fn parse_ip(text: &str) -> Option<IpAddr> {
    text.parse().ok()
}

/// The IPv4 class rule, one `if` per precedence slot. The first matching rule wins; the final fallthrough is `global`.
fn classify_v4(ip: Ipv4Addr) -> &'static str {
    // The six predicates `Ipv4Addr` ships, exactly as `classify_v6` below already calls them; only the documentation
    // arm stays hand-rolled.
    if ip.is_unspecified() {
        return "unspecified";
    }
    if ip.is_loopback() {
        return "loopback";
    }
    if ip.is_link_local() {
        return "link_local";
    }
    if ip.is_private() {
        return "private";
    }
    if ip.is_multicast() {
        return "multicast";
    }
    if ip.is_broadcast() {
        return "broadcast";
    }
    let o = ip.octets();
    // documentation: RFC 5737 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24.
    if (o[0] == 192 && o[1] == 0 && o[2] == 2)
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113)
    {
        return "documentation";
    }
    "global"
}

/// The IPv6 class rule, one `if` per precedence slot. IPv6 has no broadcast address, so the broadcast slot never
/// matches here.
fn classify_v6(ip: &Ipv6Addr) -> &'static str {
    let s = ip.segments();
    // unspecified: :: (RFC 4291).
    if ip.is_unspecified() {
        return "unspecified";
    }
    // loopback: ::1 (RFC 4291).
    if ip.is_loopback() {
        return "loopback";
    }
    // link-local: fe80::/10 (RFC 4291).
    if s[0] & 0xffc0 == 0xfe80 {
        return "link_local";
    }
    // private: unique-local fc00::/7 (RFC 4193).
    if s[0] & 0xfe00 == 0xfc00 {
        return "private";
    }
    // multicast: ff00::/8 (RFC 4291).
    if s[0] & 0xff00 == 0xff00 {
        return "multicast";
    }
    // documentation: 2001:db8::/32 (RFC 3849) and 3fff::/20 (RFC 9637).
    if (s[0] == 0x2001 && s[1] == 0x0db8) || (s[0] == 0x3fff && s[1] & 0xf000 == 0) {
        return "documentation";
    }
    "global"
}

/// The `ip_class` answer for one parsed address, in the promised precedence order (unspecified, loopback, `link_local`,
/// private, multicast, broadcast, documentation, global — first match wins).
fn ip_class_str(ip: &IpAddr) -> &'static str {
    match *ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(&v6),
    }
}

/// The IPv4 membership test: mask the top `prefix` bits of both addresses and compare. A `prefix` past 32 is out of
/// range (`None`).
fn ipv4_in_cidr(input: Ipv4Addr, network: Ipv4Addr, prefix: u8) -> Option<bool> {
    if prefix > 32 {
        return None;
    }
    let mask = if prefix == 0 {
        0u32
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    Some(input.to_bits() & mask == network.to_bits() & mask)
}

/// The IPv6 membership test on the 128-bit address bits; a `prefix` past 128 is out of range (`None`).
fn ipv6_in_cidr(input: Ipv6Addr, network: Ipv6Addr, prefix: u8) -> Option<bool> {
    if prefix > 128 {
        return None;
    }
    if prefix == 0 {
        return Some(true);
    }
    // The 128-bit mask covers the top `prefix` bits of the address bits.
    let mask = u128::MAX << (128 - u32::from(prefix));
    Some(input.to_bits() & mask == network.to_bits() & mask)
}

/// The `ip_in_cidr` membership answer for one piped address and one CIDR argument. `None` means the CIDR is malformed,
/// its prefix is out of range, or the two sides name different address families.
fn ip_in_cidr(ip: IpAddr, cidr: &str) -> Option<bool> {
    let (network_text, prefix_text) = cidr.split_once('/')?;
    let prefix: u8 = prefix_text.parse().ok()?;
    match (parse_ip(network_text)?, ip) {
        (IpAddr::V4(cidr), IpAddr::V4(input)) => ipv4_in_cidr(input, cidr, prefix),
        (IpAddr::V6(cidr), IpAddr::V6(input)) => ipv6_in_cidr(input, cidr, prefix),
        // Address-family mismatch between the piped address and the CIDR's network address is a refusal, not a `false`.
        _ => None,
    }
}

fn invalid_ip(text: &str, resources: &ResourceContext<'_>) -> EngineRunError {
    raise(&format!("invalid IP address \"{text}\""), resources)
}

fn invalid_cidr(text: &str, resources: &ResourceContext<'_>) -> EngineRunError {
    raise(&format!("invalid CIDR \"{text}\""), resources)
}

/// One net-law evaluation for exactly one tuple: the piped `subject` (its whole value) and the argument tuple `args`
/// (`ip_in_cidr`'s CIDR, or empty for the four arity-0 laws). The caller owns argument EVALUATION — it runs each
/// parameter's filter over the call's input and calls this law once per combination — so this function never reasons
/// about cardinality.
///
/// # Errors
///
/// Returns a catchable refusal for an invalid address (`ip_version`, `ip_class`, `ip_canonical`, and the piped side of
/// `ip_in_cidr`), an invalid CIDR argument, an out-of-range prefix, or an address-family mismatch.
pub fn net_law(
    law: NetLaw,
    subject: &Value,
    args: &[Value],
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let Value::String(text) = subject.untagged() else {
        return Err(raise(
            match law {
                NetLaw::Valid => "ip_valid requires a string input",
                NetLaw::Version => "ip_version requires a string input",
                NetLaw::Class => "ip_class requires a string input",
                NetLaw::Canonical => "ip_canonical requires a string input",
                NetLaw::InCidr => "ip_in_cidr requires a string input",
            },
            resources,
        ));
    };
    match law {
        NetLaw::Valid => Ok(Value::Bool(parse_ip(text.as_str()).is_some())),
        NetLaw::Version => {
            let ip = parse_ip(text.as_str()).ok_or_else(|| invalid_ip(text.as_str(), resources))?;
            let version = match ip {
                IpAddr::V4(_) => 4,
                IpAddr::V6(_) => 6,
            };
            Ok(Value::Number(Number::integer(jqf_data::Integer::from_i64(version))))
        }
        NetLaw::Class => {
            let ip = parse_ip(text.as_str()).ok_or_else(|| invalid_ip(text.as_str(), resources))?;
            let class = ip_class_str(&ip);
            Value::try_string(class).map_err(|_| EngineRunError::allocation_failure())
        }
        NetLaw::Canonical => {
            let ip = parse_ip(text.as_str()).ok_or_else(|| invalid_ip(text.as_str(), resources))?;
            let canonical = ip.to_string();
            Value::try_string(&canonical).map_err(|_| EngineRunError::allocation_failure())
        }
        NetLaw::InCidr => {
            let ip = parse_ip(text.as_str()).ok_or_else(|| invalid_ip(text.as_str(), resources))?;
            let cidr = match args.first() {
                Some(Value::String(cidr)) => cidr.as_str(),
                _ => {
                    return Err(raise("ip_in_cidr requires a CIDR string argument", resources));
                }
            };
            let member = ip_in_cidr(ip, cidr).ok_or_else(|| invalid_cidr(cidr, resources))?;
            Ok(Value::Bool(member))
        }
    }
}

// ------------------------------------------------------------------------
// Registry records.

const ONE_FILTER: &[ParameterKind] = &[ParameterKind::Filter];

const fn family(id: u16, name: &'static str, summary: &'static str, detail: &'static str) -> BuiltinFamilyRecord {
    BuiltinFamilyRecord {
        id: BuiltinFamilyId::new(id),
        canonical_name: name,
        category: "jqf-net",
        summary,
        detail,
    }
}

const fn example(program: &'static str, input: &'static str, expected: &'static str) -> BuiltinExample {
    BuiltinExample {
        program,
        input,
        expected,
    }
}

const fn overload0(
    id: u16,
    family_id: u16,
    name: &'static str,
    examples: &'static [BuiltinExample],
) -> BuiltinOverloadRecord {
    BuiltinOverloadRecord {
        id: BuiltinOverloadId::new(id),
        family: BuiltinFamilyId::new(family_id),
        canonical_name: name,
        arity: 0,
        parameters: &[],
        execution: BuiltinExecution::Evaluator,
        demand_transfer: DemandTransfer::Subtree,
        semantic_revision: SemanticRevision::new(1),
        effects: Effects::Pure,
        examples,
    }
}

const fn overload1(
    id: u16,
    family_id: u16,
    name: &'static str,
    examples: &'static [BuiltinExample],
) -> BuiltinOverloadRecord {
    BuiltinOverloadRecord {
        id: BuiltinOverloadId::new(id),
        family: BuiltinFamilyId::new(family_id),
        canonical_name: name,
        arity: 1,
        parameters: ONE_FILTER,
        execution: BuiltinExecution::Evaluator,
        demand_transfer: DemandTransfer::Subtree,
        semantic_revision: SemanticRevision::new(1),
        effects: Effects::Pure,
        examples,
    }
}

const IP_VALID_FAMILY: BuiltinFamilyRecord = family(
    id::IP_VALID_FAMILY_ID,
    "ip_valid",
    "True iff the string parses as an IPv4 or IPv6 address.",
    "",
);
const IP_VERSION_FAMILY: BuiltinFamilyRecord = family(
    id::IP_VERSION_FAMILY_ID,
    "ip_version",
    "The address family of a string address: 4 or 6.",
    "",
);
const IP_CLASS_FAMILY: BuiltinFamilyRecord = family(
    id::IP_CLASS_FAMILY_ID,
    "ip_class",
    "The classification of a string address (loopback, private, …).",
    "",
);
const IP_CANONICAL_FAMILY: BuiltinFamilyRecord = family(
    id::IP_CANONICAL_FAMILY_ID,
    "ip_canonical",
    "The canonical text form of a string address.",
    "",
);
const IP_IN_CIDR_FAMILY: BuiltinFamilyRecord = family(
    id::IP_IN_CIDR_FAMILY_ID,
    "ip_in_cidr",
    "Whether a string address lies inside a CIDR block.",
    "",
);

pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    IP_VALID_FAMILY,
    IP_VERSION_FAMILY,
    IP_CLASS_FAMILY,
    IP_CANONICAL_FAMILY,
    IP_IN_CIDR_FAMILY,
];

const IP_VALID_OVERLOAD: BuiltinOverloadRecord = overload0(
    id::IP_VALID,
    id::IP_VALID_FAMILY_ID,
    "ip_valid",
    &[
        example("ip_valid", "\"127.0.0.1\"", "true\n"),
        example("ip_valid", "\"not-an-ip\"", "false\n"),
    ],
);
const IP_VERSION_OVERLOAD: BuiltinOverloadRecord = overload0(
    id::IP_VERSION,
    id::IP_VERSION_FAMILY_ID,
    "ip_version",
    &[
        example("ip_version", "\"10.1.2.3\"", "4\n"),
        example("ip_version", "\"2001:db8::1\"", "6\n"),
    ],
);
const IP_CLASS_OVERLOAD: BuiltinOverloadRecord = overload0(
    id::IP_CLASS,
    id::IP_CLASS_FAMILY_ID,
    "ip_class",
    &[
        example("ip_class", "\"127.0.0.1\"", "\"loopback\"\n"),
        example("ip_class", "\"10.1.2.3\"", "\"private\"\n"),
        example("ip_class", "\"2001:db8::1\"", "\"documentation\"\n"),
    ],
);
const IP_CANONICAL_OVERLOAD: BuiltinOverloadRecord = overload0(
    id::IP_CANONICAL,
    id::IP_CANONICAL_FAMILY_ID,
    "ip_canonical",
    &[
        example("ip_canonical", "\"2001:0db8:0000:0000::0001\"", "\"2001:db8::1\"\n"),
        example("ip_canonical", "\"2001:db8:0:0:0:0:2:1\"", "\"2001:db8::2:1\"\n"),
    ],
);
const IP_IN_CIDR_OVERLOAD: BuiltinOverloadRecord = overload1(
    id::IP_IN_CIDR,
    id::IP_IN_CIDR_FAMILY_ID,
    "ip_in_cidr",
    &[
        example("ip_in_cidr(\"192.168.0.0/16\")", "\"192.168.0.1\"", "true\n"),
        example("ip_in_cidr(\"10.0.0.0/8\")", "\"192.168.0.1\"", "false\n"),
        example("ip_in_cidr(\"2001:db8::/32\")", "\"2001:db8::1\"", "true\n"),
    ],
);

pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    IP_VALID_OVERLOAD,
    IP_VERSION_OVERLOAD,
    IP_CLASS_OVERLOAD,
    IP_CANONICAL_OVERLOAD,
    IP_IN_CIDR_OVERLOAD,
];

/// The IP/CIDR execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
pub const PAYLOADS: &[(u16, NetLaw)] = &[
    (id::IP_VALID, NetLaw::Valid),
    (id::IP_VERSION, NetLaw::Version),
    (id::IP_CLASS, NetLaw::Class),
    (id::IP_CANONICAL, NetLaw::Canonical),
    (id::IP_IN_CIDR, NetLaw::InCidr),
];

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(1).expect("work"),
        )
        .expect("resources")
    }

    fn string(text: &str, _resources: &ResourceContext<'static>) -> Value {
        Value::try_string(text).expect("string")
    }

    /// One IPv4 address from its octets, for the tests.
    fn v4(octets: [u8; 4]) -> Ipv4Addr {
        Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3])
    }

    #[test]
    fn in_cidr_masks_address_families() {
        assert!(ip_in_cidr(v4([192, 168, 0, 1]).into(), "192.168.0.0/16").unwrap());
        assert!(!ip_in_cidr(v4([192, 168, 0, 1]).into(), "10.0.0.0/8").unwrap());
        assert!(ip_in_cidr("fe80::1".parse().unwrap(), "fe80::/10").unwrap());
        assert!(ip_in_cidr("2001:db8::1".parse().unwrap(), "2001:db8::/32").unwrap());
        // A host part in the CIDR's network address is masked away.
        assert!(ip_in_cidr(v4([10, 1, 2, 3]).into(), "10.9.9.9/8").unwrap());
        // Family mismatch and an out-of-range prefix refuse.
        assert!(ip_in_cidr(v4([192, 168, 0, 1]).into(), "2001:db8::/32").is_none());
        assert!(ip_in_cidr(v4([192, 168, 0, 1]).into(), "10.0.0.0/33").is_none());
        assert!(ip_in_cidr("fe80::1".parse().unwrap(), "fe80::/129").is_none());
    }

    #[test]
    fn version_is_4_or_6() {
        let resources = resources();
        let four = net_law(NetLaw::Version, &string("10.1.2.3", &resources), &[], &resources).expect("v4");
        let Value::Number(four) = four.untagged() else {
            panic!("expected a number");
        };
        assert_eq!(four.to_i64(), Some(4));
        let six = net_law(NetLaw::Version, &string("2001:db8::1", &resources), &[], &resources).expect("v6");
        let Value::Number(six) = six.untagged() else {
            panic!("expected a number");
        };
        assert_eq!(six.to_i64(), Some(6));
    }

    #[test]
    fn valid_records_and_rejects() {
        let resources = resources();
        let ok = net_law(NetLaw::Valid, &string("::1", &resources), &[], &resources).expect("valid");
        assert!(matches!(ok.untagged(), Value::Bool(true)));
        let bad = net_law(NetLaw::Valid, &string("not-an-ip", &resources), &[], &resources).expect("invalid");
        assert!(matches!(bad.untagged(), Value::Bool(false)));
    }
}
