//! Allocation-free validated codec registration.
//!
//! [`CodecRegistration::try_new`] checks factory presence against declared operations. Sibling: [`crate::descriptor`].

use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::{
    CodecDescriptor, CodecError, DecodeRequest, EncodeRequest, ErasedEncoderFactory, ErasedProvider,
    ErasedRecordStreamProvider, ErasedTagValidator, RecordProviderOpen,
};

/// Record-provider factory entry point.
///
/// A record format's DECODE side is not a decoder-factory operation (it produces a RECORD STREAM, not one document), so
/// it registers as a record-provider factory beside the decoder/encoder/tag-validator records; the runtime opens record
/// streams through the catalog instead of naming the codec crate. The open envelope is codec-neutral; the factory
/// downcasts the payloads to its own option and profile types.
pub type RecordProviderFactory = for<'source, 'control> fn(
    ResolvedSource<'source>,
    RecordProviderOpen,
    &mut ResourceContext<'control>,
) -> Result<ErasedRecordStreamProvider<'source>, CodecError>;

/// Record-provider factory entry record.
#[derive(Clone, Copy)]
pub struct RecordProviderFactoryRecord {
    /// Exact record-provider factory entry point.
    factory: RecordProviderFactory,
}

impl RecordProviderFactoryRecord {
    /// Creates one source-independent record-provider factory record.
    #[must_use]
    pub const fn new(factory: RecordProviderFactory) -> Self {
        Self { factory }
    }

    /// Opens one record stream over the retained source.
    pub fn create_provider<'source>(
        &self,
        source: ResolvedSource<'source>,
        open: RecordProviderOpen,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedRecordStreamProvider<'source>, CodecError> {
        (self.factory)(source, open, resources)
    }
}

impl core::fmt::Debug for RecordProviderFactoryRecord {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("RecordProviderFactoryRecord")
            .finish_non_exhaustive()
    }
}

/// Decoder factory entry point.
pub type DecoderFactory = for<'source, 'options, 'control> fn(
    ResolvedSource<'source>,
    DecodeRequest<'options>,
    &mut ResourceContext<'control>,
) -> Result<ErasedProvider<'source>, CodecError>;

/// Target-bound encoder-factory entry point.
pub type EncoderFactory = for<'target, 'options, 'control> fn(
    EncodeRequest<'target, 'options>,
    &mut ResourceContext<'control>,
) -> Result<ErasedEncoderFactory, CodecError>;

/// Target-bound tag-validator entry point.
pub type TagValidatorFactory = for<'target, 'options, 'control> fn(
    EncodeRequest<'target, 'options>,
    &mut ResourceContext<'control>,
) -> Result<ErasedTagValidator, CodecError>;

/// Decoder factory entry record.
#[derive(Clone, Copy)]
pub struct DecoderFactoryRecord {
    /// Exact decoder entry point.
    factory: DecoderFactory,
}

impl DecoderFactoryRecord {
    /// Creates one source-independent decoder factory record.
    #[must_use]
    pub const fn new(factory: DecoderFactory) -> Self {
        Self { factory }
    }
    /// Charges logical input exactly once and transfers the retained source to the concrete non-parsing factory.
    pub fn create_provider<'source>(
        &self,
        source: ResolvedSource<'source>,
        request: DecodeRequest<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedProvider<'source>, CodecError> {
        (self.factory)(source, request, resources)
    }
}

impl core::fmt::Debug for DecoderFactoryRecord {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_struct("DecoderFactoryRecord").finish_non_exhaustive()
    }
}

/// Encoder factory entry record.
#[derive(Clone, Copy)]
pub struct EncoderFactoryRecord {
    /// Exact target-bound encoder factory entry point.
    factory: EncoderFactory,
}

impl EncoderFactoryRecord {
    /// Creates one target-bound encoder factory record.
    #[must_use]
    pub const fn new(factory: EncoderFactory) -> Self {
        Self { factory }
    }
    /// Constructs a checked target-bound encoder factory.
    pub fn create_factory(
        &self,
        request: EncodeRequest<'_, '_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderFactory, CodecError> {
        (self.factory)(request, resources)
    }
}

impl core::fmt::Debug for EncoderFactoryRecord {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_struct("EncoderFactoryRecord").finish_non_exhaustive()
    }
}

/// Target tag-validator factory record.
#[derive(Clone, Copy)]
pub struct TagValidatorFactoryRecord {
    /// Exact target-bound validator entry point.
    factory: TagValidatorFactory,
}

impl TagValidatorFactoryRecord {
    /// Creates one target-bound tag-validator factory record.
    #[must_use]
    pub const fn new(factory: TagValidatorFactory) -> Self {
        Self { factory }
    }
    /// Constructs a checked target-bound tag validator.
    pub fn create_validator(
        &self,
        request: EncodeRequest<'_, '_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedTagValidator, CodecError> {
        (self.factory)(request, resources)
    }
}

impl core::fmt::Debug for TagValidatorFactoryRecord {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("TagValidatorFactoryRecord")
            .finish_non_exhaustive()
    }
}

