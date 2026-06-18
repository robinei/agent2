use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;

use interp::{StepResult, VM, compile_for_test262};

use crate::expectations::{Expectations, ExpectedResult};
use crate::frontmatter::{Negative, parse_frontmatter};
use crate::harness::Harness;

const STEP_FUEL: u64 = 200_000;
const WORKER_STACK: usize = 16 * 1024 * 1024;

thread_local! {
    /// The path of the test currently executing on this thread, so the panic
    /// hook can attribute a crash to a specific test file.
    static CURRENT_TEST: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Install a panic hook that prefixes the crashing test's path. Without it, a
/// worker-thread panic prints only `thread '<unnamed>' panicked at …` with no
/// indication of which test was running. Idempotent across `run_tests` calls.
fn install_panic_hook() {
    if HOOK_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::panic::set_hook(Box::new(move |info| {
        let test = CURRENT_TEST.with(|c| c.borrow().clone());
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        let id = thread.id();
        let loc = info.location();
        let payload = info.payload();
        let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "Box<dyn Any>".to_string()
        };
        if let Some(test) = test {
            eprintln!("── test panicked: {test} ──");
        }
        if let Some(loc) = loc {
            eprintln!(
                "thread '{name}' ({id:?}) panicked at {}:{}:{}:",
                loc.file(),
                loc.line(),
                loc.column()
            );
        } else {
            eprintln!("thread '{name}' ({id:?}) panicked:");
        }
        eprintln!("{msg}");
        eprintln!("note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace");
    }));
}

/// Pull a printable message out of a panic payload (which is either a
/// `&'static str` or a `String` for the standard `panic!`/`assert!`/`unwrap`
/// macros, or a `Box<dyn Any + Send>` otherwise).
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Features that the VM structurally cannot support.
const SKIP_FEATURES: &[&str] = &[
    "module",
    "eval",
    "SharedArrayBuffer",
    "Atomics",
    "resizable-arraybuffer",
    "Temporal",
    "Intl",
    "tail-call-optimization",
    "regexp-v-flag",
    "regexp-unicode-property-escapes",
];

// `raw`: must run with NO harness prepended (sta.js/assert.js included) — we
// always prepend, so running them would be wrong; skip honestly instead.
const SKIP_FLAGS: &[&str] = &["module", "async", "raw"];

