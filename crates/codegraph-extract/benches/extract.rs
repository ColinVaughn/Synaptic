//! Criterion benchmarks for `codegraph-extract`.
//!
//! Two input modes:
//!   * `per_language` — a single representative source file per language, with
//!     `Throughput::Bytes` so results read as MB/s of source parsed. The Rust
//!     sample is a real in-tree file (`../src/walker.rs`) via `include_bytes!`;
//!     the others are representative embedded snippets (the workspace is
//!     Rust-only, so there are no real .py/.js/.ts/.go files to walk).
//!   * `repo_walk` — parse every `.rs` file under the workspace `crates/` dir,
//!     cold vs. warm-cache, end-to-end parsing throughput.
//!
//! Run: `cargo bench -p codegraph-extract`

use std::path::{Path, PathBuf};

use codegraph_extract::python::python_config;
use codegraph_extract::{cached_extract_source, extract_source, ExtractionResult};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tree_sitter::Parser;

// Per-language samples

const PY_SAMPLE: &str = r#"
import os
import sys
from collections import defaultdict, OrderedDict
from typing import Optional, List

CONFIG = {"retries": 3, "timeout": 30}

class Cache:
    def __init__(self, capacity: int):
        self.capacity = capacity
        self._store: OrderedDict = OrderedDict()

    def get(self, key):
        if key not in self._store:
            return None
        self._store.move_to_end(key)
        return self._store[key]

    def put(self, key, value):
        self._store[key] = value
        self._store.move_to_end(key)
        if len(self._store) > self.capacity:
            self._store.popitem(last=False)

def build_index(paths: List[str]) -> dict:
    index = defaultdict(list)
    for p in paths:
        stem = os.path.basename(p).split(".")[0]
        index[stem].append(p)
    return dict(index)

def main(argv: Optional[List[str]] = None) -> int:
    argv = argv if argv is not None else sys.argv[1:]
    cache = Cache(capacity=128)
    idx = build_index(argv)
    for stem, files in idx.items():
        cache.put(stem, files)
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
"#;

const JS_SAMPLE: &str = r#"
import { readFile } from "node:fs/promises";
import path from "node:path";

const CONFIG = { retries: 3, timeout: 30 };

export class Cache {
  constructor(capacity) {
    this.capacity = capacity;
    this.store = new Map();
  }

  get(key) {
    if (!this.store.has(key)) return null;
    const value = this.store.get(key);
    this.store.delete(key);
    this.store.set(key, value);
    return value;
  }

  put(key, value) {
    this.store.set(key, value);
    if (this.store.size > this.capacity) {
      const oldest = this.store.keys().next().value;
      this.store.delete(oldest);
    }
  }
}

export function buildIndex(paths) {
  const index = {};
  for (const p of paths) {
    const stem = path.basename(p).split(".")[0];
    (index[stem] ||= []).push(p);
  }
  return index;
}

export async function loadAll(paths) {
  const cache = new Cache(128);
  const index = buildIndex(paths);
  for (const [stem, files] of Object.entries(index)) {
    const contents = await Promise.all(files.map((f) => readFile(f, "utf8")));
    cache.put(stem, contents);
  }
  return cache;
}
"#;

const TS_SAMPLE: &str = r#"
import { readFile } from "node:fs/promises";
import path from "node:path";

interface Config {
  retries: number;
  timeout: number;
}

const CONFIG: Config = { retries: 3, timeout: 30 };

export class Cache<K, V> {
  private store = new Map<K, V>();
  constructor(private capacity: number) {}

  get(key: K): V | null {
    if (!this.store.has(key)) return null;
    const value = this.store.get(key)!;
    this.store.delete(key);
    this.store.set(key, value);
    return value;
  }

  put(key: K, value: V): void {
    this.store.set(key, value);
    if (this.store.size > this.capacity) {
      const oldest = this.store.keys().next().value as K;
      this.store.delete(oldest);
    }
  }
}

