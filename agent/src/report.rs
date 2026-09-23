//! Condition + completion reports (8_HARNESS Step 4; 17_BRANCHES A4;
//! 23_ONE_AGENT.md A6).
//!
//! The harness's `Post` reply to a program's `Turn` is the product
//! surface of the whole project: it is what the LLM reads to decide what
//! program to write next. Under code mode that decision is never a pick
//! off a menu of restart schemas — a suspension gets a **handler
//! program**, a fresh completion whose `return` value *is* the restart
//! (`resume(value)`/`abandon()`), free to do anything else instead. So a
//! report's job is not to enumerate what's eligible; it is to **describe
//! what happened accurately enough that a program can be written against
//! it** (DESIGN.md, "The thesis").
//!
//! **Reports are derived, not stored.** Every report here is a pure
//! function of the log: [`derive_report`] takes `(&Tree, leaf, turn)` and
//! reads forward from that turn to its outcome — the source read
//! directly off the driving `Turn.source` (bare program text now, not
//! dug out of a tool-call arguments blob), the outcome, the `Console`,
//! and the `Result`s and menu rows on the path. No renderer touches a
//! `VM`.
//!
//! The gain is not disk. It is that a corpus of real logs can be
//! re-rendered with a *new* report format and diffed — the iteration this
//! project calls its product surface. The cost, stated plainly: the
//! rendered prefix is stable only **for a given renderer**. Editing this
//! file changes how an existing conversation re-renders; golden tests
//! exist to pin that deliberately rather than by accident.
//!
//! Every section carries a hard size bound (bounded reports, menu pruned
//! to recent entries, full data always fetchable by id).

use crate::types::{
    Author, Call, Event, EventId, EventPayload, Handback as HandbackHow, Origin, Outcome, Tree,
};

/// Max bytes of the "what happened" section (diagnostic + payload).
pub const WHAT_MAX_BYTES: usize = 2048;
/// Max bytes of a rendered condition payload (within the what section).
pub const PAYLOAD_MAX_BYTES: usize = 1024;
/// Max call-stack frames named in the where section (innermost kept).
pub const STACK_MAX_FRAMES: usize = 8;
/// Console lines quoted in a report (tail — the latest output before the
/// stop). The `Console` event itself keeps more; see [`CONSOLE_MAX_LINES`].
///
/// **A backstop, not the budget.** [`CONSOLE_SECTION_MAX_BYTES`] is the
/// real bound, and the note on it describes exactly this failure one
/// constant over: a clip that made the channel useless for what
/// programs reach for it to do, so the model routed around it. This was
/// 20, which binds long before 4 KB does — a 51-line Python file is
/// well inside the byte budget and came back as "the last 20 of 51
/// lines".
///
/// Live on 2026-09-20, and it cost the run its task. The model said
/// "I need the full files first — the earlier run only showed the
/// tail", printed them again, got the tail again, and then wrote
/// `history.note({ code, tests })` over both files — because
/// appending was the only way left to put a file in front of itself.
/// That is the copy the card forbids, produced by the harness leaving
/// no other door.
///
/// 200 with a 4 KB budget means bytes bind first for anything that
/// reads like code (~40 bytes a line), and the line cap only catches a
/// chatty loop printing something very short very often.
pub const CONSOLE_TAIL_LINES: usize = 200;
/// Lines a logged `Console` keeps. It is a **diagnostic stream, not
/// data** — a chatty loop can write megabytes — so it is capped with an
/// explicit truncation marker, and the program's own `return` is the
/// channel for anything that must survive whole.
pub const CONSOLE_MAX_LINES: usize = 2_000;
/// Bytes a logged `Console` keeps, across all its lines.
pub const CONSOLE_MAX_BYTES: usize = 256 * 1024;
/// Bytes the quoted console tail may occupy in a report, across all
/// its lines.
///
/// It was a flat 200-byte clip **per line**, which made the channel
/// useless for the thing programs actually reach for it to do: a
/// `console.log` of a 463-byte file came back cut at 200. The model
/// then used `tell` to look at content instead — where nothing comes
/// back at all — and re-read the same files next program. Half the
/// return value's budget, spent from the newest line backwards, so the
/// most recent output survives whole and an older chatty loop is what
/// gets dropped. Spent from the newest line backwards.
///
/// 4 KB, and it used to be written as half of a `RETURN_MAX_BYTES`
/// that no longer described anything: a reply has no `return` (D5),
/// and what a program hands on it hands on through `history.note`.
/// A number standing on a deleted idea reads as though it were derived
/// from something.
/// **Sized against the document budget, and it moved.** 4096 was
/// 6.25% of `DEFAULT_DOCUMENT_BUDGET`'s 64 KB, which was proportionate.
/// The budget is now the model's own window (`provider::CONTEXT_WINDOWS`,
/// capped at `DEFAULT_MAX_DOCUMENT_TOKENS`), and 4 KB of that is under
/// one percent — so a reply could see four kilobytes of whatever it had
/// read, however much it read, and the only way through a large file
/// was to read it again.
///
/// Measured on 2026-09-22: one task re-read `machine.rs` eight times,
/// `report.rs` seven and `transcript.rs` five, spending 24 replies and
/// never reaching the edit. Twenty of its seventy-one results were over
/// the cap. The comment on `render_console` below records the same
/// failure from a `sweep-40` run without connecting it to the size.
pub const CONSOLE_SECTION_MAX_BYTES: usize = 32 * 1024;
/// Artifact-menu entries shown (most recent kept; older ids stay valid).
pub const MENU_MAX_ENTRIES: usize = 20;
/// Max bytes of the annotated program source in a post-condition report.
pub const ANNOTATED_SOURCE_MAX_BYTES: usize = 4096;
/// Calls named on one annotated source line before it says "and N more".
pub const ANNOTATIONS_PER_LINE: usize = 6;
/// Per-entry preview bytes in the artifact menu.
pub const PREVIEW_MAX_BYTES: usize = 256;

/// Bytes of an appended row shown before it says how much is left.
///
/// **The row is the view; `fetch` is the value.** `history.note` was
/// the one visible thing in the system with no bound on it, and by
/// bytes it is how models actually read: 72% of everything appended
/// across 352 kept runs was a verbatim copy of a result, against a card
/// rule forbidding it in bold. A rule with 28% compliance is not being
/// disobeyed — it is the only door, and it was rendering whole on every
/// turn until something compacted it. One row reached 38,342 bytes.
///
/// 4 KB because of the distribution, not a guess: appended rows run to
/// a median of 450 bytes and a p90 of 3,759, so this leaves 90% of them
/// untouched — every genuine conclusion — and bounds only the
/// file-shaped ones, halving the bytes appended across the corpus.
///
/// Clipped at render and never on the log, like every other bound here,
/// so one `history.note(f.content)` is a bounded view *and* a whole
/// value that `history.fetch` still hands back.
/// Kept equal to [`CONSOLE_SECTION_MAX_BYTES`]: printing a thing and
/// appending it are the two ways to put it in front of the next reply,
/// and a model choosing between them should not be choosing a budget.
pub const NOTE_ROW_MAX_BYTES: usize = CONSOLE_SECTION_MAX_BYTES;

/// Max bytes of a prose segment a reply sends to the person. The reply
/// itself keeps every byte on the log (28 — the parts concatenate back
/// to it); this bounds only what is **delivered**, like
/// `strip_imitated_markers`. A degenerate reply of repeated text must
/// not land on the person in full.
pub const PROSE_MAX_BYTES: usize = 16 * 1024;

/// One artifact-menu entry: a `Call` (settled or still pending) or a
/// `ProgramResult`, named by its event id and fetchable via
/// `fetch_history(id)`.
pub struct Artifact {
    pub id: u64,
    /// Read from the call variant: `ask(to, "…")` / `tell(to, "…")`,
    /// `spawn(name)`, `name(args-preview)`, or `program result`.
    pub label: String,
    pub state: ArtifactState,
}

/// What the menu says about a row, and whether it can be fetched.
#[derive(Debug)]
pub enum ArtifactState {
    /// A `Result` landed with a value.
    Delivered(serde_json::Value),
    /// A `Result` landed saying it definitively did not happen.
    Failed(String),
    /// A `Send` with no `Result`: the answer is still coming, and the row
    /// is **re-attachable** — awaiting it by id is the correct move, never
    /// re-asking.
    PendingSend,
    /// An `Invoke`/`Spawn` with no `Result`: issued, and whether it
    /// happened is not knowable from the log. Not re-attachable.
    PendingInvoke,
    /// **The row's content is the row.** A note, a `tell`, an `ask`, an
    /// `answer`: nothing was fetched and nothing came back, so there is
    /// no "→ ok, 433 bytes" to print — the words are the whole fact, and
    /// they render verbatim.
    ///
    /// These used to live in `document.rs` as loose lines above the
    /// report while the calls lived in a menu below it, so one run's
    /// doings were split across two lists in two places with the
    /// outcome wedged between them. They are the same kind of fact —
    /// a row this run added — so they are one list, in id order.
    Whole(String),
}

/// The **where** section: where the program stopped.
///
/// A raise or a trap stopped at a point, so the honest answer is the
/// call-stack chain. A *post* stopped it nowhere in particular — the VM
/// is parked between slices — so the honest answer is the whole program
/// with its progress marked, which is also what turns a rewrite into a
/// copy-edit rather than a reconstruction.
pub enum Whence {
    /// Call-stack function names, outermost first.
    Stack(Vec<String>),
    /// The program source, every call site annotated by its artifact.
    AnnotatedSource(String),
}

/// The harness `Post` for a raise/trapped error/arrival — the report a
/// program's branch gets back when its run didn't return.
///
/// There is no separate "restarts" section any more: what `resume(value)`
/// would mean for *this* suspension (if anything) is a fact about the
/// suspension, not a menu of eligible next calls, so it is folded
/// straight into [`what`](Self::what) by [`what_happened`] — the reader
/// is writing a handler program, not choosing a tool schema.
pub struct ConditionReport {
    /// The heading this report opens with — [`RUN_HEADING`] for a run
    /// that got somewhere, [`NO_RUN_HEADING`] for one that did not, and
    /// [`PART_RUN_HEADING`] for the case that had no heading of its own
    /// until now: a reply whose earlier blocks ran and whose next one
    /// would not compile.
    pub heading: &'static str,
    /// Rendered diagnostic: condition name + payload, or the trapped
    /// error with source line and caret — plus, inline, what
    /// `resume(value)` means here or why it doesn't apply.
    pub what: String,
    pub whence: Whence,
    /// Full console log (the renderer tails it).
    pub console: Vec<String>,
    /// The `Console` event the tail comes from, named when it clips.
    pub console_id: Option<u64>,
    /// The artifacts this handback added, oldest first — the same bound
    /// the completion report uses, so the reports on a branch partition
    /// its artifacts instead of each one re-listing its predecessor's.
    /// The trailing line says how many exist in total and their id range.
    pub artifacts: Vec<Artifact>,
    /// Rows this run appended that hold bytes already on the log — see
    /// [`CompletionReport::copied_rows`], and [`copied_note`] for why
    /// this is on both shapes.
    pub copied_rows: Vec<(u64, u64)>,
}

impl ConditionReport {
    pub fn render(&self) -> String {
        let mut sections = vec![self.heading.to_owned(), clip(&self.what, WHAT_MAX_BYTES)];
        // `### where it stopped` earns its place only when it says
        // something the diagnostic did not. A trap's `what` already
        // carries the failing line with a caret under it, so a stack of
        // nothing but `<root>` is a heading, a newline and the word
        // "root" spent to repeat it — the same empty scaffolding
        // `HandbackHow::Compaction` was carved out of this report for, four
        // runs ago.
        match &self.whence {
            Whence::Stack(stack) if !stack_is_bare(stack) => {
                sections.push(format!("### where it stopped\n{}", render_stack(stack)));
            }
            Whence::AnnotatedSource(source) => {
                sections.push(format!("### where it stopped\n{}", fenced(source, "js")));
            }
            Whence::Stack(_) => {}
        }
        let rows: Vec<&Artifact> = self.artifacts.iter().collect();
        sections.extend(render_rows(&rows));
        sections.extend(render_console(&self.console, self.console_id));
        sections.extend(copied_note(&self.copied_rows));
        sections.join("\n\n")
    }
}

/// A stack that names no frame the model did not already know it was
/// in: empty, or a single synthetic frame for the top level its code
/// was already running at.
///
/// **Two names, not one.** `<root>` is what a whole-program run calls
/// its top frame; a notebook cell's has no debug info at all and comes
/// back as `<unknown>` (`interp`'s `Frame::name`). Only the first was
/// listed, so every trap under the notebook transport rendered
///
/// ```text
/// ### where it stopped
/// in <unknown>
/// ```
///
/// — the heading, a newline and a word that repeats what the caret
/// above already said, which is the exact scaffolding this function
/// exists to suppress. Seen in a live `glm-5.3` run, 2026-09-19.
fn stack_is_bare(stack: &[String]) -> bool {
    stack.is_empty() || matches!(stack, [only] if only == "<root>" || only == "<unknown>")
}

#[cfg(test)]
mod bare_stack_tests {
    use super::{diagnostic, fenced, stack_is_bare};

    /// **A wrong location is worse than none.** A site of zero means
    /// nobody knows where — an instruction whose span sits in the
    /// prelude region rebases to it — and rendering it as a location
    /// put `1:1:` and a caret under the first line of the reply, which
    /// is the model's own opening sentence. 11 of 121 diagnostics in
    /// the corpus pointed at prose that way.
    #[test]
    fn an_unknown_site_renders_no_location_at_all() {
        let reply =
            "Dead-code hunting in `helpers.py` — first, the repo.\n\n```js\nx.map(f);\n```\n";
        let out = diagnostic(reply, 0, "`map` was called on undefined");
        assert_eq!(out, "`map` was called on undefined");
        assert!(!out.contains("1:1"), "no invented line: {out}");
        assert!(
            !out.contains("Dead-code"),
            "and no caret under prose: {out}"
        );

        // A site it does know still renders whole.
        let at = reply.find("x.map").unwrap() as u32;
        let out = diagnostic(reply, at, "`map` was called on undefined");
        assert!(out.contains("x.map(f);"), "the real line is shown: {out}");
        assert!(out.contains('^'), "with its caret: {out}");
    }

    /// A console entry is one `console.log` call's output, not one
    /// line, and most end with a newline — which used to print a blank
    /// line under almost every `### it printed`.
    #[test]
    fn a_fenced_block_has_no_blank_line_before_its_closing_fence() {
        assert_eq!(fenced("a\nb\n", "text"), "```text\na\nb\n```");
        assert_eq!(fenced("a\nb", "text"), "```text\na\nb\n```");
        // Blank lines *inside* are the program's own output and stay.
        assert_eq!(fenced("a\n\nb", "text"), "```text\na\n\nb\n```");
    }

    /// Both synthetic top frames are bare — `<root>` for a whole
    /// program, `<unknown>` for a notebook cell, whose frame carries no
    /// debug info at all.
    #[test]
    fn a_lone_synthetic_top_frame_says_nothing_worth_a_heading() {
        for name in ["<root>", "<unknown>"] {
            assert!(stack_is_bare(&[name.to_owned()]), "{name}");
        }
        assert!(stack_is_bare(&[]));
    }

    /// A frame the model actually wrote is not bare, and neither is a
    /// synthetic one with a real frame under it.
    #[test]
    fn a_named_frame_earns_the_heading() {
        assert!(!stack_is_bare(&["checkShipping".to_owned()]));
        assert!(!stack_is_bare(&[
            "<unknown>".to_owned(),
            "checkShipping".to_owned()
        ]));
    }
}

