//! The acceptance harness, live: run `tasks::ALL` (`+EXPERIMENTAL` on
//! `--experimental`) against a real `host::DeepSeekClient` and print the
//! numbers `docs/23_ONE_AGENT.md`'s Pass C asks for — round-trips,
//! program length (statements), task success — per task, plus the
//! aggregate. Everything underneath (`tasks::drive`, `tasks::Task`,
//! `tasks::make_sandbox`) is already unit-tested on scripted completions
//! with no network; this module's only job is wiring a live
//! `DeepSeekClient` in, giving each task a real sandbox directory, and
//! printing — so it has no tests of its own beyond what type-checks — a
//! live model's actual behavior is not the thing to script (see
//! `tasks.rs`'s own header).
//!
//! **This module has no notion of sandboxing the process.** Whatever
//! confinement `agent eval` runs under (see `scripts/eval.sh`) is a
//! property of how the binary was launched, decided before `main`
//! dispatches to `eval`; nothing here checks for it, requires it, or
//! knows it exists. Each task still gets a fresh, real, per-task
//! directory (`tasks::make_sandbox`) — that is ordinary fixture setup,
//! not a security boundary.
//!
//! Reached through `agent eval [--experimental]`, never `cargo test`.

use super::tasks::{Outcome, Task, UnscriptedAsk};
use crate::host;

/// One task's result: whether its own success condition held, plus
/// every number [`super::tasks::Outcome`] folded from the run. Nothing
/// here is stored independently of that fold — see `tasks.rs`'s own
/// header for why a hand-threaded counter would defeat the point of
/// this harness.
pub struct TaskReport {
    pub task_name: &'static str,
    pub success: Result<(), String>,
    pub round_trips: usize,
    /// Every `Call::Invoke` in the run. With `round_trips` this is
    /// **calls per program** — see `tasks::Outcome::tool_calls` for why
    /// that ratio, not the round-trip count alone, is the number the
    /// thesis claims.
    pub tool_calls: usize,
    pub program_lengths: Vec<usize>,
    pub programs: Vec<String>,
    pub raise_count: usize,
    pub trap_count: usize,
    pub resume_count: usize,
    pub abandon_count: usize,
    pub spawn_children: usize,
    pub appended: Vec<String>,
    /// Every `ask()` the drive loop answered with the harness's own
    /// fixed non-answer, because no fixture responder matched it — see
    /// `tasks::UnscriptedAsk`'s own doc. Printed unconditionally, not
    /// only on failure: an eval where a question got answered by the
    /// harness itself, silently, is one nobody could debug.
    pub unscripted_asks: Vec<UnscriptedAsk>,
    pub errors: Vec<String>,
    /// Wall clock for the whole task, end to end.
    ///
    /// The metric that can invalidate the others. Fewer completions is
    /// only a win if the user waits less: one completion that reads
    /// 190KB and thinks for ninety seconds is worse than twelve that
    /// take three, and a report of round trips alone would call that a
    /// twelve-fold improvement. Measured because the thing being
    /// optimised is somebody's waiting, not a count.
    pub elapsed: std::time::Duration,
}

impl TaskReport {
    fn from_outcome(task: &Task, outcome: Outcome, success: Result<(), String>) -> Self {
        TaskReport {
            // Set by `run_task`, which is the only thing that can time
            // the run; `from_outcome` sees only the finished log.
            elapsed: std::time::Duration::ZERO,
            task_name: task.name,
            success,
            round_trips: outcome.round_trips,
            tool_calls: outcome.tool_calls,
            program_lengths: outcome.program_lengths,
            programs: outcome.programs,
            raise_count: outcome.raise_count,
            trap_count: outcome.trap_count,
            resume_count: outcome.resume_count,
            abandon_count: outcome.abandon_count,
            spawn_children: outcome.spawn_children,
            appended: outcome.appended,
            unscripted_asks: outcome.unscripted_asks,
            errors: outcome.errors,
        }
    }
}

/// Run one task live against `llm`: a fresh real sandbox directory
/// (`tasks::make_sandbox`), a real `Session` driven through it
/// (`tasks::drive`), then `task.check` reading the same directory back
/// off disk.
pub fn run_task(task: &Task, llm: Box<dyn host::LlmClient>) -> TaskReport {
    let sandbox = super::tasks::make_sandbox(task);
    let started = std::time::Instant::now();
    let outcome = super::tasks::drive(task, sandbox.path(), llm);
    let elapsed = started.elapsed();
    let success = (task.check)(&outcome, sandbox.path());
    let mut report = TaskReport::from_outcome(task, outcome, success);
    report.elapsed = elapsed;
    report
}

/// The middle of a sorted `Vec<usize>` — `None` on an empty run (a task
/// that made zero completions, which is itself worth surfacing rather
/// than reporting a fabricated median).
pub fn median(values: &[usize]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        Some((sorted[mid - 1] + sorted[mid]) as f64 / 2.0)
    } else {
        Some(sorted[mid] as f64)
    }
}