export function buildIndex(paths: string[]): Record<string, string[]> {
  const index: Record<string, string[]> = {};
  for (const p of paths) {
    const stem = path.basename(p).split(".")[0];
    (index[stem] ||= []).push(p);
  }
  return index;
}

export async function loadAll(paths: string[]): Promise<Cache<string, string[]>> {
  const cache = new Cache<string, string[]>(CONFIG.retries * 40);
  const index = buildIndex(paths);
  for (const [stem, files] of Object.entries(index)) {
    const contents = await Promise.all(files.map((f) => readFile(f, "utf8")));
    cache.put(stem, contents);
  }
  return cache;
}
"#;

const GO_SAMPLE: &str = r#"
package cache

import (
	"container/list"
	"path/filepath"
	"strings"
)

type entry struct {
	key   string
	value interface{}
}

type Cache struct {
	capacity int
	ll       *list.List
	items    map[string]*list.Element
}

func New(capacity int) *Cache {
	return &Cache{
		capacity: capacity,
		ll:       list.New(),
		items:    make(map[string]*list.Element),
	}
}

func (c *Cache) Get(key string) (interface{}, bool) {
	if el, ok := c.items[key]; ok {
		c.ll.MoveToFront(el)
		return el.Value.(*entry).value, true
	}
	return nil, false
}

func (c *Cache) Put(key string, value interface{}) {
	if el, ok := c.items[key]; ok {
		c.ll.MoveToFront(el)
		el.Value.(*entry).value = value
		return
	}
	el := c.ll.PushFront(&entry{key, value})
	c.items[key] = el
	if c.ll.Len() > c.capacity {
		oldest := c.ll.Back()
		if oldest != nil {
			c.ll.Remove(oldest)
			delete(c.items, oldest.Value.(*entry).key)
		}
	}
}

func BuildIndex(paths []string) map[string][]string {
	index := make(map[string][]string)
	for _, p := range paths {
		stem := strings.SplitN(filepath.Base(p), ".", 2)[0]
		index[stem] = append(index[stem], p)
	}
	return index
}
"#;

// A real in-tree Rust file: honest Rust parsing throughput.
const RS_SAMPLE: &[u8] = include_bytes!("../src/walker.rs");

// Repo fixture (real .rs files under <workspace>/crates)

/// Workspace root, derived at compile time from this crate's manifest dir
/// (`crates/codegraph-extract` -> `../..`).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("canonicalize workspace root")
}

/// Recursively collect `(path, bytes)` for every `.rs` file under `dir`,
/// skipping any `target/` directory. Reads contents up front so the timed
/// loop measures parsing, not disk I/O.
fn collect_rs(dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            collect_rs(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            if let Ok(bytes) = std::fs::read(&path) {
                out.push((path.to_string_lossy().into_owned(), bytes));
            }
        }
    }
}

fn repo_rs_files() -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    collect_rs(&workspace_root().join("crates"), &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0)); // deterministic order
    files
}

// Benchmarks

