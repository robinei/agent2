use std::path::Path;

use conformance::expectations::Expectations;
use conformance::runner::{TestOutcome, run_tests};

/// CI gate: run the conformance runner against committed expectations and fail
/// on any diff. This is `#[ignore]`-d because the full test262 sweep (50k files)
/// is too heavy for `cargo test`; CI runs it explicitly:
///     cargo test -p conformance -- --ignored --nocapture
///
/// **A diff of a dozen entries is not yet a signal.** The sweep is not
/// reproducible: two runs of the *same binary* on 2026-09-24 gave
/// 8,273 and 8,263 passes, and their regression lists differed by 19
/// entries. Every one of those was annex-B block-scoped function
/// hoisting (`annexB/language/{function,global}-code/*-existing-*fn-*`)
/// plus a couple of `language/statements/function` neighbours — the
/// same family each time, flipping in both directions.
///
/// So read a diff by *family* before believing it: a change confined
/// to those paths is the known noise, and anything outside them is
/// real. Judging a change by the pass count alone will attribute ±10
/// tests to whatever was edited last.
#[test]
#[ignore]
fn expectations_match_committed() {
    // **Anchored at the workspace root, not at the working directory.**
    // Cargo runs an integration test from its *package* directory, so
    // every one of these paths resolved under `conformance/` and missed:
    // `conformance/conformance/expectations.json`,
    // `conformance/test262/test`. `Expectations::load` answers a missing
    // file with an empty set rather than an error, and the walk of a
    // missing directory yields nothing — so an empty run was compared
    // against empty expectations, matched, and printed
    // `expectations match: total=0`. This gate covers 53,658 files and
    // was passing on none of them.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the conformance package sits in the workspace");
    let expectations_path = root.join("conformance/expectations.json");
    let previous =
        Expectations::load(&expectations_path).expect("failed to load committed expectations");
    // **A gate that ran nothing fails.** Both halves of the comparison
    // go empty together when the corpus is absent, and empty matches
    // empty — so the one thing this must never do is agree with
    // itself about nothing. The submodule is either checked out or
    // this is not a run.
    assert!(
        !previous.entries.is_empty(),
        "no committed expectations at {}: nothing to gate against",
        expectations_path.display()
    );

    let stats = run_tests(
        &root.join("test262/test"),
        &root.join("conformance/harness"),
        &root.join("test262/harness"),
        None,
        Some(&previous),
    );
    assert!(
        stats.total > 0,
        "the test262 corpus is missing or empty — `git submodule update --init test262`"
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
