# jqf-codec-yaml

Defines YAML 1.2.2 decode and encode for jqf.

This crate is `no_std` and uses `alloc`. It depends on `jqf-codec-core`,
`jqf-data`, `jqf-resource`, and `jqf-source`. It owns the YAML grammar.

What it has:

- `registration()` — one catalog entry for every input schema dialect and
  output profile
- `yaml.core@1` / `yaml.json@1` / `yaml.failsafe@1` — input schemas
- `yaml.stream-canonical@1` / `yaml.single-document@1` / `yaml.block@1` /
  `yaml.jqf-1.0@1` — output profiles
- whole-document decode over a document stream, an exact-path located
  route, and a scoped walk
- `decode_documents` — every document in one stream as owned values
- deterministic semantic encode, plus the edit-render splice policy
- `FORMAT_ID` and the dialect / route id constants

It does not evaluate programs, own I/O, resolve external entities, or
implement YAML 1.1 type tags as silent conversions.

```rust
use jqf_codec_yaml::{FORMAT_ID, YAML_CORE_DIALECT_ID, registration};

let registration = registration().unwrap();
assert_eq!(registration.descriptor().format().as_str(), FORMAT_ID);
assert!(registration
    .descriptor()
    .dialects()
    .iter()
    .any(|d| d.as_str() == YAML_CORE_DIALECT_ID));
```

## Schemas

A YAML source is a stream of documents. The catalog default input dialect
is `yaml.core@1`. Failsafe publishes only maps, sequences, and strings.
JSON adds null, bool, integer, and float, and a plain scalar that matches
none of those regexes is an error. Core uses the same seven tags and
falls back to string.

```rust
use jqf_codec_yaml::{
    YAML_CORE_DIALECT_ID, YAML_FAILSAFE_DIALECT_ID, YAML_JSON_DIALECT_ID, registration,
};

let dialects: Vec<_> = registration()
    .unwrap()
    .descriptor()
    .dialects()
    .iter()
    .map(|d| d.as_str())
    .collect();
assert!(dialects.contains(&YAML_CORE_DIALECT_ID));
assert!(dialects.contains(&YAML_JSON_DIALECT_ID));
assert!(dialects.contains(&YAML_FAILSAFE_DIALECT_ID));
```

## Decode

`decode_documents` materializes each document in stream order under the
core schema.

```rust
use jqf_codec_yaml::decode_documents;
use jqf_data::ValueKind;
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

static CONTROL: ContinueControl = ContinueControl;
let limits = ResourceLimits::new(u64::MAX, 4096, 1 << 20, u64::MAX, 100);
let mut resources = ResourceContext::new(
    RequestAccount::try_new(limits).unwrap(),
    &CONTROL,
    WorkMeter::try_new_v1(4096).unwrap(),
)
.unwrap();

let source = ResolvedSource::new(
    SourceRef::new(SourceId::new(0), SourceKind::Input),
    "example.yaml",
    b"hello: world\n",
    0,
);
let docs = decode_documents(source, &mut resources).unwrap();
assert_eq!(docs.len(), 1);
assert_eq!(docs[0].kind(), ValueKind::Object);
```

## Encode

`yaml.block@1` is the human-readable default. The two canonical profiles
are byte-frozen: every core node carries an explicit standard tag, every
scalar is double-quoted, and containers are flow-style.
`yaml.jqf-1.0@1` is the edit-render dialect.

```rust
use jqf_codec_yaml::{
    YAML_BLOCK_DIALECT_ID, YAML_JQF_1_0_DIALECT_ID, YAML_SINGLE_DOCUMENT_DIALECT_ID,
    YAML_STREAM_CANONICAL_DIALECT_ID, registration,
};

let dialects: Vec<_> = registration()
    .unwrap()
    .descriptor()
    .dialects()
    .iter()
    .map(|d| d.as_str())
    .collect();
assert!(dialects.contains(&YAML_STREAM_CANONICAL_DIALECT_ID));
assert!(dialects.contains(&YAML_SINGLE_DOCUMENT_DIALECT_ID));
assert!(dialects.contains(&YAML_BLOCK_DIALECT_ID));
assert!(dialects.contains(&YAML_JQF_1_0_DIALECT_ID));
```

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for schema, stream, key, encode, and
edit invariants. Family laws: [`../CONTRACTS.md`](../CONTRACTS.md).
