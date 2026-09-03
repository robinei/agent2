//! Condition + completion reports (8_HARNESS Step 4; 17_BRANCHES A4).
//!
//! The `run_program` tool result is the product surface of the whole
//! project: it is what the LLM reads to decide how to restart a failed
//! program.
//!
//! **Reports are derived, not stored.** Every report here is a pure
//! function of the log: [`derive_report`] takes `(&Tree, leaf, turn)` and
//! reads forward from that turn to its outcome — the source from the
//! turn's tool-call args, the outcome, the `Console`, and the `Result`s
//! and menu rows on the path. No renderer touches a `VM`.
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
    Author, Call, Cause, Event, EventId, EventPayload, Message, Origin, ToolCall, Tree,
};

/// Max bytes of the "what happened" section (diagnostic + payload).
pub const WHAT_MAX_BYTES: usize = 2048;
/// Max bytes of a rendered condition payload (within the what section).
pub const PAYLOAD_MAX_BYTES: usize = 1024;
/// Max call-stack frames named in the where section (innermost kept).
pub const STACK_MAX_FRAMES: usize = 8;
/// Console lines quoted in a report (tail — the latest output before the
/// stop). The `Console` event itself keeps more; see [`CONSOLE_MAX_LINES`].
pub const CONSOLE_TAIL_LINES: usize = 20;
/// Lines a logged `Console` keeps. It is a **diagnostic stream, not
/// data** — a chatty loop can write megabytes — so it is capped with an
/// explicit truncation marker, and the program's own `return` is the
/// channel for anything that must survive whole.
pub const CONSOLE_MAX_LINES: usize = 2_000;
/// Bytes a logged `Console` keeps, across all its lines.
pub const CONSOLE_MAX_BYTES: usize = 256 * 1024;
/// Per-line clip for quoted console output.
pub const CONSOLE_LINE_MAX_BYTES: usize = 200;
/// Artifact-menu entries shown (most recent kept; older ids stay valid).
pub const MENU_MAX_ENTRIES: usize = 20;
/// Max bytes of one arriving post quoted in a post-condition report.
pub const POST_MAX_BYTES: usize = 1024;
/// Max bytes of the annotated program source in a post-condition report.
pub const ANNOTATED_SOURCE_MAX_BYTES: usize = 4096;
/// Calls named on one annotated source line before it says "and N more".
pub const ANNOTATIONS_PER_LINE: usize = 6;
/// Per-entry preview bytes in the artifact menu.
pub const PREVIEW_MAX_BYTES: usize = 256;

/// One artifact-menu entry: a `Call` (settled or still pending) or a
/// `ProgramResult`, named by its event id and fetchable via
/// `tools.tool_result(id)`.
pub struct Artifact {
    pub id: u64,
    /// Read from the call variant: `ask(to, "…")` / `tell(to, "…")`,
    /// `spawn(name)`, `name(args-preview)`, or `program result`.
    pub label: String,
    pub state: ArtifactState,
}

/// What the menu says about a row, and whether it can be fetched.
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
}

/// What `resume(value)` means for this suspension — the restart section
/// states the exact semantics, which differ by suspension kind.
pub enum ResumeKind {
    /// Suspended on `raise(...)`: `value` becomes the result of the
    /// raise expression.
    Raise,
    /// Suspended on a trapped, resumable error: `value` stands in for
    /// the failed operation's result.
    Operation,
    /// Suspended because a message arrived: nothing asked for a value,
    /// so `resume()` just continues.
    Continue,
    /// Not resumable: `run_program` is the only restart.
    No,
}

/// The **restarts** section: what is eligible for *this suspension*.
///
/// It carries only what is a fact about this handback. The `resume`
/// wording genuinely differs by suspension kind — what `value` means for
/// a raise, for a failed operation, or for a program merely parked —
/// so it belongs in a message that renders identically forever.
///
/// Two things that used to be here are not:
///
/// - **the open-post list.** Whether a question is still owed is a fact
///   about the branch *now*, not about this handback, and a rendered
///   message keeps saying it forever: `answer(#4, value)` stays correct
///   for the moment it describes while becoming a standing invitation to
///   make an ineligible call. Obligations ride the trailing ephemeral
///   line, which is always exactly one and always current.
/// - **the explanation of `run_program`.** That is a *rule*, constant in
///   every report ever rendered, and the cache-discipline split puts
///   rules in the card where they are cached for the branch's life. The
///   name stays, because which restarts are eligible is still a
///   report-level fact; the forty words do not.
pub struct Restarts {
    pub resume: ResumeKind,
}

