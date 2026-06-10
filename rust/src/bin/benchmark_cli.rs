use anyhow::Result;
use std::env;

// Use the library crate
use search_engine::api::benchmark::{BenchmarkResult, RegexBenchmarker, TestCase};

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

    let suite = benchmarker.run_comprehensive_benchmark()?;

    // Additional analysis
    println!("\n{}", "=".repeat(80));
    println!("PERFORMANCE INSIGHTS:");
    println!("{}", "=".repeat(80));

    analyze_performance_patterns(&suite.results);

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
        TestCase {
            query_name: "Mishnaic Structure".to_string(),
            regex_terms: vec![
                "משנה".to_string(),
                "[א-ת]{1,3}".to_string(),
                "פרק".to_string(),
                "[א-ט]".to_string(),
            ],
            facets: vec![],
            slop: 4,
            max_expansions: 100,
        },
        TestCase {
            query_name: "Gematria Pattern".to_string(),
            regex_terms: vec!["[א-ת]\"[א-ת]".to_string()],
            facets: vec![],
            slop: 0,
            max_expansions: 500,
        },
        TestCase {
            query_name: "Responsa Pattern".to_string(),
            regex_terms: vec![
                "שו\"ת".to_string(),
                "[א-ת]{3,10}".to_string(),
                "סימן".to_string(),
            ],
            facets: vec![],
            slop: 3,
            max_expansions: 200,
        },
        TestCase {
            query_name: "Kabbalistic Terms".to_string(),
            regex_terms: vec![
                "(ספירות|פרצופים|עולמות)".to_string(),
                "[א-ת]{2,6}".to_string(),
            ],
            facets: vec![],
            slop: 2,
            max_expansions: 150,
        },
        TestCase {
            query_name: "Very Complex Hebrew Root".to_string(),
            regex_terms: vec![
                "[כ-מ][ת-ת][ב-ו]".to_string(),
                "[א-ה].*".to_string(),
                "[ל-ן]{1,2}".to_string(),
            ],
            facets: vec![],
            slop: 3,
            max_expansions: 400,
        },
    ];

    let suite = benchmarker.benchmark_custom_queries(custom_queries)?;

    println!("\n{}", "=".repeat(80));
    println!("HEBREW TEXT ANALYSIS:");
    println!("{}", "=".repeat(80));

    analyze_hebrew_patterns(&suite.results);

    Ok(())
}

fn analyze_performance_patterns(results: &[BenchmarkResult]) {
    // Analyze by query complexity
    let single_term: Vec<_> = results
        .iter()
        .filter(|r| r.regex_terms.len() == 1)
        .collect();
    let multi_term: Vec<_> = results.iter().filter(|r| r.regex_terms.len() > 1).collect();

    if !single_term.is_empty() {
        let avg_single = single_term.iter().map(|r| r.execution_time_ms).sum::<u64>() as f64
            / single_term.len() as f64;
        println!("Single-term queries average: {:.2}ms", avg_single);
    }

    if !multi_term.is_empty() {
        let avg_multi = multi_term.iter().map(|r| r.execution_time_ms).sum::<u64>() as f64
            / multi_term.len() as f64;
        println!("Multi-term queries average: {:.2}ms", avg_multi);
    }

    // Analyze by slop impact
    let no_slop: Vec<_> = results.iter().filter(|r| r.slop == 0).collect();
    let with_slop: Vec<_> = results.iter().filter(|r| r.slop > 0).collect();

    if !no_slop.is_empty() && !with_slop.is_empty() {
        let avg_no_slop =
            no_slop.iter().map(|r| r.execution_time_ms).sum::<u64>() as f64 / no_slop.len() as f64;
        let avg_with_slop = with_slop.iter().map(|r| r.execution_time_ms).sum::<u64>() as f64
            / with_slop.len() as f64;

        println!("No slop average: {:.2}ms", avg_no_slop);
        println!("With slop average: {:.2}ms", avg_with_slop);
        println!(
            "Slop performance impact: {:.1}x slower",
            avg_with_slop / avg_no_slop
        );
    }

    // Analyze by expansion limit impact
    let low_expansion: Vec<_> = results.iter().filter(|r| r.max_expansions <= 100).collect();
    let high_expansion: Vec<_> = results.iter().filter(|r| r.max_expansions > 100).collect();

    if !low_expansion.is_empty() && !high_expansion.is_empty() {
        let avg_low = low_expansion
            .iter()
            .map(|r| r.execution_time_ms)
            .sum::<u64>() as f64
            / low_expansion.len() as f64;
        let avg_high = high_expansion
            .iter()
            .map(|r| r.execution_time_ms)
            .sum::<u64>() as f64
            / high_expansion.len() as f64;

        println!("Low expansion (≤100) average: {:.2}ms", avg_low);
        println!("High expansion (>100) average: {:.2}ms", avg_high);
        println!("High expansion impact: {:.1}x slower", avg_high / avg_low);
    }

    // Find patterns that cause performance issues
    let slow_queries: Vec<_> = results
        .iter()
        .filter(|r| r.execution_time_ms > 200)
        .collect();
    if !slow_queries.is_empty() {
        println!("\nPerformance bottleneck patterns:");
        for query in slow_queries {
            let pattern_analysis = analyze_regex_complexity(&query.regex_terms);
            println!(
                "  - {}: {} ({})",
                query.query_name, pattern_analysis, query.execution_time_ms
            );
        }
    }
}

