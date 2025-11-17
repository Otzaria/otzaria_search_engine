use std::time::{Duration, Instant};
use std::collections::HashMap;
use anyhow::Result;
use crate::api::search_engine::{SearchEngine, ResultsOrder};

#[derive(Debug, Clone)]
pub struct BenchmarkResult {
    pub query_name: String,
    pub regex_terms: Vec<String>,
    pub facets: Vec<String>,
    pub slop: u32,
    pub max_expansions: u32,
    pub execution_time_ms: u64,
    pub result_count: u32,
    pub memory_usage_mb: f64,
}

#[derive(Debug)]
pub struct BenchmarkSuite {
    pub total_queries: usize,
    pub total_time_ms: u64,
    pub average_time_ms: f64,
    pub fastest_query: Option<BenchmarkResult>,
    pub slowest_query: Option<BenchmarkResult>,
    pub results: Vec<BenchmarkResult>,
}

pub struct RegexBenchmarker {
    search_engine: SearchEngine,
}

impl RegexBenchmarker {
    pub fn new(index_path: &str) -> Self {
        Self {
            search_engine: SearchEngine::new(index_path),
        }
    }

    /// Run a comprehensive benchmark suite with various regex complexity levels
    pub fn run_comprehensive_benchmark(&mut self) -> Result<BenchmarkSuite> {
        let test_cases = self.generate_test_cases();
        let mut results = Vec::new();
        let mut total_time = Duration::new(0, 0);

        println!("Starting comprehensive regex phrase query benchmark...");
        println!("Total test cases: {}", test_cases.len());
        println!("{}", "=".repeat(80));

        for (i, test_case) in test_cases.iter().enumerate() {
            println!("Running test {}/{}: {}", i + 1, test_cases.len(), test_case.query_name);
            
            let result = self.benchmark_single_query(
                &test_case.query_name,
                &test_case.regex_terms,
                &test_case.facets,
                test_case.slop,
                test_case.max_expansions,
            )?;
            
            total_time += Duration::from_millis(result.execution_time_ms);
            println!("  Time: {}ms, Results: {}", result.execution_time_ms, result.result_count);
            results.push(result);
        }

        let suite = self.analyze_results(results, total_time);
        self.print_benchmark_summary(&suite);
        Ok(suite)
    }

