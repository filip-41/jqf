//! Every [`RegistrationError`] variant, exercised through the public registration API (`CodecRegistration::try_new`)
//! exactly as a codec crate reaches it: one descriptor plus optional factory records, no source and no allocation.
//!
//! Each rejection case is built so every EARLIER validation law passes and the asserted variant is genuinely the check
//! that fires — the tests pin the variant identity (`assert_eq`, not merely `is_err`) and, by their construction, the
//! validation order itself.

use jqf_codec_core::{
    CodecDescriptor, CodecError, CodecOperations, CodecRegistration, DecodeRequest, DecoderFactoryRecord,
    EncodeRequest, EncoderFactoryRecord, ErasedEncoderFactory, ErasedProvider, ErasedRecordStreamProvider,
    ItemByteOwner, RecordProviderFactory, RecordProviderFactoryRecord, RecordProviderOpen, RegistrationError,
    RouteCapability, TagValidatorFactoryRecord,
};
use jqf_data::{DialectIdRef, FormatIdRef};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

const FORMAT_TEXT: &str = "jqf-core-test";
const DIALECT_A: DialectIdRef<'static> = DialectIdRef::from_static("jqf-core-test.a@1");
const DIALECT_B: DialectIdRef<'static> = DialectIdRef::from_static("jqf-core-test.b@1");

/// A factory body that must never run: the agreement laws consult PRESENCE only, so a registration check has no source
/// to open and no options to dispatch.
fn unused_record_factory<'source>(
    _source: ResolvedSource<'source>,
    _open: RecordProviderOpen,
    _resources: &mut ResourceContext<'_>,
) -> Result<ErasedRecordStreamProvider<'source>, CodecError> {
    unreachable!("registration validation never invokes a record factory")
}

/// Same presence-only contract as [`unused_record_factory`], for the encode side.
fn unused_encoder_factory(
    _request: EncodeRequest<'_, '_>,
    _resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    unreachable!("registration validation never invokes an encoder factory")
}

/// Same presence-only contract as [`unused_record_factory`], for the decode side.
fn unused_decoder_factory<'source>(
    _source: ResolvedSource<'source>,
    _request: DecodeRequest<'_>,
    _resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    unreachable!("registration validation never invokes a decoder factory")
}

fn descriptor<'a>(
    dialects: &'a [DialectIdRef<'static>],
    operations: CodecOperations,
    routes: &'a [RouteCapability],
    inter_item_byte: &'a [ItemByteOwner],
) -> CodecDescriptor<'a> {
    CodecDescriptor::new(
        FormatIdRef::from_static(FORMAT_TEXT),
        dialects,
        operations,
        routes,
        // Detection claims are unrelated to registration validation.
        &[],
        inter_item_byte,
        &[],
        // No separators: nothing here rides the adjacent-value drives.
        &[],
    )
}

fn register(
    descriptor: CodecDescriptor<'_>,
    decoder: Option<DecoderFactoryRecord>,
    encoder: Option<EncoderFactoryRecord>,
    tag_validator: Option<TagValidatorFactoryRecord>,
    record_provider: Option<RecordProviderFactoryRecord>,
) -> Result<CodecRegistration<'_>, RegistrationError> {
    CodecRegistration::try_new(descriptor, decoder, encoder, tag_validator, record_provider)
}

#[test]
fn empty_dialect_list_is_missing_dialect() {
    let error = register(
        descriptor(&[], CodecOperations::new(false, false, false), &[], &[]),
        None,
        None,
        None,
        None,
    )
    .expect_err("an empty dialect list exposes no constructible dialect");
    assert_eq!(error, RegistrationError::MissingDialect);
}

#[test]
fn repeated_dialect_identity_is_duplicate() {
    // Two rows share one identity; the duplicate check runs before the framing-arity check, so the unaligned byte list
    // below never fires.
    let dialects = [DIALECT_A, DIALECT_A];
    let error = register(
        descriptor(&dialects, CodecOperations::new(false, false, false), &[], &[]),
        None,
        None,
        None,
        None,
    )
    .expect_err("the same dialect twice");
    assert_eq!(error, RegistrationError::DuplicateDialect);
}

