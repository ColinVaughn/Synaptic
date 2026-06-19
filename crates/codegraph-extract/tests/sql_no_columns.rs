//! End-to-end checks for the `--no-columns` SQL extraction switch: the process
//! global suppresses column nodes through the real extract path, and the AST
//! cache key distinguishes the two modes (so a warm cache never serves
//! column-ful results to a no-columns run).
#![cfg(feature = "lang-sql")]

use codegraph_core::NodeKind;
use codegraph_extract::sql::extract_sql_source;
use codegraph_extract::{cached_extract_source, set_emit_sql_columns};

const SRC: &[u8] = b"CREATE TABLE t (id INT PRIMARY KEY, email TEXT);";

fn has_columns(r: &codegraph_extract::ExtractionResult) -> bool {
    r.nodes.iter().any(|n| n.kind() == Some(NodeKind::Column))
}

// One test function: the global is process-wide, so flipping it must not race
// other tests in this binary.
#[test]
fn no_columns_global_and_cache_behavior() {
    // Default: columns are emitted through the real extract path.
    set_emit_sql_columns(true);
    assert!(
        has_columns(&extract_sql_source("s.sql", SRC)),
        "columns present by default"
    );

    // Flipping the global off suppresses them.
    set_emit_sql_columns(false);
    assert!(
        !has_columns(&extract_sql_source("s.sql", SRC)),
        "global off suppresses columns"
    );

    // Cache key must include the flag: populate the cache column-ful, then a
    // no-columns read must NOT return the cached column-ful entry.
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path();
    set_emit_sql_columns(true);
    assert!(has_columns(
        &cached_extract_source(Some(cache), "s.sql", SRC).unwrap()
    ));
    set_emit_sql_columns(false);
    assert!(
        !has_columns(&cached_extract_source(Some(cache), "s.sql", SRC).unwrap()),
        "no-columns read must not hit the column-ful cache entry"
    );

    set_emit_sql_columns(true); // reset for any later tests in this process
}