/// The harness `Post` for a program that finished with a `return`.
///
/// **The return value is rendered whole, and it is the only thing here
/// that is** (27.7). This used to get the same 256-byte [`preview`] a
/// menu row gets, on the rule that nothing enters a context unchosen
/// (DESIGN.md "No exception") — sound while a `return` was read by
/// nobody, because it was then just another artifact. 27.1 made it the
/// channel the whole design runs on: a program returns, the next one is
/// written, and that value is what it is written from.
///
/// Measured 2026-09-17, before this. On `ambiguous-config`, a program
/// returned `{question, content: <the file>}`; the next one saw 256
/// bytes of it, said *"Reading the whole file — the last look was cut
/// off"*, and read the file again. So did the one after that. Three
/// programs re-fetching what the first had already handed them, each
/// one behaving perfectly reasonably given what it could see.
///
/// The "unchosen" rule is not violated by this and never was: the
/// author of the value and the reader of the report are the same mind
/// one turn apart, and the author picked it deliberately over
/// everything else it was holding. That is the definition of chosen.
/// The document now has exactly one generous channel and it is that
/// one — the menu is an index (27.2), a call's arguments are clipped,
/// a result is a size, and what a program hands on it hands on through
/// `history.note` — which is a row of its own and compacts like one.
pub struct CompletionReport {
    /// What a top-level `return` handed back, if anything.
    ///
    /// **The one thing a program says to its own next reply.** It is
    /// the channel `stop(reason)` used to be: a check came back wrong,
    /// the program ended there, and this is what it said about it.
    pub returned: Option<serde_json::Value>,
    /// Full console log (the renderer tails it).
    pub console: Vec<String>,
    /// The `Console` event the tail comes from, named when it clips.
    pub console_id: Option<u64>,
    /// Artifacts logged since the run started (its `ProgramResult`
    /// included), oldest first.
    pub new_artifacts: Vec<Artifact>,
    /// How many of this run's calls came back `Failed`.
    ///
    /// The risk the "only handbacks log a condition" rule leaves is
    /// **silent degradation**: a program that swallows five failures and
    /// returns a thin result, with a completion report that reads as
    /// success. The fix belongs in the report, not in a new event — so
    /// the report counts them and says so.
    pub failed_calls: usize,
    /// Rows this run appended whose bytes were already on the log, as
    /// `(the note, what it copied)`.
    ///
    /// **72% of everything appended across 96 kept runs was already
    /// there** — 79 KB carried twice, in 19 runs. The card says it in
    /// bold ("Not the bytes of something you read") and the worked
    /// example that broke the rule has been fixed, and a run on
    /// 2026-09-20 still opened with `history.note({cargoToml:
    /// cargoToml.content, lib: lib.content, fmt: fmt.content, …})` over
    /// three files it had read in the same program. Prose in the card
    /// is 16 KB from the decision; this is next to it.
    ///
    /// Exact byte equality only, and only for strings big enough to
    /// matter, so there is nothing to be wrong about.
    pub copied_rows: Vec<(u64, u64)>,
    /// Bytes of the longest `bash` command this run issued, when it was
    /// long enough to be a script rather than a pipeline. Zero
    /// otherwise. See `host::tools`'s command ceiling for why this is a
    /// note and no longer a refusal.
    pub long_bash: usize,
}

impl CompletionReport {
    pub fn render(&self) -> String {
        // **A reply that ended says so, and says what it returned.**
        // A program that ran off its end returned nothing and this is
        // one line; one that ended itself on a failed check put the
        // reason here, which is the whole of what it said to this
        // reply.
        let ended = match &self.returned {
            Some(v) => format!(
                "It ended with `return`, and this is what it returned:\n\n{}",
                clip(
                    &serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()),
                    PAYLOAD_MAX_BYTES
                )
            ),
            None => "It completed.".to_owned(),
        };
        let mut sections = vec![RUN_HEADING.to_owned(), ended];
        let rows: Vec<&Artifact> = self.new_artifacts.iter().collect();
        sections.extend(render_rows(&rows));
        sections.extend(render_console(&self.console, self.console_id));
        let mut out = sections.join("\n\n");

        if self.failed_calls > 0 {
            out.push_str(&format!(
                "\n\n### calls that failed\n{} of this run's calls came back failed. If your \
                 result reflects that, say so; if the program swallowed them, this report \
                 is not the success it looks like. Each failure's reason is fetchable by \
                 id from the rows above.",
                self.failed_calls
            ));
        }

        if self.long_bash > 0 {
            out.push_str(&format!(
                "\n\n### worth knowing\n\nThat was a {}-byte `bash` command — a script rather \
                 than a pipeline. It ran, and if it was the right tool then it was the \
                 right tool. Worth knowing for next time: the same logic written in the \
                 program keeps its values in variables you can use in the later calls \
                 and return at the end, and a mistake in it stops at a line rather than \
                 somewhere inside a heredoc. The 30s and 4MB limits apply to the whole \
                 script either way.",
                self.long_bash
            ));
        }

        if let Some(note) = copied_note(&self.copied_rows) {
            out.push_str("\n\n");
            out.push_str(&note);
        }

        if self.wrote_without_verifying() {
            out.push_str(
                "\n\n### worth knowing\n\nThis program wrote files but didn't check them. Don't report \
                 success unverified — run the thing that would fail (`bash` build/test, or \
                 `parse_errors` on what you wrote). And read the `diff` the write handed \
                 back: it says where the edit landed, for free, where re-reading the file \
                 costs a call. Next time, fold the check into the same program that does \
                 the writing, not a separate one.",
            );
        }
        out
    }

    /// Whether this run created or replaced files but never inspected
    /// them in the same program — the nudge condition: validation of a
    /// write belongs in the program that wrote it, not a follow-up turn
    /// (the split-validation habit the card warns against).
    fn wrote_without_verifying(&self) -> bool {
        let mut wrote = false;
        let mut verified = false;
        for a in &self.new_artifacts {
            let l = &a.label;
            if l.starts_with("create_file(") || l.starts_with("replace_file(") {
                wrote = true;
            } else if l.starts_with("parse_errors(")
                || l.starts_with("outline(")
                || l.starts_with("read_file(")
                || (l.starts_with("bash(") && looks_like_build(l))
            {
                verified = true;
            }
        }
        wrote && !verified
    }
}

/// Build/test-ish `bash` commands count as verifying a write; setup
/// commands (mkdir, cp, mv, touch) do not. Best-effort over the clipped
/// args preview — a missed match only yields a soft, advisory nudge.
fn looks_like_build(label: &str) -> bool {
    const TOKENS: [&str; 14] = [
        "build", "test", "lint", "check", "tsc", "node ", "cargo", "npm", "pnpm", "yarn", "pytest",
        "python", "make", "eslint",
    ];
    TOKENS.iter().any(|t| label.contains(t))
}

// ── section renderers (each enforces its own bound) ─────────────────

/// **The heading every run reports under.** One phrase, whatever
/// happened: a program that returned, one that raised, one that trapped
/// and one that a post suspended are all *this reply's code, run* —
/// what differs is the sentence under it, not the kind of thing being
/// reported. The document has three heading levels and this is the
/// middle one (`document.rs`'s `NEW EVENTS` is the turn, `###` below
/// are the parts of the run), so nesting is readable from the markup
/// alone rather than from having learned which of five `##` headings
/// belong together.
pub const RUN_HEADING: &str = "## RAN YOUR PROGRAM";

/// A run that never reached a VM: a cell that would not compile, or a
/// completion cut off mid-program. Saying `RAN YOUR PROGRAM` over
/// either would be false in the one way that matters — nothing ran, so
/// nothing below it is a consequence.
pub const NO_RUN_HEADING: &str = "## YOUR PROGRAM DID NOT RUN";

/// The heading for a reply whose earlier blocks ran and whose next one
/// would not compile. Neither of the other two is true of it, and
/// saying the nearer of the two wrong things cost a live run its task —
/// see the `CellFailed` arm of [`render_handback`].
pub const PART_RUN_HEADING: &str = "## YOUR PROGRAM RAN, THEN A BLOCK DID NOT COMPILE";

/// Wrap text in a fence **long enough to survive its own content**.
///
/// Console output is arbitrary bytes: in a live run on 2026-09-18 a
/// program printed two Python files straight into the document, and
/// nothing said where the output stopped and the next section began. A
/// fence says it — and it is the convention the card already teaches
/// (a block fenced anything but `js` is quoted, not run), so it costs
/// the reader nothing new to learn.
///
/// The fence grows past the longest backtick run inside, which is what
/// CommonMark requires and what keeps a printed markdown file from
/// closing the block early.
fn fenced(body: &str, tag: &str) -> String {
    // **No blank line before the closing fence.** A console entry is
    // one `console.log` call's output, not one line, and most end with
    // a newline of their own — so adding the fence's newline printed a
    // stray blank line under almost every `### it printed` in the
    // system. Display only: the bytes are on the log untouched.
    let body = body.trim_end_matches('\n');
    let longest = body
        .as_bytes()
        .split(|b| *b != b'`')
        .map(<[u8]>::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}{tag}\n{body}\n{fence}")
}

fn render_stack(stack: &[String]) -> String {
    if stack.is_empty() {
        return "in (no live frames)".into();
    }
    if stack.len() > STACK_MAX_FRAMES {
        let omitted = stack.len() - STACK_MAX_FRAMES;
        format!(
            "in … ({omitted} outer frames omitted) → {}",
            stack[omitted..].join(" → ")
        )
    } else {
        format!("in {}", stack.join(" → "))
    }
}

/// The console tail. **Every clip names a fetchable id**, not just a
/// count: the tail was the one truncation in the system with no way back
/// to the whole, so when it clips it names its `Console` event and
/// `fetch_history()` reads that event's lines.
fn render_console(lines: &[String], event: Option<u64>) -> Option<String> {
    if lines.is_empty() {
        // Nothing printed is not news. This used to render
        // `console: (no output)` on every report in the system,
        // including the ones whose program never called `print`.
        return None;
    }
    // Newest-first until the budget runs out, then back into order.
    let mut start = lines.len().saturating_sub(CONSOLE_TAIL_LINES);
    let mut used = 0usize;
    for (i, line) in lines.iter().enumerate().skip(start).rev() {
        used += line.len() + 1;
        if used > CONSOLE_SECTION_MAX_BYTES {
            start = i + 1;
            break;
        }
    }
    // **The budget may never take the last line.** The loop above walks
    // newest-first and stops at the first line that does not fit, so a
    // single line larger than the whole budget stopped it on the first
    // step: `start` became `lines.len()` and the section rendered "The
    // last 0 of 3 lines" over an empty fence. Found 2026-09-20 in a
    // `sweep-40` run whose program printed a 4,531-byte `outline` — the
    // one thing it had asked for — and was shown none of it; its next
    // two replies were spent fetching that row and re-reading the file.
    //
    // One line always survives, and `clip` below trims it to the
    // budget. A truncated answer is worth two round trips; nothing is
    // not, and "0 of 3" reads as though the program printed nothing.
    let start = start.min(lines.len() - 1);
    let shown = &lines[start..];
    // **Count the lines only when some were dropped.** "last 3 of 3
    // lines" announces a clip that did not happen, and it was every
    // single one: measured 2026-09-18, 31 of 31 console sections under
    // `Transport::Notebook` and 28 of 28 under `Transport::Program` read
    // "N of N". A heading that always says the same thing is one the
    // reader learns to skip, and this one heads the output the program
    // just produced.
    let mut out = if shown.len() == lines.len() {
        "### it printed\n".to_owned()
    } else {
        let mut out = format!(
            "### it printed\nThe last {} of {} lines",
            shown.len(),
            lines.len()
        );
        // Only a clip names the id — an untruncated tail has nothing
        // behind it to fetch.
        if let Some(id) = event {
            out.push_str(&format!("; `history.fetch({id})` for all of them"));
        }
        out.push_str(".\n");
        out
    };
    let body: Vec<String> = shown
        .iter()
        .map(|line| clip(line, CONSOLE_SECTION_MAX_BYTES))
        .collect();
    out.push_str(&fenced(&body.join("\n"), "text"));
    Some(out)
}

/// The rows this run added, in id order — its calls, its notes and the
/// words it sent, one list. `None` when it added none: a heading
/// advertising `history.fetch(id)` over the word `(none)` is an
/// invitation to fetch nothing.
fn render_rows(artifacts: &[&Artifact]) -> Option<String> {
    render_row_list("### rows it added", artifacts)
}

/// [`render_rows`] under a caller's own heading — [`render_fork`] lists
/// a branch's rows so far under one of its own, and the two must format
/// a row identically or the same fact reads as two different kinds of
/// thing depending on which report carried it.
/// One row of the menu as the model reads it — the label, then what is
/// known about how it turned out.
///
/// Split out of [`render_row_list`] so a test can ask what the model is
/// told about a single row without rebuilding the heading around it;
/// the two must not drift, so there is one of them.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn render_row(a: &Artifact) -> String {
    const PREAMBLE: &str = "`history.fetch(id)` for any of them.\n";
    let whole = render_row_list("", std::slice::from_ref(&a)).unwrap_or_default();
    // Everything after the heading the list puts above it — a row can
    // be several lines (a clipped one carries a footer saying how much
    // is left), so this is not "the last line".
    match whole.split_once(PREAMBLE) {
        Some((_, rows)) => rows.trim_start_matches('\n').trim_end().to_owned(),
        None => whole.trim().to_owned(),
    }
}

fn render_row_list(heading: &str, artifacts: &[&Artifact]) -> Option<String> {
    if artifacts.is_empty() {
        return None;
    }
    let mut out = format!("{heading}\n`history.fetch(id)` for any of them.\n");
    let start = artifacts.len().saturating_sub(MENU_MAX_ENTRIES);
    if start > 0 {
        out.push_str(&format!(
            "\n({start} older rows omitted; their ids stay fetchable)"
        ));
    }
    for a in &artifacts[start..] {
        let tail = match &a.state {
            // **A delivered value is never shown here.** This menu is an
            // index of history rows, not a replay of them: the row's
            // label says what the call was, its id fetches the value
            // whole, and fetching logs nothing — so a program reads what
            // it wants without inflicting it on the program after it.
            //
            // Measured 2026-09-17, before this: one `dead-code-sweep`
            // run's menu was 9,901 bytes of a 36,821-byte document, the
            // largest thing in it after the card, and not one byte of it
            // had been chosen by anybody. A return value did not replace
            // it either — `CompletionReport::render` emits the value and
            // then the menu regardless — so a program that distilled its
            // findings handed the next writer the distillation *and* the
            // firehose, with the firehose winning on volume.
            //
            // The size stays, because it is what a reader needs in order
            // to decide whether the fetch is worth a round trip.
            ArtifactState::Delivered(v) => delivered_tail(v, &a.label),
            // A failure keeps its text. It is exactly the thing nobody
            // chose and everybody needs: the only place the reason
            // appears, and unlike a result it is not fetchable under its
            // own id — `outcome_json` turns a `Failed` into a rejection,
            // not a value.
            ArtifactState::Failed(msg) => format!("failed: {}", clip(msg, PREVIEW_MAX_BYTES)),
            ArtifactState::PendingSend => {
                format!("pending — await history.fetch({})", a.id)
            }
            ArtifactState::PendingInvoke => "issued; no result recorded; may have happened".into(),
            // The words *are* the row, so there is no `label → value`
            // to draw: the label already says who said what, and an
            // arrow after it would point at nothing.
            ArtifactState::Whole(text) => {
                out.push_str(&format!("\n- `[{}]` {}", a.id, text));
                continue;
            }
        };
        out.push_str(&format!("\n- `[{}]` `{}` → {}", a.id, a.label, tail));
    }
    Some(out)
}

