//! The math family: every math builtin the reference's `builtins` lists.
//!
//! One job: own the family/overload records AND the per-law evaluators the executor dispatches through. Every law is a
//! pure function of its operand values — the /0 forms read the piped number, the /2 and /3 forms read their
//! parenthesized filters over the same input and IGNORE the piped value (`"x" | hypot(3;4)` is `5`, exactly as the
//! reference answers) — so the records carry [`DemandTransfer::Subtree`] throughout: a coarser declaration is always
//! sound, and none of these laws can honestly claim a shallower read.
//!
//! The number law: the reference computes every math result in binary64 and PUBLISHES the value's shortest-round-trip
//! text, but its number model has no printable infinity — an infinite result prints as `±1.7976931348623157e+308`
//! (the widest finite binary64), and a NaN result prints as `null` while STILL being a number (`nan | type` is
//! `"number"`, `nan | isnan` is `true`). Those two spellings live in `jqf-data`'s `format_binary64`, shared with the
//! JSON encoder, so this module never clamps: it calls libm and wraps the f64. A wrong-type operand raises the
//! reference's `TYPE ("value") number required` — the one math error this module owns — and a domain failure is
//! NEVER that error: it flows through libm as NaN (`(-1)|sqrt` → `null`, exit 0).
//!
//! `abs` is the single deliberate exception to the dispatch shape, because the reference defines it in the language
//! itself, not C: `def abs: if . < 0 then - . else . end;`. Under the total order that means strings, arrays and
//! objects PASS THROUGH, null and booleans raise the negate refusal (`null (null) cannot be negated`), and numbers are
//! re-signed exactly — a huge integer keeps its exact digits. The `isnan`/`isinfinite`/`isfinite`/`isnormal` quartet
//! is the second exception: the reference defines them over `type == "number" and …`, so a non-number input answers
//! `false` instead of raising.

#[cfg(not(unix))]
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

use jqf_data::{Array, Float, Integer, Number, Value};
use jqf_resource::ResourceContext;

use super::id;
use crate::error::{EngineRunError, message};
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::depth::{self, Guarded};
use crate::semantics::order;
use crate::semantics::path::raise;

// ------------------------------------------------------------------------
// Platform libm backend.
//
// The reference links the platform C library, and its math answers are that library's last ulp. The pure-Rust `libm`
// port is the same algorithm family but can differ at one ulp: `1|exp` is `2.718281828459045` under the reference and
// `2.7182818284590455` under the Rust port, and `1|expm1`, `0.5|asin`, `1|tan`, `1|cosh`, `2|erfc` and `5|lgamma` all
// diverge the same way. The family must byte-match the reference, so on unix every law calls the SAME C library the
// reference calls; elsewhere it falls back to the Rust port, which is the closest portable implementation.
#[cfg(unix)]
mod platform {
    #[link(name = "m")]
    unsafe extern "C" {
        #[link_name = "fabs"]
        fn c_fabs(value: f64) -> f64;
        #[link_name = "floor"]
        fn c_floor(value: f64) -> f64;
        #[link_name = "ceil"]
        fn c_ceil(value: f64) -> f64;
        #[link_name = "round"]
        fn c_round(value: f64) -> f64;
        #[link_name = "trunc"]
        fn c_trunc(value: f64) -> f64;
        #[link_name = "rint"]
        fn c_rint(value: f64) -> f64;
        #[link_name = "sqrt"]
        fn c_sqrt(value: f64) -> f64;
        #[link_name = "cbrt"]
        fn c_cbrt(value: f64) -> f64;
        #[link_name = "exp"]
        fn c_exp(value: f64) -> f64;
        #[link_name = "expm1"]
        fn c_expm1(value: f64) -> f64;
        #[link_name = "exp2"]
        fn c_exp2(value: f64) -> f64;
        #[link_name = "log"]
        fn c_log(value: f64) -> f64;
        #[link_name = "log1p"]
        fn c_log1p(value: f64) -> f64;
        #[link_name = "log2"]
        fn c_log2(value: f64) -> f64;
        #[link_name = "log10"]
        fn c_log10(value: f64) -> f64;
        #[link_name = "erf"]
        fn c_erf(value: f64) -> f64;
        #[link_name = "erfc"]
        fn c_erfc(value: f64) -> f64;
        #[link_name = "sin"]
        fn c_sin(value: f64) -> f64;
        #[link_name = "cos"]
        fn c_cos(value: f64) -> f64;
        #[link_name = "tan"]
        fn c_tan(value: f64) -> f64;
        #[link_name = "sinh"]
        fn c_sinh(value: f64) -> f64;
        #[link_name = "cosh"]
        fn c_cosh(value: f64) -> f64;
        #[link_name = "tanh"]
        fn c_tanh(value: f64) -> f64;
        #[link_name = "asin"]
        fn c_asin(value: f64) -> f64;
        #[link_name = "acos"]
        fn c_acos(value: f64) -> f64;
        #[link_name = "atan"]
        fn c_atan(value: f64) -> f64;
        #[link_name = "asinh"]
        fn c_asinh(value: f64) -> f64;
        #[link_name = "acosh"]
        fn c_acosh(value: f64) -> f64;
        #[link_name = "atanh"]
        fn c_atanh(value: f64) -> f64;
        #[link_name = "tgamma"]
        fn c_tgamma(value: f64) -> f64;
        #[link_name = "lgamma_r"]
        fn c_lgamma_r(value: f64, sign: *mut core::ffi::c_int) -> f64;
        #[link_name = "hypot"]
        fn c_hypot(left: f64, right: f64) -> f64;
        #[link_name = "pow"]
        fn c_pow(left: f64, right: f64) -> f64;
        #[link_name = "atan2"]
        fn c_atan2(left: f64, right: f64) -> f64;
        #[link_name = "fmod"]
        fn c_fmod(left: f64, right: f64) -> f64;
        #[link_name = "copysign"]
        fn c_copysign(left: f64, right: f64) -> f64;
        #[link_name = "remainder"]
        fn c_remainder(left: f64, right: f64) -> f64;
        #[link_name = "fdim"]
        fn c_fdim(left: f64, right: f64) -> f64;
        #[link_name = "fmin"]
        fn c_fmin(left: f64, right: f64) -> f64;
        #[link_name = "fmax"]
        fn c_fmax(left: f64, right: f64) -> f64;
        #[link_name = "ldexp"]
        fn c_ldexp(left: f64, exponent: core::ffi::c_int) -> f64;
        #[link_name = "nextafter"]
        fn c_nextafter(left: f64, right: f64) -> f64;
        #[link_name = "frexp"]
        fn c_frexp(value: f64, exponent: *mut core::ffi::c_int) -> f64;
        #[link_name = "modf"]
        fn c_modf(value: f64, integral: *mut f64) -> f64;
        #[link_name = "ilogb"]
        fn c_ilogb(value: f64) -> core::ffi::c_int;
        #[link_name = "fma"]
        fn c_fma(left: f64, middle: f64, right: f64) -> f64;
        #[link_name = "j0"]
        fn c_j0(value: f64) -> f64;
        #[link_name = "j1"]
        fn c_j1(value: f64) -> f64;
        #[link_name = "jn"]
        fn c_jn(order: core::ffi::c_int, value: f64) -> f64;
        #[link_name = "y0"]
        fn c_y0(value: f64) -> f64;
        #[link_name = "y1"]
        fn c_y1(value: f64) -> f64;
        #[link_name = "yn"]
        fn c_yn(order: core::ffi::c_int, value: f64) -> f64;
    }

