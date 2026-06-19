//! Score CodeGraph's extracted graph against the hand-labeled corpus.

#[cfg(test)]
mod discovery {
    use codegraph_incremental::{rebuild, ChangeSet, RebuildOptions};
    use std::path::PathBuf;

    #[test]
    #[ignore] // discovery only; remove after the node/label/relation format is confirmed
    fn print_fixture_graph() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/systems-rust");
        let out = rebuild(
            &RebuildOptions {
                root,
                directed: true,
                force: true,
            },
            &ChangeSet::Full,
            None,
        )
        .expect("build fixture");
        let gd = out.kg.to_graph_data();
        for n in &gd.nodes {
            eprintln!(
                "NODE id={:?} label={:?} file={:?} kind={:?}",
                n.id, n.label, n.source_file, n.kind()
            );
        }
        for e in &gd.links {
            eprintln!("EDGE {:?} -{}-> {:?}", e.source, e.relation, e.target);
        }
    }
}