/// What a delivered row says about itself: that it arrived, and how
/// big it is. Never what it contains — see [`render_menu`].
///
/// A scalar is the exception, and only because it is smaller than any
/// description of it: `→ 0` costs less than `→ ok, 1 byte` and tells a
/// reader strictly more. The rule is about not replaying *payloads*,
/// not about withholding numbers.
fn delivered_tail(v: &serde_json::Value, label: &str) -> String {
    // **A write that changed nothing is news.** `replace_file` reports
    // it by *omitting* `diff`, which is the weakest signal in the
    // system: a field that is not there. A `sweep-40` run on
    // 2026-09-20 computed a cleaned file, wrote back bytes identical
    // to what was already on disk, read no `diff`, and told the person
    // it had deleted the dead helpers. All 24 were still there. Five
    // writes in 323 changed nothing and two of their runs failed.
    //
    // Keyed on the label rather than the shape, because `create_file`
    // also returns a bare `{version}` and a new file is not "no
    // change".
    if label.starts_with("replace_file(")
        && v.get("version").is_some()
        && v.get("diff").is_none_or(serde_json::Value::is_null)
    {
        return format!("no change, {{version}}, {} bytes", v.to_string().len());
    }
    match v {
        serde_json::Value::Null => "ok".into(),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => v.to_string(),
        serde_json::Value::String(s) if s.len() <= 24 => format!("{s:?}"),
        // **The shape, never the contents.** A scalar row already
        // shows its value; a structured one showed only a size, so
        // `read_file(…) → ok, 31045 bytes` read as *a 31045-byte
        // thing*. On 2026-09-19 a run fetched exactly that row and
        // called `.slice(0, 2000)` on the `{ content, version }` it got
        // back. Naming the keys is constant-size however big the value
        // is, and it is what the row was already implying.
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&str> = map
                .keys()
                .take(SHAPE_MAX_KEYS)
                .map(String::as_str)
                .collect();
            if map.len() > SHAPE_MAX_KEYS {
                keys.push("…");
            }
            // **The status, because it is the number that varies.**
            // `bash` is the one tool whose result carries a verdict as
            // well as a value, the card opens its description with
            // "Read `status` before `stdout`", and the row said `ok,
            // {status, stdout, stderr}` either way — the field names,
            // which the card already declares, in place of the one
            // thing a reader could not predict. 105 of 1,178 bash
            // calls in the kept corpus exited non-zero and not one row
            // mentioned it; the only way to find out was to spend a
            // `fetch` on a row that looked exactly like the 1,073 that
            // had nothing to report.
            //
            // The number, not a verdict on it: 42 of those 105 are a
            // `grep` that matched nothing, where 1 is an ordinary
            // answer and the card says so ("Non-zero is a result, not
            // an error"). `status 1` is worth reading in both cases
            // and wrong in neither; "failed" would be wrong in 40% of
            // them.
            //
            // Silent when it is zero, like every other count in this
            // file: a line that says the same thing on every row is
            // one the reader learns to skip.
            //
            // **And a value that says `ok: false` is not led with the
            // word "ok".** `parse_errors` answers `{ok, errors}`, so a
            // failed syntax check rendered as `→ ok, {ok, errors}, N
            // bytes` — the row's own first word contradicting the
            // field beside it. Nothing in the kept corpus hit it (74
            // calls, every one of them `ok: true`), which is why it
            // survived; the rare case is the one the row exists for.
            let lead = match map.get("status").and_then(serde_json::Value::as_i64) {
                Some(n) if n != 0 => format!("status {n}"),
                _ if map.get("ok") == Some(&serde_json::Value::Bool(false)) => "not ok".to_owned(),
                _ => "ok".to_owned(),
            };
            format!(
                "{lead}, {{{}}}, {} bytes",
                keys.join(", "),
                v.to_string().len()
            )
        }
        serde_json::Value::Array(items) => format!(
            "ok, [{} item{}], {} bytes",
            items.len(),
            if items.len() == 1 { "" } else { "s" },
            v.to_string().len()
        ),
        other => format!("ok, {} bytes", other.to_string().len()),
    }
}

#[cfg(test)]
fn delivered_tail_t(v: &serde_json::Value) -> String {
    delivered_tail(v, "bash(\"x\")")
}

/// Keys named in a menu row's shape before it gives up and says `…`.
/// A row is one line; a wide object must not make it three.
const SHAPE_MAX_KEYS: usize = 5;

// ── rendered messages ───────────────────────────────────────────────

/// Max bytes of a post's rendered `input` preview.
pub const INPUT_PREVIEW_MAX_BYTES: usize = 512;
/// Object keys / array entries named in an `input` preview.
pub const INPUT_PREVIEW_MAX_KEYS: usize = 12;

/// Render a `Post` into the text an LLM sees: its own event id (a chat
/// message otherwise carries no id anywhere the model can read — the
/// `open on this branch: #N` tail note, and `answer(question, value)`,
/// were referencing ids with no way to resolve them to content until
/// this), the body, author-labelled when it did not come from the
/// person driving the session, plus a **bounded preview** of any
/// machine-bound `input`.
///
/// The preview is the whole point: the full value reaches the *program*
/// as the `input` const, so rendering it in full would dump a caller's
/// data into the callee's context — exactly what by-reference travel
/// exists to prevent.
pub fn render_post(id: EventId, from: Author, origin: &Origin) -> String {
    let Some((text, input, _)) = origin.direct() else {
        // An unresolved reference should never reach a renderer: a
        // `Context` materialises bodies. Say so rather than render a lie.
        return "(message body unavailable)".to_owned();
    };
    let mut out = format!("[{}] ", id.as_u64());
    match from {
        Author::User => {}
        Author::Harness => out.push_str("[harness] "),
        Author::Agent(id) => out.push_str(&format!("[agent {}] ", id.as_u64())),
    }
    out.push_str(text);
    // A `choose` asked of an agent: the options are part of the
    // question. Without them the recipient is being asked to pick from
    // a set it cannot see, and every `answer` it tries is refused.
    let options = origin.options();
    if !options.is_empty() {
        out.push_str("\n\npick one, and answer with it exactly: ");
        out.push_str(
            &options
                .iter()
                .map(|o| format!("{o:?}"))
                .collect::<Vec<_>>()
                .join(" / "),
        );
    }
    if !input.is_null() {
        out.push_str("\n\ninput: ");
        out.push_str(&input_preview(input));
    }
    out
}

/// Max bytes of a nameless branch's derived navigator label.
#[allow(dead_code)] // caller returns in Pass D: only `debug/` used this,
// and the TUI is cut from the build for Passes A-C.
pub const DERIVED_LABEL_MAX_BYTES: usize = 40;

/// A nameless branch's display label (17_BRANCHES "a nameless branch is
/// displayed, not renamed"): the first line of its own first post — at
/// or after its root, so a fork's label is about what makes *it*
/// different rather than the shared prefix — clipped short. Computed at
/// render time, logged nowhere, and superseded the moment anyone names
/// the branch. `None` when this branch has posted nothing of its own yet
/// (a fork born idle, say), in which case the navigator falls back to
/// the branch id.
#[allow(dead_code)] // caller returns in Pass D: only `debug/` used this,
// and the TUI is cut from the build for Passes A-C.
pub fn derived_branch_label(tree: &Tree, branch: EventId, leaf: EventId) -> Option<String> {
    tree.path_events(leaf).into_iter().find_map(|e| {
        if e.id.as_u64() < branch.as_u64() {
            return None; // pre-root: the shared prefix, not this branch's own
        }
        let EventPayload::Post { origin, .. } = &e.payload else {
            return None;
        };
        // The bare body, not `render_post`'s rendering: a display label
        // wants the words, not the `[#id]`/author decoration a request
        // needs.
        let (text, _, _) = origin.direct()?;
        let first_line = text.lines().next().unwrap_or("").trim();
        if first_line.is_empty() {
            return None;
        }
        Some(clip_short(first_line, DERIVED_LABEL_MAX_BYTES))
    })
}

/// Byte-bounded truncation with a bare ellipsis — for a display label,
/// where [`clip`]'s "[truncated; N bytes total]" marker would be most of
/// the label.
pub fn clip_short(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// A bounded, *shape-first* preview of machine-bound data: what kind it
/// is, which keys it has, how big it is — never the value itself.
pub fn input_preview(v: &serde_json::Value) -> String {
    let bytes = v.to_string().len();
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            let omitted = keys.len().saturating_sub(INPUT_PREVIEW_MAX_KEYS);
            keys.truncate(INPUT_PREVIEW_MAX_KEYS);
            let mut shape = format!("object, {} keys: {}", map.len(), keys.join(", "));
            if omitted > 0 {
                shape.push_str(&format!(", … ({omitted} more)"));
            }
            format!("{{{shape}}} ({bytes} bytes) — bound whole as `input`")
        }
        serde_json::Value::Array(items) => format!(
            "[array, {} items] ({bytes} bytes) — bound whole as `input`",
            items.len()
        ),
        serde_json::Value::String(text) => format!(
            "{} ({bytes} bytes) — bound whole as `input`",
            clip(
                &serde_json::Value::String(text.clone()).to_string(),
                INPUT_PREVIEW_MAX_BYTES
            )
        ),
        // Scalars are smaller than any description of them.
        other => other.to_string(),
    }
}

// ── helpers ─────────────────────────────────────────────────────────

/// Compact single-line preview of a JSON value, clipped to
/// [`PREVIEW_MAX_BYTES`].
pub fn preview(v: &serde_json::Value) -> String {
    clip(&v.to_string(), PREVIEW_MAX_BYTES)
}

/// The byte budget for *one* argument in a menu row's label.
pub const LABEL_ARG_MAX_BYTES: usize = 48;

/// The budget for a whole label, bounding a call with many arguments.
pub const LABEL_MAX_BYTES: usize = 160;

/// A call's arguments as a menu row shows them.
///
/// Each argument is clipped on its own, so the one that *identifies*
/// the call survives a huge one standing beside it:
/// `replace_file("src/lib.rs", "pub fn …")` still says which file, where
/// clipping the joined string would have spent the whole budget on the
/// path and lost the rest. The budgets are small on purpose — a label
/// is an index entry, and `replace_file(["src/lib.rs", "<the whole
/// file>"])` is a result in all but name (see [`render_menu`]).
pub fn arg_preview(args: &serde_json::Value) -> String {
    let joined = match args {
        serde_json::Value::Array(items) => items
            .iter()
            .map(|v| clip_short(&v.to_string(), LABEL_ARG_MAX_BYTES))
            .collect::<Vec<_>>()
            .join(", "),
        other => clip_short(&other.to_string(), LABEL_ARG_MAX_BYTES),
    };
    clip_short(&joined, LABEL_MAX_BYTES)
}

/// Byte-bounded truncation with an explicit marker; respects char
/// boundaries.
pub fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated; {} bytes total]", &s[..end], s.len())
}

// ── derivation: reports as pure functions of the log ────────────────

/// Bump when the rendered format changes. The report memo is a cache of
/// *one* renderer's output, so a change drops it wholesale.
pub const REPORT_FORMAT_VERSION: u32 = 2;

/// Cap a program's console for the log: a diagnostic stream, not data.
/// Keeps the **tail** (the latest output before the stop) and replaces
/// what it drops with a marker naming how much went, so the truncation is
/// never silent.
pub fn cap_console(lines: &[String], event_hint: &str) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    for line in lines.iter().rev() {
        if kept.len() >= CONSOLE_MAX_LINES || bytes + line.len() > CONSOLE_MAX_BYTES {
            break;
        }
        bytes += line.len();
        kept.push(line.clone());
    }
    kept.reverse();
    let dropped = lines.len() - kept.len();
    if dropped > 0 {
        kept.insert(
            0,
            format!("[console truncated: {dropped} earlier lines dropped; {event_hint}]"),
        );
    }
    kept
}

/// Cap prose a reply sends to the person: a message, not data. Keeps
/// the **head** (a message's opening is what a person reads first) and
/// marks where it was cut — the truncation is never silent, the same
/// rule [`cap_console`] applies to a program's prints.
pub fn cap_prose(text: &str) -> String {
    clip(text, PROSE_MAX_BYTES)
}

/// One handback's slice of a branch's path: the reply that drove it,
/// the handback itself, and the events in between.
struct Handback<'t> {
    /// The reply, verbatim — its parts concatenated (28), which is the
    /// coordinate system every `site` on the path is an offset into.
    source: String,
    /// The `Handback` event this report is derived from.
    outcome: &'t Event,
    /// The `Console` logged with the outcome, if any, and its event id —
    /// which the tail names when it clips, so the rest is fetchable.
    console: Vec<String>,
    console_id: Option<u64>,
    /// The path up to `leaf`, for the artifact menu.
    path: Vec<&'t Event>,
    /// Index of `turn` within `path`.
    turn_at: usize,
    /// Index of `outcome` within `path`. The menu stops here: a report
    /// must render identically **forever**, so a historical one cannot
    /// grow new rows as the branch continues past it.
    outcome_at: usize,
    /// The previous outcome's id on this path, or 0 at the agent root.
    /// The menu's **lower** bound: every artifact then appears in
    /// exactly one report, with no gaps and no repetition, instead of
    /// every condition report re-listing its predecessor's rows.
    previous_outcome: u64,
    /// The log, for resolving a `Post` whose body lives in its `Send`.
    /// Nothing here reads *live* state: only events, by id.
    tree: &'t Tree,
}

/// A reply's own text: the parts between it and its end, concatenated.
/// Prose and cells only — thinking is on the log and is not what the
/// model said.
pub(crate) fn reply_source(path: &[&Event], reply_at: usize) -> String {
    let mut out = String::new();
    for e in &path[reply_at + 1..] {
        match &e.payload {
            EventPayload::Part { part, .. } => match part {
                crate::types::Part::Prose(t) | crate::types::Part::Cell(t) => out.push_str(t),
                crate::types::Part::Thinking(_) => {}
            },
            EventPayload::Reply | EventPayload::Restart => break,
            _ => {}
        }
    }
    out
}

/// The handback(s) a reply produced, in log order, ending before the
/// next reply. A reply **pauses** any number of times and **ends**
/// once (28), so this returns every one of them — a `Vec`, so a caller
/// scanning a path never needs a special case at the boundary.
pub fn outcomes_of_turn(tree: &Tree, leaf: EventId, turn: EventId) -> Vec<EventId> {
    let path = tree.path_events(leaf);
    let Some(at) = path.iter().position(|e| e.id == turn) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for event in &path[at + 1..] {
        if matches!(event.payload, EventPayload::Reply | EventPayload::Restart) {
            break;
        }
        if matches!(event.payload, EventPayload::Handback { .. }) {
            out.push(event.id);
        }
    }
    out
}

