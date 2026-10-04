//! Build the library's semantic vectors from a Tantivy index, as one base package.
//!
//! An index directory, a model file and a recipe in; a base package out: `segment.oxv`, its
//! metadata, and `release.json`, the release manifest an installation is handed with the
//! segment. `TantivyCorpus` gives the sidecar's builder a corpus, and the builder applies the
//! recipe, embeds every line it keys, and writes the package. The development one-shot of
//! the build: the release pipeline's plan, embed and assemble steps are the sidecar's.
//!
//! Not part of the FFI and not something the application runs. Building an artifact is a
//! batch job on a machine with the weights and the whole library; a device installs what
//! this produces.
//!
//! ```text
//! build_semantic_artifact \
//!   --index ./tantivy-index --library-version 30 --release-tag v30-20260930120000 \
//!   --model model.json --model-file seforim-embed-round2-int8.onnx --chunking chunking.json \
//!   --out ./package
//! ```
//!
//! `--model-file` is the ONNX graph the vectors are produced with, handed to the sidecar as
//! it is; its package is the graph plus the `tokenizer.json` beside it. GGUF weights are not
//! supported.
//!
//! Requires an inference backend, because a build *is* inference: compile with
//! `--features semantic-onnx` (`semantic`, the production feature, is that backend), or
//! `--features semantic-mock` for the deterministic stand-in, which then also needs
//! `--allow-non-semantic` because its vectors carry no meaning.
//!
//! `--install <dir>` also installs the package into the vector set at `<dir>`, against the
//! manifest digest the build announces, as a device installs a release: for development and
//! tests, which then open that set. It is the one write this binary makes outside `--out`.

#[cfg(not(feature = "semantic-integration"))]
fn main() {
    eprintln!(
        "This binary was compiled without a semantic backend, and building an artifact is \
         inference.\nRebuild with --features semantic-onnx (an ONNX graph) or \
         --features semantic-mock (deterministic stand-in)."
    );
    std::process::exit(1);
}

