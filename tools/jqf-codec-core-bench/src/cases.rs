use jqf_bench_core::{BenchmarkCase, CaseMetadata, PreflightReceipt};
use jqf_codec_core::{
    AccessGuarantees, AccessRequirement, CodecDemand, CodecError, CodecFailureKind, DecodeRequest, DemandClause,
    DiagnosticPolicy, ErasedAccessSession, ErasedProvider, InputProvider, ProviderInput, RouteDescription, RouteSlot,
    ValidationMode,
};
use jqf_data::DialectId;
use jqf_resource::ResourceContext;
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use crate::fixtures;

struct DemandCase;

impl BenchmarkCase for DemandCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("demand/normalize-root-forward", 1, 19)
    }
    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        preflight(self.run())
    }
    fn run(&mut self) -> u64 {
        let resources = fixtures::resources();
        let demand = fixtures::demand(&resources);
        demand.fingerprint().value() ^ demand.clauses().len() as u64
    }
}

struct ProfileProvider {
    routes: Vec<RouteDescription>,
}
impl InputProvider for ProfileProvider {
    fn route_descriptions(&self) -> &[RouteDescription] {
        self.routes.as_slice()
    }
    fn open_route<'source>(
        &mut self,
        _input: ProviderInput<'source>,
        _slot: RouteSlot,
        _requirement: &AccessRequirement,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedAccessSession<'source>, CodecError> {
        Err(CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "bind benchmark never opens",
        }))
    }
}

struct BindCase {
    provider: ErasedProvider<'static>,
    requirement: AccessRequirement,
}

struct DispatchCase {
    provider: ErasedProvider<'static>,
    requirement: AccessRequirement,
    resources: ResourceContext<'static>,
    opens: u64,
    receipt_verified: bool,
}

impl DispatchCase {
    fn new() -> Self {
        static SOURCE: &[u8] = br#"{"catalog":[]}"#;
        let mut resources = fixtures::resources();
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(2), SourceKind::Input),
            "dispatch.json",
            SOURCE,
            0,
        );
        let provider = jqf_codec_json::registration()
            .expect("registration")
            .decoder()
            .expect("decoder")
            .create_provider(
                source,
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new("rfc8259").expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                &mut resources,
            )
            .expect("provider");
        let mut demand = CodecDemand::try_new(&resources);
        demand.try_insert(&DemandClause::SemanticRoot).expect("root");
        let requirement = AccessRequirement::try_whole(
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        Self {
            provider,
            requirement,
            resources,
            opens: 0,
            receipt_verified: false,
        }
    }
}

impl BenchmarkCase for DispatchCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("access/open-dispatch", 1, 19)
    }
    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let checksum = self.run();
        if checksum == 0 || !self.receipt_verified {
            return Err("open dispatch did not return the sealed physical receipt".into());
        }
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "{} exact_operation=true executed_receipt=true physical_route=0x{:016x} sealed_slot=0 checksum=0x{checksum:016x}",
                fixtures::provenance(),
                jqf_codec_json::FULL_PHYSICAL_ROUTE_ID.get(),
            ),
        ))
    }
    fn run(&mut self) -> u64 {
        let Ok(handle) = self.provider.bind(&self.requirement) else {
            return 0;
        };
        let Ok(session) = self.provider.open(&handle, &mut self.resources) else {
            return 0;
        };
        let Some(physical) = session.physical_route_receipt() else {
            return 0;
        };
        if physical.route() != jqf_codec_json::FULL_PHYSICAL_ROUTE_ID
            || physical.slot() != RouteSlot::new(0)
            || physical.provider_id() == 0
        {
            return 0;
        }
        self.receipt_verified = true;
        self.opens = self.opens.wrapping_add(1);
        let receipt = self.opens ^ 0xd15c_a7c4 ^ physical.route().get();
        drop(session);
        receipt
    }
}

impl BindCase {
    fn new() -> Self {
        static SOURCE: [u8; 0] = [];
        let resources = fixtures::resources();
        let routes = vec![RouteDescription::new(RouteSlot::new(0), fixtures::bundle(&resources))];
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(1), SourceKind::Input),
            "strict-json-shaped",
            &SOURCE,
            0,
        );
        let provider =
            ErasedProvider::try_new_provider(source, &resources, || Ok(ProfileProvider { routes })).expect("provider");
        let requirement = fixtures::requirement(&resources);
        Self { provider, requirement }
    }
}

impl BenchmarkCase for BindCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("access/bind-real-profile", 1, 19)
    }
    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        preflight(self.run())
    }
    fn run(&mut self) -> u64 {
        self.provider.bind(&self.requirement).map_or(0, |handle| {
            if handle.demand_fallback() {
                return 0;
            }
            self.requirement.demand().fingerprint().value() ^ 0xb1ad
        })
    }
}

fn preflight(checksum: u64) -> Result<PreflightReceipt, String> {
    if checksum == 0 {
        return Err("zero operation receipt".into());
    }
    Ok(PreflightReceipt::new(
        checksum,
        format!(
            "{} exact_operation=true checksum=0x{checksum:016x}",
            fixtures::provenance()
        ),
    ))
}

pub(crate) fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    vec![
        Box::new(DemandCase),
        Box::new(BindCase::new()),
        Box::new(DispatchCase::new()),
    ]
}
