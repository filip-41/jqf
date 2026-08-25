# The xml.xpath@1 conformance corpus: the libxml2 XPath test suite

The corpus is the XPath test suite of the GNOME libxml2 project — a real,
maintained conformance suite exercised by libxml2's own `testXPath` since
~2000, not a hand-picked case list.

- Source: `https://gitlab.gnome.org/GNOME/libxml2` (mirror
  `https://github.com/GNOME/libxml2`), directory `test/XPath` (documents),
  `test/XPath/tests` (expressions) and `result/XPath/tests` (expected
  node-set dumps).
- Pinned commit: `459146140aa0` (2024-11-05), the last commit touching
  `test/XPath` at vendoring time.
- License: MIT (libxml2), compatible with this tree's MIT OR Apache-2.0.
- Regenerate: re-download the three directories at the pinned commit and
  verify every file's SHA-256 below; the aggregate checksum is
  `c7c71643322a84e7e3f655e180b337a562dee9cda540d07f832ba7dd9c6fc7eb` over
  the sorted per-file digests.

## How the runner uses it

Each `tests/<name>` file holds one XPath expression per line, evaluated by
libxml2 with the DOCUMENT ELEMENT as context node over `docs/<fixture>`; the
corresponding `result/<name>` file holds the expected node-set dump
(`Expression:` blocks, top-level selected nodes as `N  ELEMENT name` lines,
nested nodes indented). The runner:

- **out-of-profile**: an expression the closed `xml.xpath@1` grammar rejects
  at compile is counted and must keep rejecting — the profile boundary is
  part of the conformance, not a footnote.
- **in-profile**: the expression compiles and runs over the whole-route
  decoded document with the document element as context; the selected
  elements (document order, deduplicated) must equal the oracle's top-level
  `ELEMENT` entries in order. The oracle's `#XX` byte escapes are decoded.
- **element-filtered**: the `xml.xpath@1` law restricts results to elements
  (the profile's own rule), so a case whose oracle selects the document node,
  text, comments, CDATA, or PIs is compared on its ELEMENT entries alone —
  the `/.`, `//.`, and `..` abbreviation cases.

## Per-file digests

097b08a24679441b8c27a7b0d716d61859a28097a2194ea0d56654c1784e227c  tests/usr1check
0f79ad4550060c9457a46a2815d9bf7f7f8e3ebdc509709ec2e4008055a9b86a  docs/nodes
29690b5245047fdb604a762e9e62c3684a3a78135a9a6d2f50a3739a2d036682  result/vidbase
30d1cbb59ba1dddefafc6200be1dd5d84d438c5fc8f13d8647e7389cbed9f98d  docs/mixed
31049ca2bc077397dfa9634d248c832c1699eb25e5b1b200fc42ce4870d69774  tests/nssimple
38129fa445a399d503abdba591d693329019836e3a410130e7baf450f3344acf  tests/strbase
3e157548986c04aab8b0df2ca810460958ba86247981fef663dcd962c0bcb1fb  docs/unicode
49798054762b4932fb8d1310c4744581a27ff72ed5fac9b61c0ecc2d0de40064  docs/str
519b5c398e3a5bac682d87d80f0ead75940fd2c8f315567e029cad6dde540c48  docs/simple
60005f707f6ff7165a9c1ba743a81fbb92298ea2b7442a34559de83b3b55b018  result/idsimple
685979a263d86933fe80177d3365678fd7b4a337e44b34cdabe1bb75340dcf5e  tests/vidbase
691427a163cc9005f33a910a51585a1ffe9703af30811f1124498fd375885568  result/nodespat
6c04cea58d282e454c35c07ae04a3cc344c2e4e10a84933c19c46a4867873444  tests/simplebase
6c861ceda45f583e0f6ce4d8111f7c4cb47683762f756d6adaf90d2c1f2ffb85  result/unicodesimple
6f1da9cb4503a456349f9e7fc27ddde383476c83c6dd1c7bd9e893043cde6195  tests/chaptersbase
6fb1499fd24be53e218ad9ea7f6cfcce2e6ffc8982483dc18123d96be4af2676  result/simpleabbr
73e4e5376e05288f7af738e0de48e30c52ef89d2069eeb3f9cebbdadd5841390  tests/idsimple
82dde82bb97ad197435d34cb1df12a9b9013daf6be6dd94724936cfff035490f  docs/vid
84a20d9815b067d20ca1a135a95dbeecad275eac76f2ec8395a12135e026f67e  docs/usr1
857a3d005d37333bb4ac05d560d7320c646e2587172c47128dcb0b0195db56c0  tests/nodespat
87a07078dd578f6d111af915f7976cbb62264eecc66371b99cadd704052c03ea  result/langsimple
87c2c0e46081eedb5bf94cdd2210d1fc543239aaf68fcf3115982768665706e2  tests/chaptersprefol
8f06e8c3e75fd37804821c5cb08b4b9b6c9819b2923783698da1696eef4cbdf4  docs/chapters
96541ee6bfef52cea3a71b683c13f69eedece91485fd053f5bfb4f4f8330de42  result/usr1check
98297954ea012864b53405b03cdcf7cf9dec5c43eb9e6787e7378281ff7b5d8b  result/strbase
985595e29126ead7686c4eaac7e1f7513390d5db0c441b8240cd9f96e03001f4  docs/lang
9c8af3af75adfc2a10bcf065b400c538754e030eb0948f4612caca76847b062a  docs/ns
a1a33110a81fab8c1632565b064d72a72b52abc9ac27dd699997a01dacb6543f  tests/simpleabbr
a7de34764a3743345e3e554b9a7a6ee24e3a5023c4cbcd145bde50cf7f663939  result/simplebase
b15ea261e0ed9827d416b26e17aac2e3271b38977fa0172146df83100d0d1f1d  docs/id
b617a34d81e5dbbd32b6376fe0678fea9c87980b2dda206deeb82f124224bf31  tests/unicodesimple
c57847e175d1ac8fc4fe340972c04aa06013526d3c8be3bdb9fd1ef10a4d8edb  result/chaptersbase
cc2f0d6166b0d5b66ac389b0b80141e6bb090fa196fbdb24eb1203eabe06d523  result/mixedpat
dada871cdec21d662f243a4149e45e56da19799d34f16f0fe44919071a24cf8c  result/nssimple
dbb555fafa71bd39420b674dadf8f6397b1d5334c53223d80743567d16fad412  result/chaptersprefol
e13d70572d1839b4ebdf527c361b317b0e7e70de8960686a814de2516a0c9351  tests/mixedpat
e87f4df792efae48c38b372cb1f1682cab0e84b8d86862a86fe39c91c0c6e27d  tests/langsimple
