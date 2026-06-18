use std::path::Path;

use conformance::expectations::Expectations;
use conformance::runner::{TestOutcome, run_tests};

/// CI gate: run the conformance runner against committed expectations and fail
/// on any diff. This is `#[ignore]`-d because the full test262 sweep (50k files)
/// is too heavy for `cargo test`; CI runs it explicitly:
///     cargo test -p conformance -- --ignored --nocapture
#[test]
#[ignore]
fn expectations_match_committed() {
    let expectations_path = Path::new("conformance/expectations.json");
    let previous =
        Expectations::load(expectations_path).expect("failed to load committed expectations");

    let stats = run_tests(
        Path::new("test262/test"),
        Path::new("conformance/harness"),
        Path::new("test262/harness"),
        None,
        Some(&previous),
    );

    let mut current = Expectations::default();
    for r in &stats.results {
        let expected = match r.outcome {
            TestOutcome::Pass => conformance::expectations::ExpectedResult::Pass,
            TestOutcome::Fail => conformance::expectations::ExpectedResult::Fail,
            TestOutcome::Skip => conformance::expectations::ExpectedResult::Skip(r.detail.clone()),
        };
        current.entries.insert(r.path.clone(), expected);
    }

    let mut diffs: Vec<String> = Vec::new();
    for (path, expected) in &current.entries {
        match previous.entries.get(path) {
            Some(prev) if prev == expected => {}
            Some(prev) => diffs.push(format!("  {path}: was {prev:?}, now {expected:?}")),
            None => diffs.push(format!("  {path}: new test, result {expected:?}")),
        }
    }
    for path in previous.entries.keys() {
        if !current.entries.contains_key(path) {
            diffs.push(format!("  {path}: removed"));
        }
    }

    if !diffs.is_empty() {
        panic!(
            "expectations diff ({} changes):\n{}",
            diffs.len(),
            diffs.join("\n")
        );
    }

    println!(
        "expectations match: total={} pass={} fail={} skip={}",
        stats.total, stats.pass, stats.fail, stats.skip,
    );
}