impl Restarts {
    fn render(&self) -> String {
        let mut out = String::from("## restarts\n");
        match self.resume {
            ResumeKind::Raise => out.push_str(
                "- resume(value): continue past the raise; `value` becomes \
                 the result of the raise(...) expression\n",
            ),
            ResumeKind::Operation => out.push_str(
                "- resume(value): continue as if the failed operation had \
                 produced `value`\n",
            ),
            ResumeKind::Continue => out.push_str(
                "- resume(): continue the program from where it stopped — nothing here \
                 asked for a value\n",
            ),
            ResumeKind::No => {
                out.push_str("(this condition is not resumable — resume is not offered)\n")
            }
        }
        out.push_str("- run_program(source)");
        out
    }
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

/// The `run_program` tool result for a raise/trapped error.
pub struct ConditionReport {
    /// Rendered diagnostic: condition name + payload, or the trapped
    /// error with source line and caret.
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
    pub restarts: Restarts,
}

impl ConditionReport {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("## what happened\n");
        out.push_str(&clip(&self.what, WHAT_MAX_BYTES));
        out.push_str("\n\n## where\n");
        match &self.whence {
            Whence::Stack(stack) => out.push_str(&render_stack(stack)),
            Whence::AnnotatedSource(source) => out.push_str(source),
        }
        out.push('\n');
        out.push_str(&render_console(&self.console, self.console_id));
        out.push_str("\n\n");
        out.push_str(&render_menu("new artifacts", &self.artifacts));
        out.push_str("\n\n");
        out.push_str(&self.restarts.render());
        out
    }
}

/// The `run_program` tool result for a successful run.
pub struct CompletionReport {
    /// The program's top-level return value — the agent's *answer* into
    /// context, rendered up to [`Self::budget`] with the full value kept
    /// as a fetchable `ProgramResult` artifact.
    pub value: serde_json::Value,
    /// Byte budget for the returned-value section (the agent's answer
    /// budget). Replaces the fixed `VALUE_MAX_BYTES` clip.
    pub budget: usize,
    /// Full console log (the renderer tails it).
    pub console: Vec<String>,
    /// The `Console` event the tail comes from, named when it clips.
    pub console_id: Option<u64>,
    /// Artifacts logged since the run started (its `ProgramResult`
    /// included), oldest first.
    pub new_artifacts: Vec<Artifact>,
    /// A file body longer than a snippet was inlined into `source` while
    /// this run passed no `attachments` — nudge toward the attachments
    /// channel. Computed by the machine (it has the full, unclipped args).
    pub advise_attachments: bool,
    /// How many of this run's calls came back `Failed`.
    ///
    /// The risk the "only handbacks log a condition" rule leaves is
    /// **silent degradation**: a program that swallows five failures and
    /// returns a thin result, with a completion report that reads as
    /// success. The fix belongs in the report, not in a new event — so
    /// the report counts them and says so.
    pub failed_calls: usize,
}

impl CompletionReport {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("## program completed\n");
        out.push_str("returned: ");
        out.push_str(&clip_answer(
            &self.value.to_string(),
            self.budget,
            self.result_id(),
        ));
        out.push_str("\n\n");
        out.push_str(&render_console(&self.console, self.console_id));
        out.push_str("\n\n");
        out.push_str(&render_menu("new artifacts", &self.new_artifacts));

        if self.failed_calls > 0 {
            out.push_str(&format!(
                "\n\n## calls that failed\n{} of this run's calls came back failed. If your \
                 result reflects that, say so; if the program swallowed them, this report \
                 is not the success it looks like. Each failure's reason is fetchable by \
                 id from the menu above.",
                self.failed_calls
            ));
        }

        let mut notes: Vec<&str> = Vec::new();
        if self.wrote_without_verifying() {
            notes.push(
                "This program wrote files but didn't check them. Don't report success \
                 unverified — verify now (`parse_errors`, a re-read, or a `bash` \
                 build/test). Next time, fold that check into the same program that \
                 does the writing, not a separate one.",
            );
        }
        if self.advise_attachments {
            notes.push(
                "A file body longer than a few lines was inlined into `source`. Pass \
                 it through run_program's `attachments` map instead and read it as \
                 `attachments.<name>` — keeps `source` small and the content inert \
                 (no JS-string escaping, no backtick/${} corruption).",
            );
        }
        if !notes.is_empty() {
            out.push_str("\n\n## note");
            for note in notes {
                out.push_str("\n\n");
                out.push_str(note);
            }
        }
        out
    }

    /// The `ProgramResult` artifact id (this run's full return value),
    /// named in the truncation marker so an over-budget answer stays
    /// fetchable. It is the `program result`-labelled entry among the
    /// run's new artifacts (`machine.rs` logs exactly one).
    fn result_id(&self) -> Option<u64> {
        self.new_artifacts
            .iter()
            .rev()
            .find(|a| a.label == "program result")
            .map(|a| a.id)
    }

    /// Whether this run created or replaced files but never inspected
    /// them in the same program — the nudge condition: validation of a
    /// write belongs in the program that wrote it, not a follow-up
    /// `run_program` (the split-validation habit the card warns against).
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
/// `tools.tool_result` reads that event's lines.
fn render_console(lines: &[String], event: Option<u64>) -> String {
    if lines.is_empty() {
        return "console: (no output)".into();
    }
    let start = lines.len().saturating_sub(CONSOLE_TAIL_LINES);
    let shown = &lines[start..];
    let mut out = format!("console (last {} of {} lines", shown.len(), lines.len());
    // Only a clip names the id — an untruncated tail has nothing behind
    // it to fetch, and the wording stays as it was.
    if let (true, Some(id)) = (start > 0, event) {
        out.push_str(&format!(" — tools.tool_result({id}) for all of them"));
    }
    out.push_str("):");
    for line in shown {
        out.push('\n');
        out.push_str(&clip(line, CONSOLE_LINE_MAX_BYTES));
    }
    out
}