#[cfg(feature = "semantic-integration")]
fn main() {
    use otzaria_semantic_search::cancellation::CancellationToken;
    use otzaria_semantic_search::distribution::builder::{
        build, BuildRequest, RELEASE_MANIFEST_FILENAME, SEGMENT_FILENAME,
    };
    use otzaria_semantic_search::distribution::corpus::CorpusIndex;
    use otzaria_semantic_search::semantic::chunker::ChunkerConfig;
    use otzaria_semantic_search::semantic::official_index::readable_store_identity;
    use otzaria_semantic_search::semantic::segment_set::{
        install_package, InstallExpectation, InstallSource,
    };
    use otzaria_semantic_search::semantic::versioning::{IndexVersion, ModelIdentity};
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
    let seforim_db = flag("--seforim-db");
    let out = required("--out");
    let library_version = library_version(&required("--library-version"));
    let release_tag = flag("--release-tag").unwrap_or_default();
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

    // Read-only, and literally so: `from_index_path` checks compatibility, opens the
    // directory with `open_in_dir` and never asks for a writer. Going through
    // `SearchEngine` here would create an index for a mistyped path, re-stamp metadata on
    // a legacy-compatible one, hold the writer lock for the whole build, and panic on an
    // incompatible schema instead of reporting it.
    let corpus = TantivyCorpus::from_index_path(
        Path::new(&index_path),
        seforim_db.as_deref().map(Path::new),
        library_version,
        release_tag,
        chunking.clone(),
    )
    .unwrap_or_else(|error| {
        eprintln!("Could not read the corpus at {index_path}: {error:#}");
        process::exit(1);
    });
    let identity = corpus.identity().unwrap_or_else(|error| {
        eprintln!("The corpus has no identity: {error}");
        process::exit(1);
    });
    println!(
        "Corpus: {} line(s) across {} book(s)\nLibrary: version {} ({:?}), line text version {}",
        corpus.line_count(),
        corpus.book_count(),
        identity.library_version,
        identity.library_release_tag,
        identity.text.line_text_version
    );

    let report = build(
        BuildRequest {
            output_path: PathBuf::from(&out),
            model_path: PathBuf::from(required("--model-file")),
            model: model.clone(),
            chunking,
            created_at: flag("--created-at")
                .unwrap_or_else(search_engine::semantic_plan::utc_timestamp),
            batch_size: flag("--batch")
                .and_then(|value| value.parse().ok())
                .unwrap_or(32),
            // The codec the application reads, whichever the sidecar makes its default.
            codec: Default::default(),
            allow_non_semantic_backend: args.iter().any(|arg| arg == "--allow-non-semantic"),
        },
        &corpus,
    )
    .unwrap_or_else(|error| {
        eprintln!("Build failed: {error}");
        process::exit(1);
    });

    println!("\n=== Built a base package ===");
    println!("Path:             {}", report.output_path.display());
    println!("Vectors:          {}", report.manifest.counts.slots);
    println!("Books:            {}", report.manifest.counts.books);
    println!("Planned lines:    {}", report.planned_lines);
    println!("Clipped:          {}", report.clipped_components);
    println!("Package digest:   {}", report.manifest.package_digest);
    println!("Manifest SHA-256: {}", report.manifest_sha256);
    if let Some(vectors_dir) = flag("--install") {
        let manifest_path = report.output_path.join(RELEASE_MANIFEST_FILENAME);
        let manifest_json = std::fs::read_to_string(&manifest_path).unwrap_or_else(|error| {
            eprintln!("Could not read {}: {error}", manifest_path.display());
            process::exit(1);
        });
        let identity = corpus.identity().unwrap_or_else(|error| {
            eprintln!("The corpus has no identity: {error}");
            process::exit(1);
        });
        let applied = install_package(
            Path::new(&vectors_dir),
            &InstallSource {
                segment: &report.output_path.join(SEGMENT_FILENAME),
                manifest_json: &manifest_json,
            },
            &InstallExpectation {
                identity: IndexVersion {
                    text: identity.text,
                    model,
                    store: readable_store_identity(),
                },
                published_manifest_sha256: Some(report.manifest_sha256.clone()),
            },
            &CancellationToken::new(),
        )
        .unwrap_or_else(|error| {
            eprintln!("Could not install the package into {vectors_dir}: {error}");
            process::exit(1);
        });
        println!(
            "Installed:        {vectors_dir}, generation {}",
            applied.generation
        );
    }
    println!(
        "\nPublish the manifest's SHA-256 outside it. Installed without it, a device \
         detects damage\nand the wrong release, but not one deliberately rebuilt to match."
    );
}

#[cfg(feature = "semantic-integration")]
const USAGE: &str = "\
build_semantic_artifact — a Tantivy index and a model in, a semantic artifact out

Required:
  --index <dir>              The lexical index to read the corpus from, read-only
  --library-version <N>      The library edition the index was built from: its db_version
  --model <path>             JSON ModelIdentity describing how the vectors are produced
  --model-file <path>        The model the vectors are produced with: an ONNX graph
                             (*.onnx) with its tokenizer.json beside it
  --chunking <path>          JSON ChunkerConfig — the recipe itself
  --out <dir>                Output directory for the package; must not exist, or be empty

Optional:
  --release-tag <tag>        The release that edition was published as (default: none)
  --seforim-db <path>        The library database official books' line text is read from,
                             read-only. Required when the index keeps that text there
                             (index schema 5), and must be the database the index was
                             built from
  --batch <N>                Texts per inference call (default: 32)
  --created-at <timestamp>   Manifest timestamp (default: now, UTC)
  --allow-non-semantic       Permit a backend whose vectors carry no meaning. For tests
                             only: such a package passes every check and answers nonsense.
  --install <dir>            Also install the package into the vector set at <dir>, as a
                             device installs a release

Which lines get a vector is derived by applying the recipe to the corpus, before any
inference. The recipe's three versions must name behaviour this build implements, and its
hash must be the chunking_identity the model declares.";

/// `--library-version` as the edition it names: the library's `db_version`, which starts at 1.
#[cfg(feature = "semantic-integration")]
fn library_version(value: &str) -> u32 {
    match value.parse::<u32>() {
        Ok(version) if version > 0 => version,
        _ => {
            eprintln!(
                "Error: --library-version is the library's db_version, a whole number from 1, \
                 not {value:?}.\n\n{USAGE}"
            );
            std::process::exit(1);
        }
    }
}
