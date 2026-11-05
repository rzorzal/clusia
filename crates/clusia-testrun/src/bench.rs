//! A benchmark round's pieces: the Rust loop file's test target, the settings as environment,
//! and the marked lines a loop prints, turned into metrics.

use std::collections::BTreeMap;

use clusia_core::cockpit::{BENCH_CALLS_MAX, ErrorLine, Metric};

use crate::functions::{case_env, driver_target};
use crate::scenario::{add_error, marked};

/// What a round with no `bench` line says; the exit code follows in parentheses.
pub const NO_BENCH_RESULT: &str = "The benchmark ended without a result";

/// The name of a Rust loop file's test target: `clusia_bench_` and the same ten hex as the
/// function's driver.
pub fn bench_target(slug: &str) -> String {
    let driver = driver_target(slug);
    format!("clusia_bench_{}", &driver["clusia_driver_".len()..])
}

/// The environment of a benchmark round: a case's, then the loop's settings.
pub fn bench_env(
    mark: &str,
    root: &str,
    values: &BTreeMap<String, String>,
    warmup: u32,
    secs: u32,
) -> Vec<(String, String)> {
    let mut env = case_env(mark, root, "bench", values);
    env.extend([
        ("CLUSIA_BENCH_WARMUP".to_string(), warmup.to_string()),
        ("CLUSIA_BENCH_SECS".to_string(), secs.to_string()),
        (
            "CLUSIA_BENCH_CALLS_MAX".to_string(),
            BENCH_CALLS_MAX.to_string(),
        ),
    ]);
    env
}

/// The one `bench` line of a loop: how many calls, their total time and the per-call times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BenchLine {
    pub calls: u64,
    pub total_ns: u64,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub max_ns: u64,
}

/// What a loop printed, with the time each line arrived at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BenchReport {
    pub started_ms: Option<u64>,
    pub bench: Option<BenchLine>,
    pub bench_ms: Option<u64>,
    pub errors: Vec<ErrorLine>,
    pub done: bool,
}

/// Reads one stdout line: `start`, `bench <calls> <total_ns> <p50_ns> <p95_ns> <max_ns>`,
/// `error <text>` and `done`, only when it starts with `<mark> `.
pub fn read_bench_line(mark: &str, line: &str, at_ms: u64, report: &mut BenchReport) {
    let Some(rest) = marked(mark, line) else {
        return;
    };
    if rest == "start" {
        report.started_ms.get_or_insert(at_ms);
    } else if rest == "done" {
        report.done = true;
    } else if let Some(numbers) = rest.strip_prefix("bench ") {
        let parsed: Option<Vec<u64>> = numbers.split(' ').map(|n| n.parse().ok()).collect();
        if let Some([calls, total_ns, p50_ns, p95_ns, max_ns]) = parsed.as_deref() {
            report.bench = Some(BenchLine {
                calls: *calls,
                total_ns: *total_ns,
                p50_ns: *p50_ns,
                p95_ns: *p95_ns,
                max_ns: *max_ns,
            });
            report.bench_ms = Some(at_ms);
        }
    } else if let Some(text) = rest.strip_prefix("error ") {
        add_error(&mut report.errors, text, at_ms);
    }
}

/// The metrics of a `bench` line: the median and 95th percentile per call and the calls per
/// second (absent when no time passed).
pub fn bench_metrics(line: &BenchLine) -> BTreeMap<Metric, u64> {
    let mut metrics = BTreeMap::from([(Metric::P50, line.p50_ns), (Metric::P95, line.p95_ns)]);
    if line.total_ns > 0 {
        let rate = u128::from(line.calls) * 1_000_000_000_000 / u128::from(line.total_ns);
        metrics.insert(Metric::CallsPerSec, u64::try_from(rate).unwrap_or(u64::MAX));
    }
    metrics
}

#[cfg(test)]
mod tests {
    use clusia_core::cockpit::{ErrorLine, Metric};
    use clusia_core::functions::function_slug;

