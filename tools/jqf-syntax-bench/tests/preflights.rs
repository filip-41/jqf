#[test]
fn exact_inventory_and_every_preflight_are_stable() {
    let mut cases = jqf_syntax_bench::cases();
    let metadata: Vec<_> = cases.iter().map(|case| case.metadata()).collect();
    assert_eq!(
        metadata.iter().map(|metadata| metadata.name).collect::<Vec<_>>(),
        [
            "lexer/feature-rich-query",
            "parser/short-path",
            "parser/feature-rich-query",
            "parser/string-heavy-query",
            "parser/interpolation-heavy-query",
            "parser/mixed-postfix-query",
            "parser/large-program",
            "parser/generated-program-1m",
            "visitor/generated-program-1m",
            "string-decode/escaped-256k",
        ]
    );
    assert!(
        metadata[..8]
            .iter()
            .all(|metadata| metadata.operations_per_invocation == 1)
    );
    assert_eq!(metadata[7].bytes_per_invocation, 1_048_576);
    assert_eq!(metadata[8].operations_per_invocation, 454_112);
    assert_eq!(metadata[8].bytes_per_invocation, 1_048_576);
    assert_eq!(metadata[9].operations_per_invocation, 256 * 1024);
    assert_eq!(metadata[9].bytes_per_invocation, 256 * 1024);

    for case in &mut cases {
        let name = case.metadata().name;
        let receipt = case
            .preflight()
            .unwrap_or_else(|error| panic!("{name} preflight failed: {error}"));
        assert_ne!(receipt.checksum, 0, "{name}");
        assert!(!receipt.detail.is_empty(), "{name}");
        if name == "visitor/generated-program-1m" {
            assert_eq!(receipt.checksum, 0xb5dc_8ca9_d137_95e1);
            assert!(receipt.detail.contains("source_bytes=1048576"));
            assert!(receipt.detail.contains("definitions=10811"));
            assert!(receipt.detail.contains("events=454112"));
            assert!(receipt.detail.contains("enters=227056"));
            assert!(receipt.detail.contains("exits=227056"));
            assert!(receipt.detail.contains("maximum_depth=7"));
            assert!(receipt.detail.contains("final_depth=0"));
            assert!(receipt.detail.contains("node_accessors=10811"));
            assert!(receipt.detail.contains("attributes=10811"));
        }
    }
}