fn analyze_hebrew_patterns(results: &[BenchmarkResult]) {
    println!("Hebrew text search performance analysis:");

    // Group by pattern type
    let mut talmudic = Vec::new();
    let mut biblical = Vec::new();
    let mut halachic = Vec::new();
    let mut other = Vec::new();

    for result in results {
        if result.query_name.contains("Talmudic") || result.query_name.contains("Rabbi") {
            talmudic.push(result);
        } else if result.query_name.contains("Biblical") || result.query_name.contains("Verse") {
            biblical.push(result);
        } else if result.query_name.contains("Halachic") || result.query_name.contains("Responsa") {
            halachic.push(result);
        } else {
            other.push(result);
        }
    }

    if !talmudic.is_empty() {
        let avg = talmudic.iter().map(|r| r.execution_time_ms).sum::<u64>() as f64
            / talmudic.len() as f64;
        println!("  Talmudic patterns average: {:.2}ms", avg);
    }

    if !biblical.is_empty() {
        let avg = biblical.iter().map(|r| r.execution_time_ms).sum::<u64>() as f64
            / biblical.len() as f64;
        println!("  Biblical patterns average: {:.2}ms", avg);
    }

    if !halachic.is_empty() {
        let avg = halachic.iter().map(|r| r.execution_time_ms).sum::<u64>() as f64
            / halachic.len() as f64;
        println!("  Halachic patterns average: {:.2}ms", avg);
    }

    if !other.is_empty() {
        let avg =
            other.iter().map(|r| r.execution_time_ms).sum::<u64>() as f64 / other.len() as f64;
        println!("  Other patterns average: {:.2}ms", avg);
    }

    // Analyze Hebrew-specific complexity
    println!("\nHebrew regex complexity analysis:");
    for result in results {
        if result.execution_time_ms > 100 {
            println!("  {}: {:.2}ms", result.query_name, result.execution_time_ms);
            for term in &result.regex_terms {
                if term.contains("[א-ת]") {
                    println!("    - Uses Hebrew character class: {}", term);
                }
                if term.contains(".*") && term.contains("[א-ת]") {
                    println!("    - Hebrew wildcard pattern (expensive): {}", term);
                }
            }
        }
    }
}

fn analyze_regex_complexity(terms: &[String]) -> String {
    let mut complexity_factors = Vec::new();

    for term in terms {
        if term.contains(".*") {
            complexity_factors.push("wildcard");
        }
        if term.contains("[א-ת]") {
            complexity_factors.push("Hebrew char class");
        }
        if term.contains("(") && term.contains("|") {
            complexity_factors.push("alternation");
        }
        if term.contains("{") {
            complexity_factors.push("quantifiers");
        }
        if term.contains("+") || term.contains("*") {
            complexity_factors.push("repetition");
        }
    }

    if complexity_factors.is_empty() {
        "simple pattern".to_string()
    } else {
        complexity_factors.join(", ")
    }
}
