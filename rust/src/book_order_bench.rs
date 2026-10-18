//! The cost of ordering hits by the order mapping ([`crate::book_order`]) against the columns
//! it replaces: `id` for the catalogue, and a per-document `generation` column for the
//! generations. `#[ignore]`, on a synthetic index built once into `OTZ_ORDER_BENCH_DIR`.
//!
//! Run: `cargo test --release --lib book_order_bench -- --ignored --nocapture
//! --test-threads=1`. `OTZ_ORDER_BENCH_BOOKS` and `OTZ_ORDER_BENCH_LINES` (average lines per
//! book) size the index.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tantivy::collector::sort_key::NaturalComparator;
use tantivy::collector::{Count, SegmentSortKeyComputer, SortKeyComputer, TopDocs};
use tantivy::columnar::Column;
use tantivy::query::{Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Schema, FAST, STRING, TEXT};
use tantivy::{doc, DocId, Index, Order, Score, SegmentReader, Term};

use crate::book_order::{BookNumbers, BookOrderSort, BookPlace, OrderKind, OrderTables};

/// The per-document generation column the mapping replaces, as `derived_columns` read it.
struct ColumnGenerationSort;

struct ColumnGenerationKeys {
    id: Column<u64>,
    generation: Column<u64>,
}

impl SortKeyComputer for ColumnGenerationSort {
    type SortKey = Option<u64>;
    type Child = ColumnGenerationKeys;
    type Comparator = NaturalComparator;

    fn segment_sort_key_computer(&self, reader: &SegmentReader) -> tantivy::Result<Self::Child> {
        Ok(ColumnGenerationKeys {
            id: reader.fast_fields().u64("id")?,
            generation: reader.fast_fields().u64("generation")?,
        })
    }
}

impl SegmentSortKeyComputer for ColumnGenerationKeys {
    type SortKey = Option<u64>;
    type SegmentSortKey = Option<u64>;
    type SegmentComparator = NaturalComparator;

    fn segment_sort_key(&mut self, doc: DocId, _score: Score) -> Option<u64> {
        let generation = self.generation.first(doc)?;
        let id = self.id.first(doc)?;
        Some((generation.min(255) << 56) | (id & ((1u64 << 56) - 1)))
    }

