//! Byte breakdown of a shard store: how much of each `.redb` file is node
//! rows, link rows, index blobs, and structural overhead.
//!
//! Usage: `cargo run -p synaptic-store --release --example store_report -- <store-dir>`

use synaptic_store::ShardStore;

fn mib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: store_report <store-dir>");
    let store = ShardStore::open(std::path::Path::new(&dir)).expect("opening store");
    let mut tot_file = 0u64;
    for (tag, s) in store.stats().expect("reading stats") {
        let payload = s.node_value_bytes + s.link_value_bytes + s.meta_value_bytes;
        let overhead = s.file_bytes.saturating_sub(payload + s.index_blob_bytes);
        tot_file += s.file_bytes;
        println!(
            "shard {tag}: file {:.2} MiB | nodes {} rows / {:.2} MiB | links {} rows / {:.2} MiB | index blobs {} / {:.2} MiB | redb overhead {:.2} MiB",
            mib(s.file_bytes),
            s.node_rows,
            mib(s.node_value_bytes),
            s.link_rows,
            mib(s.link_value_bytes),
            s.index_blob_rows,
            mib(s.index_blob_bytes),
            mib(overhead),
        );
    }
    println!("total store: {:.2} MiB across shard files", mib(tot_file));
}