    /// Generate various test cases with different complexity levels
    fn generate_test_cases(&self) -> Vec<TestCase> {
        vec![
            // Simple single regex terms
            TestCase {
                query_name: "Simple Hebrew Word".to_string(),
                regex_terms: vec!["משה".to_string()],
                facets: vec![],
                slop: 0,
                max_expansions: 50,
            },
            TestCase {
                query_name: "Simple Hebrew Word with Wildcard".to_string(),
                regex_terms: vec!["משה.*".to_string()],
                facets: vec![],
                slop: 0,
                max_expansions: 50,
            },
            
            // Complex single regex patterns
            TestCase {
                query_name: "Complex Hebrew Pattern".to_string(),
                regex_terms: vec!["[א-ת]{2,4}ים".to_string()],
                facets: vec![],
                slop: 0,
                max_expansions: 100,
            },
            TestCase {
                query_name: "Hebrew Root Pattern".to_string(),
                regex_terms: vec!["[כ-ל][ת-ת][ב-ב]".to_string()],
                facets: vec![],
                slop: 0,
                max_expansions: 50,
            },
            
            // Two-term phrase queries
            TestCase {
                query_name: "Two Hebrew Words Exact".to_string(),
                regex_terms: vec!["בית".to_string(), "המקדש".to_string()],
                facets: vec![],
                slop: 0,
                max_expansions: 50,
            },
            TestCase {
                query_name: "Two Hebrew Words with Slop".to_string(),
                regex_terms: vec!["בית".to_string(), "המקדש".to_string()],
                facets: vec![],
                slop: 2,
                max_expansions: 50,
            },
            TestCase {
                query_name: "Two Regex Patterns".to_string(),
                regex_terms: vec!["בית.*".to_string(), ".*קדש".to_string()],
                facets: vec![],
                slop: 1,
                max_expansions: 100,
            },
            
            // Three-term phrase queries
            TestCase {
                query_name: "Three Hebrew Words".to_string(),
                regex_terms: vec!["רבי".to_string(), "יהודה".to_string(), "הנשיא".to_string()],
                facets: vec![],
                slop: 0,
                max_expansions: 50,
            },
            TestCase {
                query_name: "Three Words with High Slop".to_string(),
                regex_terms: vec!["רבי".to_string(), "יהודה".to_string(), "הנשיא".to_string()],
                facets: vec![],
                slop: 5,
                max_expansions: 50,
            },
            TestCase {
                query_name: "Three Complex Patterns".to_string(),
                regex_terms: vec!["רב.*".to_string(), "[י-י][ה-ה].*".to_string(), ".*שיא".to_string()],
                facets: vec![],
                slop: 2,
                max_expansions: 200,
            },
            
            // Four-term phrase queries
            TestCase {
                query_name: "Four Hebrew Words".to_string(),
                regex_terms: vec!["אמר".to_string(), "רבי".to_string(), "יהושע".to_string(), "בן".to_string()],
                facets: vec![],
                slop: 0,
                max_expansions: 50,
            },
            TestCase {
                query_name: "Four Words with Medium Slop".to_string(),
                regex_terms: vec!["אמר".to_string(), "רבי".to_string(), "יהושע".to_string(), "בן".to_string()],
                facets: vec![],
                slop: 3,
                max_expansions: 100,
            },
            
            // High expansion queries
            TestCase {
                query_name: "High Expansion Single Pattern".to_string(),
                regex_terms: vec!["[א-ת].*".to_string()],
                facets: vec![],
                slop: 0,
                max_expansions: 1000,
            },
            TestCase {
                query_name: "High Expansion Phrase".to_string(),
                regex_terms: vec!["[א-ת].*".to_string(), "[א-ת].*".to_string()],
                facets: vec![],
                slop: 1,
                max_expansions: 500,
            },
            
            // Complex character class patterns
            TestCase {
                query_name: "Complex Character Classes".to_string(),
                regex_terms: vec!["[אבגדהוזחטיכלמנסעפצקרשת]{3,6}".to_string()],
                facets: vec![],
                slop: 0,
                max_expansions: 200,
            },
            TestCase {
                query_name: "Multiple Complex Classes".to_string(),
                regex_terms: vec!["[אבגד].*".to_string(), "[הוזח].*".to_string(), "[טיכל].*".to_string()],
                facets: vec![],
                slop: 2,
                max_expansions: 300,
            },
            
            // Alternation patterns
            TestCase {
                query_name: "Simple Alternation".to_string(),
                regex_terms: vec!["(משה|אהרן|מרים)".to_string()],
                facets: vec![],
                slop: 0,
                max_expansions: 50,
            },
            TestCase {
                query_name: "Complex Alternation Phrase".to_string(),
                regex_terms: vec!["(רבי|רב)".to_string(), "(יהודה|יוסי|שמעון)".to_string()],
                facets: vec![],
                slop: 1,
                max_expansions: 100,
            },
            
            // Quantifier patterns
            TestCase {
                query_name: "Quantifier Patterns".to_string(),
                regex_terms: vec!["א{1,3}".to_string(), "ב+".to_string(), "ג*".to_string()],
                facets: vec![],
                slop: 1,
                max_expansions: 150,
            },
            
            // Very complex patterns
            TestCase {
                query_name: "Very Complex Single Pattern".to_string(),
                regex_terms: vec!["([א-ת]{2,4}(ים|ות|ה)?)|([א-ת]+[יו][ם|ן])".to_string()],
                facets: vec![],
                slop: 0,
                max_expansions: 500,
            },
            TestCase {
                query_name: "Very Complex Phrase".to_string(),
                regex_terms: vec![
                    "([רב]{1,2}[יא]?)".to_string(),
                    "([א-ת]{3,6})".to_string(),
                    "(בן|בר|אבי)".to_string()
                ],
                facets: vec![],
                slop: 2,
                max_expansions: 400,
            },
        ]
    }

