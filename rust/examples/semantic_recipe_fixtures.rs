//! cargo run --example semantic_recipe_fixtures > tests/data/semantic_recipe_v4.json
fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&search_engine::semantic_source::conformance_fixture())
            .unwrap()
    );
}