/// Allocation-free codec registration validation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationError {
    /// A dialect identity occurs more than once.
    DuplicateDialect,
    /// The descriptor exposes no constructible dialect.
    MissingDialect,
    /// Descriptor decoder potential disagrees with the factory record.
    DecoderAgreement,
    /// Descriptor encoder potential disagrees with the factory record.
    EncoderAgreement,
    /// Descriptor tag-validator potential disagrees with the factory record.
    TagValidatorAgreement,
    /// A record-provider factory is registered for a descriptor whose routes do not advertise the record route. The
    /// converse is legal: a descriptor may advertise the record route with no provider (the plain JSON registration
    /// advertises it as an OUTPUT target only).
    RecordProviderAgreement,
    /// The inter-item-byte declaration has a different length than the descriptor's dialect list: one
    /// [`crate::ItemByteOwner`] per dialect, aligned by index. Also raised when value separators are declared without
    /// the [`crate::RouteCapability::AdjacentValues`] capability that is their only consumer.
    FramingArity,
}

/// One immutable validated concrete-codec registration.
#[derive(Debug)]
pub struct CodecRegistration<'registration> {
    descriptor: CodecDescriptor<'registration>,
    decoder: Option<DecoderFactoryRecord>,
    encoder: Option<EncoderFactoryRecord>,
    tag_validator: Option<TagValidatorFactoryRecord>,
    record_provider: Option<RecordProviderFactoryRecord>,
}

impl<'registration> CodecRegistration<'registration> {
    /// Validates descriptor and factory agreement without allocation or source access.
    pub fn try_new(
        descriptor: CodecDescriptor<'registration>,
        decoder: Option<DecoderFactoryRecord>,
        encoder: Option<EncoderFactoryRecord>,
        tag_validator: Option<TagValidatorFactoryRecord>,
        record_provider: Option<RecordProviderFactoryRecord>,
    ) -> Result<Self, RegistrationError> {
        let dialects = descriptor.dialects();
        if dialects.is_empty() {
            return Err(RegistrationError::MissingDialect);
        }
        if (0..dialects.len()).any(|index| dialects[index + 1..].contains(&dialects[index])) {
            return Err(RegistrationError::DuplicateDialect);
        }
        let operations = descriptor.operations();
        if operations.decode() != decoder.is_some() {
            return Err(RegistrationError::DecoderAgreement);
        }
        if operations.encode() != encoder.is_some() {
            return Err(RegistrationError::EncoderAgreement);
        }
        if operations.validate_tags() != tag_validator.is_some() {
            return Err(RegistrationError::TagValidatorAgreement);
        }
        // A record provider REQUIRES the record route; the record route does not require a provider (the plain JSON
        // registration advertises the record route as an OUTPUT target with no record decode).
        let record_route = descriptor
            .route_capabilities()
            .contains(&crate::RouteCapability::Record);
        if record_provider.is_some() && !record_route {
            return Err(RegistrationError::RecordProviderAgreement);
        }
        if descriptor.inter_item_byte().len() != dialects.len() {
            return Err(RegistrationError::FramingArity);
        }
        // Value separators exist only for the sequence drives' inter-value scan, which runs exclusively over
        // adjacent-value lanes; a registration that does not declare AdjacentValues can never consume them, so
        // declaring any is a registration bug (silently dead data).
        if !descriptor
            .route_capabilities()
            .contains(&crate::RouteCapability::AdjacentValues)
            && !descriptor.value_separators().is_empty()
        {
            return Err(RegistrationError::FramingArity);
        }
        Ok(Self {
            descriptor,
            decoder,
            encoder,
            tag_validator,
            record_provider,
        })
    }

    /// Source-independent descriptor.
    #[must_use]
    pub const fn descriptor(&self) -> &CodecDescriptor<'registration> {
        &self.descriptor
    }

    /// Optional decoder factory record.
    #[must_use]
    pub const fn decoder(&self) -> Option<DecoderFactoryRecord> {
        self.decoder
    }

    /// Optional encoder factory record.
    #[must_use]
    pub const fn encoder(&self) -> Option<EncoderFactoryRecord> {
        self.encoder
    }

    /// Optional target tag-validator factory record.
    #[must_use]
    pub const fn tag_validator(&self) -> Option<TagValidatorFactoryRecord> {
        self.tag_validator
    }

    /// Optional record-provider factory record.
    #[must_use]
    pub const fn record_provider(&self) -> Option<RecordProviderFactoryRecord> {
        self.record_provider
    }
}
