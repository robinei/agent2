use std::path::PathBuf;
use std::process;

use conformance::expectations::{Expectations, ExpectedResult};
use conformance::runner::{self, run_tests};

struct Args {
    /// Substring filter for test paths (only run matching tests).
    filter: Option<String>,
    /// Update the expectations file with current results.
    update: bool,
    /// Check mode: compare results against expectations, exit non-zero on diff.
    check: bool,
    /// Path to the test262 root (contains test/ and harness/).
    test262_root: PathBuf,
    /// Path to the expectations file.
    expectations_path: PathBuf,
}

fn parse_args() -> Args {
    let mut filter: Option<String> = None;
    let mut update = false;
    let mut check = false;
    let mut test262_root = PathBuf::from("test262");
    let mut expectations_path = PathBuf::from("conformance/expectations.json");

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--update" => update = true,
            "--check" => check = true,
            "--test262-root" => {
                i += 1;
                if i < args.len() {
                    test262_root = PathBuf::from(&args[i]);
                }
            }
            "--expectations" => {
                i += 1;
                if i < args.len() {
                    expectations_path = PathBuf::from(&args[i]);
                }
            }
            "-h" | "--help" => {
                eprintln!(
                    "Usage: conformance [--update] [--check] [--test262-root DIR] [--expectations FILE] [filter]"
                );
                eprintln!("  --update          Update expectations file with current results");
                eprintln!(
                    "  --check           Compare results against expectations, exit non-zero on mismatch"
                );
                eprintln!("  --test262-root    Path to test262 directory (default: test262)");
                eprintln!(
                    "  --expectations    Path to expectations JSON (default: conformance/expectations.json)"
                );
                eprintln!("  [filter]          Optional substring filter for test paths");
                process::exit(0);
            }
            other => {
                if filter.is_none() && !other.starts_with('-') {
                    filter = Some(other.to_string());
                }
            }
        }
        i += 1;
    }

    Args {
        filter,
        update,
        check,
        test262_root,
        expectations_path,
    }
}

fn main() {
    let args = parse_args();

    let test262_harness = args.test262_root.join("harness");
    let test_dir = args.test262_root.join("test");
    let local_harness = PathBuf::from("conformance/harness");

    if !test262_harness.is_dir() {
        eprintln!(
            "error: harness dir not found: {}",
            test262_harness.display()
        );
        process::exit(1);
    }
    if !test_dir.is_dir() {
        eprintln!("error: test dir not found: {}", test_dir.display());
        process::exit(1);
    }
    if !local_harness.is_dir() {
        eprintln!(
            "error: local harness dir not found: {}",
            local_harness.display()
        );
        process::exit(1);
    }

    let existing = if args.check {
        match Expectations::load(&args.expectations_path) {
            Ok(exp) => Some(exp),
            Err(e) => {
                eprintln!("error: failed to load expectations: {e}");
                process::exit(1);
            }
        }
    } else {
        None
    };

    let stats = run_tests(
        &test_dir,
        &local_harness,
        &test262_harness,
        args.filter.as_deref(),
        existing.as_ref(),
    );

    // Print histogram.
    eprintln!();
    eprintln!("── Failure causes ──");
    for (cause, count) in &stats.by_cause {
        eprintln!("  {count:>6}  {cause}");
    }
    if !stats.by_feature.is_empty() {
        eprintln!("── Failures by feature (first listed) ──");
        for (feature, count) in &stats.by_feature {
            eprintln!("  {count:>6}  {feature}");
        }
    }

    // Build new expectations from results.
    let mut new_expectations = Expectations::default();
    for r in &stats.results {
        let expected = match r.outcome {
            runner::TestOutcome::Pass => ExpectedResult::Pass,
            runner::TestOutcome::Fail => ExpectedResult::Fail,
            runner::TestOutcome::Skip => ExpectedResult::Skip(r.detail.clone()),
        };
        new_expectations.entries.insert(r.path.clone(), expected);
    }

    if args.update {
        // Merge: if a filter was active, keep entries outside the filter scope.
        if let Some(filter) = &args.filter
            && let Ok(existing) = Expectations::load(&args.expectations_path) {
                for (path, expected) in existing.entries {
                    if !path.contains(filter.as_str()) {
                        new_expectations.entries.entry(path).or_insert(expected);
                    }
                }
            }
        if let Err(e) = new_expectations.save(&args.expectations_path) {
            eprintln!("error: failed to save expectations: {e}");
            process::exit(1);
        }
        eprintln!("Updated expectations: {}", args.expectations_path.display());
    }

    if args.check {
        let existing = existing.unwrap();
        let mut diffs: Vec<String> = Vec::new();

        // Check for changed results.
        for (path, expected) in &new_expectations.entries {
            match existing.entries.get(path) {
                Some(prev) if prev == expected => {}
                Some(prev) => {
                    diffs.push(format!("  {path}: was {prev:?}, now {expected:?}"));
                }
                None => {
                    diffs.push(format!("  {path}: new test, result {expected:?}"));
                }
            }
        }
        // Check for removed tests: only those matching the active filter (if any).
        for path in existing.entries.keys() {
            if !new_expectations.entries.contains_key(path)
                && args
                    .filter
                    .as_ref()
                    .is_none_or(|f| path.contains(f.as_str()))
            {
                diffs.push(format!("  {path}: removed"));
            }
        }

        if !diffs.is_empty() {
            eprintln!("expectations diff ({} changes):", diffs.len());
            for d in &diffs {
                eprintln!("{d}");
            }
            process::exit(1);
        }
        eprintln!("expectations match: no changes detected.");
    }

    // Print summary to stdout for CI / scripting.
    println!(
        "total={} pass={} fail={} skip={}",
        stats.total, stats.pass, stats.fail, stats.skip,
    );
}