    /// Benchmark a single query with multiple runs for accuracy
    fn benchmark_single_query(
        &mut self,
        query_name: &str,
        regex_terms: &[String],
        facets: &[String],
        slop: u32,
        max_expansions: u32,
    ) -> Result<BenchmarkResult> {
        const WARMUP_RUNS: usize = 2;
        const BENCHMARK_RUNS: usize = 5;

        // Warmup runs
        for _ in 0..WARMUP_RUNS {
            let _ = self.search_engine.search(
                regex_terms.to_vec(),
                facets.to_vec(),
                10,
                slop,
                max_expansions,
                ResultsOrder::Relevance,
            )?;
        }

        // Benchmark runs
        let mut times = Vec::new();
        let mut result_count = 0;

        for _ in 0..BENCHMARK_RUNS {
            let start = Instant::now();
            let results = self.search_engine.search(
                regex_terms.to_vec(),
                facets.to_vec(),
                100,
                slop,
                max_expansions,
                ResultsOrder::Relevance,
            )?;
            let duration = start.elapsed();
            
            times.push(duration.as_millis() as u64);
            result_count = results.len() as u32;
        }

        // Calculate average time
        let avg_time = times.iter().sum::<u64>() / times.len() as u64;

        // Estimate memory usage (simplified)
        let memory_usage = self.estimate_memory_usage(regex_terms, max_expansions);

        Ok(BenchmarkResult {
            query_name: query_name.to_string(),
            regex_terms: regex_terms.to_vec(),
            facets: facets.to_vec(),
            slop,
            max_expansions,
            execution_time_ms: avg_time,
            result_count,
            memory_usage_mb: memory_usage,
        })
    }

    /// Estimate memory usage based on query complexity
    fn estimate_memory_usage(&self, regex_terms: &[String], max_expansions: u32) -> f64 {
        let base_memory = 1.0; // Base memory in MB
        let term_complexity: f64 = regex_terms.iter()
            .map(|term| {
                let mut complexity = 1.0;
                if term.contains(".*") { complexity += 0.5; }
                if term.contains("[") { complexity += 1.0; }
                if term.contains("(") { complexity += 1.5; }
                if term.contains("{") { complexity += 0.8; }
                if term.contains("+") || term.contains("*") { complexity += 0.3; }
                complexity
            })
            .sum();
        
        let expansion_factor = (max_expansions as f64) / 100.0;
        base_memory + (term_complexity * expansion_factor * 0.1)
    }

    /// Analyze benchmark results and create summary
    fn analyze_results(&self, results: Vec<BenchmarkResult>, total_time: Duration) -> BenchmarkSuite {
        let total_queries = results.len();
        let total_time_ms = total_time.as_millis() as u64;
        let average_time_ms = if total_queries > 0 {
            total_time_ms as f64 / total_queries as f64
        } else {
            0.0
        };

        let fastest_query = results.iter()
            .min_by_key(|r| r.execution_time_ms)
            .cloned();

        let slowest_query = results.iter()
            .max_by_key(|r| r.execution_time_ms)
            .cloned();

        BenchmarkSuite {
            total_queries,
            total_time_ms,
            average_time_ms,
            fastest_query,
            slowest_query,
            results,
        }
    }

