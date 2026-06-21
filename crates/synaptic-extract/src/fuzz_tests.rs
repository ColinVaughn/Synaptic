//! Property tests: the regex-based extractors must never panic on arbitrary
//! input. The audit converted their bare unwraps to documented `.expect()` /
//! group-0 invariants; these prove no byte sequence can trip them. Each
//! extractor is gated on its `lang-*` feature so the module compiles under any
//! feature subset (the CI matrix runs them under `--all-features`).
#![cfg(test)]

/// `mod <name> { proptest!{ fn never_panics(random bytes) { extractor(..) } } }`,
/// gated on the language feature.
macro_rules! fuzz_extractor {
    ($feat:literal, $name:ident, $func:path, $label:literal) => {
        #[cfg(feature = $feat)]
        mod $name {
            use proptest::prelude::*;
            proptest! {
                #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]
                /// Arbitrary bytes must never panic the extractor.
                #[test]
                fn never_panics_bytes(src in proptest::collection::vec(any::<u8>(), 0..4096)) {
                    let _ = $func($label, &src);
                }
                /// Arbitrary UTF-8 (exercises the regex paths more deeply) likewise.
                #[test]
                fn never_panics_utf8(src in ".{0,4096}") {
                    let _ = $func($label, src.as_bytes());
                }
            }
        }
    };
}

fuzz_extractor!(
    "lang-dart",
    dart,
    crate::dart::extract_dart_source,
    "fuzz.dart"
);
fuzz_extractor!(
    "lang-apex",
    apex,
    crate::apex::extract_apex_source,
    "fuzz.cls"
);
fuzz_extractor!(
    "lang-pascal",
    pascal,
    crate::pascal::extract_pascal_source,
    "fuzz.pas"
);
fuzz_extractor!("lang-asp", asp, crate::asp::extract_asp_source, "fuzz.asp");
fuzz_extractor!(
    "lang-dotnet",
    dotnet,
    crate::dotnet::extract_dotnet_source,
    "fuzz.csproj"
);
fuzz_extractor!(
    "lang-markdown",
    markdown,
    crate::markdown::extract_markdown_source,
    "fuzz.md"
);
fuzz_extractor!("lang-sql", sql, crate::sql::extract_sql_source, "fuzz.sql");
