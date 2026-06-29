//! Count cl100k_base tokens of the given files (or of stdin when no files are
//! passed). Used to measure the token economy of querying the graph versus
//! reading source. Reproduces the figures in the project README.
//!
//!   cargo run -q -p codegraph-server --example tokcount -- file1 file2 ...
//!   some_command | cargo run -q -p codegraph-server --example tokcount

use std::io::Read;

fn main() {
    let bpe = tiktoken_rs::cl100k_base().expect("load cl100k_base");
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut text = String::new();
    if args.is_empty() {
        std::io::stdin()
            .read_to_string(&mut text)
            .expect("read stdin");
    } else {
        for path in &args {
            match std::fs::read_to_string(path) {
                Ok(s) => {
                    text.push_str(&s);
                    text.push('\n');
                }
                Err(e) => eprintln!("skip {path}: {e}"),
            }
        }
    }
    println!("{}", bpe.encode_with_special_tokens(&text).len());
}