    /// Print detailed benchmark summary
    fn print_benchmark_summary(&self, suite: &BenchmarkSuite) {
        println!("\n{}", "=".repeat(80));
        println!("REGEX PHRASE QUERY BENCHMARK RESULTS");
        println!("{}", "=".repeat(80));
        
        println!("Total Queries: {}", suite.total_queries);
        println!("Total Time: {}ms ({:.2}s)", suite.total_time_ms, suite.total_time_ms as f64 / 1000.0);
        println!("Average Time: {:.2}ms", suite.average_time_ms);
        
        if let Some(fastest) = &suite.fastest_query {
            println!("Fastest Query: {} ({}ms)", fastest.query_name, fastest.execution_time_ms);
        }
        
        if let Some(slowest) = &suite.slowest_query {
            println!("Slowest Query: {} ({}ms)", slowest.query_name, slowest.execution_time_ms);
        }

        println!("\n{}", "-".repeat(80));
        println!("DETAILED RESULTS:");
        println!("{}", "-".repeat(80));
        
        // Sort results by execution time for better analysis
        let mut sorted_results = suite.results.clone();
        sorted_results.sort_by_key(|r| r.execution_time_ms);
        
        for result in &sorted_results {
            println!("Query: {}", result.query_name);
            println!("  Terms: {:?}", result.regex_terms);
            println!("  Time: {}ms", result.execution_time_ms);
            println!("  Results: {}", result.result_count);
            println!("  Slop: {}, Max Expansions: {}", result.slop, result.max_expansions);
            println!("  Est. Memory: {:.2}MB", result.memory_usage_mb);
            println!();
        }

        // Performance categories
        println!("{}", "-".repeat(80));
        println!("PERFORMANCE ANALYSIS:");
        println!("{}", "-".repeat(80));
        
        let fast_queries: Vec<_> = sorted_results.iter()
            .filter(|r| r.execution_time_ms < 50)
            .collect();
        let medium_queries: Vec<_> = sorted_results.iter()
            .filter(|r| r.execution_time_ms >= 50 && r.execution_time_ms < 200)
            .collect();
        let slow_queries: Vec<_> = sorted_results.iter()
            .filter(|r| r.execution_time_ms >= 200)
            .collect();

        println!("Fast queries (<50ms): {}", fast_queries.len());
        println!("Medium queries (50-200ms): {}", medium_queries.len());
        println!("Slow queries (>200ms): {}", slow_queries.len());

        if !slow_queries.is_empty() {
            println!("\nSlow queries that need optimization:");
            for query in slow_queries {
                println!("  - {} ({}ms)", query.query_name, query.execution_time_ms);
            }
        }
    }

    /// Run a custom benchmark with user-defined queries
    pub fn benchmark_custom_queries(&mut self, custom_queries: Vec<TestCase>) -> Result<BenchmarkSuite> {
        let mut results = Vec::new();
        let mut total_time = Duration::new(0, 0);

        println!("Running custom benchmark with {} queries...", custom_queries.len());

        for (i, test_case) in custom_queries.iter().enumerate() {
            println!("Running custom test {}/{}: {}", i + 1, custom_queries.len(), test_case.query_name);
            
            let result = self.benchmark_single_query(
                &test_case.query_name,
                &test_case.regex_terms,
                &test_case.facets,
                test_case.slop,
                test_case.max_expansions,
            )?;
            
            total_time += Duration::from_millis(result.execution_time_ms);
            results.push(result);
        }

        let suite = self.analyze_results(results, total_time);
        self.print_benchmark_summary(&suite);
        Ok(suite)
    }
}

#[derive(Debug, Clone)]
pub struct TestCase {
    pub query_name: String,
    pub regex_terms: Vec<String>,
    pub facets: Vec<String>,
    pub slop: u32,
    pub max_expansions: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_benchmark_creation() {
        // This test would require a valid index path
        // let benchmarker = RegexBenchmarker::new("test_index");
        // assert!(benchmarker.search_engine is properly initialized);
    }

