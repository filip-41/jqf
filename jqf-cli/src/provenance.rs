//! What produced this binary, in one `--diagnostics` line.
//!
//! Two build-time choices change jqf's wall clock without changing a byte of its output: which allocator the binary
//! links (`src/allocator.rs`) and whether it was optimized against a PGO profile (`build.rs`,
//! `tools/pgo/jqf-pgo-build.sh`). Both are invisible in `--version`, in the help text, and in the output stream, so a
//! measurement can be attributed to the wrong build with nothing on screen to contradict it. This line is the
//! contradiction: it is printed on EVERY `--diagnostics` invocation, before the request runs, whatever route the
//! request takes.
//!
//! `profile=` answers the staleness question the profile artifact policy leaves open:
//! profiles are not committed, so the identity records hashes of the training workload, product code, target, and
//! merged profile.
//!
//! `platform=`, `pcores=` and `ecores=` answer the environment-attribution question for the parallel lanes: a run's
//! `auto` worker width is the machine's PHYSICAL performance cores plus HALF its efficiency cores, so a number measured
//! on another machine — a docker lane, a CI host — can only be compared when it says which platform and which core
//! counts produced it. `ecores=0` is every non-hybrid machine. `pcore_source=` names whether the count came from a real
//! topology read (`detected`) or from the portable `available_parallelism` stand-in (`assumed`), so an assumed count is
//! visible rather than presented as a measurement.

use core::fmt;

use crate::allocator;

/// The profile identity stamped by `tools/pgo/jqf-pgo-build.sh`, empty without `-Cprofile-use`.
const PROFILE_ID: &str = env!("JQF_PGO_PROFILE_ID");

/// `pgo` or `plain`. Derived from the profile identity rather than tracked beside it, so the two facts cannot disagree.
const BUILD_KIND: &str = if PROFILE_ID.is_empty() { "plain" } else { "pgo" };

/// The build-provenance line, rendered as `key=value` tokens like the parallel plan it precedes.
pub struct BuildProvenance;

impl fmt::Display for BuildProvenance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let profile = if PROFILE_ID.is_empty() { "none" } else { PROFILE_ID };
        let topology = jqf_runtime::parallel::core_topology();
        let source = match topology.source {
            jqf_runtime::parallel::PcoreSource::Detected => "detected",
            jqf_runtime::parallel::PcoreSource::Assumed => "assumed",
        };
        write!(
            formatter,
            "build={BUILD_KIND} profile={profile} allocator={} platform={}-{} \
             pcores={} ecores={} pcore_source={source}",
            allocator::NAME,
            std::env::consts::ARCH,
            std::env::consts::OS,
            topology.pcores,
            topology.ecores,
        )
    }
}
