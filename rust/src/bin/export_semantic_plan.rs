//! The plan of a vector build, from a release index: the sidecar's plan files.
//!
//! The half of a build that needs the corpus and no model. The library's vectors are not
//! produced on the machine that holds the library: the recipe is applied here, once, and the
//! texts it leaves to embed go to whichever machine embeds them.
//!
//! ```text
//! export_semantic_plan \
//!   --index ./index --library-version 30 --release-tag v30-20260930120000 \
//!   --model model.json --out ./plan [--warehouse ./warehouse] [--previous-ledger ./ledger]
//! ```
//!
//! Writes `records.bin`, `books.json`, `embed.jsonl` with `embed-manifest.json`,
//! `tombstones.bin` and `plan-manifest.json`, each by the sidecar's own writer. **No
//! inference backend is required**, so this runs in a build that has no ONNX Runtime
//! bindings.

#[cfg(not(feature = "semantic-integration"))]
fn main() {
    eprintln!(
        "This binary was compiled without the semantic sidecar.\n\
         Rebuild with --features semantic-integration (no model is needed)."
    );
    std::process::exit(1);
}

#[cfg(feature = "semantic-integration")]
fn main() {
    use otzaria_semantic_search::distribution::ledger::Ledger;
    use otzaria_semantic_search::distribution::plan::HeldVectors;
    use otzaria_semantic_search::distribution::warehouse::{Warehouse, WarehouseIdentity};
    use otzaria_semantic_search::semantic::versioning::ModelIdentity;
    use search_engine::semantic_plan::{export_plan, utc_timestamp, PlanExport};
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
    let fail = |what: &str, error: &dyn std::fmt::Display| -> ! {
        eprintln!("{what}: {error}");
        process::exit(1);
    };

    let library_version = match required("--library-version").parse::<u32>() {
        Ok(version) if version > 0 => version,
        _ => fail(
            "Error",
            &"--library-version is the library's db_version, a whole number from 1",
        ),
    };
    let model_path = required("--model");
    let model: ModelIdentity = std::fs::read_to_string(&model_path)
        .map_err(|error| error.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
        .unwrap_or_else(|error| fail(&format!("{model_path} is not a ModelIdentity"), &error));
    let quantization = flag("--passage-quantization").unwrap_or_else(|| "fp32".to_string());
    let passage_package = model
        .query_packages
        .iter()
        .find(|package| package.quantization == quantization)
        .cloned()
        .unwrap_or_else(|| {
            fail(
                "Error",
                &format!("the family has no {quantization} package among its query packages"),
            )
        });
    let previous = flag("--previous-ledger").map(|dir| {
        Ledger::open(Path::new(&dir), None)
            .unwrap_or_else(|error| fail("Could not open the previous ledger", &error))
    });
    let warehouse = flag("--warehouse").map(|dir| {
        let warehouse = Warehouse::open(Path::new(&dir))
            .unwrap_or_else(|error| fail("Could not open the warehouse", &error));
        // Its vectors stand in for texts only when they were embedded as these would be.
        let expected = WarehouseIdentity::of(&model, &passage_package);
        if warehouse.identity() != &expected {
            fail(
                "The warehouse holds other vectors",
                &format!(
                    "{:?}, and this plan embeds {expected:?}",
                    warehouse.identity()
                ),
            );
        }
        warehouse
    });

    let report = export_plan(PlanExport {
        index_path: PathBuf::from(required("--index")),
        out_dir: PathBuf::from(required("--out")),
        library_version,
        library_release_tag: flag("--release-tag").unwrap_or_default(),
        model,
        passage_package,
        previous: previous.as_ref(),
        warehouse: warehouse
            .as_ref()
            .map(|warehouse| warehouse as &dyn HeldVectors),
        created_at: flag("--created-at").unwrap_or_else(utc_timestamp),
    })
    .unwrap_or_else(|error| fail("Planning failed", &format!("{error:#}")));

    let manifest = &report.manifest;
    let counts = manifest.counts;
    println!("\n=== Planned v{} ===", manifest.library_version);
    println!("Documents:       {}", report.documents);
    println!("PDF lines:       {} (not planned)", report.pdf_lines);
    println!("Records:         {}", counts.records);
    println!("Books:           {}", counts.books);
    println!("Distinct keys:   {}", counts.unique);
    println!("Reused:          {}", counts.reused);
    println!("To ship:         {}", counts.to_ship);
    println!("To embed:        {}", counts.to_embed);
    println!("Revived:         {}", counts.revived);
    println!("Tombstones:      {}", counts.tombstones);
    println!("Foreign pairs:   {}", counts.foreign_pairs);
    println!("Plan SHA-256:    {}", report.embed.plan_sha256);
    if report.column_checked {
        println!(
            "Parity:          {} line(s) checked against the chunkKey column, {} mismatch(es)",
            manifest.parity.checked, manifest.parity.mismatches
        );
    } else {
        println!("Parity:          no chunkKey column; every key computed from the text");
    }
    if manifest.parity.mismatches > 0 {
        eprintln!(
            "\nThe parity gate failed: {} line(s) hold a chunkKey that is not their text's. \
             No reader accepts this plan.",
            manifest.parity.mismatches
        );
        process::exit(1);
    }
}

#[cfg(feature = "semantic-integration")]
const USAGE: &str = "\
The plan of a vector build, from a release index: the sidecar's plan files.

Usage:
  export_semantic_plan --index <dir> --library-version <N> --model <model.json> --out <dir>

  --index                  The release index, opened read-only: schema 4, keyed from its
                           text, or schema 5, whose chunkKey column is held to its text
  --library-version        The library edition the index was built from: its db_version
  --release-tag            The release that edition was published as (default: none)
  --model                  JSON ModelIdentity; its chunking_identity must be this build's
  --passage-quantization   The package the passages are embedded with (default: fp32)
  --out                    Receives records.bin, books.json, embed.jsonl,
                           embed-manifest.json, tombstones.bin and plan-manifest.json
  --warehouse <dir>        Leave out of embed.jsonl every text it holds a vector for
  --previous-ledger <dir>  The previous release's ledger, to split against
  --created-at <time>      The manifest's timestamp (default: now, UTC)

Embed embed.jsonl with `otzaria-semantic-search embed-shard`, in windows of its records.";