    /// One safe wrapper per one-argument law, generated for the FFI block so the call sites stay ordinary `platform::`
    /// calls.
    macro_rules! unary {
        ($name:ident, $c:ident) => {
            pub(super) fn $name(value: f64) -> f64 {
                // SAFETY: the declaration above matches the C library's own
                // signature for this symbol, and libm's value-only laws are
                // total over `f64` (a domain failure is a NaN result, never
                // undefined behaviour), so there is no precondition to violate.
                unsafe { $c(value) }
            }
        };
    }
    macro_rules! binary {
        ($name:ident, $c:ident) => {
            pub(super) fn $name(left: f64, right: f64) -> f64 {
                // SAFETY: the declaration above matches the C library's own
                // signature for this symbol, and libm's value-only laws are
                // total over `f64` (a domain failure is a NaN result, never
                // undefined behaviour), so there is no precondition to violate.
                unsafe { $c(left, right) }
            }
        };
    }
    unary!(fabs, c_fabs);
    unary!(floor, c_floor);
    unary!(ceil, c_ceil);
    unary!(round, c_round);
    unary!(trunc, c_trunc);
    unary!(rint, c_rint);
    unary!(sqrt, c_sqrt);
    unary!(cbrt, c_cbrt);
    unary!(exp, c_exp);
    unary!(expm1, c_expm1);
    unary!(exp2, c_exp2);
    unary!(log, c_log);
    unary!(log1p, c_log1p);
    unary!(log2, c_log2);
    unary!(log10, c_log10);
    unary!(erf, c_erf);
    unary!(erfc, c_erfc);
    unary!(sin, c_sin);
    unary!(cos, c_cos);
    unary!(tan, c_tan);
    unary!(sinh, c_sinh);
    unary!(cosh, c_cosh);
    unary!(tanh, c_tanh);
    unary!(asin, c_asin);
    unary!(acos, c_acos);
    unary!(atan, c_atan);
    unary!(asinh, c_asinh);
    unary!(acosh, c_acosh);
    unary!(atanh, c_atanh);
    unary!(tgamma, c_tgamma);
    unary!(j0, c_j0);
    unary!(j1, c_j1);
    unary!(y0, c_y0);
    unary!(y1, c_y1);
    binary!(hypot, c_hypot);
    binary!(pow, c_pow);
    binary!(atan2, c_atan2);
    binary!(fmod, c_fmod);
    binary!(copysign, c_copysign);
    binary!(remainder, c_remainder);
    binary!(fdim, c_fdim);
    binary!(fmin, c_fmin);
    binary!(fmax, c_fmax);
    binary!(nextafter, c_nextafter);

    /// The `[log, sign]` pair of `lgamma_r`, the sign through an out-parameter.
    ///
    /// # Safety
    ///
    /// `sign` must be valid for one aligned, exclusive write of a `c_int`.
    pub(super) unsafe fn lgamma_r(value: f64, sign: *mut core::ffi::c_int) -> f64 {
        // SAFETY: the declaration matches the C library's signature, and this
        // function's `# Safety` contract requires `sign` to be writable for
        // the one `c_int` the callee writes.
        unsafe { c_lgamma_r(value, sign) }
    }

    pub(super) fn fma(left: f64, middle: f64, right: f64) -> f64 {
        // SAFETY: the declaration matches the C library's signature, and the
        // law is total over `f64` with no pointer operand.
        unsafe { c_fma(left, middle, right) }
    }

    pub(super) fn ldexp(left: f64, exponent: core::ffi::c_int) -> f64 {
        // SAFETY: the declaration matches the C library's signature, and the
        // law is total over `f64`/`int` with no pointer operand.
        unsafe { c_ldexp(left, exponent) }
    }

    /// The fraction of `frexp`, the exponent through an out-parameter.
    ///
    /// # Safety
    ///
    /// `exponent` must be valid for one aligned, exclusive write of a `c_int`.
    pub(super) unsafe fn frexp(value: f64, exponent: *mut core::ffi::c_int) -> f64 {
        // SAFETY: the declaration matches the C library's signature, and this
        // function's `# Safety` contract requires `exponent` to be writable
        // for the one `c_int` the callee writes.
        unsafe { c_frexp(value, exponent) }
    }

    /// The fraction of `modf`, the integral part through an out-parameter.
    ///
    /// # Safety
    ///
    /// `integral` must be valid for one aligned, exclusive write of an `f64`.
    pub(super) unsafe fn modf(value: f64, integral: *mut f64) -> f64 {
        // SAFETY: the declaration matches the C library's signature, and this
        // function's `# Safety` contract requires `integral` to be writable
        // for the one `f64` the callee writes.
        unsafe { c_modf(value, integral) }
    }

    pub(super) fn ilogb(value: f64) -> core::ffi::c_int {
        // SAFETY: the declaration matches the C library's signature, and the
        // law is total over `f64` (the degenerate inputs answer FP_ILOGB0 /
        // INT_MAX rather than trapping) with no pointer operand.
        unsafe { c_ilogb(value) }
    }

    /// `jn(order, value)`: the order is C's `int`, so the f64 truncates toward zero before the ABI call (`jn(1.9;2)` ==
    /// `jn(1;2)`).
    pub(super) fn jn(order: core::ffi::c_int, value: f64) -> f64 {
        // SAFETY: the declaration matches the C library's signature, and the
        // law is total over `int`/`f64` with no pointer operand.
        unsafe { c_jn(order, value) }
    }

    /// `yn(order, value)`, the same truncating-order ABI law as `jn`.
    pub(super) fn yn(order: core::ffi::c_int, value: f64) -> f64 {
        // SAFETY: the declaration matches the C library's signature, and the
        // law is total over `int`/`f64` with no pointer operand.
        unsafe { c_yn(order, value) }
    }
}

/// The non-unix fallback: the pure-Rust port of the same algorithm family.
#[cfg(not(unix))]
mod platform {
    pub(super) use libm::{
        acos, acosh, asin, asinh, atan, atan2, atanh, cbrt, ceil, copysign, cos, cosh, erf, erfc, exp, exp2, expm1,
        fabs, fdim, floor, fma, fmax, fmin, fmod, frexp, hypot, ilogb, ldexp, lgamma_r, log, log1p, log2, log10, modf,
        nextafter, pow, remainder, rint, round, sin, sinh, sqrt, tan, tanh, tgamma, trunc,
    };
}
// ------------------------------------------------------------------------
// Law discriminants, one per evaluator shape.

/// The unary laws the reference spells as /0 pipe builtins (plus `abs`, whose law is the reference's own `def`, and the
/// /0 rounding/transcendental core).
#[derive(Clone, Copy, Debug)]
pub enum MathLaw {
    Abs,
    Fabs,
    Floor,
    Ceil,
    Round,
    Trunc,
    Rint,
    Nearbyint,
    Sqrt,
    Cbrt,
    Exp,
    Expm1,
    Exp2,
    Exp10,
    Log,
    Log1p,
    Log2,
    Log10,
    Erf,
    Erfc,
    /// `j0/0` — Bessel J of order 0 (libm).
    J0,
    /// `j1/0` — Bessel J of order 1 (libm).
    J1,
    /// `y0/0` — Bessel Y of order 0 (libm).
    Y0,
    /// `y1/0` — Bessel Y of order 1 (libm).
    Y1,
}

/// The twelve /0 trigonometric and inverse-trigonometric laws.
#[derive(Clone, Copy, Debug)]
pub enum MathLawTrig {
    Sin,
    Cos,
    Tan,
    Sinh,
    Cosh,
    Tanh,
    Asin,
    Acos,
    Atan,
    Asinh,
    Acosh,
    Atanh,
}

/// The gamma family: `gamma`/`tgamma` (one name, two spellings), `lgamma` (the signed-magnitude log), and `lgamma_r`
/// (the `[log, sign]` pair).
#[derive(Clone, Copy, Debug)]
pub enum MathLawGamma {
    Gamma,
    Tgamma,
    Lgamma,
    LgammaR,
}

/// The /0 special-value laws. `frexp` and `modf` publish two-element arrays (`[fraction, exponent]` and `[fraction,
/// integral]` — the reference's order), which is why they cannot be plain scalar laws.
#[derive(Clone, Copy, Debug)]
pub enum MathLawSpecial {
    Significand,
    Logb,
    Frexp,
    Modf,
}

/// The number-predicate quartet. All four answer `false` for a non-number input (`"x" | isnan` is `false`, not an
/// error) and implement the reference's own definitions over the f64 value: `isnan` is NaN-ness, `isinfinite` is IEEE
/// infinity (which the reference's `0 | log` produces before clamping the PRINT), `isfinite` is `isinfinite | not` —
/// so `nan | isfinite` is TRUE, the reference's surprising law — and `isnormal` is IEEE normal (`nan`, `0`,
/// infinities and subnormals answer `false`).
#[derive(Clone, Copy, Debug)]
pub enum MathLawCheck {
    Isnan,
    Isinfinite,
    Isfinite,
    IsNormal,
}

/// The two /0 constructible special values. Both IGNORE the piped input (`"x" | infinite` is
/// `1.7976931348623157e+308`).
#[derive(Clone, Copy, Debug)]
pub enum MathLawConst {
    Nan,
    Infinite,
}

/// The fifteen /2 laws. The argument order is the reference's: both filters are evaluated over the same input and the
/// piped value is ignored.
#[derive(Clone, Copy, Debug)]
pub enum MathLawBin {
    Hypot,
    Pow,
    Atan2,
    Fmod,
    Copysign,
    Remainder,
    Drem,
    Fdim,
    Fmin,
    Fmax,
    Ldexp,
    Scalbln,
    Scalb,
    Nexttoward,
    Nextafter,
    /// `jn/2` — Bessel J of the (truncated) order argument (libm).
    Jn,
    /// `yn/2` — Bessel Y of the (truncated) order argument (libm).
    Yn,
}