/// Collapse a per-test `detail` string to a **coarse, stable** histogram key.
/// `detail` carries specifics (full parse diagnostics, raised condition names)
/// that are unique per test — bucketing on it directly explodes the histogram
/// into thousands of one-off entries and destroys the "by cause" signal. The
/// runtime/error-kind details are already coarse (a bounded `ErrorKind` set)
/// and pass through.
fn coarse_cause(detail: &str) -> String {
    if detail.starts_with("parse:") {
        "parse error".to_string()
    } else if detail.starts_with("semantic:") {
        "semantic error".to_string()
    } else if detail.starts_with("harness:") {
        "harness load error".to_string()
    } else if detail.starts_with("vm-init:") {
        "vm-init error".to_string()
    } else if detail.starts_with("unexpected raise:") {
        "unexpected raise".to_string()
    } else if detail.starts_with("panic:") {
        "panic".to_string()
    } else {
        // Already coarse: "runtime: TypeError", "out of fuel",
        // "expected error, got success", "panic (…)", "unexpected pending".
        detail.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestOutcome {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone)]
pub struct TestResult {
    pub path: String,
    pub outcome: TestOutcome,
    pub detail: String,
    pub features: Vec<String>,
}

#[derive(Debug, Default)]
pub struct RunStats {
    pub total: usize,
    pub pass: usize,
    pub fail: usize,
    pub skip: usize,
    pub by_cause: BTreeMap<String, usize>,
    pub by_feature: BTreeMap<String, usize>,
    pub results: Vec<TestResult>,
}

/// Packaged work for a worker thread — all strings, fully Send.
struct WorkItem {
    path: String,
    full_source: String,
    negative: Option<Negative>,
    features: Vec<String>,
    /// Parse in strict mode (test262 `onlyStrict`); non-strict script
    /// otherwise (`noStrict` and unflagged tests).
    strict: bool,
}

pub fn run_tests(
    test_root: &Path,
    local_harness: &Path,
    test262_harness: &Path,
    filter: Option<&str>,
    expectations: Option<&Expectations>,
) -> RunStats {
    let mut harness = Harness::new(local_harness.to_path_buf(), test262_harness.to_path_buf());
    let mut stats = RunStats::default();
    let started = Instant::now();
    install_panic_hook();

    // Collect test paths.
    let mut test_paths: Vec<PathBuf> = Vec::new();
    for entry in walkdir::WalkDir::new(test_root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.path().extension().is_some_and(|e| e == "js") {
            let rel = entry.path().strip_prefix(test_root).unwrap_or(entry.path());
            let rel_str = rel.to_string_lossy();
            if let Some(f) = filter
                && !rel_str.contains(f)
            {
                continue;
            }
            test_paths.push(entry.path().to_path_buf());
        }
    }
    test_paths.sort();

    // Build work items — main thread does all file I/O and source assembly.
    // Compilation + VM execution happen in worker threads.

    // Single-threaded work dispatch: compile/run in a worker thread to survive
    // stack overflows. We batch work to avoid spawning 50k threads.
    for path in &test_paths {
        stats.total += 1;
        let rel = path.strip_prefix(test_root).unwrap_or(path);
        let rel_str = rel.to_string_lossy().to_string();

        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                stats.skip += 1;
                stats.results.push(TestResult {
                    path: rel_str,
                    outcome: TestOutcome::Skip,
                    detail: format!("read error: {e}"),
                    features: vec![],
                });
                continue;
            }
        };

        let (fm, test_body) = match parse_frontmatter(&source) {
            Some(pair) => pair,
            None => {
                stats.skip += 1;
                stats.results.push(TestResult {
                    path: rel_str,
                    outcome: TestOutcome::Skip,
                    detail: "no frontmatter".to_string(),
                    features: vec![],
                });
                continue;
            }
        };

        // Feature/flag skip.
        let mut skip_reason: Option<String> = None;
        for feature in &fm.features {
            if SKIP_FEATURES.contains(&feature.as_str()) {
                skip_reason = Some(format!("skip feature: {feature}"));
                break;
            }
        }
        if skip_reason.is_none() {
            for flag in &fm.flags {
                if SKIP_FLAGS.contains(&flag.as_str()) {
                    skip_reason = Some(format!("skip flag: {flag}"));
                    break;
                }
            }
        }
        if skip_reason.is_none()
            && let Some(exp) = expectations
            && let Some(expected) = exp.entries.get(&rel_str)
        {
            match expected {
                ExpectedResult::Skip(reason) => {
                    skip_reason = Some(format!("expected skip: {reason}"));
                }
                ExpectedResult::KnownDivergence(reason) => {
                    skip_reason = Some(format!("known-divergence: {reason}"));
                }
                _ => {}
            }
        }
        if let Some(reason) = skip_reason {
            stats.skip += 1;
            stats.results.push(TestResult {
                path: rel_str,
                outcome: TestOutcome::Skip,
                detail: reason,
                features: fm.features,
            });
            continue;
        }

        // Assemble full source.
        let full_source = match harness.build_source(&fm.includes, test_body) {
            Ok(s) => s,
            Err(e) => {
                stats.fail += 1;
                *stats
                    .by_cause
                    .entry(coarse_cause(&format!("harness: {e}")))
                    .or_insert(0) += 1;
                stats.results.push(TestResult {
                    path: rel_str,
                    outcome: TestOutcome::Fail,
                    detail: format!("harness: {e}"),
                    features: fm.features,
                });
                continue;
            }
        };

        // Run in a worker thread with a large stack to survive compiler recursion.
        let strict = fm.flags.iter().any(|f| f == "onlyStrict");
        let work = WorkItem {
            path: rel_str,
            full_source,
            negative: fm.negative,
            features: fm.features,
            strict,
        };
        let work_path = work.path.clone();
        let work_features = work.features.clone();

        let result = (|| -> Result<TestResult, Box<dyn std::any::Any + Send>> {
            let handle = thread::Builder::new()
                .stack_size(WORKER_STACK)
                .spawn(move || run_work_item(work))
                .map_err(|e| -> Box<dyn std::any::Any + Send> {
                    Box::new(format!("spawn: {e}"))
                })?;
            handle.join()
        })();

        match result {
            Ok(r) => {
                match r.outcome {
                    TestOutcome::Pass => stats.pass += 1,
                    TestOutcome::Fail => {
                        stats.fail += 1;
                        *stats.by_cause.entry(coarse_cause(&r.detail)).or_insert(0) += 1;
                        if let Some(f) = r.features.first() {
                            *stats.by_feature.entry(f.clone()).or_insert(0) += 1;
                        }
                    }
                    TestOutcome::Skip => stats.skip += 1,
                }
                stats.results.push(r);
            }
            Err(payload) => {
                stats.fail += 1;
                let detail = format!("panic: {}", panic_message(&payload));
                *stats.by_cause.entry(coarse_cause(&detail)).or_insert(0) += 1;
                stats.results.push(TestResult {
                    path: work_path,
                    outcome: TestOutcome::Fail,
                    detail,
                    features: work_features,
                });
            }
        }
    }

    eprintln!(
        "Ran {} tests in {:.1}s: {} pass, {} fail, {} skip",
        stats.total,
        started.elapsed().as_secs_f64(),
        stats.pass,
        stats.fail,
        stats.skip,
    );
    stats
}