/// Assemble the inputs one report needs, all from the log.
fn handback<'t>(tree: &'t Tree, leaf: EventId, outcome: EventId) -> Option<Handback<'t>> {
    let path = tree.path_events(leaf);
    let at = path.iter().position(|e| e.id == outcome)?;
    // Where this report's menu starts: the outcome before it, so the
    // reports on a branch partition its artifacts rather than each one
    // re-rendering the last one's rows.
    let previous_outcome = path[..at]
        .iter()
        .rposition(|e| matches!(e.payload, EventPayload::Handback { .. }))
        .map(|i| path[i].id.as_u64())
        .unwrap_or(0);
    // The turn this outcome belongs to: the nearest `Turn` above it.
    let turn_at = path[..at]
        .iter()
        .rposition(|e| matches!(e.payload, EventPayload::Reply | EventPayload::Restart))?;
    // **The source is the reply**, not one cell (28): its parts
    // concatenated, which is the coordinate system every `site` on the
    // path is already an offset into.
    let source = reply_source(&path, turn_at);
    // The `Console` logged with this outcome sits immediately after it,
    // before the next outcome.
    let (console, console_id) = path[at + 1..]
        .iter()
        .take_while(|e| !matches!(e.payload, EventPayload::Handback { .. }))
        .find_map(|e| match &e.payload {
            EventPayload::Console { lines } => Some((lines.clone(), Some(e.id.as_u64()))),
            _ => None,
        })
        .unwrap_or_default();
    Some(Handback {
        tree,
        source,
        outcome: path[at],
        console,
        console_id,
        path: path.clone(),
        turn_at,
        outcome_at: at,
        previous_outcome,
    })
}

/// Render the harness `Post` answering the `Turn` that produced
/// `outcome`. Memoised on the `Tree` by the outcome's id — the report is
/// a pure function of the log, so the same outcome always renders the
/// same string for a given renderer.
///
/// `budget` is still threaded through from the caller's configured
/// per-agent answer budget (`machine.rs`'s `DEFAULT_ANSWER_BUDGET`), but
/// only [`answer_ack`] still spends it — [`CompletionReport`] does not
/// (see its own doc: no return value gets a budgeted copy any more). It
/// stays a parameter here rather than being threaded away, since
/// `machine.rs`/`host/mod.rs`/`tree.rs` call this across the whole tree
/// and narrowing the signature is not this step's job.
pub fn derive_report(tree: &Tree, leaf: EventId, outcome: EventId, budget: usize) -> String {
    if let Some(hit) = tree.memoised_report(outcome) {
        return hit;
    }
    let handback = handback(tree, leaf, outcome);
    let text = match &handback {
        Some(h) => render_handback(h, budget),
        None => "(no outcome recorded for this call)".to_owned(),
    };
    // **A report holding a live `peek` is not final yet.** Everything
    // else here is a pure function of the log up to `outcome`, which is
    // what makes the memo safe and the prefix immutable. A peeked row
    // is the one thing that is not: it shows for exactly one request
    // and then stops, so the report it sits in renders one way now and
    // another way after the next reply, and memoising the first would
    // freeze it there forever.
    //
    // It costs one uncached suffix, once, and only for the request that
    // spends the peek — a peek expires on the very next reply, so what
    // changes is always near the end of the document. That is the trade
    // the verb *is*: a row you do not go on paying for.
    if !handback.is_some_and(|h| holds_a_live_peek(&h)) {
        tree.memoise_report(outcome, text.clone());
    }
    text
}

/// Whether this report shows a `peek` that has not been spent yet — see
/// [`derive_report`], which declines to memoise when it has.
fn holds_a_live_peek(h: &Handback<'_>) -> bool {
    h.path[..=h.outcome_at]
        .iter()
        .filter(|e| e.id.as_u64() > h.previous_outcome)
        .any(|e| {
            matches!(
                &e.payload,
                EventPayload::Render {
                    mode: crate::types::RenderMode::Peeked,
                    ..
                }
            ) && !h
                .path
                .iter()
                .any(|l| l.id > e.id && matches!(l.payload, EventPayload::Reply))
        })
}