fn bench_per_language(c: &mut Criterion) {
    let cases: [(&str, &str, &[u8]); 5] = [
        ("python", "sample.py", PY_SAMPLE.as_bytes()),
        ("javascript", "sample.js", JS_SAMPLE.as_bytes()),
        ("typescript", "sample.ts", TS_SAMPLE.as_bytes()),
        ("go", "sample.go", GO_SAMPLE.as_bytes()),
        ("rust", "walker.rs", RS_SAMPLE),
    ];

    let mut group = c.benchmark_group("extract/per_language");
    for (lang, path, src) in cases {
        group.throughput(Throughput::Bytes(src.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(lang),
            &(path, src),
            |b, (p, s)| {
                b.iter(|| black_box(extract_source(black_box(p), black_box(s))));
            },
        );
    }
    group.finish();
}

fn bench_repo_walk(c: &mut Criterion) {
    let files = repo_rs_files();
    assert!(!files.is_empty(), "expected .rs files under crates/");
    let total_bytes: u64 = files.iter().map(|(_, b)| b.len() as u64).sum();

    let mut group = c.benchmark_group("extract/repo_walk");
    group.throughput(Throughput::Bytes(total_bytes));
    group.sample_size(20);

    // Cold: parse every file with no cache.
    group.bench_function("cold_no_cache", |b| {
        b.iter(|| {
            for (p, s) in &files {
                black_box(extract_source(p, s));
            }
        });
    });

    // Warm cache: pre-populate, then measure cache-hit reads.
    let cache_dir = tempfile::tempdir().expect("tempdir");
    for (p, s) in &files {
        let _ = cached_extract_source(Some(cache_dir.path()), p, s);
    }
    group.bench_function("warm_cache_hits", |b| {
        b.iter(|| {
            for (p, s) in &files {
                black_box(cached_extract_source(Some(cache_dir.path()), p, s));
            }
        });
    });

    group.finish();
}

/// Isolates the cost of `extract_with_config`'s per-file parser construction
/// (`walker.rs`: `Parser::new()` + `set_language()` on every call) by comparing
/// reconstruct-per-parse against reusing one parser. Parsing only — no symbol
/// extraction — so the delta is purely tree-sitter setup overhead.
fn bench_parser_setup(c: &mut Criterion) {
    let cfg = python_config();
    let src = PY_SAMPLE.as_bytes();

    let mut group = c.benchmark_group("extract/parser_setup");
    group.throughput(Throughput::Bytes(src.len() as u64));

    group.bench_function("reconstruct_per_parse", |b| {
        b.iter(|| {
            let mut parser = Parser::new();
            parser
                .set_language(&(cfg.language)())
                .expect("load language");
            black_box(parser.parse(black_box(src), None));
        });
    });

    group.bench_function("reuse_parser", |b| {
        let mut parser = Parser::new();
        parser
            .set_language(&(cfg.language)())
            .expect("load language");
        b.iter(|| {
            black_box(parser.parse(black_box(src), None));
        });
    });

    group.finish();
}

/// A/B of the AST-cache serialization format: deserialize every repo file's
/// `ExtractionResult` from an in-memory JSON blob vs a MessagePack blob. In-memory
/// (no file I/O) isolates the pure decode cost -- the part a format change moves --
/// from the per-file syscall overhead `warm_cache_hits` also pays. The `msgpack`
/// arm and the size report only exist under `--features cache-binary`.
fn bench_cache_format(c: &mut Criterion) {
    let files = repo_rs_files();
    let results: Vec<ExtractionResult> = files
        .iter()
        .filter_map(|(p, s)| extract_source(p, s))
        .collect();
    let json_blobs: Vec<Vec<u8>> = results
        .iter()
        .map(|r| serde_json::to_vec(r).expect("json serialize"))
        .collect();
    let total_json: u64 = json_blobs.iter().map(|b| b.len() as u64).sum();

    let mut group = c.benchmark_group("extract/cache_format");
    group.throughput(Throughput::Bytes(total_json));
    group.sample_size(20);

    group.bench_function("json_deserialize", |b| {
        b.iter(|| {
            for blob in &json_blobs {
                let r: ExtractionResult = serde_json::from_slice(blob).unwrap();
                black_box(r);
            }
        });
    });

    #[cfg(feature = "cache-binary")]
    {
        // MessagePack (rmp-serde, named maps so `#[serde(flatten)]` round-trips).
        let mp_blobs: Vec<Vec<u8>> = results
            .iter()
            .map(|r| rmp_serde::to_vec_named(r).expect("msgpack serialize"))
            .collect();
        let total_mp: u64 = mp_blobs.iter().map(|b| b.len() as u64).sum();
        eprintln!(
            "cache_format on-disk size: json={total_json} B, msgpack={total_mp} B \
             ({:.1}% of json)",
            100.0 * total_mp as f64 / total_json as f64
        );
        group.bench_function("msgpack_deserialize", |b| {
            b.iter(|| {
                for blob in &mp_blobs {
                    let r: ExtractionResult = rmp_serde::from_slice(blob).unwrap();
                    black_box(r);
                }
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_per_language,
    bench_repo_walk,
    bench_parser_setup,
    bench_cache_format
);
criterion_main!(benches);