/// The one /3 law: `fma(x;y;z)` = `x*y + z` with a single rounding.
#[derive(Clone, Copy, Debug)]
pub enum MathLawTern {
    Fma,
}

/// The evaluator payload the registry dispatches for every math overload.
#[derive(Clone, Copy, Debug)]
pub enum MathEvaluator {
    Unary(MathLaw),
    Trig(MathLawTrig),
    Gamma(MathLawGamma),
    Special(MathLawSpecial),
    Check(MathLawCheck),
    Const(MathLawConst),
    Bin(MathLawBin),
    Tern(MathLawTern),
}

// ------------------------------------------------------------------------
// Number plumbing.

fn number_from_f64(value: f64) -> Value {
    Value::Number(Number::float(Float::new(value)))
}

/// Reads one operand as f64 or raises jq's `TYPE ("value") number required`.
fn number_as_f64_or_raise(input: &Value, resources: &ResourceContext<'_>) -> Result<f64, EngineRunError> {
    if let Value::Number(number) = input.untagged() {
        Ok(order::to_f64(number))
    } else {
        let text = message::number_required(input)?;
        Err(raise(&text, resources))
    }
}

// ------------------------------------------------------------------------
// Unary laws.

pub fn apply_unary(law: MathLaw, subject: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    if let MathLaw::Abs = law {
        return apply_abs(subject, resources);
    }
    let value = number_as_f64_or_raise(subject, resources)?;
    Ok(number_from_f64(match law {
        MathLaw::Fabs => platform::fabs(value),
        MathLaw::Floor => platform::floor(value),
        MathLaw::Ceil => platform::ceil(value),
        // The reference's `round` is C's `round`: half AWAY from zero — `(2.5|round)` is `3`, `(-2.5|round)` is `-3`.
        // `rint`/`nearbyint` are C's `rint`: half to EVEN — `(2.5|rint)` is `2`. The two families diverge at every
        // `.5`, and both are corpus rows.
        MathLaw::Round => platform::round(value),
        MathLaw::Trunc => platform::trunc(value),
        MathLaw::Rint | MathLaw::Nearbyint => platform::rint(value),
        MathLaw::Sqrt => platform::sqrt(value),
        MathLaw::Cbrt => platform::cbrt(value),
        MathLaw::Exp => platform::exp(value),
        MathLaw::Expm1 => platform::expm1(value),
        MathLaw::Exp2 => platform::exp2(value),
        MathLaw::Exp10 => platform::pow(10.0, value),
        MathLaw::Log => platform::log(value),
        MathLaw::Log1p => platform::log1p(value),
        MathLaw::Log2 => platform::log2(value),
        MathLaw::Log10 => platform::log10(value),
        MathLaw::Erf => platform::erf(value),
        MathLaw::Erfc => platform::erfc(value),
        MathLaw::J0 | MathLaw::J1 | MathLaw::Y0 | MathLaw::Y1 => {
            return apply_bessel_unary(law, value, resources);
        }
        MathLaw::Abs => unreachable!("abs handled before the f64 read"),
    }))
}

/// The four /0 Bessel laws. On unix they call the SAME libm the reference links (byte parity is the point); elsewhere
/// they raise the reference's own build-time absence message, because the Rust `libm` port has no Bessel family.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the evaluator family returns Result uniformly; the non-unix arm raises the reference's               build-time absence message"
)]
#[cfg_attr(
    not(unix),
    allow(
        unused_variables,
        reason = "the /0 bessel law's piped value is consumed only by the unix (libm) arm; \
                   the non-unix arm raises the reference's build-time absence message"
    )
)]
fn apply_bessel_unary(law: MathLaw, value: f64, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    #[cfg(unix)]
    {
        Ok(number_from_f64(match law {
            MathLaw::J0 => platform::j0(value),
            MathLaw::J1 => platform::j1(value),
            MathLaw::Y0 => platform::y0(value),
            MathLaw::Y1 => platform::y1(value),
            _ => unreachable!("bessel dispatch over a non-bessel law"),
        }))
    }
    #[cfg(not(unix))]
    {
        let name = match law {
            MathLaw::J0 => "j0",
            MathLaw::J1 => "j1",
            MathLaw::Y0 => "y0",
            MathLaw::Y1 => "y1",
            _ => unreachable!("bessel dispatch over a non-bessel law"),
        };
        let text = format!("Error: {name}/0 not found at build time");
        Err(raise(&text, _resources))
    }
}

/// The reference's `def abs: if . < 0 then - . else . end;`, transcribed onto the exact number model: an exact integer
/// or decimal keeps its digits (`-123…|abs` prints them back), a float is re-signed, and the pass-through arm returns
/// strings/arrays/objects unchanged while the negate arm raises the reference's `cannot be negated` for null and
/// booleans.
fn apply_abs(subject: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let zero = Value::Number(Number::integer(Integer::from_i64(0)));
    let ordering =
        order::total_cmp(subject, &zero).map_err(|_| raise(depth::message(Guarded::Comparison), resources))?;
    if ordering == Ordering::Less {
        if let Value::Number(number) = subject.untagged() {
            Ok(Value::Number(
                number.try_negated().map_err(|_| EngineRunError::allocation_failure())?,
            ))
        } else {
            // `null` and the booleans rank below every number, so the reference's `if . < 0` takes the negate arm and
            // refuses with its message.
            let operand = message::dump_trunc_owned(subject)?;
            let text = message::not_negatable_message(subject.kind(), &operand)?;
            Err(raise(&text, resources))
        }
    } else {
        Ok(subject.clone())
    }
}

pub fn apply_trig(law: MathLawTrig, subject: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let value = number_as_f64_or_raise(subject, resources)?;
    Ok(number_from_f64(match law {
        MathLawTrig::Sin => platform::sin(value),
        MathLawTrig::Cos => platform::cos(value),
        MathLawTrig::Tan => platform::tan(value),
        MathLawTrig::Sinh => platform::sinh(value),
        MathLawTrig::Cosh => platform::cosh(value),
        MathLawTrig::Tanh => platform::tanh(value),
        MathLawTrig::Asin => platform::asin(value),
        MathLawTrig::Acos => platform::acos(value),
        MathLawTrig::Atan => platform::atan(value),
        MathLawTrig::Asinh => platform::asinh(value),
        MathLawTrig::Acosh => platform::acosh(value),
        MathLawTrig::Atanh => platform::atanh(value),
    }))
}

pub fn apply_gamma(
    law: MathLawGamma,
    subject: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let value = number_as_f64_or_raise(subject, resources)?;
    match law {
        // `gamma` and `tgamma` are the same law under two reference names; both are libm's tgamma, so poles flow
        // through as infinities (`0|gamma` prints the clamped widest binary64) and negative-integer poles as NaN
        // (`(-1)|tgamma` prints `null`).
        MathLawGamma::Gamma | MathLawGamma::Tgamma => Ok(number_from_f64(platform::tgamma(value))),
        MathLawGamma::Lgamma => Ok(number_from_f64(lgamma(value))),
        MathLawGamma::LgammaR => {
            let (log, sign) = lgamma_r(value);
            array_value(
                vec![
                    number_from_f64(log),
                    Value::Number(Number::integer(Integer::from_i64(i64::from(sign)))),
                ],
                resources,
            )
        }
    }
}

pub fn apply_special(
    law: MathLawSpecial,
    subject: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let value = number_as_f64_or_raise(subject, resources)?;
    match law {
        MathLawSpecial::Significand => Ok(number_from_f64(significand(value))),
        MathLawSpecial::Logb => Ok(number_from_f64(logb(value))),
        MathLawSpecial::Frexp => {
            let (fraction, exponent) = frexp_pair(value);
            array_value(
                vec![
                    number_from_f64(fraction),
                    Value::Number(Number::integer(Integer::from_i64(i64::from(exponent)))),
                ],
                resources,
            )
        }
        MathLawSpecial::Modf => {
            let (fraction, integral) = modf_pair(value);
            array_value(vec![number_from_f64(fraction), number_from_f64(integral)], resources)
        }
    }
}

/// The gamma family's platform-libm calls.
///
/// The reference links the platform C library, so its `tgamma`/`lgamma` answer the PLATFORM's last ulp, and the
/// pure-Rust `libm` port can differ at one ulp
fn lgamma(value: f64) -> f64 {
    lgamma_r(value).0
}

fn lgamma_r(value: f64) -> (f64, i32) {
    #[cfg(unix)]
    {
        let mut sign = 0;
        // SAFETY: `&raw mut sign` points at a live, initialized `c_int` local
        // this call holds exclusively, satisfying the shim's contract.
        let log = unsafe { platform::lgamma_r(value, &raw mut sign) };
        (log, sign)
    }
    #[cfg(not(unix))]
    {
        platform::lgamma_r(value)
    }
}

