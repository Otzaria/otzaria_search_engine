use crate::api::search_engine::{ResultsOrder, SearchEngine};
use anyhow::Result;
use std::time::Instant;

pub fn run_focused_benchmark() -> Result<()> {
    let index_path = r"C:\אוצריא\index";

    if !std::path::Path::new(index_path).exists() {
        println!("Index not found at {}", index_path);
        return Ok(());
    }

    println!("Running focused benchmark on regex phrase queries...");
    println!("Index: {}", index_path);
    println!("{}", "=".repeat(60));

    let mut search_engine = SearchEngine::new(index_path);

    // Test cases with different complexity levels
    let test_cases = vec![
        // Simple exact word
        ("Simple exact word", vec!["משה".to_string()], 0, 50),
        // Word with .* at beginning and end (as requested)
        ("Word with wildcards", vec![".*משה.*".to_string()], 0, 2000),
        // Two words exact
        (
            "Two words exact",
            vec!["בית".to_string(), "המקדש".to_string()],
            0,
            50,
        ),
        // Two words with wildcards
        (
            "Two words with wildcards",
            vec![".*בית.*".to_string(), ".*המקדש.*".to_string()],
            0,
            5000,
        ),
        // Complex pattern
        (
            "Complex Hebrew pattern",
            vec!["[א-ת]{2,4}".to_string()],
            0,
            1000,
        ),
        // Very broad wildcard (potentially expensive)
        ("Broad wildcard", vec![".*[א-ת].*".to_string()], 0, 10000),
        // Additional test cases with higher expansions
        (
            "Three words with wildcards",
            vec![
                ".*רבי.*".to_string(),
                ".*יהודה.*".to_string(),
                ".*הנשיא.*".to_string(),
            ],
            0,
            8000,
        ),
        // Alternation pattern
        (
            "Alternation pattern",
            vec!["(משה|אהרן|מרים)".to_string()],
            0,
            1000,
        ),
        // Complex Hebrew root pattern
        (
            "Hebrew root pattern",
            vec!["[כ-ל][ת-ת][ב-ב]".to_string()],
            0,
            2000,
        ),
    ];

    for (name, regex_terms, slop, max_expansions) in test_cases {
        println!("Testing: {}", name);
        println!("  Pattern: {:?}", regex_terms);

        // Warmup
        let _ = search_engine.search(
            regex_terms.clone(),
            vec![],
            10,
            0,
            slop,
            max_expansions,
            ResultsOrder::Relevance,
            None,
        );

        // Actual benchmark
        let start = Instant::now();
        match search_engine.search(
            regex_terms,
            vec![],
            100,
            0,
            slop,
            max_expansions,
            ResultsOrder::Relevance,
            None,
        ) {
            Ok(results) => {
                let duration = start.elapsed();
                println!("  Time: {}ms", duration.as_millis());
                println!("  Results: {}", results.len());
                println!("  Max expansions: {}", max_expansions);
            }
            Err(e) => {
                let duration = start.elapsed();
                println!("  Time: {}ms (failed)", duration.as_millis());
                println!("  Error: {}", e);
            }
        }
        println!();
    }

    println!("{}", "=".repeat(60));
    println!("Benchmark completed!");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_focused_benchmark() {
        match run_focused_benchmark() {
            Ok(_) => println!("Focused benchmark completed successfully"),
            Err(e) => println!("Focused benchmark failed: {}", e),
        }
    }
}
