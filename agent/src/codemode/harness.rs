//! Part H's regression harness, live: run `tasks::ALL` against a real
//! `LiveSource` and report the three numbers Part H asks for — median
//! program length (statements), LLM round-trips per user request, and
//! task success — per task, plus the aggregate. Everything underneath
//! (`runner::run`, `tasks::Task`, `tasks::RecordingTools`) is already
//! unit-tested on scripted completions with no network; this module's
//! only job is wiring a real `LiveSource` in and aggregating, so it
//! has no tests of its own beyond what type-checks — a live model's
//! actual behavior is not the thing to script (Part H's own header
//! note: "Part G2's harness is not a test... a separate opt-in
//! binary, never part of `cargo test`" — same reasoning applies here).

use super::runner::{self, CompletionSource, LiveSource, RunConfig};
use super::tasks::Task;
use super::transport::Completion;
use super::{document::Document, fence};

/// Wraps a live source to capture what Part H actually wants counted
/// *and* what it says is the real instrument: the statement length of
/// every program the model produced during the run, in order, and the
/// program's own extracted source — aggregate numbers alone don't
/// explain a failure, reading the program does. A thin pass-through
/// otherwise — `run`'s own fence-stripping and truncation handling are
/// untouched, this just mirrors that same extraction on the side to
/// capture it.
struct RecordingSource<'a> {
    inner: LiveSource<'a>,
    lengths: Vec<usize>,
    programs: Vec<String>,
}

impl CompletionSource for RecordingSource<'_> {
    fn complete(&mut self, doc: &Document) -> Result<Completion, String> {
        let completion = self.inner.complete(doc)?;
        if !completion.was_truncated() && !completion.text.trim().is_empty() {
            let source = fence::extract(&completion.text);
            self.lengths.push(interp::count_statements(&source));
            self.programs.push(source);
        }
        Ok(completion)
    }
}

/// One task's result: whether its own success condition held, the
/// number of completions the run actually needed, the length
/// (statements) of each program along the way, and the programs
/// themselves — Part H's "reading transcripts is the real instrument"
/// bullet needs the text, not just the count.
pub struct TaskReport {
    pub task_name: &'static str,
    pub success: Result<(), String>,
    pub round_trips: usize,
    pub program_lengths: Vec<usize>,
    pub programs: Vec<String>,
}

/// Run one task live. `card` is the system prompt under test — passed
/// in, not read from `codemode::card::CARD` internally, so tuning runs
/// (Part H: "tune, and record what moved what") can pass a variant
/// without this module knowing anything changed.
pub fn run_task(
    task: &Task,
    card: &str,
    endpoint: super::transport::Endpoint<'_>,
    model: &str,
    max_tokens: u32,
) -> TaskReport {
    // Step C2: "the names and signatures are card surface" — a real
    // agent's card already bakes in its own configured tools; this
    // harness has one shared base card, so a task's tool manifest is
    // appended per run rather than duplicated into `codemode::card`
    // itself, which stays the harness-vocabulary-only base every task
    // shares (Step C4).
    let card = if task.tool_manifest.is_empty() {
        card.to_owned()
    } else {
        format!("{card}\n\n{}", task.tool_manifest)
    };
    let card = card.as_str();

    let tools = (task.tools)();
    let mut source = RecordingSource {
        inner: LiveSource {
            endpoint,
            model,
            max_tokens,
        },
        lengths: Vec::new(),
        programs: Vec::new(),
    };
    let run_config = RunConfig {
        exemplars: super::card::SEED_EXEMPLARS,
        ..RunConfig::default()
    };
    match runner::run(card, task.user_message, &tools, &mut source, &run_config) {
        Ok(outcome) => TaskReport {
            task_name: task.name,
            success: (task.check)(&outcome, &tools),
            round_trips: outcome.completions_used,
            program_lengths: source.lengths,
            programs: source.programs,
        },
        Err(e) => TaskReport {
            task_name: task.name,
            success: Err(format!("run did not complete: {e:?}")),
            round_trips: source.lengths.len(),
            program_lengths: source.lengths,
            programs: source.programs,
        },
    }
}

/// The middle of a sorted `Vec<usize>` — `None` on an empty run (a
/// task that made zero completions, which is itself worth surfacing
/// rather than reporting a fabricated median).
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