/// The fraction/exponent pair of `frexp`, with the platform signature hidden.
fn frexp_pair(value: f64) -> (f64, i32) {
    #[cfg(unix)]
    {
        let mut exponent = 0;
        // SAFETY: `&raw mut exponent` points at a live, initialized `c_int`
        // local this call holds exclusively, satisfying the shim's contract.
        let fraction = unsafe { platform::frexp(value, &raw mut exponent) };
        (fraction, exponent)
    }
    #[cfg(not(unix))]
    {
        platform::frexp(value)
    }
}

/// The fraction/integral pair of `modf`, with the platform signature hidden.
fn modf_pair(value: f64) -> (f64, f64) {
    #[cfg(unix)]
    {
        let mut integral = 0.0;
        // SAFETY: `&raw mut integral` points at a live, initialized `f64`
        // local this call holds exclusively, satisfying the shim's contract.
        let fraction = unsafe { platform::modf(value, &raw mut integral) };
        (fraction, integral)
    }
    #[cfg(not(unix))]
    {
        platform::modf(value)
    }
}

/// The base-2 exponent of `ilogb` as `i32`.
///
/// The per-platform selection lives in the two `mod platform` bodies (the C shim on unix, `libm` elsewhere) — both
/// expose the SAME `ilogb` signature, so this wrapper is a single call with no cfg split of its own. The two libraries'
/// degenerate answers differ (`FP_ILOGBNAN` is `INT_MAX` in C on macOS but `INT_MIN` in libm), but [`logb`] guards
/// zero/NaN/infinity before this runs, so only finite non-zero values reach it, where both libraries agree (the arms
/// used to be byte-identical under two `#[cfg]`s, which read as a selection that was not there).
fn ilogb(value: f64) -> i32 {
    platform::ilogb(value)
}

/// The `significand` law: `x / 2^logb(x)` — for `8` that is `1`, for `0` it is `0`, and NaN/infinities pass through.
fn significand(value: f64) -> f64 {
    if value == 0.0 || value.is_nan() || value.is_infinite() {
        value
    } else {
        frexp_pair(value).0 * 2.0
    }
}

/// The `logb` law: the unbiased base-2 exponent — `8|logb` is `3`, `0.5|logb` is `-1`, `0|logb` is `-infinity`
/// (printed clamped), and NaN/±infinity answer their own magnitude.
fn logb(value: f64) -> f64 {
    if value == 0.0 {
        f64::NEG_INFINITY
    } else if value.is_nan() || value.is_infinite() {
        value.abs()
    } else {
        f64::from(ilogb(value))
    }
}

pub fn apply_check(law: MathLawCheck, subject: &Value) -> Value {
    let answer = match subject.untagged() {
        Value::Number(number) => {
            let value = order::to_f64(number);
            match law {
                MathLawCheck::Isnan => value.is_nan(),
                MathLawCheck::Isinfinite => value.is_infinite(),
                MathLawCheck::Isfinite => !value.is_infinite(),
                MathLawCheck::IsNormal => value.is_normal(),
            }
        }
        _ => false,
    };
    Value::Bool(answer)
}

pub fn apply_const(law: MathLawConst) -> Value {
    match law {
        MathLawConst::Nan => number_from_f64(f64::NAN),
        MathLawConst::Infinite => number_from_f64(f64::INFINITY),
    }
}

// ------------------------------------------------------------------------
// Binary and ternary laws.

pub fn apply_bin(
    law: MathLawBin,
    lhs: &Value,
    rhs: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let lhs = number_as_f64_or_raise(lhs, resources)?;
    let rhs = number_as_f64_or_raise(rhs, resources)?;
    Ok(number_from_f64(match law {
        MathLawBin::Hypot => platform::hypot(lhs, rhs),
        MathLawBin::Pow => platform::pow(lhs, rhs),
        MathLawBin::Atan2 => platform::atan2(lhs, rhs),
        MathLawBin::Fmod => platform::fmod(lhs, rhs),
        MathLawBin::Copysign => platform::copysign(lhs, rhs),
        MathLawBin::Remainder | MathLawBin::Drem => platform::remainder(lhs, rhs),
        MathLawBin::Fdim => platform::fdim(lhs, rhs),
        MathLawBin::Fmin => platform::fmin(lhs, rhs),
        MathLawBin::Fmax => platform::fmax(lhs, rhs),
        // The reference's `ldexp`/`scalbln`/`scalb` truncate the exponent toward zero (`ldexp(1;0.5)` is `1`,
        // `ldexp(1;3.9)` is `8`), clamping to the i32 range.
        MathLawBin::Ldexp | MathLawBin::Scalbln => platform::ldexp(lhs, truncating_i32_arg(rhs)),
        // `scalb/2` alone answers NaN for a NaN exponent: `[scalb(1;nan)]` is `[null]` while `[ldexp(1;nan)]` is `[1]`.
        // The reference's `scalb` is libm's double-exponent form, whose NaN propagates; the i32-truncating siblings
        // coerce the exponent toward zero first.
        MathLawBin::Scalb => {
            if rhs.is_nan() {
                f64::NAN
            } else {
                platform::ldexp(lhs, truncating_i32_arg(rhs))
            }
        }
        // `nexttoward` and `nextafter` are the same ULP walk in the reference's binary64 world (`nexttoward(1;2)` and
        // `nextafter(1;2)` both answer `1.0000000000000002`).
        MathLawBin::Nexttoward | MathLawBin::Nextafter => platform::nextafter(lhs, rhs),
        MathLawBin::Jn | MathLawBin::Yn => {
            return apply_bessel_bin(law, lhs, rhs, resources);
        }
    }))
}

/// The two /2 Bessel laws. The ORDER argument converts with C's double→int truncation (`jn(1.9;2)` is `jn(1;2)`); the
/// piped value is ignored, exactly like every other /2 math law.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the evaluator family returns Result uniformly; the non-unix arm raises the reference's               build-time absence message"
)]
#[cfg_attr(
    not(unix),
    allow(
        unused_variables,
        reason = "the /2 bessel law's order and piped value are consumed only by the unix \
                   (libm) arm; the non-unix arm raises the reference's build-time absence message"
    )
)]
fn apply_bessel_bin(
    law: MathLawBin,
    order: f64,
    value: f64,
    _resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    #[cfg(unix)]
    {
        Ok(number_from_f64(match law {
            MathLawBin::Jn => platform::jn(truncating_i32_arg(order), value),
            MathLawBin::Yn => platform::yn(truncating_i32_arg(order), value),
            _ => unreachable!("bessel dispatch over a non-bessel law"),
        }))
    }
    #[cfg(not(unix))]
    {
        let name = match law {
            MathLawBin::Jn => "jn",
            MathLawBin::Yn => "yn",
            _ => unreachable!("bessel dispatch over a non-bessel law"),
        };
        let text = format!("Error: {name}/2 not found at build time");
        Err(raise(&text, _resources))
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "the range checks inside bound the truncated value to the i32 range before the cast"
)]
fn truncating_i32_arg(value: f64) -> i32 {
    if value.is_nan() {
        0
    } else if value >= f64::from(i32::MAX) {
        i32::MAX
    } else if value <= f64::from(i32::MIN) {
        i32::MIN
    } else {
        platform::trunc(value) as i32
    }
}

pub fn apply_tern(
    law: MathLawTern,
    x: &Value,
    y: &Value,
    z: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let x = number_as_f64_or_raise(x, resources)?;
    let y = number_as_f64_or_raise(y, resources)?;
    let z = number_as_f64_or_raise(z, resources)?;
    Ok(number_from_f64(match law {
        MathLawTern::Fma => platform::fma(x, y, z),
    }))
}

