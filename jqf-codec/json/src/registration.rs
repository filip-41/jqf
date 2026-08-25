//! The strict-JSON codec registration: descriptor identities and the allocation-free validated `registration()` entry
//! point.
//!
//! The registration is the catalog's contract with the codec: the format and dialect identities, the CLI-facing route
//! capabilities, and the decode / encode / tag-validate factory records. Consumers reach it through the crate root's
//! `registration()` re-export.

use jqf_codec_core::{
    CodecDescriptor, CodecOperations, CodecRegistration, DecoderFactoryRecord, EncoderFactoryRecord, ItemByteOwner,
    RegistrationError, RouteCapability, TagValidatorFactoryRecord,
};
use jqf_data::{DialectIdRef, FormatIdRef};

use crate::decode;
use crate::encode;
use crate::tag;

/// Stable strict-JSON format identity text.
pub const FORMAT_ID: &str = jqf_codec_core::record_options::JSON_FORMAT_ID;
/// Stable RFC 8259 dialect identity text.
pub const RFC8259_DIALECT_ID: &str = jqf_codec_core::record_options::RFC8259_DIALECT_ID;
const FORMAT: FormatIdRef<'static> = FormatIdRef::from_static(FORMAT_ID);
const DIALECTS: [DialectIdRef<'static>; 1] = [DialectIdRef::from_static(RFC8259_DIALECT_ID)];

/// The CLI-facing routes the strict-JSON registration serves: the adjacent-value input model and the record route's
/// JSON output target.
const ROUTES: [RouteCapability; 3] = [
    RouteCapability::Record,
    RouteCapability::AdjacentValues,
    RouteCapability::Edit,
];

/// Constructs the allocation-free validated strict-JSON decoder registration.
pub fn registration() -> Result<CodecRegistration<'static>, RegistrationError> {
    CodecRegistration::try_new(
        CodecDescriptor::new(
            FORMAT,
            &DIALECTS,
            CodecOperations::new(true, true, true),
            &ROUTES,
            &["json"],
            // The facade owns the inter-item byte for the adjacent-value RFC 8259 stream; the item encoding ends at
            // `}`.
            &[ItemByteOwner::Facade],
            &[],
            // RFC 8259 insignificant inter-value whitespace.
            crate::VALUE_SEPARATORS,
        ),
        Some(DecoderFactoryRecord::new(decode::create_provider)),
        Some(EncoderFactoryRecord::new(encode::create_factory)),
        Some(TagValidatorFactoryRecord::new(tag::create_validator)),
        None,
    )
}