    fn convert_segment_sort_key(&self, sort_key: Option<u64>) -> Option<u64> {
        sort_key
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A small deterministic generator, so every run builds the same index.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    /// A word rank with a Zipf-like distribution over `vocabulary` words.
    fn zipf(&mut self, vocabulary: u64) -> u64 {
        let u = (self.next() % 1_000_000) as f64 / 1_000_000.0;
        ((vocabulary as f64).powf(u) - 1.0) as u64
    }
}

fn word(rank: u64) -> String {
    format!("w{rank}")
}

#[test]
#[ignore]
fn order_mapping_costs_no_more_than_the_columns() {
    let books = env_usize("OTZ_ORDER_BENCH_BOOKS", 6_000) as u32;
    let lines = env_usize("OTZ_ORDER_BENCH_LINES", 1_000) as u64;
    let dir = std::env::var("OTZ_ORDER_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("otz-order-bench"));
    let mut schema = Schema::builder();
    let text_f = schema.add_text_field("text", TEXT);
    let id_f = schema.add_u64_field("id", FAST);
    let generation_f = schema.add_u64_field("generation", FAST);
    let path_f = schema.add_text_field("filePath", STRING | FAST);
    let schema = schema.build();

    let mut rng = Lcg(42);
    // Catalogue position and generation of each book, by its number - 1.
    let mut catalogue: Vec<u32> = (0..books).collect();
    for i in (1..catalogue.len()).rev() {
        catalogue.swap(i, (rng.next() % (i as u64 + 1)) as usize);
    }
    let generations: Vec<u32> = (0..books).map(|_| (rng.next() % 10) as u32).collect();

    let index = if dir.join("meta.json").exists() {
        Index::open_in_dir(&dir).unwrap()
    } else {
        std::fs::create_dir_all(&dir).unwrap();
        let index = Index::create_in_dir(&dir, schema.clone()).unwrap();
        let started = Instant::now();
        let mut writer = index.writer(1 << 30).unwrap();
        let mut docs = 0u64;
        for book in 0..books {
            // Ids as the old scheme composed them, so the column sorts and the mapping
            // sorts must agree hit for hit.
            let base = (u64::from(catalogue[book as usize]) + 1) << 32;
            let count = lines / 4 + rng.next() % (lines * 3 / 2);
            for line in 0..count {
                let text: Vec<String> = (0..10).map(|_| word(rng.zipf(20_000))).collect();
                writer
                    .add_document(doc!(
                        text_f => text.join(" "),
                        id_f => base + line + 1,
                        generation_f => u64::from(generations[book as usize]),
                        path_f => format!("b{book}"),
                    ))
                    .unwrap();
                docs += 1;
            }
        }
        writer.commit().unwrap();
        let segments = index.searchable_segment_ids().unwrap();
        if segments.len() > 1 {
            writer.merge(&segments).wait().unwrap();
        }
        writer.wait_merging_threads().unwrap();
        println!(
            "built {docs} docs of {books} books in {:?}",
            started.elapsed()
        );
        index
    };
    let reader = index.reader().unwrap();
    let searcher = reader.searcher();
    println!(
        "{} docs, {} segment(s)",
        searcher.num_docs(),
        searcher.segment_readers().len()
    );

    let mut loads = Vec::new();
    let mut numbers = None;
    for _ in 0..3 {
        let started = Instant::now();
        numbers = Some(BookNumbers::load(&searcher, &dir).unwrap());
        loads.push(started.elapsed());
    }
    let numbers = numbers.unwrap();
    let places: HashMap<String, BookPlace> = (0..books)
        .map(|book| {
            (
                format!("b{book}"),
                BookPlace {
                    catalogue_order: catalogue[book as usize],
                    generation_order: generations[book as usize],
                },
            )
        })
        .collect();
    let mut builds = Vec::new();
    let mut tables = None;
    for _ in 0..3 {
        let started = Instant::now();
        tables = Some(Arc::new(OrderTables::build(numbers.paths(), &places)));
        builds.push(started.elapsed());
    }
    let tables = tables.unwrap();
    println!("book numbers read in {loads:?}, tables built in {builds:?}");

    let queries: Vec<(String, Box<dyn Query>)> = [0u64, 3, 30, 300, 3_000]
        .iter()
        .map(|&rank| {
            let term = Term::from_field_text(text_f, &word(rank));
            let query: Box<dyn Query> = Box::new(TermQuery::new(term, IndexRecordOption::Basic));
            (word(rank), query)
        })
        .collect();
    const RUNS: usize = 41;
    const WINDOW: usize = 160;
    println!("query     hits      | id col  map cat | gen col map gen   (median ms of {RUNS})");
    for (name, query) in &queries {
        let mut times: [Vec<f64>; 4] = Default::default();
        let mut found: [Vec<tantivy::DocAddress>; 4] = Default::default();
        let top = || TopDocs::with_limit(WINDOW);
        let addresses = |hits: Vec<(Option<u64>, tantivy::DocAddress)>| {
            hits.into_iter().map(|(_, a)| a).collect::<Vec<_>>()
        };
        let mapped = |kind| BookOrderSort {
            tables: Arc::clone(&tables),
            kind,
        };
        let variants: [&dyn Fn() -> Vec<tantivy::DocAddress>; 4] = [
            &|| {
                let top = top().order_by_fast_field::<u64>("id", Order::Asc);
                addresses(searcher.search(query.as_ref(), &(top, Count)).unwrap().0)
            },
            &|| {
                let top = top().order_by((mapped(OrderKind::Catalogue), Order::Asc));
                addresses(searcher.search(query.as_ref(), &(top, Count)).unwrap().0)
            },
            &|| {
                let top = top().order_by((ColumnGenerationSort, Order::Asc));
                addresses(searcher.search(query.as_ref(), &(top, Count)).unwrap().0)
            },
            &|| {
                let top = top().order_by((mapped(OrderKind::Generation), Order::Asc));
                addresses(searcher.search(query.as_ref(), &(top, Count)).unwrap().0)
            },
        ];
        for run in 0..RUNS {
            // Each variant takes each position in turn, so none always runs on a warm cache.
            for step in 0..4 {
                let variant = (run + step) % 4;
                let started = Instant::now();
                found[variant] = variants[variant]();
                times[variant].push(started.elapsed().as_secs_f64() * 1e3);
            }
            assert_eq!(found[0], found[1], "the catalogue mapping reorders {name}");
            assert_eq!(found[2], found[3], "the generation mapping reorders {name}");
        }
        let hits = searcher.search(query.as_ref(), &Count).unwrap();
        let median = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let [a, b, c, d] = &mut times;
        println!(
            "{name:<8} {hits:>9} | {:>7.2} {:>7.2} | {:>7.2} {:>7.2}",
            median(a),
            median(b),
            median(c),
            median(d)
        );
    }
}