#[test]
fn decode_potential_without_a_decoder_is_decoder_disagreement() {
    // One distinct dialect (no duplicate) and an aligned byte row, so only the decode agreement can fire: the
    // descriptor declares decode but carries no decoder factory.
    let dialects = [DIALECT_A];
    let error = register(
        descriptor(
            &dialects,
            CodecOperations::new(true, false, false),
            &[],
            &[ItemByteOwner::Facade],
        ),
        None,
        None,
        None,
        None,
    )
    .expect_err("decode declared with no decoder");
    assert_eq!(error, RegistrationError::DecoderAgreement);
}

#[test]
fn encode_potential_without_an_encoder_is_encoder_disagreement() {
    let dialects = [DIALECT_A];
    let error = register(
        descriptor(
            &dialects,
            CodecOperations::new(false, true, false),
            &[],
            &[ItemByteOwner::Facade],
        ),
        None,
        None,
        None,
        None,
    )
    .expect_err("encode declared with no encoder");
    assert_eq!(error, RegistrationError::EncoderAgreement);
}

#[test]
fn tag_validation_without_a_validator_is_validator_disagreement() {
    let dialects = [DIALECT_A];
    let error = register(
        descriptor(
            &dialects,
            CodecOperations::new(false, false, true),
            &[],
            &[ItemByteOwner::Facade],
        ),
        None,
        None,
        None,
        None,
    )
    .expect_err("tag validation declared with no validator");
    assert_eq!(error, RegistrationError::TagValidatorAgreement);
}

#[test]
fn record_provider_over_no_record_route_is_provider_disagreement() {
    // All three operation agreements hold (nothing declared, nothing carried); the route list omits `Record` while a
    // provider is present, which is exactly the disagreement direction the API rejects.
    let factory: RecordProviderFactory = unused_record_factory;
    let dialects = [DIALECT_A];
    let error = register(
        descriptor(
            &dialects,
            CodecOperations::new(false, false, false),
            &[],
            &[ItemByteOwner::Facade],
        ),
        None,
        None,
        None,
        Some(RecordProviderFactoryRecord::new(factory)),
    )
    .expect_err("record provider registered over a non-record descriptor");
    assert_eq!(error, RegistrationError::RecordProviderAgreement);
}

#[test]
fn byte_row_length_off_the_dialect_count_is_framing_arity() {
    // Two distinct dialects pass the duplicate law; with no operations and no factories, every agreement holds — the
    // misaligned one-row byte list against two dialects is what fires.
    let dialects = [DIALECT_A, DIALECT_B];
    let error = register(
        descriptor(
            &dialects,
            CodecOperations::new(false, false, false),
            &[],
            &[ItemByteOwner::Facade],
        ),
        None,
        None,
        None,
        None,
    )
    .expect_err("one inter-item-byte row for two dialects");
    assert_eq!(error, RegistrationError::FramingArity);
}

#[test]
fn an_aligned_registration_validates_and_round_trips() {
    // Positive control: the harness above constructs valid registrations — otherwise every rejection case could pass
    // vacuously.
    let dialects = [DIALECT_A];
    let registration = register(
        descriptor(
            &dialects,
            CodecOperations::new(true, true, false),
            &[],
            &[ItemByteOwner::Facade],
        ),
        Some(DecoderFactoryRecord::new(unused_decoder_factory)),
        Some(EncoderFactoryRecord::new(unused_encoder_factory)),
        None,
        None,
    )
    .expect("an aligned declaration validates");
    assert_eq!(registration.descriptor().format().as_str(), FORMAT_TEXT);
    assert_eq!(registration.descriptor().dialects().len(), 1);
    assert_eq!(registration.descriptor().dialects()[0].as_str(), DIALECT_A.as_str());
    assert!(registration.decoder().is_some());
    assert!(registration.encoder().is_some());
    assert!(registration.tag_validator().is_none());
    assert!(registration.record_provider().is_none());
}

#[test]
fn a_record_route_without_a_provider_stays_legal() {
    // The converse direction the API documents as legal: the descriptor advertises the record route while carrying no
    // record provider (plain JSON registers this shape — record OUTPUT target only).
    let dialects = [DIALECT_A];
    let served = register(
        descriptor(
            &dialects,
            CodecOperations::new(false, false, false),
            &[RouteCapability::Record],
            &[ItemByteOwner::Facade],
        ),
        None,
        None,
        None,
        None,
    )
    .expect("a record route with no provider is the legal converse");
    assert!(served.record_provider().is_none());
}
