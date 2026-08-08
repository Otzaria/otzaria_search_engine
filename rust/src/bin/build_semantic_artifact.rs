//! Build the official semantic artifact from a Tantivy index (S4b).
//!
//! The build-machine entry point: an index directory, a model file and a recipe in, a
//! verified semantic artifact out. This is the only place the whole pipeline runs outside a
//! test — `TantivyCorpus` gives the sidecar's builder a corpus, the builder applies the
//! recipe and produces the vectors, and the packer writes and re-verifies the result.
//!
//! Not part of the FFI and not something the application runs. Building an artifact is a
//! batch job on a machine with the weights and the whole library; a device installs what
//! this produces.
//!
//! ```text
//! build_semantic_artifact \
//!   --index ./tantivy-index --library-version otzaria-library-2026-08 \
//!   --model model.json --model-file model.gguf --chunking chunking.json \
//!   --out ./artifact
//! ```
//!
//! Requires an inference backend, because a build *is* inference: compile with
//! `--features semantic-real` for GGUF weights, or `--features semantic-mock` for the
//! deterministic stand-in, which then also needs `--allow-non-semantic` because its vectors
//! carry no meaning.

#[cfg(not(feature = "semantic-integration"))]
fn main() {
    eprintln!(
        "This binary was compiled without a semantic backend, and building an artifact is \
         inference.\nRebuild with --features semantic-real (GGUF weights) or \
         --features semantic-mock (deterministic stand-in)."
    );
    std::process::exit(1);
}

#[cfg(feature = "semantic-integration")]
fn main() {
    use otzaria_semantic_search::distribution::builder::{build, BuildRequest};
    use otzaria_semantic_search::distribution::corpus::CorpusIndex;
    use otzaria_semantic_search::semantic::chunker::ChunkerConfig;
    use otzaria_semantic_search::semantic::versioning::ModelIdentity;
    use search_engine::api::search_engine::SearchEngine;
    use search_engine::semantic_corpus::TantivyCorpus;
    use std::path::PathBuf;
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
    let out = required("--out");
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

    // The index is opened read-only in every sense that matters: nothing below writes to
    // it, and the corpus takes one snapshot of it for the whole build.
    let engine = SearchEngine::new(&index_path);
    let corpus = TantivyCorpus::from_engine(&engine, library_version, chunking.clone())
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

    let report = build(
        BuildRequest {
            output_path: PathBuf::from(&out),
            model_path: PathBuf::from(required("--model-file")),
            model,
            chunking,
            created_at: flag("--created-at").unwrap_or_else(utc_timestamp),
            collection_name: flag("--collection").unwrap_or_else(|| "chunks".to_string()),
            batch_size: flag("--batch")
                .and_then(|value| value.parse().ok())
                .unwrap_or(32),
            allow_non_semantic_backend: args.iter().any(|arg| arg == "--allow-non-semantic"),
        },
        &corpus,
    )
    .unwrap_or_else(|error| {
        eprintln!("Build failed: {error}");
        process::exit(1);
    });

    println!("\n=== Built an official artifact ===");
    println!("Path:          {}", report.artifact_path.display());
    println!("Vectors:       {}", report.vector_count);
    println!("Books:         {}", report.book_count);
    println!("Payload bytes: {}", report.total_size_bytes);
    println!("Digest:        {}", report.digest);
    println!(
        "\nPublish that digest outside the artifact. Verified without it, an install \
         detects damage\nand the wrong artifact, but not one deliberately rebuilt to match."
    );
}

#[cfg(feature = "semantic-integration")]
const USAGE: &str = "\
build_semantic_artifact — a Tantivy index and a model in, a semantic artifact out

Required:
  --index <dir>              The lexical index to read the corpus from, read-only
  --library-version <name>   Catalogue release the index was built from
  --model <path>             JSON ModelIdentity describing how the vectors are produced
  --model-file <path>        The GGUF the vectors are produced with
  --chunking <path>          JSON ChunkerConfig — the recipe itself
  --out <dir>                Output directory; must not exist, or be empty

Optional:
  --batch <N>                Texts per inference call (default: 32)
  --collection <name>        Collection name in the payload header (default: \"chunks\")
  --created-at <timestamp>   Manifest timestamp (default: now, UTC)
  --allow-non-semantic       Permit a backend whose vectors carry no meaning. For tests
                             only: such an artifact passes every check and answers nonsense.

Which lines get a vector is derived by applying the recipe to the corpus, before any
inference. The recipe's three versions must name behaviour this build implements, and its
hash must be the chunking_identity the model declares.";

/// `YYYY-MM-DDTHH:MM:SSZ` for the manifest, without pulling in a date crate for one string.
#[cfg(feature = "semantic-integration")]
fn utc_timestamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);

    // Howard Hinnant's civil_from_days: exact over the proleptic Gregorian calendar.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3600,
        (second_of_day % 3600) / 60,
        second_of_day % 60
    )
}
