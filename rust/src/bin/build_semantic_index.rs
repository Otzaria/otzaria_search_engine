//! Rebuild semantic provenance while preserving released catalogue and document ids.
#[cfg(all(feature = "semantic-integration", feature = "sqlite-bundled"))]
fn main() {
    use search_engine::semantic_index_builder::{build, BuildIndexRequest};
    use std::collections::HashMap;
    use std::path::PathBuf;
    let run = || -> anyhow::Result<_> {
        let args: Vec<_> = std::env::args().skip(1).collect();
        anyhow::ensure!(args.len()==8,"Usage: build_semantic_index --source-index INDEX --seforim-db DB --out NEW_DIRECTORY --library-version VERSION");
        let mut flags = HashMap::new();
        for pair in args.chunks_exact(2) {
            anyhow::ensure!(
                [
                    "--source-index",
                    "--seforim-db",
                    "--out",
                    "--library-version"
                ]
                .contains(&pair[0].as_str())
                    && flags.insert(pair[0].clone(), pair[1].clone()).is_none(),
                "unknown or repeated argument {:?}",
                pair[0]
            );
        }
        build(BuildIndexRequest {
            source_index: PathBuf::from(&flags["--source-index"]),
            seforim_db: PathBuf::from(&flags["--seforim-db"]),
            out: PathBuf::from(&flags["--out"]),
            library_version: flags["--library-version"].parse()?,
        })
    };
    match run() {
        Ok(report) => println!("{}", serde_json::to_string_pretty(&report).unwrap()),
        Err(error) => {
            eprintln!("Fresh semantic index rebuild failed: {error:#}");
            std::process::exit(1);
        }
    }
}
#[cfg(not(all(feature = "semantic-integration", feature = "sqlite-bundled")))]
fn main() {
    eprintln!("build_semantic_index requires --features semantic-integration,sqlite-bundled");
    std::process::exit(1);
}
