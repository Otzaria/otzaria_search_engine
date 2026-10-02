//! How a vector set lands on a release index: how much of the index's keyed lines it covers,
//! and where each of its records resolves.
//!
//! ```text
//! validate_semantic_vectors --index ./index --vectors ./vectors [--plan ./plan]
//! ```
//!
//! The set is an installed one, as a device holds it. Nothing is written: the index is
//! opened read-only, and its lines are keyed from their text as `export_semantic_plan`
//! keys them. Retrieval is not measured here.

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
    use otzaria_semantic_search::distribution::gates::coverage;
    use otzaria_semantic_search::distribution::plan::Plan;
    use otzaria_semantic_search::semantic::segment_set::SegmentSet;
    use search_engine::semantic_plan::validate;
    use std::path::Path;
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
    let percent = |part: u64, whole: u64| {
        if whole == 0 {
            0.0
        } else {
            100.0 * part as f64 / whole as f64
        }
    };

    let vectors = required("--vectors");
    let set = SegmentSet::open(Path::new(&vectors))
        .unwrap_or_else(|error| fail("Could not open the vector set", &error));
    let info = set.info();
    println!(
        "Vector set:      generation {}, library version {} ({:?}), {} segment(s), {} live and \
         {} dead slot(s)",
        info.generation,
        info.library_version,
        info.library_release_tag,
        info.segments.len(),
        info.slots_live,
        info.slots_dead
    );
    if let Some(plan) = flag("--plan") {
        let plan = Plan::open(Path::new(&plan))
            .unwrap_or_else(|error| fail("Could not open the plan", &error));
        let reached = coverage(&set, &plan);
        println!(
            "Plan coverage:   {} of {} record(s) reachable ({:.4}%){}",
            reached.reachable,
            reached.records,
            percent(reached.reachable, reached.records),
            reached
                .first_unreachable
                .map(|(book, ordinal)| format!(", the first not: {book} line {ordinal}"))
                .unwrap_or_default()
        );
    }
    let index = required("--index");
    let validation = validate(Path::new(&index), &set)
        .unwrap_or_else(|error| fail("Could not validate", &format!("{error:#}")));
    println!(
        "Index coverage:  {} of {} keyed line(s) recorded in their book ({:.4}%)",
        validation.covered_lines,
        validation.keyed_lines,
        percent(validation.covered_lines, validation.keyed_lines)
    );
    println!(
        "Resolution:      {} record(s): {} at their hint ({:.4}%), {} moved in their book, {} \
         whose book no longer holds them",
        validation.records,
        validation.at_hint,
        percent(validation.at_hint, validation.records),
        validation.moved,
        validation.gone
    );
}

#[cfg(feature = "semantic-integration")]
const USAGE: &str = "\
How a vector set lands on a release index.

Usage:
  validate_semantic_vectors --index <dir> --vectors <dir> [--plan <dir>]

  --index     The release index, opened read-only
  --vectors   An installed vector set
  --plan      The plan the set was assembled from: how many of its records a scan reaches";
