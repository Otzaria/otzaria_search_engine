//! Pack ready-made vectors into an official artifact, against a live Tantivy index.
//!
//! The last step of a sharded build: `export_semantic_plan` wrote the work, GPUs
//! elsewhere embedded it, `assemble` put the pieces back in one pair of files, and this
//! joins that pair to the corpus it claims to describe.
//!
//! ```text
//! pack_semantic_artifact \
//!   --index ./tantivy-index --library-version v20-… \
//!   --vectors merged/vectors.f32 --records merged/records.jsonl \
//!   --model model.json --chunking chunking.json --out ./artifact
//! ```
//!
//! **Everything the vectors cannot vouch for is checked here**, and only here: that every
//! `source_line_sha256` matches the line the index actually holds — which is what catches
//! a vector file that drifted out of step with its id list — and that the set of ids is
//! exactly the set the recipe embeds, no more and no fewer. Nothing upstream can perform
//! either check: a GPU worker has no corpus, and the assembler only counts.
//!
//! **No inference backend is required.** Packing never turns text into a vector.

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
    use otzaria_semantic_search::distribution::packer::{pack, read_vector_inputs, PackRequest};
    use otzaria_semantic_search::semantic::chunker::ChunkerConfig;
    use otzaria_semantic_search::semantic::versioning::ModelIdentity;
    use search_engine::semantic_corpus::TantivyCorpus;
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
    let vectors = required("--vectors");
    let records = required("--records");
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

    let corpus =
        TantivyCorpus::from_index_path(Path::new(&index_path), library_version, chunking.clone())
            .unwrap_or_else(|error| {
                eprintln!("Could not read the corpus at {index_path}: {error:#}");
                process::exit(1);
            });
    println!(
        "Corpus: {} line(s) across {} book(s)\ncorpus_id: {}",
        corpus.line_count(),
        corpus.book_count(),
        corpus
            .identity()
            .map(|identity| identity.corpus_id)
            .unwrap_or_default()
    );

    let inputs = read_vector_inputs(
        Path::new(&vectors),
        Path::new(&records),
        model.embedding_dim,
    )
    .unwrap_or_else(|error| {
        eprintln!("Could not read the vectors: {error}");
        process::exit(1);
    });

    let report = pack(
        PackRequest {
            output_path: out,
            model,
            created_at: flag("--created-at").unwrap_or_else(utc_timestamp),
            collection_name: flag("--collection").unwrap_or_else(|| "chunks".to_string()),
        },
        inputs,
        &corpus,
    )
    .unwrap_or_else(|error| {
        eprintln!("Packing failed: {error}");
        process::exit(1);
    });

    println!("\n=== Packed an official artifact ===");
    println!("Path:          {}", report.artifact_path.display());
    println!("Vectors:       {}", report.vector_count);
    println!("Books:         {}", report.book_count);
    println!("Payload bytes: {}", report.total_size_bytes);
    println!("Identity:      {}", report.identity);
    println!("Digest:        {}", report.digest);
    println!(
        "\nPublish that digest outside the artifact. Verified without it, an install \
         detects damage\nand the wrong artifact, but not one deliberately rebuilt to match."
    );
}

#[cfg(feature = "semantic-integration")]
fn utc_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs();
    let (days, rest) = (now / 86_400, now % 86_400);
    let (mut year, mut day) = (1970i64, days as i64);
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let length = if leap { 366 } else { 365 };
        if day < length {
            break;
        }
        day -= length;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let months = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0;
    while day >= months[month] {
        day -= months[month];
        month += 1;
    }
    format!(
        "{year:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        month + 1,
        day + 1,
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

#[cfg(feature = "semantic-integration")]
const USAGE: &str = "\
Pack ready-made vectors into an official artifact, against a live Tantivy index.

Usage:
  pack_semantic_artifact --index <dir> --library-version <version> \\
      --vectors <vectors.f32> --records <records.jsonl> \\
      --model <model.json> --chunking <chunking.json> --out <dir>

  --index            Tantivy index directory, opened read-only
  --library-version  The library version the index was built from
  --vectors          Raw little-endian f32, count x embedding_dim, no header
  --records          JSONL, one record per vector, in the same order
  --model            JSON ModelIdentity
  --chunking         JSON ChunkerConfig; its hash must be the model's chunking_identity
  --out              Output directory; must not exist, or be empty

Every check happens here: each record's source digest against the line the index holds,
and the whole id set against the one the recipe embeds.";