fn render_menu(title: &str, artifacts: &[Artifact]) -> String {
    let mut out = format!("## {title} — fetch with tools.tool_result(id)");
    if artifacts.is_empty() {
        out.push_str("\n(none)");
        return out;
    }
    let start = artifacts.len().saturating_sub(MENU_MAX_ENTRIES);
    if start > 0 {
        out.push_str(&format!(
            "\n({start} older artifacts omitted; their ids stay fetchable)"
        ));
    }
    for a in &artifacts[start..] {
        let tail = match &a.state {
            ArtifactState::Delivered(v) => preview(v),
            ArtifactState::Failed(msg) => format!("failed: {}", clip(msg, PREVIEW_MAX_BYTES)),
            ArtifactState::PendingSend => {
                format!("pending — await tools.tool_result(#{})", a.id)
            }
            ArtifactState::PendingInvoke => "issued; no result recorded; may have happened".into(),
        };
        out.push_str(&format!("\n[#{}] {} → {}", a.id, a.label, tail));
    }
    out
}

// ── rendered messages ───────────────────────────────────────────────

/// Max bytes of a post's rendered `input` preview.
pub const INPUT_PREVIEW_MAX_BYTES: usize = 512;
/// Object keys / array entries named in an `input` preview.
pub const INPUT_PREVIEW_MAX_KEYS: usize = 12;

/// Render a `Post` into the text an LLM sees: the body, author-labelled
/// when it did not come from the person driving the session, plus a
/// **bounded preview** of any machine-bound `input`.
///
/// The preview is the whole point: the full value reaches the *program*
/// as the `input` const, so rendering it in full would dump a caller's
/// data into the callee's context — exactly what by-reference travel
/// exists to prevent.
pub fn render_post(from: Author, origin: &Origin) -> String {
    let Some((text, input, _)) = origin.direct() else {
        // An unresolved reference should never reach a renderer: a
        // `Context` materialises bodies. Say so rather than render a lie.
        return "(message body unavailable)".to_owned();
    };
    let mut out = String::new();
    match from {
        Author::User => {}
        Author::Harness => out.push_str("[harness] "),
        Author::Agent(id) => out.push_str(&format!("[agent {}] ", id.as_u64())),
    }
    out.push_str(text);
    if !input.is_null() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("input: ");
        out.push_str(&input_preview(input));
    }
    out
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

/// Clip an agent's *answer* (the returned value) to its budget. Unlike
/// [`clip`], an over-budget answer's marker names the fetch id so the full
/// value stays reachable (`tools.tool_result(#id)`) — the answer is the
/// one value the model may genuinely need in full (12_ANSWERS).
pub fn clip_answer(s: &str, max: usize, id: Option<u64>) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    match id {
        Some(id) => format!(
            "{}… [+{} B — tools.tool_result(#{})]",
            &s[..end],
            s.len() - end,
            id
        ),
        None => format!("{}… [truncated; {} bytes total]", &s[..end], s.len()),
    }
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

/// One program run's slice of a branch's path: the turn that drove it,
/// the outcome it produced, and the events in between.
struct Handback<'t> {
    /// The source the turn asked to run (empty for a non-`run_program`).
    source: String,
    /// Whether the turn passed a non-empty `attachments` map.
    had_attachments: bool,
    /// The one outcome event: a `Return` or a `Condition`.
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

/// The `run_program` source and attachment flag carried by a tool call.
fn program_args(call: &ToolCall) -> (String, bool) {
    let source = call
        .arguments
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let had_attachments = call
        .arguments
        .get("attachments")
        .and_then(|v| v.as_object())
        .is_some_and(|m| !m.is_empty());
    (source, had_attachments)
}

/// Whether a payload is an outcome — the event a tool call is answered
/// from. **Every tool call has exactly one**, which is what lets every
/// report derive from one event rather than from recomputed history:
/// `run_program`/`resume` produce a `Return` or a `Condition`, an
/// ineligible call a `Condition{Refused}`, and `answer` an `Answer`.
fn is_outcome(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::Return { .. } | EventPayload::Condition { .. } | EventPayload::Answer { .. }
    )
}