/// The "you copied a row" advisory, shared by both report shapes.
///
/// **On both, because the rows were added either way.** It lived only
/// on the completion report at first, and the run that prompted it
/// appended four copies in a program that ended in a `ReferenceError`
/// — so the one report it would have helped was the one shape it did
/// not appear on. The same mistake as `CellFailed` hiding the work a
/// reply had already done, one field over.
fn copied_note(rows: &[(u64, u64)]) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    // Grouped by the note, so four sources read as one sentence
    // rather than four.
    let mut by_note: Vec<(u64, Vec<u64>)> = Vec::new();
    for (note, src) in rows {
        match by_note.iter_mut().find(|(n, _)| n == note) {
            Some((_, srcs)) => srcs.push(*src),
            None => by_note.push((*note, vec![*src])),
        }
    }
    let pairs = by_note
        .iter()
        .map(|(note, srcs)| {
            let list = srcs
                .iter()
                .map(|s| format!("`[{s}]`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("`[{note}]` holds the bytes of {list}")
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some(format!(
        "### worth knowing\n\n{pairs}. A call's result is already kept — \
         `history.fetch(id)` hands it back whole, from the log, for nothing — so a \
         copy of it is a second charge on every turn from here for something you \
         already had. Keep the id. What is worth a row of its own is what you \
         concluded from those bytes."
    ))
}

/// How long a string has to be before carrying it twice is worth a
/// word. Shorter than a `read_file` of anything real, longer than any
/// conclusion worth appending.
const COPY_MIN_BYTES: usize = 400;

/// Notes this run appended that hold bytes already on the log — see
/// [`CompletionReport::copied_rows`].
///
/// Exact equality on string leaves, so there is nothing to be wrong
/// about: either those bytes are on the log twice or they are not. The
/// earliest matching result wins, because that is the one whose id the
/// model should have kept.
fn copied_rows(h: &Handback<'_>) -> Vec<(u64, u64)> {
    fn strings(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::String(s) if s.len() >= COPY_MIN_BYTES => out.push(s.clone()),
            serde_json::Value::Array(xs) => xs.iter().for_each(|x| strings(x, out)),
            serde_json::Value::Object(m) => m.values().for_each(|x| strings(x, out)),
            _ => {}
        }
    }
    // Everything a result on this branch has already delivered, keyed
    // by the bytes and valued by the *call* id — which is the id a row
    // advertises and a program reuses. A map rather than a scan: this
    // runs on every completed program, and comparing every note's
    // strings against every result's is quadratic in a conversation's
    // length for no reason.
    let mut delivered: std::collections::HashMap<String, u64> = Default::default();
    for ev in h.path.iter() {
        if let EventPayload::Result {
            call,
            outcome: crate::types::Outcome::Delivered(v),
        } = &ev.payload
        {
            let mut found = Vec::new();
            strings(v, &mut found);
            for s in found {
                // The earliest delivery of these bytes is the id the
                // program should have kept.
                delivered.entry(s).or_insert(call.as_u64());
            }
        }
    }
    let mut out = Vec::new();
    for ev in &h.path[h.turn_at + 1..=h.outcome_at] {
        let EventPayload::Note { value, .. } = &ev.payload else {
            continue;
        };
        let mut mine = Vec::new();
        strings(value, &mut mine);
        // **Every source, not the first.** A note holding four files
        // holds four results, and naming one of them understates what
        // it cost — seen on `sweep-8` at HEAD, where `[18]` carried
        // `[9]`, `[10]`, `[11]` and `[12]` and the report said `[10]`.
        let mut seen = Vec::new();
        for m in &mine {
            if let Some(src) = delivered.get(m)
                && !seen.contains(src)
            {
                seen.push(*src);
                out.push((ev.id.as_u64(), *src));
            }
        }
    }
    out
}

fn render_handback(h: &Handback<'_>, budget: usize) -> String {
    let _ = budget;
    let EventPayload::Handback {
        how, site, stack, ..
    } = &h.outcome.payload
    else {
        return "(not an outcome)".to_owned();
    };
    match how {
        // A rested program reports like any other completion: this is
        // only reached when something later woke the branch — a new
        // question from the person, which the report is there to hand
        // the previous program's rows to.
        HandbackHow::Completed { value, .. } => CompletionReport {
            returned: value.clone(),
            console: h.console.clone(),
            console_id: h.console_id,
            new_artifacts: menu_since(h, h.previous_outcome),
            copied_rows: copied_rows(h),
            long_bash: h.path[h.turn_at + 1..=h.outcome_at]
                .iter()
                .filter_map(|e| match &e.payload {
                    EventPayload::Call(crate::types::Call::Invoke { name, args, .. })
                        if name == "bash" =>
                    {
                        args.get(0).and_then(|a| a.as_str()).map(str::len)
                    }
                    _ => None,
                })
                .filter(|n| *n > crate::host::tools::BASH_COMMAND_LONG_BYTES)
                .max()
                .unwrap_or(0),
            failed_calls: h.path[h.turn_at + 1..=h.outcome_at]
                .iter()
                .filter(|e| {
                    matches!(
                        &e.payload,
                        EventPayload::Result {
                            outcome: crate::types::Outcome::Failed(_),
                            ..
                        }
                    )
                })
                .count(),
        }
        .render(),
        // **A cell that would not compile built no VM — and the cells
        // before it in the same reply did.**
        //
        // This arm used to be the diagnostic and nothing else, under
        // "YOUR PROGRAM DID NOT RUN", on the reasoning that a cell
        // which does not compile has no console, no rows and no stack.
        // True of that cell. The blocks of one reply are one program
        // that pauses between them, so by the time the third one fails
        // to compile the first two have run, made their calls and
        // appended their rows — and the report threw all of it away
        // and told the model nothing had happened.
        //
        // Live on 2026-09-20, `dead-code-sweep`: a reply read both
        // source files, appended two rows holding them and ran a
        // `cargo check`, then hit `\`lib\` is already declared` in a
        // later block. Six rows on the log, and a 474-byte report
        // saying the program did not run. The next reply started the
        // task from the beginning — re-reading both files — and got it
        // wrong.
        HandbackHow::CellFailed { message } => {
            let artifacts = menu_since(h, h.previous_outcome);
            let ran_something = !artifacts.is_empty() || !h.console.is_empty();
            ConditionReport {
                heading: if ran_something {
                    PART_RUN_HEADING
                } else {
                    NO_RUN_HEADING
                },
                what: message.clone(),
                // The failing cell has no frames; its diagnostic
                // already carries the line and the caret.
                whence: Whence::Stack(Vec::new()),
                console: h.console.clone(),
                console_id: h.console_id,
                artifacts,
                copied_rows: copied_rows(h),
            }
            .render()
        }
        _ => ConditionReport {
            heading: RUN_HEADING,
            what: what_happened(h, how, *site),
            // A post stopped the reply nowhere in particular: the useful
            // "where" is the whole reply with its progress marked, which
            // is what a rewrite copy-edits.
            whence: match how {
                HandbackHow::Posted { .. } => Whence::AnnotatedSource(annotated_source(h)),
                _ => Whence::Stack(stack.clone()),
            },
            console: h.console.clone(),
            console_id: h.console_id,
            artifacts: menu_since(h, h.previous_outcome),
            copied_rows: copied_rows(h),
        }
        .render(),
    }
}

/// [`HandbackHow::Compaction`]'s report — written to be unmistakable.
///
/// The whole conversation is in front of the model already, and re-
/// sending it is nearly free: it is the cache prefix, 86-96% of those
/// prompt tokens are cache hits, and one more completion on the end of
/// it costs almost nothing. So this does not replace the document or
/// summarise it. It is the last thing in it, and its only job is to be
/// impossible to read as a suggestion.
///
/// That job is harder than it sounds. Three live runs on 2026-09-16
/// read a politely-worded version of this and did the unfinished task
/// instead — the last of them opening with "picking up mid-task —
/// card.rs was read but its summary never came back". The pull of
/// visible unfinished work beat a paragraph asking for something else,
/// which is the same thing that happened to every other paragraph
/// written this week. So the register here is a stop, not a request:
/// the run is *blocked*, and the only thing that unblocks it is a
/// compaction program.
/// `fixed` is how much of `measured` is the preamble — the card and
/// the worked examples — which no compaction program can touch.
///
/// **Without it the number is the wrong job.** Live on 2026-09-20 the
/// directive read "35552 Bytes against a 42000-Byte budget", of which
/// 27797 was the card: the model read "free 4,000 of 35,552", which is
/// eleven percent and sounds easy, and wrote a program that removed
/// eight rows. The real ask was 4,000 of a 7,668-byte conversation,
/// which is more than half of it, and needed the reply's own blocks
/// and its console gone rather than a tidy-up. It fired again with 87
/// bytes freed.
///
/// `None` where the measure is tokens: the split is known in bytes and
/// there is no tokenizer here to convert it, and a number in the wrong
/// unit is worse than no number.
/// The heaviest addressable entries in a rendered document, as
/// `(id, bytes, replies_ago)`, biggest first.
///
/// **The one fact a compaction program cannot see.** It is shown a menu
/// of thirty rows and its own replies above them, and nothing in either
/// says which of them is the 4 KB one: a call's row reads `→ ok, 210
/// bytes` about a result that is not rendered at all, a note's row is
/// clipped at 32 KB without saying where in that range it falls, and a
/// block of its own reply has no size on it anywhere. So it picks by
/// what it remembers being long, which is not the same list.
///
/// **Report-only, and best effort.** It attributes every byte of the
/// rendered conversation to the last id it saw announced — a `↓
/// history[N]` line above a block, or a `- \`[N]\`` menu row — which is
/// how the document announces them and therefore what the model is
/// reading too. A byte before the first id belongs to nothing and is
/// dropped. Nothing acts on this; it tells the program where to look.
pub(crate) fn heaviest_rows(
    doc: &crate::document::Document,
    ages: &std::collections::HashMap<u64, usize>,
    want: usize,
) -> Vec<(u64, usize, usize)> {
    let mut weight: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    for message in doc.conversation() {
        // A message boundary ends whatever block was open: the next
        // one's bytes are not the last one's, whatever it announced.
        let mut current: Option<u64> = None;
        for line in message.content.split_inclusive('\n') {
            if let Some(id) = announced_id(line) {
                current = Some(id);
            }
            if let Some(id) = current {
                *weight.entry(id).or_default() += line.len();
            }
        }
    }
    let mut rows: Vec<(u64, usize, usize)> = weight
        .into_iter()
        .map(|(id, bytes)| (id, bytes, ages.get(&id).copied().unwrap_or(0)))
        .collect();
    // Biggest first, and by id where two are the same size, so the line
    // is the same line twice for the same document.
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    rows.truncate(want);
    rows
}

/// The id a line announces, if it announces one: `↓ history[12]` above
/// a block, or `- \`[12]\`` at the head of a menu row.
fn announced_id(line: &str) -> Option<u64> {
    let rest = match line.trim_start().strip_prefix(crate::document::BLOCK_ARROW) {
        Some(rest) => rest.strip_prefix(" history[")?,
        None => line.trim_start().strip_prefix("- `[")?,
    };
    let close = rest.find(|c: char| !c.is_ascii_digit())?;
    rest[..close].parse().ok()
}

/// One line naming the heaviest entries and how long each has sat
/// there, for the compaction directive to carry. Empty when there is
/// nothing worth naming.
pub(crate) fn heaviest_line(rows: &[(u64, usize, usize)]) -> String {
    // Under a line of text is not worth a reader's attention, and a
    // list of four such rows is worse than no list: it says "these are
    // the ones" about entries whose removal frees nothing.
    let named: Vec<String> = rows
        .iter()
        .filter(|(_, bytes, _)| *bytes >= 512)
        .map(|(id, bytes, age)| {
            let ago = match age {
                0 => "this reply".to_owned(),
                1 => "1 reply ago".to_owned(),
                n => format!("{n} replies ago"),
            };
            format!("#{id} {:.1} KB, {ago}", *bytes as f64 / 1024.0)
        })
        .collect();
    if named.is_empty() {
        return String::new();
    }
    format!(
        "\n\n**The heaviest entries, and how long each has been in front of you:** {}. \
         Size and age are not the same reason to drop something — an entry you read once \
         and have been carrying for eleven replies is the cheap one to lose, and the big \
         one you were just handed may be the task.",
        named.join(", ")
    )
}

pub(crate) fn compaction_message(
    measured: usize,
    limit: usize,
    unit: crate::types::Measure,
    fixed: Option<usize>,
    heaviest: &str,
) -> String {
    let noun = unit.noun();
    // Built from the constant rather than spelled out, because a
    // directive that names a marker the renderer no longer emits is a
    // lie told at the worst possible moment — see the forgery guard,
    // which matched a row shape nothing emitted for the same reason.
    let arrow = crate::document::BLOCK_ARROW;
    // What is actually on the table, when that is knowable.
    let share = match fixed {
        Some(fixed) if fixed < measured => format!(
            " Of that, {fixed} is the card and the worked examples, which do not change: the \
             conversation itself is {}, and it is the only thing that can get smaller.",
            measured - fixed
        ),
        _ => String::new(),
    };
    format!(
        "## STOP — THIS CONVERSATION IS FULL\n\n\
         {measured} {noun}s against a {limit}-{noun} budget.{share} **The task above is not being \
         worked on in this program.** No tool call, no answer and no continuation of it will \
         be accepted from here; the only thing that can happen next is that the history gets \
         smaller.\n\n\
         Write a compaction program. Nothing else. It resumes the work by itself once you \
         return.\n\n\
         `history.remove(id)` shows nothing for that entry from here on, and \
         `history.remove(from, to)` does the same for every entry in an inclusive range. \
         `history.replace(id, text)` shows `text` in its place instead — and `text` can be \
         a piece of what is already there rather than a summary of it, `(await \
         history.fetch(id)).slice(0, 500)`, where writing the summary would cost you more \
         than the 500 characters. Everything carrying \
         an id above can be named — a report, a note, a post, any single line of a \
         report's menu, and **any block of any reply you have written**, which is what the \
         `{arrow} history[12]` line above a block is for. Anything else you name is simply \
         skipped, and everything you name that does exist still applies.\n\n\
         Your own blocks are usually the cheapest thing to drop: a program that has \
         already run sits directly above the report saying what it did, and the report is \
         the part worth keeping.\n\n\
         This card and the worked examples before the conversation carry no id, so they \
         cannot be named and are not yours to shrink.\n\n\
         An entry shown as `[id] … text` is already standing in for something longer. \
         Shortening *that* summarises a summary, and what goes first is the detail that \
         made it useful — a run on 2026-09-17 did it four times over and lost the one fact \
         that told it which test framework the project used. If such an entry still needs \
         shortening, `history.fetch(id)` returns what it replaced: write the new version \
         from the original, not from the summary. Otherwise leave it alone.\n\n\
         Prefer removing outright and keeping the rest verbatim; replace only what is worth \
         keeping a shorter version of, and spend the words on what you concluded rather \
         than on saying something was removed.\n\n\
         **Remove what you could get again; rewrite what you could not.** A copied file, a \
         listing, a command's output: removing it costs nothing, because the thing itself \
         is still where you read it. A conclusion is different. Nothing else holds it, and \
         once it is out of view you will not know there is anything to fetch. Those are \
         the entries to `replace` with a shorter version rather than remove.\n\n\
         **Nothing is lost by this.** Removing an entry takes it out of what you are shown, \
         never off the log: `history.fetch(id)` still returns it whole, so an id you keep \
         is an id you can still read. So drop whatever you judge least useful to have in \
         front of you from here, which is a judgement only you can make: you are the one \
         who has read this conversation. The task itself is never a target.{heaviest}\n\n\
         Return when you are done. Do not do anything else."
    )
}

/// The "what happened" diagnostic, rebuilt from the logged cause, the
/// logged site, and the source read off the driving `Turn.source` — the
/// three inputs that used to live only in the VM.
///
/// Folds in, inline, what `resume(value)` would mean for *this*
/// suspension (or why it doesn't apply) — there is no separate "restarts"
/// menu any more (DESIGN.md "The thesis": a suspension gets a handler
/// *program*, not a pick off a schema list), so this is simply one more
/// fact about what happened, stated where the reader is already looking.
/// `X is not defined`, where `X` **was** defined — in a reply that has
/// already ended.
///
/// The card says it ("Nothing else crosses between replies — least of
/// all your variables") and a model reaching for `f` in this reply
/// because it bound `f` in the last one is not reading the card at that
/// moment; it is reading a report. Two of the eight `is not defined`
/// traps across 96 kept runs were exactly this — `tests` and `f`, both
/// bound one reply earlier.
///
/// **Only when it can be proved**, which is what keeps it free of false
/// positives: the declaration has to be findable in an earlier reply's
/// own source. A name that was never bound anywhere is an ordinary
/// typo and gets the ordinary message.
fn bound_in_an_earlier_reply(h: &Handback<'_>, message: &str) -> Option<String> {
    let name = message.strip_suffix(" is not defined")?;
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let mut earlier = None;
    for (at, ev) in h.path.iter().enumerate() {
        if !matches!(ev.payload, EventPayload::Reply | EventPayload::Restart) {
            continue;
        }
        if at >= h.turn_at {
            break;
        }
        let src = reply_source(&h.path, at);
        if ["const", "let", "var", "function"]
            .iter()
            .any(|kw| src.contains(&format!("{kw} {name}")))
        {
            earlier = Some(ev.id.as_u64());
        }
    }
    let id = earlier?;
    Some(format!(
        "\n\n`{name}` was bound in an earlier reply — the one at `[{id}]` — and nothing \
         crosses between replies but the record. Bind it again in this one, from what the \
         rows above hand back."
    ))
}

fn what_happened(h: &Handback<'_>, cause: &HandbackHow, site: u32) -> String {
    let source = &h.source;
    match cause {
        HandbackHow::Raised { name, payload } => {
            let mut what = diagnostic(source, site, &format!("condition `{name}` raised"));
            what.push_str("\npayload: ");
            let rendered = payload
                .as_ref()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "(none)".into());
            what.push_str(&clip(&rendered, PAYLOAD_MAX_BYTES));
            what.push_str(
                "\n\nWrite a reply that decides it. `history.note(resume(value))` \
                 continues past the raise with `value` becoming the result of the \
                 `raise(...)` expression; `history.note(abandon())` gives up on it. \
                 Or do neither and handle this some other way — a reply has no \
                 `return`, so appending the decision is how you make it.",
            );
            what
        }
        // **A decision, rendered as one.** A trap is something going
        // wrong that the program did not foresee; this is the program
        // foreseeing it and saying so. Rendering them alike taught that
        // stopping yourself is a kind of failure, which is the opposite
        // of what it is — and it is why `tell(…); tell("done."); finish()` on a failed
        // check looked like the tidier option.
        HandbackHow::Trapped {
            message, resumable, ..
        } => {
            let mut what = diagnostic(source, site, message);
            if let Some(line) = bound_in_an_earlier_reply(h, message) {
                what.push_str(&line);
            }
            what.push_str(if *resumable {
                "\n\nresume(value) continues as if the failed operation had produced \
                 `value`."
            } else {
                "\n\nnot resumable — resume() will not stand in for this; write a program \
                 that recovers a different way."
            });
            what
        }
        // The product surface of this phase: the report a running branch
        // gets when someone speaks to it. What happened **is** the
        // message — author-labelled, and marked with what it owes you.
        HandbackHow::Posted { ids } => {
            // **The messages themselves are not repeated here.** They
            // arrive as rows of their own, in the same user turn, a few
            // lines above — `document.rs` renders every `Post` that way
            // whether it suspended a program or not. Printing the
            // bodies again under `what happened` put the same words in
            // the same message twice, which is how a reader learns that
            // one of the two copies is decoration. What this owes the
            // reader is the *fact*: which rows did it, and what a
            // resume means.
            let named = ids
                .iter()
                .filter_map(|id| {
                    let EventPayload::Post { from, origin } = &h.tree.events.get(id)?.payload
                    else {
                        return None;
                    };
                    let origin = h.tree.resolve(origin);
                    // *asked* versus *told*: what it owes, which is the
                    // difference between `ask` and `tell` and the only
                    // thing the model has to decide about differently.
                    let owed = match origin.direct() {
                        Some((_, _, true)) => "asked",
                        _ => "told",
                    };
                    Some(format!(
                        "`[{}]`, where {} {} you",
                        id.as_u64(),
                        author_label(*from),
                        owed
                    ))
                })
                .collect::<Vec<_>>();
            format!(
                "It is paused at its last fuel slice, because someone spoke to you while it \
                 was running: {}. Nothing was lost.\n\n`resume()` continues it from where \
                 it stopped — nothing here asked for a value.",
                match named.len() {
                    0 => "see the rows above".to_owned(),
                    _ => named.join(", "),
                }
            )
        }
        HandbackHow::Interrupted => {
            "This program was interrupted before completing — the process died and the VM \
             went with it. Not resumable: there is nothing left to resume. The artifacts \
             below are still fetchable by id; rewrite to continue."
                .to_owned()
        }
        HandbackHow::Superseded => {
            "You wrote a new program over one that was suspended, so that one is gone — \
             nothing resumed it and nothing had to. Calls it had already issued still \
             settle, and everything it completed is below, fetchable by id."
                .to_owned()
        }
        HandbackHow::Abandoned => {
            "A handler abandoned this program: it was discarded rather than continued, and \
             its VM is gone. Calls it had already issued still settle, and everything it \
             completed is below, fetchable by id. Nothing is suspended — whatever happens \
             next is a fresh program."
                .to_owned()
        }
        // Never reached: `render_handback` sends both of these
        // elsewhere — a completed reply to `CompletionReport`, and a
        // block that would not compile to a `ConditionReport` built
        // around the diagnostic, because the blocks before it in the
        // same reply did run and have rows to show. Kept so this match
        // stays exhaustive over `Handback` rather than letting a
        // wildcard hide a variant added later.
        HandbackHow::Completed { .. } => String::new(),
        HandbackHow::CellFailed { message } => message.clone(),
    }
}

/// Who a post is from, for the report's author label. A post from the
/// person driving the session is unlabelled in the transcript, but a
/// report *about* an arrival has to name them.
pub(crate) fn author_label(from: Author) -> String {
    match from {
        Author::User => "the user".into(),
        Author::Harness => "the harness".into(),
        Author::Agent(id) => format!("agent {}", id.as_u64()),
    }
}

/// `line:col: message` with the source line and a caret, or the bare
/// message when there is no source to point into. `site` (a `Condition`'s
/// recorded point, not a range — see `machine::span_at`) renders as a
/// single caret rather than an underline: underlining the whole offending
/// expression would need `Condition` to carry an end too, which nothing
/// here asks for yet — only `Call::Send` gained one (`site_end`, for a
/// host to log the whole call), so this stays a caret.
fn diagnostic(source: &str, site: u32, message: &str) -> String {
    if source.is_empty() {
        return message.to_owned();
    }
    // **A site of zero means nobody knows where.** It is already the
    // convention for that — the `CellFailed` arm parks there on
    // purpose — and it reaches here whenever an instruction's span sits
    // in the prelude region, because rebasing subtracts the prelude's
    // length and saturates. Rendered as a location it becomes `1:1:`
    // with a caret under the first line of the reply, which is almost
    // always the model's own opening sentence:
    //
    // ```text
    // 1:1: cannot read .length of undefined
    // Dead-code hunting in `helpers.py` — first, what's in the repo…
    // ^
    // ```
    //
    // 11 of 121 diagnostics in the corpus pointed at prose that way.
    // A wrong location is worse than none: it is the one part of a
    // diagnostic a reader trusts without checking.
    if site == 0 {
        return message.to_owned();
    }
    interp::Diagnostic {
        kind: interp::DiagKind::Semantic,
        span: interp::Span::point(site),
        message: message.to_owned(),
    }
    .render(source)
}

/// The artifact menu for this handback: every call **up to this
/// outcome** (settled or pending) plus prior returns, optionally
/// restricted to what this run produced.
///
/// Both bounds matter. The lower one is clean-room scoping — a program
/// may fetch ids on its own branch's path and no others (decision 3). The
/// upper one is prefix immutability: without it a report rendered today
/// would list artifacts that landed tomorrow, and every cached branch
/// walking through it would change under the model.
fn menu_since(h: &Handback<'_>, since: u64) -> Vec<Artifact> {
    let start = h.path[..=h.turn_at]
        .iter()
        .rposition(|e| matches!(e.payload, EventPayload::Agent { .. }))
        .unwrap_or(0);
    let segment: Vec<&Event> = h.path[start..=h.outcome_at].to_vec();
    // The whole path, not the segment: a row is compacted *after* it is
    // logged, so the `Compacted` event that removes it routinely sits
    // past `outcome_at`.
    crate::machine::menu_rows(&segment, since, &compacted_rows(&h.path), &h.path)
}

/// What a `Compacted` event did to each row it names: `None` removed it
/// outright, `Some(text)` stood something shorter in its place. Both
/// are honoured by [`crate::machine::menu_rows`] — a removed row is not
/// listed, a replaced one lists its replacement — so the one place the
/// card promises removal means removal is the same list it advertises.
fn compacted_rows(
    path: &[&Event],
) -> std::collections::HashMap<EventId, crate::tree::CompactedView> {
    path.iter()
        .filter_map(|e| match &e.payload {
            EventPayload::Compacted { of, text } => {
                Some((*of, crate::tree::CompactedView { text: text.clone() }))
            }
            _ => None,
        })
        .collect()
}

/// One call's dispatch site and whether it is a `Send` (re-awaitable by
/// id when pending, unlike a host call) — the per-call input
/// [`annotate_calls`] needs, independent of whether it came from a
/// finished handback's path or a still-running program.
pub struct CallSite {
    pub site: u32,
    pub id: EventId,
    pub is_send: bool,
}

/// The program source with **every call site annotated by its
/// settlement** — the pure core a finished handback's report
/// ([`annotated_source`], below) and a running program's live pane
/// (17_BRANCHES Part D) both call, so the two can never disagree: both
/// derive from the log — a handback's own path, or `Tree::programs_for`
/// walked to the branch's current leaf — never from a live VM.
///
/// `site` on every `Call` is what makes this possible: a byte offset
/// logged at dispatch, so the annotation is derived from the log alone.
/// Pending *sends* are re-awaitable by id and say so; pending host calls
/// are not, and say that instead.
pub fn annotate_calls<'a>(
    source: &str,
    calls: &[CallSite],
    settled: impl Fn(EventId) -> Option<&'a Outcome>,
) -> String {
    // Line starts, so a byte offset becomes a line index.
    let line_of = |offset: u32| -> usize {
        source
            .bytes()
            .take(offset as usize)
            .filter(|b| *b == b'\n')
            .count()
    };
    let mut notes: Vec<Vec<String>> = vec![Vec::new(); source.lines().count().max(1)];
    for call in calls {
        let id = call.id.as_u64();
        let note = match settled(call.id) {
            Some(Outcome::Delivered(_)) => format!("#{id} done"),
            Some(Outcome::Failed(_)) => format!("#{id} failed"),
            None if call.is_send => format!("#{id} pending — await history.fetch({id})"),
            None => format!("#{id} issued; may have happened"),
        };
        let line = line_of(call.site).min(notes.len().saturating_sub(1));
        notes[line].push(note);
    }
    let mut out = String::new();
    for (n, line) in source.lines().enumerate() {
        out.push_str(line);
        let on_this_line = notes.get(n).map(Vec::as_slice).unwrap_or_default();
        if !on_this_line.is_empty() {
            let shown = on_this_line.len().min(ANNOTATIONS_PER_LINE);
            out.push_str("  // → ");
            out.push_str(&on_this_line[..shown].join(", "));
            if on_this_line.len() > shown {
                out.push_str(&format!(", and {} more", on_this_line.len() - shown));
            }
        }
        out.push('\n');
    }
    clip(out.trim_end(), ANNOTATED_SOURCE_MAX_BYTES)
}