/// Executed inside a worker thread — all non-Send types (Program, VM, etc.)
/// are created and destroyed within this thread. Only String/TestResult cross
/// the thread boundary.
fn run_work_item(item: WorkItem) -> TestResult {
    CURRENT_TEST.with(|c| *c.borrow_mut() = Some(item.path.clone()));
    let program = match compile_for_test262(&item.full_source, item.strict) {
        Ok(p) => p,
        Err(diags) => {
            let first = diags.first();
            let msg = first
                .map(|d| d.render(&item.full_source))
                .unwrap_or_else(|| "compile error".to_string());
            // Bucket by phase: oxc parse error vs. our own semantic rejection
            // (undeclared global, unsupported feature, assignment to constant,
            // …). The distinction is what makes the failure histogram useful.
            let phase = match first.map(|d| d.kind) {
                Some(interp::DiagKind::Parse) => "parse",
                Some(interp::DiagKind::Semantic) => "semantic",
                None => "compile",
            };
            if std::env::var_os("PROBE_PARSE").is_some() {
                let phase_uc = phase.to_uppercase();
                let first_line = msg.split('\n').next().unwrap_or("");
                eprintln!("{phase_uc} {} :: {first_line}", item.path);
            }
            if let Some(neg) = &item.negative
                && neg.phase.as_deref() == Some("parse")
            {
                return TestResult {
                    path: item.path,
                    outcome: TestOutcome::Pass,
                    detail: "expected parse error".to_string(),
                    features: item.features,
                };
            }
            return TestResult {
                path: item.path,
                outcome: TestOutcome::Fail,
                detail: format!("{phase}: {msg}"),
                features: item.features,
            };
        }
    };

    let mut vm = match VM::for_program(program, serde_json::Value::Null) {
        Ok(vm) => vm,
        Err(e) => {
            return TestResult {
                path: item.path,
                outcome: TestOutcome::Fail,
                detail: format!("vm-init: {e:?}"),
                features: item.features,
            };
        }
    };

    match vm.step(STEP_FUEL) {
        Ok(StepResult::Done { .. }) => {
            if item.negative.is_some() {
                return TestResult {
                    path: item.path,
                    outcome: TestOutcome::Fail,
                    detail: "expected error, got success".to_string(),
                    features: item.features,
                };
            }
            TestResult {
                path: item.path,
                outcome: TestOutcome::Pass,
                detail: "ok".to_string(),
                features: item.features,
            }
        }
        Ok(StepResult::Pending { .. }) => TestResult {
            path: item.path,
            outcome: TestOutcome::Fail,
            detail: "unexpected pending".to_string(),
            features: item.features,
        },
        Ok(StepResult::Raise { condition, .. }) => TestResult {
            path: item.path,
            outcome: TestOutcome::Fail,
            detail: format!("unexpected raise: {condition}"),
            features: item.features,
        },
        Ok(StepResult::OutOfFuel) => TestResult {
            path: item.path,
            outcome: TestOutcome::Fail,
            detail: "out of fuel".to_string(),
            features: item.features,
        },
        Err(e) => {
            let error_name = format!("{:?}", e.kind);
            if let Some(neg) = &item.negative {
                let phase_match = match neg.phase.as_deref() {
                    Some("runtime") | None => true,
                    Some("parse") => false,
                    Some("early") => true,
                    _ => true,
                };
                if phase_match {
                    if let Some(expected) = &neg.error_type {
                        let matches = match expected.as_str() {
                            "TypeError" | "SyntaxError" | "ReferenceError" | "RangeError" => {
                                error_name == "TypeError"
                            }
                            "Test262Error" => error_name == "UncaughtException",
                            _ => error_name.contains(expected.as_str()),
                        };
                        if matches {
                            return TestResult {
                                path: item.path,
                                outcome: TestOutcome::Pass,
                                detail: format!("expected error: {error_name}"),
                                features: item.features,
                            };
                        }
                    } else {
                        return TestResult {
                            path: item.path,
                            outcome: TestOutcome::Pass,
                            detail: format!("expected runtime error: {error_name}"),
                            features: item.features,
                        };
                    }
                }
            }
            TestResult {
                path: item.path,
                outcome: TestOutcome::Fail,
                detail: format!("runtime: {error_name}"),
                features: item.features,
            }
        }
    }
}
