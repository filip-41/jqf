# jqf-codec-html Contracts

Invariants for this crate and for hosts. Type overview and examples live
in [README.md](README.md).

This crate does not execute scripts, fetch URLs, open files, or splice
`--edit`. Family bind, publish, and error laws live in
[../CONTRACTS.md](../CONTRACTS.md).

## Membership

Two registrations, disjoint dialect sets. The catalog matches decoder and
encoder against the same descriptor list, so a dialect lives in exactly
one registration.

`registration()` carries `html.document@1`, `html.source@1`, and
`html.document-serialize@1`, and owns the `html` / `htm` extensions.
`registration_fragment()` carries `html.fragment@1` and no extensions.
`html.fragment-serialize@1` is reserved: fragment encode needs a context
channel this crate does not serve.

The CLI route fact is `Record`. There is no edit lane and no adjacent-value
stream. Every byte reaches the decoder.

## Recovery

The tokenizer and tree builder recover a document. Recovery mutates; it
does not reject a well-formedness error and it does not accept-then-fail.
Scripting is disabled, so `noscript` follows the scripting-disabled rules
and no script body runs.

The whole-document session admits once per tokenizer step. A text run
consumes at most 256 bytes per step. The located session recovers the
whole tree in one uncheckpointed pass before any selection is
authoritative.

Tokens cover the decoded text contiguously: token `i` ends where token
`i + 1` begins. For identity decodes the spans are raw-byte spans. Invalid
UTF-8 replacement shortens the decoded text; those spans are decoded
coordinates.

## Encoding determination

Order: UTF-8 BOM, then a `meta charset` prescan of the first 1024 bytes,
then windows-1252. UTF-16 BOMs are `UnsupportedRepresentation`. A prescan
label that is not a UTF-8 or windows-1252 alias is the same failure; v1
does not implement the other Encoding Standard indexes.

## Value model

An element is an array of recovered children: child elements and text
runs. Comments are attached facts (`html.comment@1`), never child values.
The element's name and attributes are facts; one fact per attribute so
`.&name` serves each. A recovered attribute name that is not a data
identity uses `html.attr-bytes@1`.

Document-level facts on the document element: mode, pragma-set default
language, doctype. A comment-only or doctype-only input still recovers;
the projection synthesizes the document element the tree builder produced.

Located navigation walks the same child projection the builder uses,
by array-index and array-range steps. A member step that hits two or more
children is a stream: the located session declines
(`RequirementMismatch`) and the whole-document floor plus engine
navigation produce the items.

Duplicate start-tag attributes are first-wins.

## Access

After recover, empty-path `length` and Whole bare-root `type` may project
a measure skeleton: the document element plus NAME-only child stubs.
`.[]` does not — measure children are not recovered elements.

## Fragment

`html.fragment@1` runs the fragment algorithm with the fixed context
element `div`. A per-invocation context is not served.

## Encode

`html.source@1` echoes the sealed source of an unchanged whole HTML
document whose located item is the document root. Any other item is
`UnsupportedRepresentation`. There is no serializer fallback under the
source profile.

`html.document-serialize@1` emits exactly one UTF-8 BOM, then the
document element. Void elements have no end tag. A void name with a
non-null value is refused, never dropped. Comment facts on one node are
merged by tree position so a leading comment is not overwritten by an
inline one.
A doctype-bearing document is `UnsupportedRepresentation`: the byte law
writes the document element only, and re-decoding that output would
recover quirks mode. A subtree needs a fragment serializer this crate
does not serve.

A located node of a non-HTML document, or an owned value, lowers into
the element model and serializes with the same algorithm. The document
element is `root`. An object member `k` is a child named `k`. An array
item is a child named `item`. A string, number, or boolean is a text
run. `null` is an empty element. A key that is not a tag name, and
byte, temporal, or tagged values, are refused, never renamed.

Failed encode publishes zero bytes. Recycled encoder state equals fresh
state.

## Errors

User-reachable renderings are prose. Encoding and representation refusals
are `UnsupportedRepresentation`. A bind that the advertised slots cannot
serve is `ProviderRouteMismatch`.