/// A program's annotated source **live**: the same derivation as a
/// finished handback's report, over whatever `Tree::programs_for`
/// currently reconstructs for it — never a VM, so a running branch's
/// chat pane and the model's own report can never disagree
/// (17_BRANCHES Part D: "a pane that shows something the model also
/// sees must derive it the same way the model's copy is derived").
#[allow(dead_code)] // caller returns in Pass D: only `debug/` used this,
// and the TUI is cut from the build for Passes A-C.
pub fn annotate_program(pv: &crate::tree::ProgramView) -> String {
    let calls: Vec<CallSite> = pv
        .invokes
        .iter()
        .map(|iv| CallSite {
            site: iv.site,
            id: iv.id,
            is_send: iv.is_send,
        })
        .collect();
    annotate_calls(&pv.source, &calls, |id| {
        pv.invokes
            .iter()
            .find(|iv| iv.id == id)
            .and_then(|iv| iv.outcome.as_ref())
    })
}

/// A finished handback's annotated source: every call between its
/// driving `Turn` and its `outcome`, settled from that same segment.
fn annotated_source(h: &Handback<'_>) -> String {
    let segment: Vec<&Event> = h.path[..=h.outcome_at].to_vec();
    let calls: Vec<CallSite> = h.path[h.turn_at + 1..=h.outcome_at]
        .iter()
        .filter_map(|event| {
            let EventPayload::Call(call) = &event.payload else {
                return None;
            };
            Some(CallSite {
                site: call.site(),
                id: event.id,
                is_send: matches!(call, Call::Send { .. }),
            })
        })
        .collect();
    annotate_calls(&h.source, &calls, |id| {
        crate::machine::settlement_of(&segment, id)
    })
}

