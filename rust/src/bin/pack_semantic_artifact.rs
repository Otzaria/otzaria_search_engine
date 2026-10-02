//! Retired: the packer this joined ready-made vectors with went with the v1 artifact.
//!
//! A vector set is now immutable segments addressed by key, and the sharded build's last
//! step is the sidecar's own `assemble`, which needs no Tantivy index: it writes the segment
//! and its release manifest from the plan and the embedded shards. The checks this binary
//! made against the live index move to a validation of the assembled vectors against the
//! release index, which follows with the sidecar's assembler. Until then this binary exists,
//! so a pipeline that calls it learns why from the binary rather than from cargo, and does
//! nothing.

fn main() {
    eprintln!(
        "pack_semantic_artifact is retired with the v1 artifact it packed. A sharded build now \
         ends with the sidecar's `otzaria-semantic-search assemble`, which writes the vector \
         set's segment and its release manifest; build_semantic_artifact builds a base \
         package in one process."
    );
    std::process::exit(2);
}