/// Every outcome a turn produced, in log order, ending before the next
/// `Turn`. Pairing is **positional, not stored**: outcome `k` answers
/// tool call `k`, which is why the machine logs a deferred refusal after
/// the outcome of the call that preceded it.
pub fn outcomes_of_turn(tree: &Tree, leaf: EventId, turn: EventId) -> Vec<EventId> {
    let path = tree.path_events(leaf);
    let Some(at) = path.iter().position(|e| e.id == turn) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for event in &path[at + 1..] {
        if matches!(event.payload, EventPayload::Message(Message::Turn { .. })) {
            break;
        }
        if is_outcome(&event.payload) {
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
        .rposition(|e| is_outcome(&e.payload))
        .map(|i| path[i].id.as_u64())
        .unwrap_or(0);
    // The turn this outcome belongs to: the nearest `Turn` above it.
    let turn_at = path[..at]
        .iter()
        .rposition(|e| matches!(e.payload, EventPayload::Message(Message::Turn { .. })))?;
    let turn = path[turn_at];
    let EventPayload::Message(Message::Turn { tool_calls, .. }) = &turn.payload else {
        return None;
    };
    let (source, had_attachments) = tool_calls
        .iter()
        .find(|c| c.name == crate::machine::TOOL_RUN_PROGRAM)
        .map(program_args)
        .unwrap_or_default();
    // The `Console` logged with this outcome sits immediately after it,
    // before the next outcome.
    let (console, console_id) = path[at + 1..]
        .iter()
        .take_while(|e| !is_outcome(&e.payload))
        .find_map(|e| match &e.payload {
            EventPayload::Console { lines } => Some((lines.clone(), Some(e.id.as_u64()))),
            _ => None,
        })
        .unwrap_or_default();
    Some(Handback {
        tree,
        source,
        had_attachments,
        outcome: path[at],
        console,
        console_id,
        path: path.clone(),
        turn_at,
        outcome_at: at,
        previous_outcome,
    })
}

/// Render the tool-role message answering the call that produced
/// `outcome`. Memoised on the `Tree` by the outcome's id — the report is
/// a pure function of the log, so the same outcome always renders the
/// same string for a given renderer.
pub fn derive_report(tree: &Tree, leaf: EventId, outcome: EventId, budget: usize) -> String {
    if let Some(hit) = tree.memoised_report(outcome) {
        return hit;
    }
    let text = match handback(tree, leaf, outcome) {
        Some(h) => render_handback(&h, budget),
        None => "(no outcome recorded for this call)".to_owned(),
    };
    tree.memoise_report(outcome, text.clone());
    text
}

fn render_handback(h: &Handback<'_>, budget: usize) -> String {
    match &h.outcome.payload {
        EventPayload::Return { value } => CompletionReport {
            value: value.clone(),
            budget,
            console: h.console.clone(),
            console_id: h.console_id,
            new_artifacts: menu_since(h, h.previous_outcome),
            advise_attachments: !h.had_attachments && inlined_large_body(h),
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
        EventPayload::Condition { cause, site, stack } => match cause {
            // A compile failure ran no VM: the diagnostic alone, no
            // console and no artifacts.
            Cause::CompileFailed { message } => message.clone(),
            // A refusal changes no state; it states what is true and what
            // is valid now, so the model recovers on its next turn.
            Cause::Refused { reason } => format!("refused: {reason}"),
            _ => ConditionReport {
                what: what_happened(h, cause, *site),
                // A post stopped the program nowhere in particular: the
                // useful "where" is the whole program with its progress
                // marked, which is what a rewrite copy-edits.
                whence: match cause {
                    Cause::Posted { .. } => Whence::AnnotatedSource(annotated_source(h)),
                    _ => Whence::Stack(stack.clone()),
                },
                console: h.console.clone(),
                console_id: h.console_id,
                artifacts: menu_since(h, h.previous_outcome),
                restarts: Restarts {
                    resume: resume_kind(cause),
                },
            }
            .render(),
        },
        // The **answer ack**: what was answered and where it went. The
        // API needs every tool call replied to, and `answer` is a tool
        // call — without this its `tool_call_id` dangles and the next
        // request is rejected outright.
        EventPayload::Answer { question, value } => answer_ack(h, *question, value, budget),
        _ => "(not an outcome)".to_owned(),
    }
}

/// The tool message answering an `answer(question, value)` call.
///
/// Where it went is walked out of the closed loop of ids —
/// `Answer.question → Post`, `Post.origin → Send`, and the `Send`'s
/// position **is** the asker's branch — so the ack is a pure function of
/// the log like every other report.
fn answer_ack(
    h: &Handback<'_>,
    question: EventId,
    value: &serde_json::Value,
    budget: usize,
) -> String {
    let id = question.as_u64();
    let routed = match h.tree.events.get(&question).map(|e| &e.payload) {
        Some(EventPayload::Message(Message::Post { from, origin })) => match origin {
            Origin::Sent(send) => match h.tree.branch_of(*send) {
                Some(branch) => format!("delivered to branch #{}", branch.as_u64()),
                None => "delivered to whoever sent it".to_owned(),
            },
            // The user has no branch and no program, so there is nothing
            // to settle: they read it where it sits.
            Origin::Direct { .. } => match from {
                Author::User => "read inline by the user, who has no branch to deliver to".into(),
                _ => "read where it sits".to_owned(),
            },
        },
        _ => format!("#{id} is not a post"),
    };
    format!(
        "## answered\n#{id} — {routed}\n\nvalue: {}",
        clip_answer(&value.to_string(), budget, None)
    )
}

/// The "what happened" diagnostic, rebuilt from the logged cause, the
/// logged site, and the source in the turn's tool-call args — the three
/// inputs that used to live only in the VM.
fn what_happened(h: &Handback<'_>, cause: &Cause, site: u32) -> String {
    let source = &h.source;
    match cause {
        Cause::Raised { name, payload } => {
            let mut what = diagnostic(source, site, &format!("condition `{name}` raised"));
            what.push_str("\npayload: ");
            let rendered = payload
                .as_ref()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "(none)".into());
            what.push_str(&clip(&rendered, PAYLOAD_MAX_BYTES));
            what
        }
        Cause::Trapped { message, .. } => diagnostic(source, site, message),
        // The product surface of this phase: the report a running branch
        // gets when someone speaks to it. What happened **is** the
        // message — author-labelled, and marked with what it owes you.
        Cause::Posted { ids } => {
            let mut what = String::from(
                "Someone spoke to you while your program was running. It is paused at \
                 its last fuel slice; nothing was lost.\n",
            );
            for id in ids {
                let Some(EventPayload::Message(post)) = h.tree.events.get(id).map(|e| &e.payload)
                else {
                    continue;
                };
                let Message::Post { from, origin } = h.tree.resolve(post) else {
                    continue;
                };
                // *asks you* versus *tells you*: what it owes, which is
                // the difference between `ask` and `tell` and the only
                // thing the model has to decide about differently.
                let owed = match origin.direct() {
                    Some((_, _, true)) => "asks you",
                    _ => "tells you",
                };
                what.push_str(&format!(
                    "\n[#{}] {} — {}\n{}\n",
                    id.as_u64(),
                    author_label(from),
                    owed,
                    clip(&render_post(from, &origin), POST_MAX_BYTES),
                ));
            }
            what.trim_end().to_owned()
        }
        Cause::Interrupted => {
            "This program was interrupted before completing — the process died and the VM \
             went with it. The artifacts below are still fetchable by id; rewrite to \
             continue."
                .to_owned()
        }
        Cause::CompileFailed { message } | Cause::Refused { reason: message } => message.clone(),
    }
}

/// Who a post is from, for the report's author label. A post from the
/// person driving the session is unlabelled in the transcript, but a
/// report *about* an arrival has to name them.
fn author_label(from: Author) -> String {
    match from {
        Author::User => "the user".into(),
        Author::Harness => "the harness".into(),
        Author::Agent(id) => format!("agent {}", id.as_u64()),
    }
}

/// `line:col: message` with the source line and a caret, or the bare
/// message when there is no source to point into.
fn diagnostic(source: &str, site: u32, message: &str) -> String {
    if source.is_empty() {
        return message.to_owned();
    }
    interp::Diagnostic {
        kind: interp::DiagKind::Semantic,
        span: site,
        message: message.to_owned(),
    }
    .render(source)
}

fn resume_kind(cause: &Cause) -> ResumeKind {
    match cause {
        Cause::Raised { .. } => ResumeKind::Raise,
        Cause::Trapped {
            resumable: true, ..
        } => ResumeKind::Operation,
        // A post suspended the program; nothing asked for a value, so
        // `resume()` just continues (B3 renders this one).
        Cause::Posted { .. } => ResumeKind::Continue,
        _ => ResumeKind::No,
    }
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
    crate::machine::menu_rows(&segment, since)
}

/// The program source with **every call site annotated by its
/// artifact** — the section that makes "change course" a copy-edit
/// rather than a reconstruction.
///
/// `site` on every `Call` is what makes this possible: a byte offset
/// logged at dispatch, so the annotation is derived from the log with no
/// live VM. Pending *sends* are re-awaitable by id and say so; pending
/// host calls are not, and say that instead.
fn annotated_source(h: &Handback<'_>) -> String {
    // Line starts, so a byte offset becomes a line index.
    let line_of = |offset: u32| -> usize {
        h.source
            .bytes()
            .take(offset as usize)
            .filter(|b| *b == b'\n')
            .count()
    };
    let mut notes: Vec<Vec<String>> = vec![Vec::new(); h.source.lines().count().max(1)];
    let segment: Vec<&Event> = h.path[..=h.outcome_at].to_vec();
    for event in &h.path[h.turn_at + 1..=h.outcome_at] {
        let EventPayload::Call(call) = &event.payload else {
            continue;
        };
        let id = event.id.as_u64();
        let note = match crate::machine::settlement_of(&segment, event.id) {
            Some(crate::types::Outcome::Delivered(_)) => format!("#{id} done"),
            Some(crate::types::Outcome::Failed(_)) => format!("#{id} failed"),
            None => match call {
                Call::Send { .. } => {
                    format!("#{id} pending — await tools.tool_result({id})")
                }
                _ => format!("#{id} issued; may have happened"),
            },
        };
        let line = line_of(call.site()).min(notes.len().saturating_sub(1));
        notes[line].push(note);
    }
    let mut out = String::new();
    for (n, line) in h.source.lines().enumerate() {
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

/// **What a `Fork` renders as** — the honest lever, and the only one.
///
/// Retry and sidebar are not modes: fork *before* a question and say "try
/// again with X", or fork *at the running leaf* and ask "what are you
/// doing?". The same gesture, and this line is what tells the model which
/// it is.
///
/// Two shapes, chosen by whether the fork point left a tool call
/// unanswered on this path:
///
/// - an **ordinary** fork point: a harness line naming the branch the
///   pre-fork questions stayed with.
/// - a **mid-program** fork point: that call's tool result, naming the
///   original branch and the artifacts so far — which also keeps the
///   API's adjacency rule satisfied instead of leaving a dangling call.
///
/// Every input is an event id, so it renders identically forever.
pub fn render_fork(
    tree: &Tree,
    leaf: EventId,
    fork: EventId,
    dangling: &[ToolCall],
) -> Vec<crate::machine::Rendered> {
    let at = tree.events.get(&fork).and_then(|e| e.parent_id);
    let origin = at.and_then(|at| tree.branch_of(at));
    let branch = origin
        .map(|b| format!("#{}", b.as_u64()))
        .unwrap_or_else(|| "the original".to_owned());
    if dangling.is_empty() {
        let point = at
            .map(|at| format!(" at #{}", at.as_u64()))
            .unwrap_or_default();
        return vec![crate::machine::Rendered::User(format!(
            "[harness] fork of branch {branch}{point} — questions before this line are \
             being handled there; do not redo its work unless asked."
        ))];
    }
    // The fork point's last turn is still running on the original. Its
    // artifacts crossed the fork (history and artifacts do); the run
    // itself did not (the VM is never copied).
    let path = tree.path_events(leaf);
    let fork_at = path.iter().position(|e| e.id == fork).unwrap_or(0);
    let turn_at = path[..fork_at]
        .iter()
        .rposition(|e| matches!(e.payload, EventPayload::Message(Message::Turn { .. })));
    let program = turn_at
        .map(|i| format!("#{}", path[i].id.as_u64()))
        .unwrap_or_else(|| "the program".to_owned());
    let start = path[..fork_at]
        .iter()
        .rposition(|e| matches!(e.payload, EventPayload::Agent { .. }))
        .unwrap_or(0);
    let segment: Vec<&Event> = path[start..fork_at].to_vec();
    let artifacts = crate::machine::menu_rows(&segment, 0);
    let head = format!(
        "program {program} is running on branch {branch}, not here. This fork inherited \
         its history and its artifacts; the run itself stayed there, so nothing you do \
         here disturbs it.\n\n{}",
        render_menu("artifacts so far", &artifacts)
    );
    dangling
        .iter()
        .map(|call| crate::machine::Rendered::Tool {
            call_id: call.id.clone(),
            text: head.clone(),
        })
        .collect()
}

/// Whether this run inlined a file body longer than a snippet into
/// `source` — the nudge condition, read off the logged call args.
fn inlined_large_body(h: &Handback<'_>) -> bool {
    h.path[h.turn_at + 1..=h.outcome_at]
        .iter()
        .any(|e| match &e.payload {
            EventPayload::Call(Call::Invoke { name, args, .. })
                if name == "create_file" || name == "replace_file" =>
            {
                args.as_array()
                    .and_then(|a| a.last())
                    .and_then(|v| v.as_str())
                    .is_some_and(|c| c.len() > INLINE_BODY_ADVICE_BYTES)
            }
            _ => false,
        })
}

/// A `create_file`/`replace_file` whose inline content exceeds this draws
/// the attachments nudge (when the run passed no `attachments`).
pub const INLINE_BODY_ADVICE_BYTES: usize = 512;

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

    use crate::types::{Cause, EventPayload, Message, Origin, ToolCall, Tree};

    /// A one-branch fixture log: a `Turn` carrying `run_program(source)`
    /// followed by whatever outcome the caller wants.
    fn fixture(source: &str, outcome: EventPayload) -> (Tree, crate::types::EventId) {
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "SYSTEM")
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Message(Message::Post {
                from: Author::User,
                origin: Origin::Direct {
                    text: "go".into(),
                    input: json!(null),
                    expects_reply: true,
                },
            }),
        )
        .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Message(Message::Turn {
                author: Author::Agent(crate::types::EventId::new(1)),
                text: String::new(),
                thinking: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "run_program".into(),
                    arguments: json!({ "source": source }),
                }],
            }),
        )
        .unwrap();
        let outcome = tree.append(&mut spine, outcome).unwrap();
        (tree, outcome)
    }

    /// Every report kind renders from fixture events alone — no VM, no
    /// live state. This is the whole claim of "reports are derived".
    #[test]
    fn every_report_kind_renders_from_the_log() {
        // Completion.
        let (tree, o) = fixture("return 1;", EventPayload::Return { value: json!(1) });
        let leaf = tree.list_leaves()[0].0;
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert!(text.starts_with("## program completed"), "{text}");
        assert!(text.contains("returned: 1"), "{text}");

        // Condition: a raise, with the caret placed from the logged site
        // and the source read out of the turn's own tool-call args.
        let src = "raise(\"need\", { got: 1 });";
        let (tree, o) = fixture(
            src,
            EventPayload::Condition {
                cause: Cause::Raised {
                    name: "need".into(),
                    payload: Some(json!({ "got": 1 })),
                },
                site: 0,
                stack: vec!["<root>".into()],
            },
        );
        let leaf = tree.list_leaves()[0].0;
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert!(text.contains("1:1: condition `need` raised"), "{text}");
        assert!(text.contains(src), "the source line is quoted: {text}");
        assert!(text.contains(r#"payload: {"got":1}"#), "{text}");
        assert!(text.contains("- resume(value)"), "{text}");

        // Condition: a trapped error, not resumable → no resume offered.
        let (tree, o) = fixture(
            "return null.x;",
            EventPayload::Condition {
                cause: Cause::Trapped {
                    kind: "TypeError".into(),
                    message: "cannot read property 'x' on null".into(),
                    resumable: false,
                },
                site: 7,
                stack: Vec::new(),
            },
        );
        let leaf = tree.list_leaves()[0].0;
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert!(text.contains("1:8: cannot read property"), "{text}");
        assert!(text.contains("not resumable"), "{text}");

        // Compile error: the diagnostic alone — no VM was built, so this
        // run has no console and no artifacts.
        let (tree, o) = fixture(
            "let = ;",
            EventPayload::Condition {
                cause: Cause::CompileFailed {
                    message: "compile error:\n1:5: unexpected token".into(),
                },
                site: 0,
                stack: Vec::new(),
            },
        );
        let leaf = tree.list_leaves()[0].0;
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert_eq!(text, "compile error:\n1:5: unexpected token");

        // Refusal: what is true and what to do, changing no state.
        let (tree, o) = fixture(
            "",
            EventPayload::Condition {
                cause: Cause::Refused {
                    reason: "nothing to resume".into(),
                },
                site: 0,
                stack: Vec::new(),
            },
        );
        let leaf = tree.list_leaves()[0].0;
        assert_eq!(
            derive_report(&tree, leaf, o, 64 * 1024),
            "refused: nothing to resume"
        );

        // The answer ack: what was answered and where it went. The
        // API needs every tool call replied to, and `answer` **is** a
        // tool call, so without this its id dangles and the next request
        // is rejected outright.
        let (mut tree, _) = fixture("", EventPayload::Return { value: json!(1) });
        let leaf = tree.list_leaves()[0].0;
        let mut spine = tree.spine_at(leaf);
        let post = tree
            .append(
                &mut spine,
                EventPayload::Message(Message::Post {
                    from: Author::User,
                    origin: Origin::Direct {
                        text: "which one?".into(),
                        input: serde_json::Value::Null,
                        expects_reply: true,
                    },
                }),
            )
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Message(Message::Turn {
                author: Author::User,
                text: String::new(),
                thinking: None,
                tool_calls: vec![crate::types::ToolCall {
                    id: "a1".into(),
                    name: "answer".into(),
                    arguments: json!({ "question": post.as_u64(), "value": "the second" }),
                }],
            }),
        )
        .unwrap();
        let o = tree
            .append(
                &mut spine,
                EventPayload::Answer {
                    question: post,
                    value: json!("the second"),
                },
            )
            .unwrap();
        let leaf = spine.leaf_id;
        // The `Answer` is an outcome like any other, so the turn's one
        // call is paired with it positionally.
        assert_eq!(
            outcomes_of_turn(&tree, leaf, tree.events[&o].parent_id.unwrap()),
            [o]
        );
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert!(text.starts_with("## answered"), "{text}");
        assert!(text.contains(&format!("#{}", post.as_u64())), "{text}");
        assert!(
            text.contains("read inline by the user"),
            "the user has no branch to deliver to: {text}"
        );
        assert!(text.contains(r#"value: "the second""#), "{text}");

        // Interruption: the VM went with the process; rewrite to continue.
        let (tree, o) = fixture(
            "return 1;",
            EventPayload::Condition {
                cause: Cause::Interrupted,
                site: 0,
                stack: Vec::new(),
            },
        );
        let leaf = tree.list_leaves()[0].0;
        let text = derive_report(&tree, leaf, o, 64 * 1024);
        assert!(text.contains("interrupted before completing"), "{text}");
        assert!(text.contains("not resumable"), "{text}");
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
        let mut spine = tree.start_agent(None, None, "agent", None, "").unwrap();
        let mut ids = Vec::new();
        let mut outcomes = Vec::new();
        // Two handbacks, each with its own call and result.
        for run in 0..2 {
            tree.append(
                &mut spine,
                EventPayload::Message(Message::Turn {
                    author: Author::Agent(EventId::new(1)),
                    text: String::new(),
                    thinking: None,
                    tool_calls: vec![crate::types::ToolCall {
                        id: format!("c{run}"),
                        name: "run_program".into(),
                        arguments: json!({ "source": "return 1;" }),
                    }],
                }),
            )
            .unwrap();
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
                tree.append(&mut spine, EventPayload::Return { value: json!(run) })
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
        // …and no gaps: every artifact — each call, and each run's own
        // `Return` — is listed by exactly one report.
        let every: Vec<u64> = ids
            .iter()
            .copied()
            .chain(outcomes.iter().map(|o| o.as_u64()))
            .collect();
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

    #[test]
    fn console_tails_with_counts_and_clips_lines() {
        let mut lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        lines.push("y".repeat(1000));
        let rendered = render_console(&lines, None);
        assert!(rendered.starts_with("console (last 20 of 31 lines):"));
        // 31 lines, tail of 20: lines 0–10 dropped, 11–29 + long kept.
        assert!(!rendered.contains("line 0"), "older lines dropped");
        assert!(!rendered.contains("line 10\n"), "older lines dropped");
        assert!(rendered.contains("line 11"), "tail kept");
        assert!(rendered.contains("[truncated; 1000 bytes total]"));
    }

    #[test]
    fn menu_prunes_to_recent_entries() {
        let artifacts: Vec<Artifact> = (1..=25)
            .map(|i| artifact(i, &format!("tool_{i}([])"), json!(i)))
            .collect();
        let rendered = render_menu("artifacts", &artifacts);
        assert!(rendered.contains("(5 older artifacts omitted; their ids stay fetchable)"));
        assert!(!rendered.contains("[#5]"), "old entries gone");
        assert!(rendered.contains("[#6]") && rendered.contains("[#25]"));
    }

    /// The two pending kinds render differently because only one can be
    /// re-attached: a `Send`'s answer is still coming and is awaited by
    /// id, while an `Invoke`'s worker died with the process.
    #[test]
    fn pending_rows_say_which_can_be_reattached() {
        let rendered = render_menu(
            "artifacts",
            &[
                pending(11, "ask(#3, \"which file?\")", ArtifactState::PendingSend),
                pending(12, "send_email([\"…\"])", ArtifactState::PendingInvoke),
                pending(
                    13,
                    "fetch([\"x\"])",
                    ArtifactState::Failed("host is down".into()),
                ),
            ],
        );
        assert!(
            rendered.contains(
                "[#11] ask(#3, \"which file?\") → pending — await tools.tool_result(#11)"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "[#12] send_email([\"…\"]) → issued; no result recorded; may have happened"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("[#13] fetch([\"x\"]) → failed: host is down"),
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
            what: "w".repeat(10_000),
            whence: Whence::Stack(vec!["<root>".into()]),
            console: Vec::new(),
            console_id: None,
            artifacts: Vec::new(),
            restarts: Restarts {
                resume: ResumeKind::Raise,
            },
        };
        let rendered = report.render();
        let what = rendered.split("\n\n## where").next().unwrap();
        assert!(what.len() < WHAT_MAX_BYTES + 100);
        assert!(what.contains("[truncated; 10000 bytes total]"));
    }

    #[test]
    fn answer_within_budget_is_verbatim() {
        // An answer that fits the budget is delivered in full — the read /
        // summarize happy path (12_ANSWERS).
        let report = CompletionReport {
            value: json!("z".repeat(5_000)),
            budget: 64 * 1024,
            console: Vec::new(),
            console_id: None,
            new_artifacts: Vec::new(),
            advise_attachments: false,
            failed_calls: 0,
        };
        let rendered = report.render();
        let line = rendered.lines().nth(1).unwrap();
        assert!(line.contains(&"z".repeat(5_000)), "delivered in full");
        assert!(
            !line.contains("tools.tool_result"),
            "no spill marker: {line}"
        );
    }

    #[test]
    fn over_budget_answer_names_fetch_id() {
        // Past budget the context copy is truncated, but the marker names
        // the `ProgramResult` id so the full value stays fetchable.
        let report = CompletionReport {
            value: json!("z".repeat(5_000)),
            budget: 1_000,
            console: Vec::new(),
            console_id: None,
            new_artifacts: vec![artifact(9, "program result", json!("z".repeat(5_000)))],
            advise_attachments: false,
            failed_calls: 0,
        };
        let rendered = report.render();
        let line = rendered.lines().nth(1).unwrap();
        assert!(line.contains("tools.tool_result(#9)"), "{line}");
    }

    #[test]
    fn artifact_previews_are_bounded() {
        let report = ConditionReport {
            what: "boom".into(),
            whence: Whence::Stack(vec!["<root>".into()]),
            console: Vec::new(),
            console_id: None,
            artifacts: vec![artifact(7, "fetch([\"big\"])", json!("b".repeat(9000)))],
            restarts: Restarts {
                resume: ResumeKind::Operation,
            },
        };
        let rendered = report.render();
        let menu_line = rendered.lines().find(|l| l.starts_with("[#7]")).unwrap();
        assert!(menu_line.len() < PREVIEW_MAX_BYTES + 100, "{menu_line}");
        assert!(menu_line.contains("[truncated; 9002 bytes total]"));
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
            value: json!({ "status": "done" }),
            budget: 64 * 1024,
            console: Vec::new(),
            console_id: None,
            new_artifacts: artifacts,
            advise_attachments: false,
            failed_calls: 0,
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
        assert!(rendered.contains("## note"), "{rendered}");
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
                !rendered.contains("## note"),
                "unexpected nudge: {rendered}"
            );
        }
    }

    #[test]
    fn inlined_body_draws_the_attachments_nudge() {
        let report = CompletionReport {
            value: json!("done"),
            budget: 64 * 1024,
            console: Vec::new(),
            console_id: None,
            new_artifacts: vec![artifact(5, "create_file([\"/x/a.js\", \"…\"])", json!({}))],
            advise_attachments: true,
            failed_calls: 0,
        }
        .render();
        assert!(report.contains("## note"), "{report}");
        assert!(report.contains("inlined into `source`"), "{report}");
        assert!(report.contains("attachments"), "{report}");
    }

    #[test]
    fn no_write_no_nudge() {
        // A pure read/compute program never gets the write nudge, even
        // with a setup-only bash call.
        let rendered = completion(vec![
            artifact(4, "bash([\"mkdir -p /x\"])", json!({ "status": 0 })),
            artifact(5, "read_file([\"/x/a.js\"])", json!({ "content": "…" })),
        ]);
        assert!(!rendered.contains("## note"), "{rendered}");
    }

    #[test]
    fn not_resumable_drops_resume_and_says_so() {
        let report = ConditionReport {
            what: "stack overflow".into(),
            whence: Whence::Stack(Vec::new()),
            console: Vec::new(),
            console_id: None,
            artifacts: Vec::new(),
            restarts: Restarts {
                resume: ResumeKind::No,
            },
        };
        let rendered = report.render();
        assert!(!rendered.contains("- resume(value)"));
        assert!(rendered.contains("not resumable"));
        assert!(rendered.contains("- run_program(source)"));
    }
}
