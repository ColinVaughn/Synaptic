//! Byte-for-byte output guards for the memory-reduction work.
//!
//! Every optimisation in that series claims "the artifacts are unchanged".
//! These tests are what makes the claim checkable: they pin the `graph.json`
//! shape and hash the secondary writers, so a refactor that moves a byte fails
//! here rather than silently changing a committed artifact.

use crate::tests_support::{kg_federated, kg_with_asset, sample_kg};
use crate::{to_cypher_string, to_dot_string, to_graphml_string, to_json_value};

/// FNV-1a over the writer output. Deliberately dependency-free and
/// endian-independent so the expected values are stable on every CI platform.
fn fingerprint(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `to_json_value` is the exact value pretty-printed into `graph.json`.
/// Pinning its structure pins the file.
#[test]
fn graph_json_structure_is_stable() {
    for (name, kg) in [
        ("sample", sample_kg()),
        ("asset", kg_with_asset()),
        ("federated", kg_federated()),
    ] {
        let text = serde_json::to_string_pretty(&to_json_value(&kg)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        let nodes = parsed["nodes"].as_array().unwrap();
        assert!(!nodes.is_empty(), "{name}: nodes present");
        for n in nodes {
            let obj = n.as_object().unwrap();
            for key in ["id", "label", "file_type", "source_file", "norm_label"] {
                assert!(obj.contains_key(key), "{name}: {key} is a top-level key");
            }
            assert!(
                !obj.contains_key("extra"),
                "{name}: extra must stay flattened, never nested"
            );
        }
        for l in parsed["links"].as_array().unwrap() {
            let obj = l.as_object().unwrap();
            assert!(
                obj.contains_key("confidence_score"),
                "{name}: confidence_score is defaulted on export"
            );
        }
    }
}

/// The streaming writer must produce exactly what the buffered
/// build-a-`Value`-then-a-`String` path produced.
#[test]
fn to_json_file_matches_pretty_printed_value() {
    for (name, kg) in [
        ("sample", sample_kg()),
        ("asset", kg_with_asset()),
        ("federated", kg_federated()),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        crate::to_json(&kg, &path).unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        let expected = serde_json::to_string_pretty(&to_json_value(&kg)).unwrap();
        assert_eq!(on_disk, expected, "{name}: streamed bytes match buffered");
    }
}

/// Whole-graph text exports are unusable past a few hundred thousand nodes
/// (a 379k-node corpus produced a 342 MB graphml and a 259 MB graph-3d.html),
/// so past the cap they must be skipped rather than written.
#[test]
fn bulk_exports_are_skipped_over_the_cap() {
    assert!(
        crate::bulk_export_is_viable(1_000),
        "ordinary graphs still export"
    );
    assert!(
        crate::bulk_export_is_viable(crate::BULK_EXPORT_MAX_NODES),
        "the cap itself is inclusive"
    );
    assert!(
        !crate::bulk_export_is_viable(crate::BULK_EXPORT_MAX_NODES + 1),
        "over-cap graphs are skipped"
    );
}

/// The writers must be pure functions of the graph.
#[test]
fn string_writers_are_deterministic() {
    let kg = sample_kg();
    assert_eq!(to_graphml_string(&kg), to_graphml_string(&kg));
    assert_eq!(to_cypher_string(&kg), to_cypher_string(&kg));
    assert_eq!(to_dot_string(&kg), to_dot_string(&kg));
}

/// Exact content hashes, recorded from the pre-refactor implementation. A
/// change here means the emitted bytes moved: either fix the regression or
/// update these values deliberately, in the same commit that changes them.
#[test]
fn writer_output_hashes_are_stable() {
    let kg = sample_kg();
    let graphml = fingerprint(&to_graphml_string(&kg));
    let cypher = fingerprint(&to_cypher_string(&kg));
    let dot = fingerprint(&to_dot_string(&kg));
    let json = fingerprint(&serde_json::to_string_pretty(&to_json_value(&kg)).unwrap());
    println!("GRAPHML={graphml:#x} CYPHER={cypher:#x} DOT={dot:#x} JSON={json:#x}");

    assert_eq!(graphml, GRAPHML_FINGERPRINT, "graphml bytes changed");
    assert_eq!(cypher, CYPHER_FINGERPRINT, "cypher bytes changed");
    assert_eq!(dot, DOT_FINGERPRINT, "dot bytes changed");
    assert_eq!(json, JSON_FINGERPRINT, "graph.json bytes changed");
}

// Recorded from the implementation as it stood before the memory work began.
const GRAPHML_FINGERPRINT: u64 = 0x5ba4_57d6_c0af_92e8;
const CYPHER_FINGERPRINT: u64 = 0x8ab6_7ebc_dbe5_f733;
const DOT_FINGERPRINT: u64 = 0xf89f_061c_3371_b2e1;
const JSON_FINGERPRINT: u64 = 0x8718_e2a0_1aaf_9a97;