    use super::*;
    use crate::functions::driver_target;

    const MARK: &str = "0123456789abcdef";

    #[test]
    fn bench_names_and_env_follow_the_function() {
        let slug = function_slug("src/store.rs", "TokenStore::refresh");
        let hex = &slug[slug.len() - 10..];
        assert_eq!(bench_target(&slug), format!("clusia_bench_{hex}"));
        assert_eq!(&driver_target(&slug)["clusia_driver_".len()..], hex);
        let other = bench_target("not a slug");
        assert!(other.starts_with("clusia_bench_") && other.len() == "clusia_bench_".len() + 10);

        let hostile = "$(touch /tmp/pwned)'; rm -rf ~ #";
        let values = BTreeMap::from([
            ("stored_token".to_string(), hostile.to_string()),
            ("endpoint_status".to_string(), "500".to_string()),
        ]);
        let env = bench_env(MARK, "/work", &values, 10, 6);
        let expected: Vec<(String, String)> = [
            ("CLUSIA_MARK", MARK),
            ("CLUSIA_ROOT", "/work"),
            ("CLUSIA_CASE", "bench"),
            ("CLUSIA_ARG_ENDPOINT_STATUS", "500"),
            ("CLUSIA_ARG_STORED_TOKEN", hostile),
            ("CLUSIA_BENCH_WARMUP", "10"),
            ("CLUSIA_BENCH_SECS", "6"),
            ("CLUSIA_BENCH_CALLS_MAX", "10000000"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(env, expected);
    }

    #[test]
    fn bench_lines_are_read_as_they_arrive() {
        let mut report = BenchReport::default();
        let mut read = |line: String, at: u64| read_bench_line(MARK, &line, at, &mut report);
        read(format!("{MARK} start\n"), 100);
        read(format!("{MARK} start"), 150);
        read(
            format!("{MARK} bench 1000 6000000000 5000000 9000000 20000000"),
            6200,
        );
        read(
            format!("{MARK} bench 2000 6000000000 3000000 4000000 7000000"),
            6300,
        );
        read(format!("{MARK} bench 1 2 3 4"), 6400);
        read(format!("{MARK} bench 1 2 3 4 5 6"), 6401);
        read(format!("{MARK} bench a b c d e"), 6402);
        read(format!("{MARK} bench 1 2 3 4 -5"), 6403);
        read("ffffffffffffffff bench 9 9 9 9 9".to_string(), 6404);
        read(format!("{MARK} error slow call"), 6500);
        read(format!("{MARK} error slow call"), 6600);
        read(format!("{MARK} done"), 6700);
        assert_eq!(
            report,
            BenchReport {
                started_ms: Some(100),
                bench: Some(BenchLine {
                    calls: 2000,
                    total_ns: 6_000_000_000,
                    p50_ns: 3_000_000,
                    p95_ns: 4_000_000,
                    max_ns: 7_000_000,
                }),
                bench_ms: Some(6300),
                errors: vec![ErrorLine {
                    text: "slow call".into(),
                    count: 2,
                    first_ms: 6500
                }],
                done: true,
            }
        );
    }

    #[test]
    fn bench_metrics_are_per_call_and_calls_per_second() {
        let line = BenchLine {
            calls: 2000,
            total_ns: 6_000_000_000,
            p50_ns: 3_000_000,
            p95_ns: 4_000_000,
            max_ns: 7_000_000,
        };
        assert_eq!(
            bench_metrics(&line),
            BTreeMap::from([
                (Metric::P50, 3_000_000),
                (Metric::P95, 4_000_000),
                (Metric::CallsPerSec, 333_333),
            ])
        );
        let empty = BenchLine {
            total_ns: 0,
            ..line
        };
        assert!(!bench_metrics(&empty).contains_key(&Metric::CallsPerSec));
        let huge = BenchLine {
            calls: u64::MAX,
            total_ns: 1,
            ..line
        };
        assert_eq!(bench_metrics(&huge)[&Metric::CallsPerSec], u64::MAX);
    }
}
