//! Apply the embedding recipe to a Tantivy index and write the work out.
//!
//! The half of a build that needs the corpus and no model. It exists separately from
//! `build_semantic_artifact` because the library's vectors are not produced on the
//! machine that holds the library: the corpus lives here, the only affordable inference
//! is on rented GPUs that never see it, and the recipe must be applied exactly once so
//! that two workers on different hardware cannot disagree about what to embed.
//!
//! ```text
//! export_semantic_plan \
//!   --index ./tantivy-index --library-version otzaria-library-2026-08 \
//!   --model model.json --chunking chunking.json --out ./plan
//! ```
//!
//! Writes `plan.jsonl` — one record per line that gets a vector, carrying the finished
//! embedding text and both digests — plus `export-manifest.json` and the
//! `corpus-identity.json` the merge will pack against. **No inference backend is
//! required**, so this runs in a build that never compiles llama.cpp.

#[cfg(not(feature = "semantic-integration"))]
fn main() {
    eprintln!(
        "This binary was compiled without the semantic sidecar.\n\
         Rebuild with --features semantic-integration (no GGUF weights are needed)."
    );
    std::process::exit(1);
}

#[cfg(feature = "semantic-integration")]
fn main() {
    use otzaria_semantic_search::distribution::corpus::CorpusIndex;
    use otzaria_semantic_search::distribution::shard::export_plan;
    use otzaria_semantic_search::semantic::chunker::ChunkerConfig;
    use otzaria_semantic_search::semantic::versioning::ModelIdentity;
    use search_engine::semantic_corpus::TantivyCorpus;
    use std::io::BufWriter;
    use std::path::{Path, PathBuf};
    use std::process;

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        return;
    }

    let flag = |name: &str| -> Option<String> {
        args.windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].clone())
    };
    let required = |name: &str| -> String {
        flag(name).unwrap_or_else(|| {
            eprintln!("Error: {name} is required.\n\n{USAGE}");
            process::exit(1);
        })
    };
    let read_json = |what: &str, path: &str| -> serde_json::Value {
        let text = std::fs::read_to_string(path).unwrap_or_else(|error| {
            eprintln!("Could not read {what} at {path}: {error}");
            process::exit(1);
        });
        serde_json::from_str(&text).unwrap_or_else(|error| {
            eprintln!("{path} is not a {what}: {error}");
            process::exit(1);
        })
    };

    let index_path = required("--index");
    let out = PathBuf::from(required("--out"));
    let library_version = required("--library-version");
    let model: ModelIdentity =
        serde_json::from_value(read_json("model identity", &required("--model"))).unwrap_or_else(
            |error| {
                eprintln!("the model file is not a ModelIdentity: {error}");
                process::exit(1);
            },
        );
    let chunking: ChunkerConfig =
        serde_json::from_value(read_json("chunker configuration", &required("--chunking")))
            .unwrap_or_else(|error| {
                eprintln!("the chunking file is not a ChunkerConfig: {error}");
                process::exit(1);
            });

    // Read-only, and literally so — the same door `build_semantic_artifact` uses. Going
    // through `SearchEngine` would create an index for a mistyped path and hold a writer
    // lock over a read that takes an hour.
    let corpus =
        TantivyCorpus::from_index_path(Path::new(&index_path), library_version, chunking.clone())
            .unwrap_or_else(|error| {
                eprintln!("Could not read the corpus at {index_path}: {error:#}");
                process::exit(1);
            });
    let identity = corpus.identity().unwrap_or_else(|error| {
        eprintln!("The corpus has no identity: {error}");
        process::exit(1);
    });
    println!(
        "Corpus: {} line(s) across {} book(s)\ncorpus_id: {}",
        corpus.line_count(),
        corpus.book_count(),
        identity.corpus_id
    );

    std::fs::create_dir_all(&out).unwrap_or_else(|error| {
        eprintln!("Could not create {}: {error}", out.display());
        process::exit(1);
    });
    // Written before the plan: the merge packs against this identity, and recomputing it
    // there would mean scanning six million lines a second time to learn what is already
    // known here.
    std::fs::write(
        out.join("corpus-identity.json"),
        serde_json::to_vec_pretty(&identity).expect("a corpus identity serialises"),
    )
    .unwrap_or_else(|error| {
        eprintln!("Could not write the corpus identity: {error}");
        process::exit(1);
    });

    let plan_path = out.join("plan.jsonl");
    let file = std::fs::File::create(&plan_path).unwrap_or_else(|error| {
        eprintln!("Could not write {}: {error}", plan_path.display());
        process::exit(1);
    });
    let mut sink = BufWriter::new(file);

    let report = export_plan(&corpus, &chunking, &model, &mut sink).unwrap_or_else(|error| {
        eprintln!("Export failed: {error}");
        process::exit(1);
    });
    std::fs::write(
        out.join("export-manifest.json"),
        serde_json::to_vec_pretty(&report).expect("a plan report serialises"),
    )
    .unwrap_or_else(|error| {
        eprintln!("Could not write the export manifest: {error}");
        process::exit(1);
    });

    println!("\n=== Exported a build plan ===");
    println!("Plan:          {}", plan_path.display());
    println!("Records:       {}", report.records);
    println!(
        "line_id range: {}..={}",
        report.min_line_id, report.max_line_id
    );
    println!("Plan SHA-256:  {}", report.plan_sha256);
    println!("Chunking:      {}", report.chunking_identity);
}

#[cfg(feature = "semantic-integration")]
const USAGE: &str = "\
Apply the embedding recipe to a Tantivy index and write the work out.

Usage:
  export_semantic_plan --index <dir> --library-version <version> \\
      --model <model.json> --chunking <chunking.json> --out <dir>

  --index            Tantivy index directory, opened read-only
  --library-version  The library version the index was built from
  --model            JSON ModelIdentity; no model file is opened
  --chunking         JSON ChunkerConfig, whose hash must be the model's chunking_identity
  --out              Receives plan.jsonl, export-manifest.json, corpus-identity.json

Split plan.jsonl by record with `otzaria-semantic-search embed-shard --skip/--take`, on a
machine with a GPU. Every record must fall in exactly one window; the merge refuses a hole
rather than packing around it.";
