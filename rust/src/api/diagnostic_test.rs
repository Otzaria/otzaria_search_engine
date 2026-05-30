use std::time::Instant;
use anyhow::Result;
use crate::api::search_engine::{SearchEngine, ResultsOrder};

pub fn run_diagnostic_test() -> Result<()> {
    let index_path = r"C:\אוצריא\index";
    
    if !std::path::Path::new(index_path).exists() {
        println!("Index not found at {}", index_path);
        return Ok(());
    }

    println!("Running diagnostic test to understand the index...");
    println!("Index: {}", index_path);
    println!("{}", "=".repeat(60));

    let mut search_engine = SearchEngine::new(index_path);

    // Test with very simple patterns first
    let simple_tests = vec![
        // Try common Hebrew words
        ("Hebrew letter aleph", vec!["א".to_string()]),
        ("Hebrew letter bet", vec!["ב".to_string()]),
        ("Hebrew letter gimel", vec!["ג".to_string()]),
        
        // Try common words
        ("Word: and", vec!["ו".to_string()]),
        ("Word: the", vec!["ה".to_string()]),
        ("Word: in", vec!["ב".to_string()]),
        
        // Try simple regex patterns
        ("Any Hebrew letter", vec!["[א-ת]".to_string()]),
        ("Two Hebrew letters", vec!["[א-ת][א-ת]".to_string()]),
        
        // Try wildcard patterns
        ("Simple wildcard", vec![".*".to_string()]),
    ];

    for (name, regex_terms) in simple_tests {
        println!("Testing: {}", name);
        println!("  Pattern: {:?}", regex_terms);
        
        let start = Instant::now();
        match search_engine.search(
            regex_terms,
            vec![], // No facet filtering
            10,     // Small limit for testing
            0,      // No offset
            0,      // No slop
            100,    // Reasonable expansion limit
            ResultsOrder::Relevance,
            None,
        ) {
            Ok(results) => {
                let duration = start.elapsed();
                println!("  Time: {}ms", duration.as_millis());
                println!("  Results: {}", results.len());
                
                // Show first few results if any
                if !results.is_empty() {
                    println!("  Sample results:");
                    for (i, result) in results.iter().take(3).enumerate() {
                        println!("    {}: {} - {}", i+1, result.title, 
                               result.text.chars().take(100).collect::<String>());
                    }
                }
            },
            Err(e) => {
                let duration = start.elapsed();
                println!("  Time: {}ms (failed)", duration.as_millis());
                println!("  Error: {}", e);
            }
        }
        println!();
    }

    // Test count functionality to see if there's any data at all
    println!("Testing count functionality:");
    match search_engine.count(vec![".*".to_string()], &[], 0, 1000) {
        Ok(count) => println!("  Total documents matching '.*': {}", count),
        Err(e) => println!("  Count failed: {}", e),
    }

    println!("{}", "=".repeat(60));
    println!("Diagnostic test completed!");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diagnostic() {
        match run_diagnostic_test() {
            Ok(_) => println!("Diagnostic test completed successfully"),
            Err(e) => println!("Diagnostic test failed: {}", e),
        }
    }
}
