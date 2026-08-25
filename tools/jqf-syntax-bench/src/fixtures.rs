use std::fmt::Write as _;

pub(crate) const GENERATED_PROGRAM_BYTES: usize = 1_048_576;
pub(crate) const GENERATED_PROGRAM_DEFINITIONS: usize = 10_811;
pub(crate) const ESCAPED_LITERAL_BYTES: usize = 256 * 1024;

pub(crate) struct GeneratedProgram {
    pub(crate) source: String,
    pub(crate) definition_count: usize,
}

pub(crate) struct EscapedLiteral {
    pub(crate) source: String,
    pub(crate) expected: String,
    pub(crate) escape_count: usize,
}

pub(crate) fn feature_rich_query() -> String {
    r#"def normalize($fallback):
  (.name // $fallback) as $name
  | {
      id: .id,
      name: $name,
      tags: [.tags[]?],
      semantic_tag: .price.@tag,
      link: .link.&href,
      display: "id=\(.id) name=\($name)"
    };
.items[] | select(.active) | normalize("unknown")"#
        .into()
}

pub(crate) fn string_heavy_query() -> String {
    r#"{
  title: "jqf syntax benchmark: escaped quote=\" slash=\\ newline=\n tab=\t unicode=\u03bb",
  summary: "A representative string-heavy filter keeps realistic text, URLs, and escaped JSON fragments together.",
  endpoint: "https://api.example.test/v1/items?include=prices%2Cavailability&locale=en-GB",
  payload: "{\"kind\":\"example\",\"enabled\":true,\"notes\":[\"first\",\"second\"]}",
  labels: ["catalog", "featured", "summer collection", "long-form merchandising copy"]
}"#
    .into()
}

pub(crate) fn interpolation_heavy_query() -> String {
    r#"{
  title: "\(.product.name) — \(.product.variant // "standard")",
  price: "\(.product.price.amount) \(.product.price.currency)",
  availability: "stock=\(.inventory.available) reserved=\(.inventory.reserved) warehouse=\(.inventory.warehouse.code)",
  url: "https://shop.example.test/\(.product.slug)?campaign=\(.campaign.id)&locale=\(.locale)",
  audit: "id=\(.product.id) tag=\(.product.@tag) owner=\(.product.owner.name)"
}"#
    .into()
}

pub(crate) fn mixed_postfix_query() -> String {
    r#".catalog.items[]?
| select(.availability.regions[.request.region].enabled)
| {
    id: .identity.primary["sku"],
    title: .content.localized[.request.locale].title?,
    semantic_tag: .pricing.current.@tag,
    comment: .content.localized[.request.locale].@comment.leading?,
    href: .links["product"].&href?,
    aria_label: .links["product"].&["aria-label"]?,
    source_name: .provenance.@(.request.source_fact)?
  }"#
    .into()
}

pub(crate) fn large_program() -> String {
    let mut source = String::with_capacity(32 * 1024);
    source.push_str(
        "module {name: \"syntax-benchmark\"};\n\
         import \"support\" as support {search: \".\"};\n\
         include \"strings\";\n",
    );
    for index in 0..128 {
        writeln!(
            source,
            "def project_{index}($value): \
             {{index: {index}, value: $value, tag: $value.@tag, href: $value.&href}};"
        )
        .expect("writing to String cannot fail");
    }
    source.push_str("project_127(.items[])\n");
    source
}

pub(crate) fn generated_program_1m() -> GeneratedProgram {
    const TAIL: &str = ".items[]";

    let mut source = String::with_capacity(GENERATED_PROGRAM_BYTES);
    source.push_str(
        "module {name: \"generated-syntax-benchmark\"};\n\
         import \"support\" as support {search: \".\"};\n\
         include \"strings\";\n",
    );
    let mut definition_count = 0;
    loop {
        let mut definition = String::new();
        writeln!(
            definition,
            "def generated_{definition_count:05}($value): \
             {{index: {definition_count}, value: $value, tag: $value.@tag, \
             href: $value.&href}};"
        )
        .expect("writing to String cannot fail");
        if source.len() + definition.len() + TAIL.len() > GENERATED_PROGRAM_BYTES {
            break;
        }
        source.push_str(&definition);
        definition_count += 1;
    }
    let padding = GENERATED_PROGRAM_BYTES - source.len() - TAIL.len();
    source.extend(std::iter::repeat_n(' ', padding));
    source.push_str(TAIL);
    assert_eq!(source.len(), GENERATED_PROGRAM_BYTES);
    assert_eq!(definition_count, GENERATED_PROGRAM_DEFINITIONS);
    GeneratedProgram {
        source,
        definition_count,
    }
}

pub(crate) fn escaped_literal_256k() -> EscapedLiteral {
    const ENCODED_PATTERN: &str = r#"a\\b\/c\n\t\u03bb\uD83D\uDE00\"|"#;
    const DECODED_PATTERN: &str = "a\\b/c\n\tλ😀\"|";
    const ESCAPES_PER_PATTERN: usize = 8;

    let repetitions = ESCAPED_LITERAL_BYTES / ENCODED_PATTERN.len();
    let remainder = ESCAPED_LITERAL_BYTES % ENCODED_PATTERN.len();
    let mut source = String::with_capacity(ESCAPED_LITERAL_BYTES + 2);
    let mut expected = String::with_capacity(repetitions * DECODED_PATTERN.len() + remainder);
    source.push('"');
    for _ in 0..repetitions {
        source.push_str(ENCODED_PATTERN);
        expected.push_str(DECODED_PATTERN);
    }
    source.extend(std::iter::repeat_n('x', remainder));
    expected.extend(std::iter::repeat_n('x', remainder));
    source.push('"');
    assert_eq!(source.len(), ESCAPED_LITERAL_BYTES + 2);
    EscapedLiteral {
        source,
        expected,
        escape_count: repetitions * ESCAPES_PER_PATTERN,
    }
}