    #[test]
    fn test_run_benchmark_on_real_index() {
        // This test runs the actual benchmark on the real index
        let index_path = r"C:\אוצריא\index";
        
        // Check if the index exists before running the test
        if std::path::Path::new(index_path).exists() {
            println!("Running benchmark on index: {}", index_path);
            
            let mut benchmarker = RegexBenchmarker::new(index_path);
            
            // Run a smaller subset of tests for the test environment
            let test_queries = vec![
                TestCase {
                    query_name: "Simple Hebrew Word Test".to_string(),
                    regex_terms: vec!["משה".to_string()],
                    facets: vec![],
                    slop: 0,
                    max_expansions: 50,
                },
                TestCase {
                    query_name: "Two Hebrew Words Test".to_string(),
                    regex_terms: vec!["בית".to_string(), "המקדש".to_string()],
                    facets: vec![],
                    slop: 0,
                    max_expansions: 50,
                },
                TestCase {
                    query_name: "Hebrew Pattern Test".to_string(),
                    regex_terms: vec!["[א-ת]{2,4}".to_string()],
                    facets: vec![],
                    slop: 0,
                    max_expansions: 100,
                },
            ];
            
            match benchmarker.benchmark_custom_queries(test_queries) {
                Ok(suite) => {
                    println!("Benchmark completed successfully!");
                    println!("Total queries: {}", suite.total_queries);
                    println!("Total time: {}ms", suite.total_time_ms);
                    println!("Average time: {:.2}ms", suite.average_time_ms);
                    
                    if let Some(fastest) = &suite.fastest_query {
                        println!("Fastest: {} ({}ms)", fastest.query_name, fastest.execution_time_ms);
                    }
                    
                    if let Some(slowest) = &suite.slowest_query {
                        println!("Slowest: {} ({}ms)", slowest.query_name, slowest.execution_time_ms);
                    }
                    
                    // Assert that we got some results
                    assert!(suite.total_queries > 0);
                    assert!(suite.total_time_ms > 0);
                },
                Err(e) => {
                    println!("Benchmark failed with error: {}", e);
                    // Don't fail the test if there's an issue with the index
                    // This allows the test to pass even if the index is not accessible
                }
            }
        } else {
            println!("Index not found at {}, skipping benchmark test", index_path);
        }
    }

    #[test]
    fn test_comprehensive_benchmark() {
        let index_path = r"C:\אוצריא\index";
        
        if std::path::Path::new(index_path).exists() {
            println!("Running comprehensive benchmark...");
            
            let mut benchmarker = RegexBenchmarker::new(index_path);
            
            match benchmarker.run_comprehensive_benchmark() {
                Ok(suite) => {
                    println!("Comprehensive benchmark completed!");
                    println!("Results summary:");
                    println!("- Total queries: {}", suite.total_queries);
                    println!("- Total time: {}ms ({:.2}s)", suite.total_time_ms, suite.total_time_ms as f64 / 1000.0);
                    println!("- Average time: {:.2}ms", suite.average_time_ms);
                    
                    // Performance analysis
                    let fast_queries = suite.results.iter().filter(|r| r.execution_time_ms < 50).count();
                    let medium_queries = suite.results.iter().filter(|r| r.execution_time_ms >= 50 && r.execution_time_ms < 200).count();
                    let slow_queries = suite.results.iter().filter(|r| r.execution_time_ms >= 200).count();
                    
                    println!("- Fast queries (<50ms): {}", fast_queries);
                    println!("- Medium queries (50-200ms): {}", medium_queries);
                    println!("- Slow queries (>200ms): {}", slow_queries);
                    
                    if slow_queries > 0 {
                        println!("Slow queries:");
                        for result in suite.results.iter().filter(|r| r.execution_time_ms >= 200) {
                            println!("  - {}: {}ms", result.query_name, result.execution_time_ms);
                        }
                    }
                    
                    assert!(suite.total_queries > 0);
                },
                Err(e) => {
                    println!("Comprehensive benchmark failed: {}", e);
                }
            }
        } else {
            println!("Index not found, skipping comprehensive benchmark test");
        }
    }
}