/// Builds one owned array from owned element values, mapping allocation failure onto the machine's codec error.
fn array_value(elements: Vec<Value>, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut array = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    for element in elements {
        array
            .try_push(element)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    Ok(Value::Array(array))
}

// ------------------------------------------------------------------------
// Registry records.

const TWO_FILTERS: &[ParameterKind] = &[ParameterKind::Filter, ParameterKind::Filter];
const THREE_FILTERS: &[ParameterKind] = &[ParameterKind::Filter, ParameterKind::Filter, ParameterKind::Filter];

macro_rules! family {
    ($id:expr, $canonical_name:literal) => {
        BuiltinFamilyRecord {
            id: BuiltinFamilyId::new($id),
            canonical_name: $canonical_name,
            category: "math",
            summary: concat!("The ", $canonical_name, " family."),
            detail: "",
        }
    };
}

macro_rules! overload {
    ($id:expr, $name:literal, $family_id:expr, $program:literal, $input:literal, $expected:expr) => {
        BuiltinOverloadRecord {
            id: BuiltinOverloadId::new($id),
            family: BuiltinFamilyId::new($family_id),
            canonical_name: $name,
            arity: 0,
            parameters: &[],
            execution: BuiltinExecution::Evaluator,
            demand_transfer: DemandTransfer::Subtree,
            semantic_revision: SemanticRevision::new(1),
            effects: Effects::Pure,
            examples: &[BuiltinExample {
                program: $program,
                input: $input,
                expected: $expected,
            }],
        }
    };
}

macro_rules! overload2 {
    ($id:expr, $name:literal, $family_id:expr, $program:literal, $input:literal, $expected:literal) => {
        BuiltinOverloadRecord {
            id: BuiltinOverloadId::new($id),
            family: BuiltinFamilyId::new($family_id),
            canonical_name: $name,
            arity: 2,
            parameters: TWO_FILTERS,
            execution: BuiltinExecution::Evaluator,
            demand_transfer: DemandTransfer::Subtree,
            semantic_revision: SemanticRevision::new(1),
            effects: Effects::Pure,
            examples: &[BuiltinExample {
                program: $program,
                input: $input,
                expected: $expected,
            }],
        }
    };
}

macro_rules! overload3 {
    ($id:expr, $name:literal, $family_id:expr, $program:literal, $input:literal, $expected:literal) => {
        BuiltinOverloadRecord {
            id: BuiltinOverloadId::new($id),
            family: BuiltinFamilyId::new($family_id),
            canonical_name: $name,
            arity: 3,
            parameters: THREE_FILTERS,
            execution: BuiltinExecution::Evaluator,
            demand_transfer: DemandTransfer::Subtree,
            semantic_revision: SemanticRevision::new(1),
            effects: Effects::Pure,
            examples: &[BuiltinExample {
                program: $program,
                input: $input,
                expected: $expected,
            }],
        }
    };
}

const ABS_FAMILY: BuiltinFamilyRecord = family!(id::ABS_FAMILY_ID, "abs");
const FABS_FAMILY: BuiltinFamilyRecord = family!(id::FABS_FAMILY_ID, "fabs");
const FLOOR_FAMILY: BuiltinFamilyRecord = family!(id::FLOOR_FAMILY_ID, "floor");
const CEIL_FAMILY: BuiltinFamilyRecord = family!(id::CEIL_FAMILY_ID, "ceil");
const ROUND_FAMILY: BuiltinFamilyRecord = family!(id::ROUND_FAMILY_ID, "round");
const TRUNC_FAMILY: BuiltinFamilyRecord = family!(id::TRUNC_FAMILY_ID, "trunc");
const RINT_FAMILY: BuiltinFamilyRecord = family!(id::RINT_FAMILY_ID, "rint");
const NEARBYINT_FAMILY: BuiltinFamilyRecord = family!(id::NEARBYINT_FAMILY_ID, "nearbyint");
const SQRT_FAMILY: BuiltinFamilyRecord = family!(id::SQRT_FAMILY_ID, "sqrt");
const CBRT_FAMILY: BuiltinFamilyRecord = family!(id::CBRT_FAMILY_ID, "cbrt");
const EXP_FAMILY: BuiltinFamilyRecord = family!(id::EXP_FAMILY_ID, "exp");
const EXPM1_FAMILY: BuiltinFamilyRecord = family!(id::EXPM1_FAMILY_ID, "expm1");
const EXP2_FAMILY: BuiltinFamilyRecord = family!(id::EXP2_FAMILY_ID, "exp2");
const EXP10_FAMILY: BuiltinFamilyRecord = family!(id::EXP10_FAMILY_ID, "exp10");
const LOG_FAMILY: BuiltinFamilyRecord = family!(id::LOG_FAMILY_ID, "log");
const LOG1P_FAMILY: BuiltinFamilyRecord = family!(id::LOG1P_FAMILY_ID, "log1p");
const LOG2_FAMILY: BuiltinFamilyRecord = family!(id::LOG2_FAMILY_ID, "log2");
const LOG10_FAMILY: BuiltinFamilyRecord = family!(id::LOG10_FAMILY_ID, "log10");
const ERF_FAMILY: BuiltinFamilyRecord = family!(id::ERF_FAMILY_ID, "erf");
const ERFC_FAMILY: BuiltinFamilyRecord = family!(id::ERFC_FAMILY_ID, "erfc");
const SIN_FAMILY: BuiltinFamilyRecord = family!(id::SIN_FAMILY_ID, "sin");
const COS_FAMILY: BuiltinFamilyRecord = family!(id::COS_FAMILY_ID, "cos");
const TAN_FAMILY: BuiltinFamilyRecord = family!(id::TAN_FAMILY_ID, "tan");
const SINH_FAMILY: BuiltinFamilyRecord = family!(id::SINH_FAMILY_ID, "sinh");
const COSH_FAMILY: BuiltinFamilyRecord = family!(id::COSH_FAMILY_ID, "cosh");
const TANH_FAMILY: BuiltinFamilyRecord = family!(id::TANH_FAMILY_ID, "tanh");
const ASIN_FAMILY: BuiltinFamilyRecord = family!(id::ASIN_FAMILY_ID, "asin");
const ACOS_FAMILY: BuiltinFamilyRecord = family!(id::ACOS_FAMILY_ID, "acos");
const ATAN_FAMILY: BuiltinFamilyRecord = family!(id::ATAN_FAMILY_ID, "atan");
const ASINH_FAMILY: BuiltinFamilyRecord = family!(id::ASINH_FAMILY_ID, "asinh");
const ACOSH_FAMILY: BuiltinFamilyRecord = family!(id::ACOSH_FAMILY_ID, "acosh");
const ATANH_FAMILY: BuiltinFamilyRecord = family!(id::ATANH_FAMILY_ID, "atanh");
const GAMMA_FAMILY: BuiltinFamilyRecord = family!(id::GAMMA_FAMILY_ID, "gamma");
const LGAMMA_FAMILY: BuiltinFamilyRecord = family!(id::LGAMMA_FAMILY_ID, "lgamma");
const LGAMMA_R_FAMILY: BuiltinFamilyRecord = family!(id::LGAMMA_R_FAMILY_ID, "lgamma_r");
const SIGNIFICAND_FAMILY: BuiltinFamilyRecord = family!(id::SIGNIFICAND_FAMILY_ID, "significand");
const LOGB_FAMILY: BuiltinFamilyRecord = family!(id::LOGB_FAMILY_ID, "logb");
const FREXP_FAMILY: BuiltinFamilyRecord = family!(id::FREXP_FAMILY_ID, "frexp");
const MODF_FAMILY: BuiltinFamilyRecord = family!(id::MODF_FAMILY_ID, "modf");
const NAN_FAMILY: BuiltinFamilyRecord = family!(id::NAN_FAMILY_ID, "nan");
const INFINITE_FAMILY: BuiltinFamilyRecord = family!(id::INFINITE_FAMILY_ID, "infinite");
const ISNAN_FAMILY: BuiltinFamilyRecord = family!(id::ISNAN_FAMILY_ID, "isnan");
const ISINFINITE_FAMILY: BuiltinFamilyRecord = family!(id::ISINFINITE_FAMILY_ID, "isinfinite");
const ISFINITE_FAMILY: BuiltinFamilyRecord = family!(id::ISFINITE_FAMILY_ID, "isfinite");
const ISNORMAL_FAMILY: BuiltinFamilyRecord = family!(id::ISNORMAL_FAMILY_ID, "isnormal");
const HYPOT_FAMILY: BuiltinFamilyRecord = family!(id::HYPOT_FAMILY_ID, "hypot");
const POW_FAMILY: BuiltinFamilyRecord = family!(id::POW_FAMILY_ID, "pow");
const ATAN2_FAMILY: BuiltinFamilyRecord = family!(id::ATAN2_FAMILY_ID, "atan2");
const FMOD_FAMILY: BuiltinFamilyRecord = family!(id::FMOD_FAMILY_ID, "fmod");
const COPYSIGN_FAMILY: BuiltinFamilyRecord = family!(id::COPYSIGN_FAMILY_ID, "copysign");
const REMAINDER_FAMILY: BuiltinFamilyRecord = family!(id::REMAINDER_FAMILY_ID, "remainder");
const DREM_FAMILY: BuiltinFamilyRecord = family!(id::DREM_FAMILY_ID, "drem");
const FDIM_FAMILY: BuiltinFamilyRecord = family!(id::FDIM_FAMILY_ID, "fdim");
const FMIN_FAMILY: BuiltinFamilyRecord = family!(id::FMIN_FAMILY_ID, "fmin");
const FMAX_FAMILY: BuiltinFamilyRecord = family!(id::FMAX_FAMILY_ID, "fmax");
const LDEXP_FAMILY: BuiltinFamilyRecord = family!(id::LDEXP_FAMILY_ID, "ldexp");
const SCALBLN_FAMILY: BuiltinFamilyRecord = family!(id::SCALBLN_FAMILY_ID, "scalbln");
const SCALB_FAMILY: BuiltinFamilyRecord = family!(id::SCALB_FAMILY_ID, "scalb");
const NEXTTOWARD_FAMILY: BuiltinFamilyRecord = family!(id::NEXTTOWARD_FAMILY_ID, "nexttoward");
const NEXTAFTER_FAMILY: BuiltinFamilyRecord = family!(id::NEXTAFTER_FAMILY_ID, "nextafter");
const FMA_FAMILY: BuiltinFamilyRecord = family!(id::FMA_FAMILY_ID, "fma");
const J0_FAMILY: BuiltinFamilyRecord = family!(id::J0_FAMILY_ID, "j0");
const J1_FAMILY: BuiltinFamilyRecord = family!(id::J1_FAMILY_ID, "j1");
const JN_FAMILY: BuiltinFamilyRecord = family!(id::JN_FAMILY_ID, "jn");
const Y0_FAMILY: BuiltinFamilyRecord = family!(id::Y0_FAMILY_ID, "y0");
const Y1_FAMILY: BuiltinFamilyRecord = family!(id::Y1_FAMILY_ID, "y1");
const YN_FAMILY: BuiltinFamilyRecord = family!(id::YN_FAMILY_ID, "yn");

pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    ABS_FAMILY,
    FABS_FAMILY,
    FLOOR_FAMILY,
    CEIL_FAMILY,
    ROUND_FAMILY,
    TRUNC_FAMILY,
    RINT_FAMILY,
    NEARBYINT_FAMILY,
    SQRT_FAMILY,
    CBRT_FAMILY,
    EXP_FAMILY,
    EXPM1_FAMILY,
    EXP2_FAMILY,
    EXP10_FAMILY,
    LOG_FAMILY,
    LOG1P_FAMILY,
    LOG2_FAMILY,
    LOG10_FAMILY,
    ERF_FAMILY,
    ERFC_FAMILY,
    SIN_FAMILY,
    COS_FAMILY,
    TAN_FAMILY,
    SINH_FAMILY,
    COSH_FAMILY,
    TANH_FAMILY,
    ASIN_FAMILY,
    ACOS_FAMILY,
    ATAN_FAMILY,
    ASINH_FAMILY,
    ACOSH_FAMILY,
    ATANH_FAMILY,
    GAMMA_FAMILY,
    LGAMMA_FAMILY,
    LGAMMA_R_FAMILY,
    SIGNIFICAND_FAMILY,
    LOGB_FAMILY,
    FREXP_FAMILY,
    MODF_FAMILY,
    NAN_FAMILY,
    INFINITE_FAMILY,
    ISNAN_FAMILY,
    ISINFINITE_FAMILY,
    ISFINITE_FAMILY,
    ISNORMAL_FAMILY,
    HYPOT_FAMILY,
    POW_FAMILY,
    ATAN2_FAMILY,
    FMOD_FAMILY,
    COPYSIGN_FAMILY,
    REMAINDER_FAMILY,
    DREM_FAMILY,
    FDIM_FAMILY,
    FMIN_FAMILY,
    FMAX_FAMILY,
    LDEXP_FAMILY,
    SCALBLN_FAMILY,
    SCALB_FAMILY,
    NEXTTOWARD_FAMILY,
    NEXTAFTER_FAMILY,
    FMA_FAMILY,
    J0_FAMILY,
    J1_FAMILY,
    JN_FAMILY,
    Y0_FAMILY,
    Y1_FAMILY,
    YN_FAMILY,
];

