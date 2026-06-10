use anyhow::Result;
use search_engine::api::benchmark::{RegexBenchmarker, TestCase};
use std::env;

fn main() -> Result<()> {
    // Initialize logging
    env_logger::init();

    let args: Vec<String> = env::args().collect();

    // Default index path
    let index_path = if args.len() > 1 {
        &args[1]
    } else {
        r"C:\אוצריא\index"
    };

    println!("Initializing benchmark with index: {}", index_path);

    let mut benchmarker = RegexBenchmarker::new(index_path);

    // Check if custom benchmark mode is requested
    if args.len() > 2 && args[2] == "--custom" {
        run_custom_benchmark(&mut benchmarker)?;
    } else {
        run_comprehensive_benchmark(&mut benchmarker)?;
    }

    Ok(())
}

fn run_comprehensive_benchmark(benchmarker: &mut RegexBenchmarker) -> Result<()> {
    println!("Running comprehensive regex phrase query benchmark...");
    println!("This will test various complexity levels of regex patterns.\n");

    let _suite = benchmarker.run_comprehensive_benchmark()?;

    Ok(())
}

fn run_custom_benchmark(benchmarker: &mut RegexBenchmarker) -> Result<()> {
    println!("Running custom benchmark with Hebrew text patterns...");

    let custom_queries = vec![
        // Talmudic patterns
        TestCase {
            query_name: "Talmudic Citation Pattern".to_string(),
            regex_terms: vec![
                "תלמוד".to_string(),
                "בבלי".to_string(),
                "[א-ת]+".to_string(),
            ],
            facets: vec![],
            slop: 2,
            max_expansions: 100,
        },
        TestCase {
            query_name: "Rabbi Name Pattern".to_string(),
            regex_terms: vec!["רבי".to_string(), "[א-ת]{2,8}".to_string()],
            facets: vec![],
            slop: 1,
            max_expansions: 200,
        },
        TestCase {
            query_name: "Halachic Term Pattern".to_string(),
            regex_terms: vec!["(מותר|אסור|חייב|פטור)".to_string()],
            facets: vec![],
            slop: 0,
            max_expansions: 50,
        },
        TestCase {
            query_name: "Biblical Verse Pattern".to_string(),
            regex_terms: vec!["שנאמר".to_string(), "[א-ת\\s]{10,50}".to_string()],
            facets: vec![],
            slop: 3,
            max_expansions: 150,
        },
        TestCase {
            query_name: "Complex Aramaic Pattern".to_string(),
            regex_terms: vec![
                "[א-ת]*א".to_string(),
                "[א-ת]*ן".to_string(),
                "[א-ת]*ה".to_string(),
            ],
            facets: vec![],
            slop: 5,
            max_expansions: 300,
        },
    ];

    let _suite = benchmarker.benchmark_custom_queries(custom_queries)?;

    Ok(())
}
