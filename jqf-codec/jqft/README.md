# jqf-codec-jqft

Defines the jqft text profile, the jqfjson JSON envelope, and the jqfb
binary image.

This crate is `no_std` and uses `alloc`. It depends on `jqf-codec-core`,
`jqf-data`, `jqf-resource`, `jqf-source`, and `blake3`. It owns three
format ids because the renderings disagree: jqft spells tags, bytes, and
temporals in-grammar; jqfjson is strict JSON and refuses those spellings;
jqfb is a chunked machine image with a footer directory.

What it has:

- `registration_jqft` / `registration_jqfjson` / `registration_jqfb` — one
  catalog entry per format
- `jqft.document@1` / `jqfjson.document@1` / `jqfb.document@1` — input
  dialects
- `jqft.canonical@1` / `jqfjson.canonical@1` / `jqfb.canonical@1` — output
  profiles
- Whole and Exact decode for the text formats; jqfb Exact is the
  node-table walk
- deterministic canonical encode under each output profile
- `FORMAT_ID`, `JQFJSON_FORMAT_ID`, `FORMAT_ID_JQFB`, and the dialect /
  route id constants
- `JqftEncodeOptions` / `JqfbEncodeOptions` — `with_source` requests the
  retained-source emission surface

It does not evaluate programs, open files, or treat the jqfb image as a
stable archive format.

```rust
use jqf_codec_jqft::{
    FORMAT_ID, FORMAT_ID_JQFB, JQFJSON_FORMAT_ID, registration_jqfb, registration_jqfjson,
    registration_jqft,
};

assert_eq!(FORMAT_ID, "jqft");
assert_eq!(JQFJSON_FORMAT_ID, "jqfjson");
assert_eq!(FORMAT_ID_JQFB, "jqfb");
assert!(registration_jqft().is_ok());
assert!(registration_jqfjson().is_ok());
assert!(registration_jqfb().is_ok());
```

## jqft

`registration_jqft()` serves `jqft` (extension `jqft`). A source is a
`---` document stream that starts with `%jqft 1`. Core values, `@tag`
layers, bytes, and temporal literals round-trip. Comments parse and are
skipped on the value; markup nodes decode as tagged child arrays.

```rust
use jqf_codec_jqft::{FORMAT_ID, JQFT_DOCUMENT_DIALECT_ID, registration_jqft};

let registration = registration_jqft().unwrap();
assert_eq!(registration.descriptor().format().as_str(), FORMAT_ID);
assert!(registration
    .descriptor()
    .dialects()
    .iter()
    .any(|d| d.as_str() == JQFT_DOCUMENT_DIALECT_ID));
```

## jqfjson

`registration_jqfjson()` serves `jqfjson` (extension `jqfjson`). One
strict JSON document per source; a stream of adjacent envelopes is the
adjacent-value lane. Bytes, temporals, and tags have no spelling.

```rust
use jqf_codec_jqft::{JQFJSON_DOCUMENT_DIALECT_ID, JQFJSON_FORMAT_ID, registration_jqfjson};

let registration = registration_jqfjson().unwrap();
assert_eq!(registration.descriptor().format().as_str(), JQFJSON_FORMAT_ID);
assert!(registration
    .descriptor()
    .dialects()
    .iter()
    .any(|d| d.as_str() == JQFJSON_DOCUMENT_DIALECT_ID));
```

## jqfb

`registration_jqfb()` serves `jqfb` (extension `jqfb`). One binary
document per source. The image is a header, chunks, and a footer
directory of type / offset / length / digest. Unknown ignorable chunks
are skipped; unknown critical chunks refuse the file.

```rust
use jqf_codec_jqft::{FORMAT_ID_JQFB, JQFB_DOCUMENT_DIALECT_ID, registration_jqfb};

let registration = registration_jqfb().unwrap();
assert_eq!(registration.descriptor().format().as_str(), FORMAT_ID_JQFB);
assert!(registration
    .descriptor()
    .dialects()
    .iter()
    .any(|d| d.as_str() == JQFB_DOCUMENT_DIALECT_ID));
```

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for the three grammar, value, encode,
and edit invariants. Family laws: [`../CONTRACTS.md`](../CONTRACTS.md).