const ABS_OVERLOAD: BuiltinOverloadRecord = overload!(id::ABS, "abs", id::ABS_FAMILY_ID, "abs", "-3", "3\n");
const FABS_OVERLOAD: BuiltinOverloadRecord = overload!(id::FABS, "fabs", id::FABS_FAMILY_ID, "fabs", "-1.5", "1.5\n");
const FLOOR_OVERLOAD: BuiltinOverloadRecord = overload!(id::FLOOR, "floor", id::FLOOR_FAMILY_ID, "floor", "2.7", "2\n");
const CEIL_OVERLOAD: BuiltinOverloadRecord = overload!(id::CEIL, "ceil", id::CEIL_FAMILY_ID, "ceil", "2.1", "3\n");
const ROUND_OVERLOAD: BuiltinOverloadRecord = overload!(id::ROUND, "round", id::ROUND_FAMILY_ID, "round", "2.5", "3\n");
const TRUNC_OVERLOAD: BuiltinOverloadRecord =
    overload!(id::TRUNC, "trunc", id::TRUNC_FAMILY_ID, "trunc", "-0.9", "-0\n");
const RINT_OVERLOAD: BuiltinOverloadRecord = overload!(id::RINT, "rint", id::RINT_FAMILY_ID, "rint", "2.5", "2\n");
const NEARBYINT_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::NEARBYINT,
    "nearbyint",
    id::NEARBYINT_FAMILY_ID,
    "nearbyint",
    "-2.5",
    "-2\n"
);
const SQRT_OVERLOAD: BuiltinOverloadRecord = overload!(id::SQRT, "sqrt", id::SQRT_FAMILY_ID, "sqrt", "16", "4\n");
const CBRT_OVERLOAD: BuiltinOverloadRecord = overload!(id::CBRT, "cbrt", id::CBRT_FAMILY_ID, "cbrt", "27", "3\n");
const EXP_OVERLOAD: BuiltinOverloadRecord = overload!(id::EXP, "exp", id::EXP_FAMILY_ID, "exp", "0", "1\n");
const EXPM1_OVERLOAD: BuiltinOverloadRecord = overload!(id::EXPM1, "expm1", id::EXPM1_FAMILY_ID, "expm1", "0", "0\n");
const EXP2_OVERLOAD: BuiltinOverloadRecord = overload!(id::EXP2, "exp2", id::EXP2_FAMILY_ID, "exp2", "3", "8\n");
const EXP10_OVERLOAD: BuiltinOverloadRecord = overload!(id::EXP10, "exp10", id::EXP10_FAMILY_ID, "exp10", "2", "100\n");
const LOG_OVERLOAD: BuiltinOverloadRecord = overload!(id::LOG, "log", id::LOG_FAMILY_ID, "log", "1", "0\n");
const LOG1P_OVERLOAD: BuiltinOverloadRecord = overload!(id::LOG1P, "log1p", id::LOG1P_FAMILY_ID, "log1p", "0", "0\n");
const LOG2_OVERLOAD: BuiltinOverloadRecord = overload!(id::LOG2, "log2", id::LOG2_FAMILY_ID, "log2", "8", "3\n");
const LOG10_OVERLOAD: BuiltinOverloadRecord = overload!(id::LOG10, "log10", id::LOG10_FAMILY_ID, "log10", "100", "2\n");
const ERF_OVERLOAD: BuiltinOverloadRecord = overload!(id::ERF, "erf", id::ERF_FAMILY_ID, "erf", "0", "0\n");
const ERFC_OVERLOAD: BuiltinOverloadRecord = overload!(id::ERFC, "erfc", id::ERFC_FAMILY_ID, "erfc", "0", "1\n");
const SIN_OVERLOAD: BuiltinOverloadRecord = overload!(id::SIN, "sin", id::SIN_FAMILY_ID, "sin", "0", "0\n");
const COS_OVERLOAD: BuiltinOverloadRecord = overload!(id::COS, "cos", id::COS_FAMILY_ID, "cos", "0", "1\n");
const TAN_OVERLOAD: BuiltinOverloadRecord = overload!(id::TAN, "tan", id::TAN_FAMILY_ID, "tan", "0", "0\n");
const SINH_OVERLOAD: BuiltinOverloadRecord = overload!(id::SINH, "sinh", id::SINH_FAMILY_ID, "sinh", "0", "0\n");
const COSH_OVERLOAD: BuiltinOverloadRecord = overload!(id::COSH, "cosh", id::COSH_FAMILY_ID, "cosh", "0", "1\n");
const TANH_OVERLOAD: BuiltinOverloadRecord = overload!(id::TANH, "tanh", id::TANH_FAMILY_ID, "tanh", "0", "0\n");
const ASIN_OVERLOAD: BuiltinOverloadRecord = overload!(id::ASIN, "asin", id::ASIN_FAMILY_ID, "asin", "0", "0\n");
const ACOS_OVERLOAD: BuiltinOverloadRecord = overload!(id::ACOS, "acos", id::ACOS_FAMILY_ID, "acos", "1", "0\n");
const ATAN_OVERLOAD: BuiltinOverloadRecord = overload!(id::ATAN, "atan", id::ATAN_FAMILY_ID, "atan", "0", "0\n");
const ASINH_OVERLOAD: BuiltinOverloadRecord = overload!(id::ASINH, "asinh", id::ASINH_FAMILY_ID, "asinh", "0", "0\n");
const ACOSH_OVERLOAD: BuiltinOverloadRecord = overload!(id::ACOSH, "acosh", id::ACOSH_FAMILY_ID, "acosh", "1", "0\n");
const ATANH_OVERLOAD: BuiltinOverloadRecord = overload!(id::ATANH, "atanh", id::ATANH_FAMILY_ID, "atanh", "0", "0\n");
const GAMMA_OVERLOAD: BuiltinOverloadRecord = overload!(id::GAMMA, "gamma", id::GAMMA_FAMILY_ID, "gamma", "5", "24\n");
const TGAMMA_OVERLOAD: BuiltinOverloadRecord =
    overload!(id::TGAMMA, "tgamma", id::GAMMA_FAMILY_ID, "tgamma", "5", "24\n");
