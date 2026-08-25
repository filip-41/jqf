# jqf-codec-ini

Defines `.properties`, INI, and dotenv decode and encode.

This crate is `no_std` and uses `alloc`. It depends on `jqf-codec-core`,
`jqf-data`, `jqf-resource`, and `jqf-source`. It owns three format ids
because the grammars disagree: `;` is a comment in INI and a value byte
in `.properties`; `export X=1` is a key named `export X` unless the
dialect is dotenv.

What it has:

- `registration` / `registration_ini` / `registration_dotenv` — one catalog
  entry per format
- `properties.jdk@1` / `ini.jqf-strict@1` / `dotenv.jqf-strict@1` — input
  dialects
- `properties.jqf-1.0@1` / `ini.jqf-1.0@1` / `dotenv.jqf-1.0@1` — output
  profiles
- whole-document decode and deterministic semantic encode
- `FORMAT_ID`, `INI_FORMAT_ID`, `DOTENV_FORMAT_ID`, and the dialect ids

It does not evaluate programs, interpolate `$VAR`, own I/O, or claim
`.conf`.

```rust
use jqf_codec_ini::{
    DOTENV_FORMAT_ID, FORMAT_ID, INI_FORMAT_ID, registration, registration_dotenv,
    registration_ini,
};

assert_eq!(FORMAT_ID, "properties");
assert_eq!(INI_FORMAT_ID, "ini");
assert_eq!(DOTENV_FORMAT_ID, "dotenv");
assert!(registration().is_ok());
assert!(registration_ini().is_ok());
assert!(registration_dotenv().is_ok());
```

## Properties

`registration()` serves `properties` (extension `properties`). A logical
line is a key/value pair. `#` and `!` open comments. `\` continues a
line. Values keep trailing blanks.

```rust
use jqf_codec_ini::{FORMAT_ID, PROPERTIES_JDK_DIALECT_ID, registration};

let registration = registration().unwrap();
assert_eq!(registration.descriptor().format().as_str(), FORMAT_ID);
assert!(registration
    .descriptor()
    .dialects()
    .iter()
    .any(|d| d.as_str() == PROPERTIES_JDK_DIALECT_ID));
```

## INI

`registration_ini()` serves `ini` (extensions `ini`, `cfg` — not `conf`).
`[section]` is one nesting level. `;` and `#` open a line comment.
There are no escapes and no line continuations.

```rust
use jqf_codec_ini::{INI_FORMAT_ID, INI_JQF_STRICT_DIALECT_ID, registration_ini};

let registration = registration_ini().unwrap();
assert_eq!(registration.descriptor().format().as_str(), INI_FORMAT_ID);
assert!(registration
    .descriptor()
    .dialects()
    .iter()
    .any(|d| d.as_str() == INI_JQF_STRICT_DIALECT_ID));
```

## dotenv

`registration_dotenv()` serves `dotenv` (filenames `.env` and `.env.*`).
`export ` is stripped. Quoted values follow the dialect's quote rules.
`$VAR` is not expanded.

```rust
use jqf_codec_ini::{DOTENV_FORMAT_ID, DOTENV_JQF_STRICT_DIALECT_ID, registration_dotenv};

let registration = registration_dotenv().unwrap();
assert_eq!(registration.descriptor().format().as_str(), DOTENV_FORMAT_ID);
assert!(registration
    .descriptor()
    .dialects()
    .iter()
    .any(|d| d.as_str() == DOTENV_JQF_STRICT_DIALECT_ID));
```

## Contracts

See [`CONTRACTS.md`](CONTRACTS.md) for the three grammar, value, encode,
and edit invariants. Family laws: [`../CONTRACTS.md`](../CONTRACTS.md).
