//! Numbers: machine integers, big integers, retained spellings, float render.
//!
//! Run with `cargo run -p jqf-data --example numbers`.

use jqf_data::{BigInt, Integer, Number, NumberCategory, format_binary64};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Numbers are UNACCOUNTED: no resource ledger is needed to construct them — every allocation is a spelling copy
    // of bytes the request already charged for (see the number module doc).

    // A literal past 2^53 stays exact — never rounded on the way in.
    let past_f64 = Number::try_json_literal("9007199254740993")?;
    assert_eq!(past_f64.to_i64(), Some(9_007_199_254_740_993));
    // It fits i64, so it lives on the inline machine arm: no allocation.
    assert_eq!(past_f64.as_machine(), Some(9_007_199_254_740_993));

    // A magnitude past i64 keeps its exact canonical digits.
    let wide = Number::try_json_literal("123456789012345678901234567890")?;
    assert_eq!(wide.to_i64(), None);
    assert_eq!(wide.category(), NumberCategory::Integer);
    let integer = wide.to_integer().expect("integer category");
    assert_eq!(integer.as_str(), "123456789012345678901234567890");

    // A decimal literal retains its source spelling: `1.000` is not `1`.
    let retained = Number::try_json_literal("1.000")?;
    assert_eq!(retained.category(), NumberCategory::Decimal);

    // `Integer::parse` normalizes program-literal spellings.
    assert_eq!(Integer::parse("007")?.as_str(), "7");
    assert_eq!(Integer::parse("-0")?.as_str(), "0");

    // `BigInt` is scratch arithmetic. Convert text in, compute, convert back out.
    let a = BigInt::parse("123456789012345678901234567890").ok_or("digits")?;
    let b = BigInt::parse("987654321098765432109876543210").ok_or("digits")?;
    let product = a.mul(&b);
    assert_eq!(
        product.to_decimal_string(),
        "121932631137021795226185032733622923332237463801111263526900"
    );
    let (quotient, remainder) = product.div_rem(&b).ok_or("nonzero divisor")?;
    assert_eq!(quotient.to_decimal_string(), a.to_decimal_string());
    assert!(remainder.is_zero());

    // Computed binary64 renders shortest-round-trip.
    let float = format_binary64(0.1_f64 + 0.2_f64).ok_or("finite")?;
    assert_eq!(float.as_str(), "0.30000000000000004");

    println!("exact integers, retained spellings, and float rendering all hold");
    Ok(())
}