const LGAMMA_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::LGAMMA,
    "lgamma",
    id::LGAMMA_FAMILY_ID,
    "lgamma",
    "5",
    // The platform C library's LAST ULP differs between libms, and this law links the SAME libm the reference links
    // (byte parity is the point): glibc answers ...58 where macOS libm answers ...53. The example pin follows the
    // platform the binary was built for, so the byte-exact harness holds on every Tier-1 target.
    if cfg!(target_os = "linux") {
        "3.1780538303479458\n"
    } else {
        "3.1780538303479453\n"
    }
);
const LGAMMA_R_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::LGAMMA_R,
    "lgamma_r",
    id::LGAMMA_R_FAMILY_ID,
    "lgamma_r",
    "5",
    // Same last-ulp split as the `lgamma` example above.
    if cfg!(target_os = "linux") {
        "[3.1780538303479458,1]\n"
    } else {
        "[3.1780538303479453,1]\n"
    }
);
const SIGNIFICAND_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::SIGNIFICAND,
    "significand",
    id::SIGNIFICAND_FAMILY_ID,
    "significand",
    "8",
    "1\n"
);
const LOGB_OVERLOAD: BuiltinOverloadRecord = overload!(id::LOGB, "logb", id::LOGB_FAMILY_ID, "logb", "8", "3\n");
const FREXP_OVERLOAD: BuiltinOverloadRecord =
    overload!(id::FREXP, "frexp", id::FREXP_FAMILY_ID, "frexp", "5", "[0.625,3]\n");
const MODF_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::MODF,
    "modf",
    id::MODF_FAMILY_ID,
    "modf",
    "5.7",
    "[0.7000000000000002,5]\n"
);
const NAN_OVERLOAD: BuiltinOverloadRecord = overload!(id::NAN, "nan", id::NAN_FAMILY_ID, "nan", "null", "null\n");
const INFINITE_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::INFINITE,
    "infinite",
    id::INFINITE_FAMILY_ID,
    "infinite",
    "null",
    "1.7976931348623157e+308\n"
);
const ISNAN_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::ISNAN,
    "isnan",
    id::ISNAN_FAMILY_ID,
    "[nan, 1] | map(isnan)",
    "null",
    "[true,false]\n"
);
const ISINFINITE_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::ISINFINITE,
    "isinfinite",
    id::ISINFINITE_FAMILY_ID,
    "isinfinite",
    "1",
    "false\n"
);
const ISFINITE_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::ISFINITE,
    "isfinite",
    id::ISFINITE_FAMILY_ID,
    "isfinite",
    "1",
    "true\n"
);
const ISNORMAL_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::ISNORMAL,
    "isnormal",
    id::ISNORMAL_FAMILY_ID,
    "isnormal",
    "0.5",
    "true\n"
);
const HYPOT_OVERLOAD: BuiltinOverloadRecord =
    overload2!(id::HYPOT, "hypot", id::HYPOT_FAMILY_ID, "hypot(3;4)", "null", "5\n");
const POW_OVERLOAD: BuiltinOverloadRecord =
    overload2!(id::POW, "pow", id::POW_FAMILY_ID, "pow(2;10)", "null", "1024\n");
const ATAN2_OVERLOAD: BuiltinOverloadRecord = overload2!(
    id::ATAN2,
    "atan2",
    id::ATAN2_FAMILY_ID,
    "atan2(1;1)",
    "null",
    "0.7853981633974483\n"
);
const FMOD_OVERLOAD: BuiltinOverloadRecord =
    overload2!(id::FMOD, "fmod", id::FMOD_FAMILY_ID, "fmod(5.5;3)", "null", "2.5\n");
const COPYSIGN_OVERLOAD: BuiltinOverloadRecord = overload2!(
    id::COPYSIGN,
    "copysign",
    id::COPYSIGN_FAMILY_ID,
    "copysign(-1;2)",
    "null",
    "1\n"
);
const REMAINDER_OVERLOAD: BuiltinOverloadRecord = overload2!(
    id::REMAINDER,
    "remainder",
    id::REMAINDER_FAMILY_ID,
    "remainder(7;3)",
    "null",
    "1\n"
);
const DREM_OVERLOAD: BuiltinOverloadRecord =
    overload2!(id::DREM, "drem", id::DREM_FAMILY_ID, "drem(3;4)", "null", "-1\n");
const FDIM_OVERLOAD: BuiltinOverloadRecord =
    overload2!(id::FDIM, "fdim", id::FDIM_FAMILY_ID, "fdim(1;2)", "null", "0\n");
const FMIN_OVERLOAD: BuiltinOverloadRecord =
    overload2!(id::FMIN, "fmin", id::FMIN_FAMILY_ID, "fmin(1;2)", "null", "1\n");
const FMAX_OVERLOAD: BuiltinOverloadRecord =
    overload2!(id::FMAX, "fmax", id::FMAX_FAMILY_ID, "fmax(1;2)", "null", "2\n");
const LDEXP_OVERLOAD: BuiltinOverloadRecord =
    overload2!(id::LDEXP, "ldexp", id::LDEXP_FAMILY_ID, "ldexp(1;3)", "null", "8\n");
const SCALBLN_OVERLOAD: BuiltinOverloadRecord = overload2!(
    id::SCALBLN,
    "scalbln",
    id::SCALBLN_FAMILY_ID,
    "scalbln(1;3)",
    "null",
    "8\n"
);
const SCALB_OVERLOAD: BuiltinOverloadRecord =
    overload2!(id::SCALB, "scalb", id::SCALB_FAMILY_ID, "scalb(1;3)", "null", "8\n");
const NEXTTOWARD_OVERLOAD: BuiltinOverloadRecord = overload2!(
    id::NEXTTOWARD,
    "nexttoward",
    id::NEXTTOWARD_FAMILY_ID,
    "nexttoward(1;2)",
    "null",
    "1.0000000000000002\n"
);
const NEXTAFTER_OVERLOAD: BuiltinOverloadRecord = overload2!(
    id::NEXTAFTER,
    "nextafter",
    id::NEXTAFTER_FAMILY_ID,
    "nextafter(1;2)",
    "null",
    "1.0000000000000002\n"
);
const FMA_OVERLOAD: BuiltinOverloadRecord = overload3!(id::FMA, "fma", id::FMA_FAMILY_ID, "fma(3;4;5)", "null", "17\n");
const J0_OVERLOAD: BuiltinOverloadRecord =
    overload!(id::J0, "j0", id::J0_FAMILY_ID, "j0", "2", "0.22389077914123567\n");
const J1_OVERLOAD: BuiltinOverloadRecord = overload!(id::J1, "j1", id::J1_FAMILY_ID, "j1", "2", "0.5767248077568733\n");
const Y0_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::Y0,
    "y0",
    id::Y0_FAMILY_ID,
    "y0",
    "2",
    // libm last-ulp split like the lgamma example: glibc's x86_64 y0(2) shortest-round-trips to ...451 where macOS libm
    // (and glibc aarch64) answer ...45.
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "0.5103756726497451\n"
    } else {
        "0.510375672649745\n"
    }
);
const Y1_OVERLOAD: BuiltinOverloadRecord =
    overload!(id::Y1, "y1", id::Y1_FAMILY_ID, "y1", "2", "-0.10703243154093756\n");
const JN_OVERLOAD: BuiltinOverloadRecord = overload2!(
    id::JN,
    "jn",
    id::JN_FAMILY_ID,
    "jn(1;2)",
    "null",
    "0.5767248077568733\n"
);
const YN_OVERLOAD: BuiltinOverloadRecord = overload2!(
    id::YN,
    "yn",
    id::YN_FAMILY_ID,
    "yn(1;2)",
    "null",
    "-0.10703243154093756\n"
);

pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    ABS_OVERLOAD,
    FABS_OVERLOAD,
    FLOOR_OVERLOAD,
    CEIL_OVERLOAD,
    ROUND_OVERLOAD,
    TRUNC_OVERLOAD,
    RINT_OVERLOAD,
    NEARBYINT_OVERLOAD,
    SQRT_OVERLOAD,
    CBRT_OVERLOAD,
    EXP_OVERLOAD,
    EXPM1_OVERLOAD,
    EXP2_OVERLOAD,
    EXP10_OVERLOAD,
    LOG_OVERLOAD,
    LOG1P_OVERLOAD,
    LOG2_OVERLOAD,
    LOG10_OVERLOAD,
    ERF_OVERLOAD,
    ERFC_OVERLOAD,
    SIN_OVERLOAD,
    COS_OVERLOAD,
    TAN_OVERLOAD,
    SINH_OVERLOAD,
    COSH_OVERLOAD,
    TANH_OVERLOAD,
    ASIN_OVERLOAD,
    ACOS_OVERLOAD,
    ATAN_OVERLOAD,
    ASINH_OVERLOAD,
    ACOSH_OVERLOAD,
    ATANH_OVERLOAD,
    GAMMA_OVERLOAD,
    TGAMMA_OVERLOAD,
    LGAMMA_OVERLOAD,
    LGAMMA_R_OVERLOAD,
    SIGNIFICAND_OVERLOAD,
    LOGB_OVERLOAD,
    FREXP_OVERLOAD,
    MODF_OVERLOAD,
    NAN_OVERLOAD,
    INFINITE_OVERLOAD,
    ISNAN_OVERLOAD,
    ISINFINITE_OVERLOAD,
    ISFINITE_OVERLOAD,
    ISNORMAL_OVERLOAD,
    HYPOT_OVERLOAD,
    POW_OVERLOAD,
    ATAN2_OVERLOAD,
    FMOD_OVERLOAD,
    COPYSIGN_OVERLOAD,
    REMAINDER_OVERLOAD,
    DREM_OVERLOAD,
    FDIM_OVERLOAD,
    FMIN_OVERLOAD,
    FMAX_OVERLOAD,
    LDEXP_OVERLOAD,
    SCALBLN_OVERLOAD,
    SCALB_OVERLOAD,
    NEXTTOWARD_OVERLOAD,
    NEXTAFTER_OVERLOAD,
    FMA_OVERLOAD,
    J0_OVERLOAD,
    J1_OVERLOAD,
    JN_OVERLOAD,
    Y0_OVERLOAD,
    Y1_OVERLOAD,
    YN_OVERLOAD,
];

/// The math execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment
/// — length equality alone would not catch a swapped pair (`Abs` where `Fabs` belongs passes every length and kind
/// check and silently computes the wrong law).
pub const PAYLOADS: &[(u16, MathEvaluator)] = &[
    (id::ABS, MathEvaluator::Unary(MathLaw::Abs)),
    (id::FABS, MathEvaluator::Unary(MathLaw::Fabs)),
    (id::FLOOR, MathEvaluator::Unary(MathLaw::Floor)),
    (id::CEIL, MathEvaluator::Unary(MathLaw::Ceil)),
    (id::ROUND, MathEvaluator::Unary(MathLaw::Round)),
    (id::TRUNC, MathEvaluator::Unary(MathLaw::Trunc)),
    (id::RINT, MathEvaluator::Unary(MathLaw::Rint)),
    (id::NEARBYINT, MathEvaluator::Unary(MathLaw::Nearbyint)),
    (id::SQRT, MathEvaluator::Unary(MathLaw::Sqrt)),
    (id::CBRT, MathEvaluator::Unary(MathLaw::Cbrt)),
    (id::EXP, MathEvaluator::Unary(MathLaw::Exp)),
    (id::EXPM1, MathEvaluator::Unary(MathLaw::Expm1)),
    (id::EXP2, MathEvaluator::Unary(MathLaw::Exp2)),
    (id::EXP10, MathEvaluator::Unary(MathLaw::Exp10)),
    (id::LOG, MathEvaluator::Unary(MathLaw::Log)),
    (id::LOG1P, MathEvaluator::Unary(MathLaw::Log1p)),
    (id::LOG2, MathEvaluator::Unary(MathLaw::Log2)),
    (id::LOG10, MathEvaluator::Unary(MathLaw::Log10)),
    (id::ERF, MathEvaluator::Unary(MathLaw::Erf)),
    (id::ERFC, MathEvaluator::Unary(MathLaw::Erfc)),
    (id::SIN, MathEvaluator::Trig(MathLawTrig::Sin)),
    (id::COS, MathEvaluator::Trig(MathLawTrig::Cos)),
    (id::TAN, MathEvaluator::Trig(MathLawTrig::Tan)),
    (id::SINH, MathEvaluator::Trig(MathLawTrig::Sinh)),
    (id::COSH, MathEvaluator::Trig(MathLawTrig::Cosh)),
    (id::TANH, MathEvaluator::Trig(MathLawTrig::Tanh)),
    (id::ASIN, MathEvaluator::Trig(MathLawTrig::Asin)),
    (id::ACOS, MathEvaluator::Trig(MathLawTrig::Acos)),
    (id::ATAN, MathEvaluator::Trig(MathLawTrig::Atan)),
    (id::ASINH, MathEvaluator::Trig(MathLawTrig::Asinh)),
    (id::ACOSH, MathEvaluator::Trig(MathLawTrig::Acosh)),
    (id::ATANH, MathEvaluator::Trig(MathLawTrig::Atanh)),
    (id::GAMMA, MathEvaluator::Gamma(MathLawGamma::Gamma)),
    (id::TGAMMA, MathEvaluator::Gamma(MathLawGamma::Tgamma)),
    (id::LGAMMA, MathEvaluator::Gamma(MathLawGamma::Lgamma)),
    (id::LGAMMA_R, MathEvaluator::Gamma(MathLawGamma::LgammaR)),
    (id::SIGNIFICAND, MathEvaluator::Special(MathLawSpecial::Significand)),
    (id::LOGB, MathEvaluator::Special(MathLawSpecial::Logb)),
    (id::FREXP, MathEvaluator::Special(MathLawSpecial::Frexp)),
    (id::MODF, MathEvaluator::Special(MathLawSpecial::Modf)),
    (id::NAN, MathEvaluator::Const(MathLawConst::Nan)),
    (id::INFINITE, MathEvaluator::Const(MathLawConst::Infinite)),
    (id::ISNAN, MathEvaluator::Check(MathLawCheck::Isnan)),
    (id::ISINFINITE, MathEvaluator::Check(MathLawCheck::Isinfinite)),
    (id::ISFINITE, MathEvaluator::Check(MathLawCheck::Isfinite)),
    (id::ISNORMAL, MathEvaluator::Check(MathLawCheck::IsNormal)),
    (id::HYPOT, MathEvaluator::Bin(MathLawBin::Hypot)),
    (id::POW, MathEvaluator::Bin(MathLawBin::Pow)),
    (id::ATAN2, MathEvaluator::Bin(MathLawBin::Atan2)),
    (id::FMOD, MathEvaluator::Bin(MathLawBin::Fmod)),
    (id::COPYSIGN, MathEvaluator::Bin(MathLawBin::Copysign)),
    (id::REMAINDER, MathEvaluator::Bin(MathLawBin::Remainder)),
    (id::DREM, MathEvaluator::Bin(MathLawBin::Drem)),
    (id::FDIM, MathEvaluator::Bin(MathLawBin::Fdim)),
    (id::FMIN, MathEvaluator::Bin(MathLawBin::Fmin)),
    (id::FMAX, MathEvaluator::Bin(MathLawBin::Fmax)),
    (id::LDEXP, MathEvaluator::Bin(MathLawBin::Ldexp)),
    (id::SCALBLN, MathEvaluator::Bin(MathLawBin::Scalbln)),
    (id::SCALB, MathEvaluator::Bin(MathLawBin::Scalb)),
    (id::NEXTTOWARD, MathEvaluator::Bin(MathLawBin::Nexttoward)),
    (id::NEXTAFTER, MathEvaluator::Bin(MathLawBin::Nextafter)),
    (id::FMA, MathEvaluator::Tern(MathLawTern::Fma)),
    (id::J0, MathEvaluator::Unary(MathLaw::J0)),
    (id::J1, MathEvaluator::Unary(MathLaw::J1)),
    (id::JN, MathEvaluator::Bin(MathLawBin::Jn)),
    (id::Y0, MathEvaluator::Unary(MathLaw::Y0)),
    (id::Y1, MathEvaluator::Unary(MathLaw::Y1)),
    (id::YN, MathEvaluator::Bin(MathLawBin::Yn)),
];
