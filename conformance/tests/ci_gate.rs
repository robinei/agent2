use std::path::Path;

use conformance::expectations::Expectations;
use conformance::runner::{TestOutcome, run_tests};

/// CI gate: run the conformance runner against committed expectations and fail
/// on any diff. This is `#[ignore]`-d because the full test262 sweep (50k files)
/// is too heavy for `cargo test`; CI runs it explicitly:
///     cargo test -p conformance -- --ignored --nocapture
///
/// **The sweep is reproducible, and a diff means something.** It was
/// not, briefly: two runs of the *same binary* on 2026-09-24 gave
/// 8,273 and 8,263 passes with regression lists differing by 19
/// entries, all annex-B block-scoped function hoisting. This comment
/// used to say so and advise reading a diff by family, treating that
/// family as noise.
///
/// That advice was wrong, and wrong in the expensive direction. The
/// variance was a real defect — `register_const_fns` walked a
/// `HashSet<usize>`, whose seed changes per process, to fill a
/// name-keyed table that holds one entry per name — sitting on top of
/// a real spec bug, block-level declarations being hoisted into the
/// prologue instead of stored where they stand. Calling it noise is
/// what a whole afternoon nearly did. Both are fixed (`298aa54`);
/// annex-B passes went from a wandering 102–109 to a fixed 157.
///
/// So: read a diff. Every entry in one is now a change something
/// made.
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