/// **What a `Fork` renders as** — the honest lever, and the only one.
///
/// Retry and sidebar are not modes: fork *before* a question and say "try
/// again with X", or fork *at the running leaf* and ask "what are you
/// doing?". The same gesture, and this line is what tells the model which
/// it is.
///
/// Always exactly **one** harness `Post`, not the old one-per-dangling-
/// tool-call fan-out: there is no tool-call adjacency to keep satisfied
/// any more (a `Turn` gets one `Post` back, full stop — 23_ONE_AGENT.md's
/// substitution table), so what to say no longer depends on caller-
/// tracked bookkeeping. It depends only on the log, derived fresh here
/// (a `Handback`, same as `handback`): did the fork point's last reply
/// already have its outcome by the time the fork was taken?
///
/// - **already settled** (an ordinary fork point): a harness line naming
///   the branch the pre-fork questions stayed with.
/// - **not yet settled** (a **mid-program** fork point): the
///   still-running program's own turn had no `Return`/`Condition` on
///   this path yet, so the note instead names the original branch and
///   the artifacts so far.
///
/// Every input is an event id, so it renders identically forever.
pub fn render_fork(tree: &Tree, leaf: EventId, fork: EventId) -> String {
    let at = tree.events.get(&fork).and_then(|e| e.parent_id);
    let origin = at.and_then(|at| tree.branch_of(at));
    let branch = origin
        .map(|b| format!("#{}", b.as_u64()))
        .unwrap_or_else(|| "the original".to_owned());

    let path = tree.path_events(leaf);
    let fork_at = path.iter().position(|e| e.id == fork).unwrap_or(0);
    let turn_at = path[..fork_at]
        .iter()
        .rposition(|e| matches!(e.payload, EventPayload::Reply));
    // Mid-program iff that turn's outcome had not landed by the fork.
    let running = turn_at.is_some_and(|ti| {
        !path[ti + 1..fork_at]
            .iter()
            .any(|e| matches!(e.payload, EventPayload::Handback { .. }))
    });

    if !running {
        let point = at
            .map(|at| format!(" at #{}", at.as_u64()))
            .unwrap_or_default();
        return format!(
            "[harness] fork of branch {branch}{point} — questions before this line are \
             being handled there; do not redo its work unless asked."
        );
    }
    // The fork point's last turn is still running on the original. Its
    // artifacts crossed the fork (history and artifacts do); the run
    // itself did not (the VM is never copied).
    let program = turn_at
        .map(|i| format!("#{}", path[i].id.as_u64()))
        .unwrap_or_else(|| "the program".to_owned());
    let start = path[..fork_at]
        .iter()
        .rposition(|e| matches!(e.payload, EventPayload::Agent { .. }))
        .unwrap_or(0);
    let segment: Vec<&Event> = path[start..fork_at].to_vec();
    let artifacts = crate::machine::menu_rows(&segment, 0, &compacted_rows(&path), &path);
    let menu: Vec<&Artifact> = artifacts.iter().collect();
    let head = format!(
        "program {program} is running on branch {branch}, not here. This fork inherited \
         its history and its artifacts; the run itself stayed there, so nothing you do \
         here disturbs it."
    );
    match render_row_list("### rows so far", &menu) {
        Some(m) => format!("{head}\n\n{m}"),
        None => head,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn artifact(id: u64, label: &str, result: serde_json::Value) -> Artifact {
        Artifact {
            id,
            label: label.into(),
            state: ArtifactState::Delivered(result),
        }
    }

    fn pending(id: u64, label: &str, state: ArtifactState) -> Artifact {
        Artifact {
            id,
            label: label.into(),
            state,
        }
    }

    use crate::types::{EventPayload, Origin, Tree};

    /// A one-branch fixture log: a `Turn` whose entire content is a bare
    /// program (`Turn.source` — no tool-call wrapper any more), followed
    /// by whatever outcome the caller wants.
    fn fixture(source: &str, outcome: EventPayload) -> (Tree, crate::types::EventId) {
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "SYSTEM", Vec::new())
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Post {
                from: Author::User,
                origin: Origin::Direct {
                    text: "go".into(),
                    input: json!(null),
                    options: Vec::new(),
                    expects_reply: true,
                },
            },
        )
        .unwrap();
        // A reply and its one cell — the reply's text is its parts (28).
        let reply = tree.append(&mut spine, EventPayload::Reply).unwrap();
        tree.append(
            &mut spine,
            EventPayload::Part {
                reply,
                part: crate::types::Part::Cell(source.to_owned()),
            },
        )
        .unwrap();
        let outcome = tree.append(&mut spine, outcome).unwrap();
        (tree, outcome)
    }

    /// **The blocks before the one that would not compile did run.**
    /// A reply is one program that pauses between its blocks, so by the
    /// time a later block fails to compile the earlier ones have made
    /// their calls and appended their rows. Live on 2026-09-20 that
    /// report was 474 bytes reading "YOUR PROGRAM DID NOT RUN", over
    /// six rows on the log, and the next reply started the task again.
    #[test]
    fn a_reply_that_ran_and_then_failed_to_compile_says_what_ran() {
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "SYSTEM", Vec::new())
            .unwrap();
        let reply = tree.append(&mut spine, EventPayload::Reply).unwrap();
        tree.append(
            &mut spine,
            EventPayload::Part {
                reply,
                part: crate::types::Part::Cell("await tools.read_file(\"a.rs\");".into()),
            },
        )
        .unwrap();
        let call = tree
            .append(
                &mut spine,
                EventPayload::Call(crate::types::Call::Invoke {
                    name: "read_file".into(),
                    args: json!(["a.rs"]),
                    site: 0,
                }),
            )
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Result {
                call,
                outcome: crate::types::Outcome::Delivered(
                    json!({"content": "fn main(){}", "version": "v"}),
                ),
            },
        )
        .unwrap();
        let o = tree
            .append(
                &mut spine,
                EventPayload::Handback {
                    program: reply,
                    how: HandbackHow::CellFailed {
                        message: "12:7: `lib` is already declared".into(),
                    },
                    site: 0,
                    stack: Vec::new(),
                },
            )
            .unwrap();
        // The console is logged after the outcome it belongs to —
        // `handback` finds it by that position.
        tree.append(
            &mut spine,
            EventPayload::Console {
                lines: vec!["read it".into()],
            },
        )
        .unwrap();
        let leaf = tree.list_leaves()[0].0;
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert!(
            text.starts_with(PART_RUN_HEADING),
            "not the blunt heading: {text}"
        );
        assert!(text.contains("already declared"), "the diagnostic: {text}");
        assert!(
            text.contains(&format!("`[{}]`", call.as_u64())),
            "and the row the earlier block added: {text}"
        );
        assert!(text.contains("read it"), "and what it printed: {text}");
    }

    /// **A name that is gone says where it went.** Two of the eight
    /// `is not defined` traps across 96 kept runs were a variable the
    /// previous reply had bound — `tests` and `f`. The card says
    /// nothing crosses between replies; the report is what the model is
    /// reading at the moment it finds out.
    #[test]
    fn a_variable_from_a_finished_reply_is_named_as_such() {
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "SYSTEM", Vec::new())
            .unwrap();
        // The reply that bound it, then the one that reached for it.
        let first = tree.append(&mut spine, EventPayload::Reply).unwrap();
        tree.append(
            &mut spine,
            EventPayload::Part {
                reply: first,
                part: crate::types::Part::Cell("const f = await tools.read_file(\"a\");".into()),
            },
        )
        .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Handback {
                program: first,
                how: crate::types::Handback::Completed {
                    value: None,
                    rested: false,
                },
                site: 0,
                stack: Vec::new(),
            },
        )
        .unwrap();
        let second = tree.append(&mut spine, EventPayload::Reply).unwrap();
        tree.append(
            &mut spine,
            EventPayload::Part {
                reply: second,
                part: crate::types::Part::Cell("console.log(f.content);".into()),
            },
        )
        .unwrap();
        let o = tree
            .append(
                &mut spine,
                EventPayload::Handback {
                    program: second,
                    how: HandbackHow::Trapped {
                        kind: "ReferenceError".into(),
                        message: "f is not defined".into(),
                        resumable: true,
                    },
                    site: 12,
                    stack: Vec::new(),
                },
            )
            .unwrap();
        let leaf = tree.list_leaves()[0].0;
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert!(text.contains("f is not defined"), "{text}");
        assert!(
            text.contains(&format!("the one at `[{}]`", first.as_u64())),
            "names the reply that bound it: {text}"
        );
        assert!(text.contains("nothing crosses between replies"), "{text}");
    }

    /// And a name that was never bound anywhere is an ordinary typo,
    /// which is what keeps the line above free of false positives.
    #[test]
    fn a_name_bound_nowhere_gets_the_ordinary_message() {
        let (tree, o) = fixture(
            "console.log(nope);",
            condition(
                HandbackHow::Trapped {
                    kind: "ReferenceError".into(),
                    message: "nope is not defined".into(),
                    resumable: true,
                },
                0,
                Vec::new(),
            ),
        );
        let leaf = tree.list_leaves()[0].0;
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert!(text.contains("nope is not defined"), "{text}");
        assert!(!text.contains("earlier reply"), "{text}");
    }

    /// A `Condition` payload with `disposition: Pushed` — the ordinary
    /// deliberation case every fixture below wants; only replay's depth
    /// counter (not this renderer) reads it.
    fn condition(cause: HandbackHow, site: u32, stack: Vec<String>) -> EventPayload {
        EventPayload::Handback {
            program: EventId::new(1),
            how: cause,
            site,
            stack,
        }
    }

    /// Every report kind renders from fixture events alone — no VM, no
    /// **The heaviest line reads the document the way the model does.**
    ///
    /// Every byte belongs to the last id the page announced — a `↓
    /// history[N]` above a block, or a `- `[N]`` at the head of a menu
    /// row — because those are the only two places the document says an
    /// id out loud, so anything else would be attributing bytes by a
    /// rule the reader cannot check.
    #[test]
    fn the_heaviest_line_names_the_big_row_and_not_the_small_ones() {
        let msg = |role, content: String| crate::document::ChatMessage {
            role,
            content,
            call: None,
            result_for: None,
            thinking: None,
        };
        let doc = crate::document::Document {
            messages: vec![
                msg(
                    crate::document::ChatRole::Assistant,
                    format!(
                        "{} history[7]\n{}\n",
                        crate::document::BLOCK_ARROW,
                        "x".repeat(4096)
                    ),
                ),
                msg(
                    crate::document::ChatRole::User,
                    "- `[9]` `bash(\"ls\")` → ok, 12 bytes\n- `[11]` noted: 3\n".to_owned(),
                ),
            ],
            preamble: 0,
        };
        let ages = std::collections::HashMap::from([(7, 11), (9, 2), (11, 0)]);
        let rows = heaviest_rows(&doc, &ages, 5);
        assert_eq!(rows.first().map(|r| r.0), Some(7), "biggest first: {rows:?}");
        assert!(rows[0].1 > 4000, "it carries the block's bytes: {rows:?}");

        let line = heaviest_line(&rows);
        assert!(line.contains("#7 4.0 KB, 11 replies ago"), "{line}");
        // The two small rows are named nowhere: a list that included
        // them would say "these are the ones" about entries whose
        // removal frees nothing.
        assert!(!line.contains("#9") && !line.contains("#11"), "{line}");
    }

    /// A document with nothing heavy in it gets no line at all, rather
    /// than a line naming its three biggest 40-byte rows.
    #[test]
    fn a_light_document_gets_no_heaviest_line() {
        let doc = crate::document::Document {
            messages: vec![crate::document::ChatMessage {
                role: crate::document::ChatRole::User,
                content: "- `[9]` noted: 3\n".to_owned(),
                call: None,
                result_for: None,
                thinking: None,
            }],
            preamble: 0,
        };
        let rows = heaviest_rows(&doc, &std::collections::HashMap::new(), 5);
        assert!(heaviest_line(&rows).is_empty(), "{rows:?}");
    }

    /// live state. This is the whole claim of "reports are derived".
    #[test]
    fn every_report_kind_renders_from_the_log() {
        // Completion: the return value never gets a full copy, only a
        // bounded preview — even a tiny one like this — named beside the
        // `program result` artifact id the run itself logs (the `Return`
        // is its own menu row; see `machine::menu_rows`).
        let (tree, o) = fixture(
            "return 1;",
            EventPayload::Handback {
                program: EventId::new(1),
                how: crate::types::Handback::Completed {
                    value: None,
                    rested: false,
                },
                site: 0,
                stack: Vec::new(),
            },
        );
        let leaf = tree.list_leaves()[0].0;
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert!(text.starts_with(RUN_HEADING), "{text}");
        assert!(text.contains("It completed."), "{text}");

        // Condition: a raise, with the caret placed from the logged site
        // and the source read straight off the turn's own `source`. What
        // `resume(value)` means for a raise is now inline in "what
        // happened", not a separate restart menu.
        let src = "raise(\"need\", { got: 1 });";
        let (tree, o) = fixture(
            src,
            condition(
                HandbackHow::Raised {
                    name: "need".into(),
                    payload: Some(json!({ "got": 1 })),
                },
                0,
                vec!["<root>".into()],
            ),
        );
        let leaf = tree.list_leaves()[0].0;
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert!(text.contains("condition `need` raised"), "{text}");
        assert!(text.contains(r#"payload: {"got":1}"#), "{text}");
        assert!(text.contains("history.note(resume(value))"), "{text}");

        // Condition: a trapped error, not resumable — the report says so
        // in prose now, inline with the diagnostic.
        let (tree, o) = fixture(
            "return null.x;",
            condition(
                HandbackHow::Trapped {
                    kind: "TypeError".into(),
                    message: "cannot read property 'x' on null".into(),
                    resumable: false,
                },
                7,
                Vec::new(),
            ),
        );
        let leaf = tree.list_leaves()[0].0;
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert!(text.contains("1:8: cannot read property"), "{text}");
        assert!(text.contains("not resumable"), "{text}");

        // **A copy of a result is a second charge for bytes already
        // kept.** 72% of everything appended across 96 kept runs was
        // already on the log; a run on 2026-09-20 opened with
        // `history.note({lib: lib.content, …})` over files it had read
        // in the same program.
        let big = "x".repeat(600);
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "SYSTEM", Vec::new())
            .unwrap();
        let reply = tree.append(&mut spine, EventPayload::Reply).unwrap();
        tree.append(
            &mut spine,
            EventPayload::Part {
                reply,
                part: crate::types::Part::Cell("…".into()),
            },
        )
        .unwrap();
        let call = tree
            .append(
                &mut spine,
                EventPayload::Call(crate::types::Call::Invoke {
                    name: "read_file".into(),
                    args: json!(["a.rs"]),
                    site: 0,
                }),
            )
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Result {
                call,
                outcome: crate::types::Outcome::Delivered(json!({"content": big, "version": "v1"})),
            },
        )
        .unwrap();
        let note = tree
            .append(
                &mut spine,
                EventPayload::Note {
                    value: json!({ "lib": big, "note": "short and mine" }),
                    site: 0,
                    site_end: 0,
                },
            )
            .unwrap();
        let o = tree
            .append(
                &mut spine,
                EventPayload::Handback {
                    program: reply,
                    how: crate::types::Handback::Completed {
                        value: None,
                        rested: false,
                    },
                    site: 0,
                    stack: Vec::new(),
                },
            )
            .unwrap();
        let leaf = tree.list_leaves()[0].0;
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert!(
            text.contains(&format!(
                "`[{}]` holds the bytes of `[{}]`",
                note.as_u64(),
                call.as_u64()
            )),
            "names both rows: {text}"
        );
        assert!(text.contains("Keep the id"), "{text}");

        // Compaction: the handler has to be told what it is being asked
        // for and how much, or it reads the report as an ordinary
        // interruption and carries on with the task — which is what a
        // live run did on 2026-09-16, answering the user's question
        // instead of compacting anything.
        let text = compaction_message(60_555, 32_768, crate::types::Measure::Bytes, None, "");
        assert!(text.contains("60555 bytes"), "says how big it is: {text}");
        assert!(
            text.contains("32768-byte budget"),
            "and what the budget is: {text}"
        );
        // And when the count is what filled up, it says so in tokens —
        // naming a byte budget that is not the binding constraint asks
        // the handler to shrink against the wrong number.
        let counted = compaction_message(43_100, 57_344, crate::types::Measure::Tokens, None, "");
        assert!(counted.contains("43100 tokens"), "{counted}");
        assert!(counted.contains("57344-token budget"), "{counted}");
        assert!(text.contains("compaction program"), "{text}");
        // Read as a request rather than a stop, this loses to the pull
        // of visible unfinished work — measured three times.
        assert!(text.contains("STOP"), "{text}");
        // And it is the whole report: no VM ran, so the condition
        // scaffolding around it ("## where", an empty artifact menu)
        // is noise that framed a directive as a post-mortem.
        assert!(!text.contains("### where it stopped"), "{text}");
        assert!(!text.contains("new rows"), "{text}");
        assert!(text.contains("history.remove"), "names the verbs: {text}");
        assert!(text.contains("history.replace"), "{text}");

        // Compile error: the diagnostic alone — no VM was built, so this
        // run has no console and no artifacts.
        let (tree, o) = fixture(
            "let = ;",
            condition(
                crate::types::Handback::CellFailed {
                    message: "compile error:\n1:5: unexpected token".into(),
                },
                0,
                Vec::new(),
            ),
        );
        let leaf = tree.list_leaves()[0].0;
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert_eq!(
            text,
            format!("{NO_RUN_HEADING}\n\ncompile error:\n1:5: unexpected token"),
            "nothing ran, so the heading is still the blunt one"
        );

        // Interrupted: the run stopped and there is nothing to resume.
        // **Truncation is no longer one of these** — it is a fact about
        // the *text*, carried on `ReplyEnd` (28), and the model is shown
        // what it wrote with a marker on the end rather than a sentence
        // instead of it.
        let (tree, o) = fixture("", condition(HandbackHow::Interrupted, 0, Vec::new()));
        let leaf = tree.list_leaves()[0].0;
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert!(text.contains("interrupted"), "{text}");
    }

    /// **The directive names the job, not the window.** Most of the
    /// document is the card and the worked examples, which no
    /// compaction program can touch — so a number that includes them
    /// describes a much easier task than the one being asked for.
    ///
    /// Live on 2026-09-20: "35552 Bytes against a 42000-Byte budget",
    /// of which 27797 was the card. The model read that as freeing an
    /// eleventh, removed eight rows, and freed 87 bytes. The real ask
    /// was more than half of a 7,668-byte conversation.
    #[test]
    fn the_compaction_directive_says_how_much_is_actually_yours() {
        let with = compaction_message(35_552, 42_000, crate::types::Measure::Bytes, Some(27_797), "");
        assert!(with.contains("27797 is the card"), "{with}");
        assert!(
            with.contains("conversation itself is 7755"),
            "and what is left is the job: {with}"
        );

        // In tokens the split is not knowable — it is measured in bytes
        // and there is no tokenizer here. A number in the wrong unit is
        // worse than no number.
        let tokens = compaction_message(43_100, 57_344, crate::types::Measure::Tokens, None, "");
        assert!(!tokens.contains("is the card"), "{tokens}");
    }

    /// **Every artifact appears in exactly one report**: each menu is
    /// bounded below by the previous outcome, so the reports on a branch
    /// partition its artifacts with no gaps and no repetition. Before
    /// this, every condition report re-listed its predecessor's rows, and
    /// a branch with N conditions carried N near-identical menus in a
    /// prefix it can never shed.
    #[test]
    fn reports_partition_the_artifacts_they_list() {
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "agent", None, "", Vec::new())
            .unwrap();
        let mut ids = Vec::new();
        let mut outcomes = Vec::new();
        // Two handbacks, each with its own call and result.
        for run in 0..2 {
            tree.append(&mut spine, EventPayload::Restart).unwrap();
            let call = tree
                .append(
                    &mut spine,
                    EventPayload::Call(Call::Invoke {
                        name: "read".into(),
                        args: json!([run]),
                        site: 0,
                    }),
                )
                .unwrap();
            ids.push(call.as_u64());
            tree.append(
                &mut spine,
                EventPayload::Result {
                    call,
                    outcome: crate::types::Outcome::Delivered(json!(run)),
                },
            )
            .unwrap();
            outcomes.push(
                tree.append(
                    &mut spine,
                    EventPayload::Handback {
                        program: EventId::new(1),
                        how: crate::types::Handback::Completed {
                            value: None,
                            rested: false,
                        },
                        site: 0,
                        stack: Vec::new(),
                    },
                )
                .unwrap(),
            );
        }
        let leaf = spine.leaf_id;
        let rows = |o: EventId| -> Vec<u64> {
            let h = handback(&tree, leaf, o).expect("a handback");
            menu_since(&h, h.previous_outcome)
                .into_iter()
                .map(|a| a.id)
                .collect()
        };
        let first = rows(outcomes[0]);
        let second = rows(outcomes[1]);
        // No repetition…
        assert!(
            first.iter().all(|id| !second.contains(id)),
            "{first:?} / {second:?}"
        );
        // …and no gaps: every call is listed by exactly one report. A
        // handback is not itself a row — it carries no value (D5), so
        // there is nothing to list.
        let every: Vec<u64> = ids.to_vec();
        for id in &every {
            assert!(
                first.contains(id) ^ second.contains(id),
                "#{id} listed once: {first:?} / {second:?}"
            );
        }
    }

    #[test]
    fn clip_bounds_and_marks() {
        let big = "x".repeat(5000);
        let clipped = clip(&big, 100);
        assert!(clipped.len() < 200, "bounded");
        assert!(clipped.contains("[truncated; 5000 bytes total]"));
        // Multi-byte safety: clipping mid-codepoint backs up.
        let uni = "é".repeat(60);
        let c = clip(&uni, 99);
        assert!(c.contains("[truncated;"));
    }

    /// Prose a reply sends to the person is bounded like console
    /// output: a degenerate reply of repeated text must not deliver
    /// megabytes to the person. The log keeps every byte (28); this
    /// caps only what is delivered, like `strip_imitated_markers`.
    #[test]
    fn prose_sent_to_the_person_is_capped() {
        // An ordinary message passes through whole.
        let short = "a short message";
        assert_eq!(cap_prose(short), short);

        // A degenerate one keeps its head, stays near the budget, and
        // says it was cut — the never-silent rule `cap_console`
        // documents.
        let long = "x".repeat(PROSE_MAX_BYTES * 2);
        let capped = cap_prose(&long);
        assert!(capped.starts_with("xxx"), "the head survives");
        assert!(
            capped.len() <= PROSE_MAX_BYTES + 64,
            "stays near the budget: {}",
            capped.len()
        );
        assert!(
            capped.contains("[truncated;"),
            "the clip is never silent: {capped}"
        );

        // Multi-byte safety: clipping mid-codepoint backs up.
        let uni = "é".repeat(PROSE_MAX_BYTES + 1);
        let capped = cap_prose(&uni);
        assert!(capped.contains("[truncated;"));
        assert!(capped.ends_with("bytes total]"));
    }

    /// The tail is bounded in **total** bytes, not by a flat per-line
    /// clip. A `console.log` of a file used to come back cut at 200
    /// bytes, which made the one channel that shows a value to the next
    /// program useless for the thing programs reach for it to do.
    #[test]
    fn console_keeps_recent_output_whole_within_a_total_budget() {
        // **A file-sized print arrives whole.** 31 lines and a
        // kilobyte is well inside the budget, and the line cap used to
        // cut it to 20 anyway — which is what made a model append a
        // file to look at it.
        let mut lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        lines.push("y".repeat(1000));
        let rendered = render_console(&lines, None).expect("lines present");
        assert_eq!(
            rendered.lines().next(),
            Some("### it printed"),
            "nothing was clipped, so nothing announces a clip: {rendered}"
        );
        assert!(rendered.contains("line 0"), "the whole print: {rendered}");
        assert!(
            rendered.contains(&"y".repeat(1000)),
            "a 1000-byte line is well inside the budget and survives whole"
        );

        // **One line always survives, however big it is.** A single
        // line past the whole budget used to stop the newest-first walk
        // on its first step, so the section read "The last 0 of 3
        // lines" over an empty fence — the program's own output,
        // withheld in full, announced as a clip of nothing.
        let huge = vec![
            "small".to_string(),
            "also small".to_string(),
            "x".repeat(CONSOLE_SECTION_MAX_BYTES * 2),
        ];
        let rendered = render_console(&huge, Some(4)).expect("lines present");
        assert!(
            rendered.contains("The last 1 of 3 lines"),
            "the newest line is kept: {rendered}"
        );
        assert!(
            rendered.contains("xxxx"),
            "and its bytes are actually there: {}",
            &rendered[..200.min(rendered.len())]
        );
        assert!(
            !rendered.contains("The last 0 of"),
            "never zero: {rendered}"
        );

        // And bytes are what bind when something really is too big.
        //
        // **Sized off the budget, not off a number.** These were ten
        // 900-byte lines, which overflowed 4096 and stopped doing so
        // the moment the cap was raised to keep its proportion to the
        // document budget — a test that silently stops testing is worse
        // than one that breaks.
        let line = CONSOLE_SECTION_MAX_BYTES / 8;
        let fat: Vec<String> = (0..10).map(|i| format!("{i}{}", "z".repeat(line))).collect();
        let rendered = render_console(&fat, Some(7)).expect("lines present");
        assert!(
            rendered.contains("of 10 lines; `history.fetch(7)` for all of them"),
            "a clip names the id to fetch: {rendered}"
        );
        assert!(
            !rendered.contains("0zzz"),
            "the oldest goes first: {rendered}"
        );
        assert!(rendered.contains("9zzz"), "the newest survives: {rendered}");

        // Past the budget, the oldest of the tail goes rather than every
        // line losing its end.
        // Same reason as above: the lines have to actually exceed the
        // budget, whatever the budget currently is.
        let fat: Vec<String> = (0..10)
            .map(|i| format!("{i}") + &"z".repeat(line))
            .collect();
        let rendered = render_console(&fat, None).expect("lines present");
        // **Whole lines go, from the oldest.** Counting rendered lines
        // counts the heading and the fences too, which made this pass
        // or fail on the wrapper rather than on the budget.
        assert!(
            !rendered.contains(&("0".to_owned() + &"z".repeat(line))),
            "the oldest whole line is dropped, not every line's end: {}",
            &rendered[..120.min(rendered.len())]
        );
        assert!(
            rendered.len() <= CONSOLE_SECTION_MAX_BYTES + 200,
            "and stays inside it: {} bytes",
            rendered.len()
        );
        assert!(
            rendered.contains(&("9".to_owned() + &"z".repeat(line))),
            "the newest line is the one guaranteed to survive"
        );
    }

    #[test]
    fn menu_prunes_to_recent_entries() {
        let artifacts: Vec<Artifact> = (1..=25)
            .map(|i| artifact(i, &format!("tool_{i}([])"), json!(i)))
            .collect();
        let menu: Vec<&Artifact> = artifacts.iter().collect();
        let rendered = render_row_list("### artifacts", &menu).expect("25 rows");
        assert!(rendered.contains("(5 older rows omitted; their ids stay fetchable)"));
        assert!(!rendered.contains("[5]"), "old entries gone");
        assert!(rendered.contains("[6]") && rendered.contains("[25]"));
    }

    /// The two pending kinds render differently because only one can be
    /// re-attached: a `Send`'s answer is still coming and is awaited by
    /// id, while an `Invoke`'s worker died with the process.
    #[test]
    fn pending_rows_say_which_can_be_reattached() {
        let rows = [
            pending(11, "ask(#3, \"which file?\")", ArtifactState::PendingSend),
            pending(12, "send_email([\"…\"])", ArtifactState::PendingInvoke),
            pending(
                13,
                "fetch([\"x\"])",
                ArtifactState::Failed("host is down".into()),
            ),
        ];
        let menu: Vec<&Artifact> = rows.iter().collect();
        let rendered = render_row_list("### artifacts", &menu).expect("three rows");
        assert!(
            rendered.contains(
                "- `[11]` `ask(#3, \"which file?\")` → pending — await history.fetch(11)"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "- `[12]` `send_email([\"…\"])` → issued; no result recorded; may have happened"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("- `[13]` `fetch([\"x\"])` → failed: host is down"),
            "{rendered}"
        );
    }

    #[test]
    fn stack_keeps_innermost_frames() {
        let stack: Vec<String> = (0..12).map(|i| format!("f{i}")).collect();
        let rendered = render_stack(&stack);
        assert!(rendered.contains("(4 outer frames omitted)"));
        assert!(!rendered.contains("f3 →"), "outer contexts gone");
        assert!(rendered.ends_with("f11"), "innermost kept: {rendered}");
    }

    #[test]
    fn what_section_is_bounded() {
        let report = ConditionReport {
            heading: RUN_HEADING,
            copied_rows: Vec::new(),
            what: "w".repeat(10_000),
            whence: Whence::Stack(vec!["<root>".into()]),
            console: Vec::new(),
            console_id: None,
            artifacts: Vec::new(),
        };
        let rendered = report.render();
        let what = rendered.split("\n\n### where it stopped").next().unwrap();
        assert!(what.len() < WHAT_MAX_BYTES + 100);
        assert!(what.contains("[truncated; 10000 bytes total]"));
    }

    /// **What crosses to the next reply is a row, and it crosses
    /// whole.** This was three tests about a `return` value — the
    /// channel a reply no longer has (D5). What replaced it is
    /// `history.note`, and the thing worth guarding is the same: the
    /// value the author chose reaches the next reader unclipped, where
    /// everything else in the report is an index entry.
    #[test]
    fn an_appended_row_reaches_the_next_reply_whole() {
        let long = "z".repeat(5_000);
        let rendered = CompletionReport {
            returned: None,
            console: Vec::new(),
            console_id: None,
            new_artifacts: vec![Artifact {
                id: 9,
                label: String::new(),
                state: ArtifactState::Whole(format!("appended: {long}")),
            }],
            failed_calls: 0,
            long_bash: 0,
            copied_rows: Vec::new(),
        }
        .render();
        assert!(rendered.contains(&long), "the row is not clipped");
        assert!(rendered.contains("- `[9]` appended:"), "{rendered}");
    }

    /// **A structured row names its shape.** Keys, never contents: a
    /// row that said only `ok, 31045 bytes` read as *a 31045-byte
    /// thing*, and on 2026-09-19 a run fetched one and called
    /// `.slice(0, 2000)` on the `{ content, version }` it got back.
    #[test]
    fn a_structured_row_says_what_kind_of_thing_it_indexes() {
        let file = json!({ "content": "x".repeat(4000), "version": "abc" });
        let tail = delivered_tail_t(&file);
        assert!(tail.starts_with("ok, {content, version}, "), "{tail}");
        assert!(!tail.contains("xxxx"), "the contents stay out: {tail}");

        assert!(delivered_tail_t(&json!([1, 2, 3])).starts_with("ok, [3 items], "));
        assert!(delivered_tail_t(&json!([1])).starts_with("ok, [1 item], "));

        // A wide object gives up rather than spilling onto three lines.
        let wide = json!({"a":1,"b":2,"c":3,"d":4,"e":5,"f":6,"g":7});
        assert!(delivered_tail_t(&wide).starts_with("ok, {a, b, c, d, e, …}, "));

        // Scalars are unchanged — they already showed their value.
        // **A command that failed says so in its row.** The card
        // opens `bash` with "Read `status` before `stdout`", and the
        // row printed the field names either way — 105 of 1,178 bash
        // calls in the kept corpus exited non-zero and not one row
        // mentioned it.
        assert_eq!(
            delivered_tail_t(&json!({"status": 1, "stdout": "", "stderr": "boom"})),
            "status 1, {status, stdout, stderr}, 40 bytes"
        );
        // Silent when it is zero, like every other count here.
        assert!(
            delivered_tail_t(&json!({"status": 0, "stdout": "hi", "stderr": ""}))
                .starts_with("ok, {status,"),
        );
        // And an object with no status is untouched.
        assert!(
            delivered_tail_t(&json!({"content": "x", "version": "v"})).starts_with("ok, {content,")
        );

        // **A write that changed nothing is news**, and it was
        // reported by omitting a field. A `sweep-40` run wrote back
        // bytes identical to what was on disk, read no `diff`, and
        // told the person it had deleted the dead helpers; all 24
        // were still there.
        assert_eq!(
            delivered_tail(
                &json!({"version": "abc"}),
                "replace_file(\"helpers.py\", …)"
            ),
            "no change, {version}, 17 bytes"
        );
        // A write that did something keeps its shape.
        assert!(
            delivered_tail(
                &json!({"version": "abc", "diff": "@@ -1 +1 @@"}),
                "replace_file(\"helpers.py\", …)"
            )
            .starts_with("ok, {version, diff}")
        );
        // And `create_file` returns a bare `{version}` too — a new
        // file is not "no change".
        assert!(
            delivered_tail(&json!({"version": "abc"}), "create_file(\"n.md\", …)")
                .starts_with("ok, {version}")
        );

        // A value whose own `ok` is false is not led with "ok".
        assert!(
            delivered_tail_t(&json!({"ok": false, "errors": [{"line": 7}]}))
                .starts_with("not ok, {ok, errors}"),
            "got: {}",
            delivered_tail_t(&json!({"ok": false, "errors": []}))
        );
        assert!(delivered_tail_t(&json!({"ok": true, "errors": []})).starts_with("ok, {ok,"));

        assert_eq!(delivered_tail_t(&json!(null)), "ok");
        assert_eq!(delivered_tail_t(&json!(42)), "42");
        assert_eq!(delivered_tail_t(&json!("short")), "\"short\"");
    }

    /// A menu row says a call arrived and how big its value is — never
    /// what the value was. Bounded is not enough: a bounded preview of
    /// every call is still a replay nobody asked for, and on one live
    /// run it was 27% of the document.
    #[test]
    fn a_menu_row_indexes_a_value_instead_of_replaying_it() {
        let report = ConditionReport {
            heading: RUN_HEADING,
            copied_rows: Vec::new(),
            what: "boom".into(),
            whence: Whence::Stack(vec!["<root>".into()]),
            console: Vec::new(),
            console_id: None,
            artifacts: vec![artifact(7, "fetch([\"big\"])", json!("b".repeat(9000)))],
        };
        let rendered = report.render();
        let menu_line = rendered.lines().find(|l| l.starts_with("- `[7]`")).unwrap();
        assert!(!menu_line.contains("bbbb"), "value replayed: {menu_line}");
        assert!(
            menu_line.contains("9002 bytes"),
            "size is kept: {menu_line}"
        );
        assert!(menu_line.len() < 120, "{menu_line}");
    }

    /// A failure keeps its text: it is the one thing nobody chose and
    /// everybody needs, and unlike a delivered value it cannot be
    /// fetched back — `outcome_json` turns it into a rejection.
    #[test]
    fn a_failed_row_still_carries_its_reason() {
        let report = ConditionReport {
            heading: RUN_HEADING,
            copied_rows: Vec::new(),
            what: "boom".into(),
            whence: Whence::Stack(vec!["<root>".into()]),
            console: Vec::new(),
            console_id: None,
            artifacts: vec![pending(
                7,
                "bash([\"build\"])",
                ArtifactState::Failed("no such file or directory".into()),
            )],
        };
        let line = report
            .render()
            .lines()
            .find(|l| l.starts_with("- `[7]`"))
            .unwrap()
            .to_owned();
        assert!(line.contains("no such file or directory"), "{line}");
    }

    /// A scalar is smaller than any description of it, so it is shown.
    #[test]
    fn a_small_delivered_scalar_is_shown_whole() {
        assert_eq!(delivered_tail_t(&json!(0)), "0");
        assert_eq!(delivered_tail_t(&json!(null)), "ok");
        assert_eq!(delivered_tail_t(&json!("v2")), "\"v2\"");
        assert!(delivered_tail_t(&json!("x".repeat(400))).starts_with("ok, "));
    }

    #[test]
    fn multi_mb_artifact_preview_stays_within_bound() {
        // A 2 MB artifact value — the preview must fit within PREVIEW_MAX_BYTES.
        let big = "z".repeat(2_000_000);
        let pv = preview(&json!(big));
        assert!(
            pv.len() <= PREVIEW_MAX_BYTES + 60,
            "preview bounded: {}",
            pv
        );
        assert!(pv.contains("[truncated; 2000002 bytes total]"));
    }

    fn completion(artifacts: Vec<Artifact>) -> String {
        CompletionReport {
            returned: None,
            console: Vec::new(),
            console_id: None,
            new_artifacts: artifacts,
            failed_calls: 0,
            long_bash: 0,
            copied_rows: Vec::new(),
        }
        .render()
    }

    #[test]
    fn write_without_verify_gets_a_nudge() {
        // Wrote two files, no inspection in the same program → nudge.
        let rendered = completion(vec![
            artifact(4, "bash([\"mkdir -p /x\"])", json!({ "status": 0 })),
            artifact(
                5,
                "create_file([\"/x/index.html\", \"…\"])",
                json!({ "version": "a" }),
            ),
            artifact(
                6,
                "create_file([\"/x/game.js\", \"…\"])",
                json!({ "version": "b" }),
            ),
        ]);
        assert!(rendered.contains("### worth knowing"), "{rendered}");
        assert!(rendered.contains("wrote files but didn't check them"));
    }

    #[test]
    fn write_then_verify_in_program_has_no_nudge() {
        // parse_errors, a re-read, or a bash build each count as the
        // in-program check — no nudge.
        for verify in [
            artifact(7, "parse_errors([\"/x/game.js\"])", json!({ "ok": true })),
            artifact(7, "read_file([\"/x/game.js\"])", json!({ "content": "…" })),
            artifact(7, "bash([\"cargo build\"])", json!({ "status": 0 })),
        ] {
            let rendered = completion(vec![
                artifact(
                    5,
                    "create_file([\"/x/game.js\", \"…\"])",
                    json!({ "version": "b" }),
                ),
                verify,
            ]);
            assert!(
                !rendered.contains("### worth knowing"),
                "unexpected nudge: {rendered}"
            );
        }
    }

    #[test]
    fn no_write_no_nudge() {
        // A pure read/compute program never gets the write nudge, even
        // with a setup-only bash call.
        let rendered = completion(vec![
            artifact(4, "bash([\"mkdir -p /x\"])", json!({ "status": 0 })),
            artifact(5, "read_file([\"/x/a.js\"])", json!({ "content": "…" })),
        ]);
        assert!(!rendered.contains("### worth knowing"), "{rendered}");
    }

    /// A derived branch label is the post's bare words — no `[#id]`, no
    /// author decoration. `render_post` adds both (so the model can
    /// resolve `answer(question, value)`'s `question`), but a navigator
    /// label is for a human's eye, not a restart target.
    /// The line count appears only when lines were actually dropped.
    /// "last 3 of 3 lines" announces a clip that did not happen, and it
    /// was every one: 31 of 31 console sections under the notebook
    /// transport and 28 of 28 under the program transport read "N of N"
    /// on 2026-09-18. Same principle as the `(no output)` line removed
    /// just above — a heading that never varies is one the reader skips.
    #[test]
    fn the_console_counts_lines_only_when_it_dropped_some() {
        let short: Vec<String> = vec!["one".into(), "two".into()];
        let rendered = render_console(&short, Some(9)).expect("two lines");
        assert!(
            rendered.starts_with("### it printed\n```text"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("of 2 lines"),
            "no phantom clip: {rendered}"
        );
        assert!(
            !rendered.contains("history.fetch"),
            "nothing behind it: {rendered}"
        );

        // Enough bytes to force a drop.
        let long: Vec<String> = (0..400)
            .map(|i| format!("line {i} {}", "x".repeat(200)))
            .collect();
        let rendered = render_console(&long, Some(9)).expect("many lines");
        assert!(
            rendered.contains("of 400 lines"),
            "a real clip counts: {rendered}"
        );
        assert!(
            rendered.contains("history.fetch(9)"),
            "and names where the rest is: {rendered}"
        );
    }

    /// **A reply that finished says so and no more.** There is no
    /// `return` (D5), so there is no value to print — the line that
    /// used to carry one read `returned: null` on 68 handbacks out of
    /// 68 before the field was removed from the type entirely.
    #[test]
    fn a_finished_reply_prints_no_value() {
        let rendered = CompletionReport {
            returned: None,
            console: Vec::new(),
            console_id: None,
            new_artifacts: Vec::new(),
            failed_calls: 0,
            long_bash: 0,
            copied_rows: Vec::new(),
        }
        .render();
        assert_eq!(rendered, format!("{RUN_HEADING}\n\nIt completed."));
    }

    /// **A removed row leaves the menu too.** `document.rs` drops a
    /// compacted row from the history log through its `CompactedView`
    /// shadow, but `menu_rows` had no compaction awareness at all — so a
    /// row deleted with `history.remove` vanished from the log and went
    /// on being advertised in the index printed directly beneath it.
    ///
    /// Found on 2026-09-18 in a kept eval log, not by a test: the
    /// existing menu tests all call `render_menu` with synthetic
    /// artifacts and never exercise `menu_rows`, which is the function
    /// that decides *which* rows exist. A `replace` is deliberately not
    /// filtered — that row still exists and is still worth fetching.
    #[test]
    fn a_removed_row_is_dropped_from_the_menu_not_only_from_the_log() {
        fn ev(id: u64, payload: EventPayload) -> Event {
            Event {
                id: EventId::new(id),
                parent_id: None,
                timestamp: jiff::Timestamp::UNIX_EPOCH,
                payload,
            }
        }
        let call = |id: u64, name: &str| {
            ev(
                id,
                EventPayload::Call(Call::Invoke {
                    name: name.into(),
                    args: json!([]),
                    site: 0,
                }),
            )
        };
        let owned = [
            call(5, "outline"),
            call(6, "read_file"),
            call(7, "grep"),
            ev(
                8,
                EventPayload::Compacted {
                    of: EventId::new(5),
                    text: None,
                },
            ),
            ev(
                9,
                EventPayload::Compacted {
                    of: EventId::new(6),
                    text: Some("kept, shortened".into()),
                },
            ),
        ];
        let path: Vec<&Event> = owned.iter().collect();
        let shadows = compacted_rows(&path);
        assert_eq!(shadows.len(), 2, "both kinds are carried: {shadows:?}");

        let rows = crate::machine::menu_rows(&path, 0, &shadows, &path);
        let ids: Vec<u64> = rows.iter().map(|a| a.id).collect();
        assert!(!ids.contains(&5), "the removed row is gone: {ids:?}");
        assert!(ids.contains(&6), "a replaced row stays fetchable: {ids:?}");
        assert!(ids.contains(&7), "an untouched row stays: {ids:?}");
        let replaced = rows.iter().find(|a| a.id == 6).unwrap();
        assert!(
            matches!(&replaced.state, ArtifactState::Whole(t) if t == "… kept, shortened"),
            "a replaced row shows its replacement, not what it replaced"
        );
    }

    #[test]
    fn derived_label_has_no_id_or_author_decoration() {
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "SYSTEM", Vec::new())
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Post {
                from: Author::User,
                origin: Origin::Direct {
                    text: "read the config and summarize it".into(),
                    input: json!(null),
                    options: Vec::new(),
                    expects_reply: true,
                },
            },
        )
        .unwrap();
        let (branch, leaf) = tree.branches()[0];
        let label = derived_branch_label(&tree, branch, leaf).expect("a post exists");
        assert_eq!(label, "read the config and summarize it");
        assert!(!label.contains('#'), "{label}");
    }
}