/// `agent eval [--experimental]`'s whole job: run every fixed task (plus
/// the experimental set when asked), print each one's numbers as it
/// finishes, and close with the aggregate Pass C's bar is stated
/// against (`docs/23_ONE_AGENT.md`, C2).
pub fn run_cli(experimental: bool) -> Result<(), String> {
    let mut tasks: Vec<&Task> = super::tasks::ALL.iter().collect();
    if experimental {
        tasks.extend(super::tasks::EXPERIMENTAL.iter());
    }

    let mut reports = Vec::with_capacity(tasks.len());
    for task in &tasks {
        eprintln!("=== {} ===", task.name);
        let llm: Box<dyn host::LlmClient> = Box::new(host::DeepSeekClient::from_env()?);
        let report = run_task(task, llm);
        print_report(&report);
        reports.push(report);
    }
    print_summary(&reports);
    Ok(())
}

fn print_report(r: &TaskReport) {
    match &r.success {
        Ok(()) => println!("[{}] PASS", r.task_name),
        Err(e) => println!("[{}] FAIL: {e}", r.task_name),
    }
    println!(
        "  round-trips: {}  tool calls: {}  calls/program: {:.1}  wall: {:.1}s",
        r.round_trips,
        r.tool_calls,
        r.tool_calls as f64 / r.round_trips.max(1) as f64,
        r.elapsed.as_secs_f64(),
    );
    println!("  program lengths (statements): {:?}", r.program_lengths);
    println!(
        "  raise: {}  trap: {}  resume: {}  abandon: {}  spawned children: {}",
        r.raise_count, r.trap_count, r.resume_count, r.abandon_count, r.spawn_children
    );
    if !r.appended.is_empty() {
        println!("  appended: {:?}", r.appended);
    }
    if !r.unscripted_asks.is_empty() {
        println!("  unscripted ask(s) — harness answered, not a real user:");
        for a in &r.unscripted_asks {
            println!("    Q: {}", a.question);
            println!("    A (harness non-answer): {}", a.answer);
        }
    }
    if !r.errors.is_empty() {
        println!("  errors: {:?}", r.errors);
    }
    // Reading the program is the real instrument when a number alone
    // doesn't explain a failure (`tasks::Outcome::programs`'s own doc) —
    // printed on a failing task, so a passing run stays scannable, and
    // **also whenever a program trapped**. A task that traps, abandons
    // and rewrites still passes, and its round trips still count; the
    // first program is where the reason lives, and printing only on
    // failure hides exactly the case worth reading.
    if r.success.is_err() || r.trap_count > 0 {
        for (i, program) in r.programs.iter().enumerate() {
            println!("  --- program {i} ---\n{program}");
        }
    }
}

fn print_summary(reports: &[TaskReport]) {
    let passed = reports.iter().filter(|r| r.success.is_ok()).count();
    println!("\n=== summary ===");
    println!("{passed}/{} tasks passed", reports.len());
    let round_trips: Vec<usize> = reports.iter().map(|r| r.round_trips).collect();
    let mean_round_trips = if round_trips.is_empty() {
        0.0
    } else {
        round_trips.iter().sum::<usize>() as f64 / round_trips.len() as f64
    };
    println!("mean round-trips per task: {mean_round_trips:.2}");

    // The two numbers that keep the round-trip count honest.
    //
    // Calls per program is what the thesis actually claims: a tool loop
    // pays one completion per call, and the whole argument is that the
    // mechanical ones collapse into a program the model wrote ahead of
    // time. If this trends to 1, the founding complaint of
    // `20_CODE_MODE.md` has returned and round trips will not show it.
    //
    // Wall clock is what can invalidate the rest. Fewer completions is a
    // win only if somebody waits less; one completion that reads 190KB
    // and thinks for ninety seconds beats twelve three-second ones on
    // every count here except the one that matters.
    let total_calls: usize = reports.iter().map(|r| r.tool_calls).sum();
    let total_trips: usize = round_trips.iter().sum();
    println!(
        "calls per program: {:.1} ({total_calls} calls over {total_trips} programs)",
        total_calls as f64 / total_trips.max(1) as f64
    );
    let wall: f64 = reports.iter().map(|r| r.elapsed.as_secs_f64()).sum();
    println!(
        "wall clock: {wall:.1}s total, {:.1}s per task",
        wall / reports.len().max(1) as f64
    );
    if let Some(m) = median(&round_trips) {
        println!("median round-trips per task: {m}");
    }
    let all_lengths: Vec<usize> = reports
        .iter()
        .flat_map(|r| r.program_lengths.iter().copied())
        .collect();
    if let Some(m) = median(&all_lengths) {
        println!("median program length (statements): {m}");
    }
    let total_spawn_children: usize = reports.iter().map(|r| r.spawn_children).sum();
    println!("spawn_children across the run: {total_spawn_children}");
}

#[cfg(test)]
mod tests {
    use super::median;

    #[test]
    fn median_of_empty_is_none() {
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn median_of_odd_count_is_the_middle_value() {
        assert_eq!(median(&[1, 5, 3]), Some(3.0));
    }

    #[test]
    fn median_of_even_count_averages_the_middle_two() {
        assert_eq!(median(&[1, 2, 3, 4]), Some(2.5));
    }
}
