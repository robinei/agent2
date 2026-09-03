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
    /// Not resumable: `run_program` is the only restart.
    No,
}

/// The `run_program` tool result for a raise/trapped error.
pub struct ConditionReport {
    /// Rendered diagnostic: condition name + payload, or the trapped
    /// error with source line and caret.
    pub what: String,
    /// Call-stack function names, outermost first.
    pub stack: Vec<String>,
    /// Full console log (the renderer tails it).
    pub console: Vec<String>,
    /// Every artifact on the agent so far, oldest first (the renderer
    /// prunes to the most recent).
    pub artifacts: Vec<Artifact>,
    pub resume: ResumeKind,
}

impl ConditionReport {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("## what happened\n");
        out.push_str(&clip(&self.what, WHAT_MAX_BYTES));
        out.push_str("\n\n## where\n");
        out.push_str(&render_stack(&self.stack));
        out.push('\n');
        out.push_str(&render_console(&self.console));
        out.push_str("\n\n");
        out.push_str(&render_menu("artifacts", &self.artifacts));
        out.push_str("\n\n## restarts\n");
        match self.resume {
            ResumeKind::Raise => out.push_str(
                "- resume(value): continue past the raise; `value` becomes \
                 the result of the raise(...) expression\n",
            ),
            ResumeKind::Operation => out.push_str(
                "- resume(value): continue as if the failed operation had \
                 produced `value`\n",
            ),
            ResumeKind::No => {
                out.push_str("(this condition is not resumable — resume is not offered)\n")
            }
        }
        out.push_str(
            "- run_program(source): replace the program — new source runs in a \
             fresh VM; results in the artifact menu stay fetchable via \
             tools.tool_result(id), so reuse them instead of repeating calls",
        );
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
    /// Artifacts logged since the run started (its `ProgramResult`
    /// included), oldest first.
    pub new_artifacts: Vec<Artifact>,
    /// A file body longer than a snippet was inlined into `source` while
    /// this run passed no `attachments` — nudge toward the attachments
    /// channel. Computed by the machine (it has the full, unclipped args).
    pub advise_attachments: bool,
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
        out.push_str(&render_console(&self.console));
        out.push_str("\n\n");
        out.push_str(&render_menu("new artifacts", &self.new_artifacts));

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

fn render_console(lines: &[String]) -> String {
    if lines.is_empty() {
        return "console: (no output)".into();
    }
    let start = lines.len().saturating_sub(CONSOLE_TAIL_LINES);
    let shown = &lines[start..];
    let mut out = format!("console (last {} of {} lines):", shown.len(), lines.len());
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
pub const REPORT_FORMAT_VERSION: u32 = 1;

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
    /// The `Turn` whose tool call this report answers.
    turn: &'t Event,
    /// The source the turn asked to run (empty for a non-`run_program`).
    source: String,
    /// Whether the turn passed a non-empty `attachments` map.
    had_attachments: bool,
    /// The one outcome event: a `Return` or a `Condition`.
    outcome: &'t Event,
    /// The `Console` logged with the outcome, if any.
    console: Vec<String>,
    /// The path up to `leaf`, for the artifact menu.
    path: Vec<&'t Event>,
    /// Index of `turn` within `path`.
    turn_at: usize,
    /// Index of `outcome` within `path`. The menu stops here: a report
    /// must render identically **forever**, so a historical one cannot
    /// grow new rows as the branch continues past it.
    outcome_at: usize,
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

/// Whether a payload is one of the two outcome kinds.
fn is_outcome(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::Return { .. } | EventPayload::Condition { .. }
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
    let console = path[at + 1..]
        .iter()
        .take_while(|e| !is_outcome(&e.payload))
        .find_map(|e| match &e.payload {
            EventPayload::Console { lines } => Some(lines.clone()),
            _ => None,
        })
        .unwrap_or_default();
    Some(Handback {
        turn,
        source,
        had_attachments,
        outcome: path[at],
        console,
        path: path.clone(),
        turn_at,
        outcome_at: at,
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
            new_artifacts: menu_since(h, h.turn.id.as_u64()),
            advise_attachments: !h.had_attachments && inlined_large_body(h),
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
                what: what_happened(cause, *site, &h.source),
                stack: stack.clone(),
                console: h.console.clone(),
                artifacts: menu_since(h, 0),
                resume: resume_kind(cause),
            }
            .render(),
        },
        _ => "(not an outcome)".to_owned(),
    }
}

/// The "what happened" diagnostic, rebuilt from the logged cause, the
/// logged site, and the source in the turn's tool-call args — the three
/// inputs that used to live only in the VM.
fn what_happened(cause: &Cause, site: u32, source: &str) -> String {
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
        Cause::Posted { ids } => {
            let names: Vec<String> = ids.iter().map(|id| format!("#{}", id.as_u64())).collect();
            format!(
                "message(s) arrived while the program was running: {}",
                names.join(", ")
            )
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
        let mut spine = tree.start_agent(None, None, "root", "SYSTEM").unwrap();
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
        let rendered = render_console(&lines);
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
            stack: vec!["<root>".into()],
            console: Vec::new(),
            artifacts: Vec::new(),
            resume: ResumeKind::Raise,
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
            new_artifacts: Vec::new(),
            advise_attachments: false,
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
            new_artifacts: vec![artifact(9, "program result", json!("z".repeat(5_000)))],
            advise_attachments: false,
        };
        let rendered = report.render();
        let line = rendered.lines().nth(1).unwrap();
        assert!(line.contains("tools.tool_result(#9)"), "{line}");
    }

    #[test]
    fn artifact_previews_are_bounded() {
        let report = ConditionReport {
            what: "boom".into(),
            stack: vec!["<root>".into()],
            console: Vec::new(),
            artifacts: vec![artifact(7, "fetch([\"big\"])", json!("b".repeat(9000)))],
            resume: ResumeKind::Operation,
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
            new_artifacts: artifacts,
            advise_attachments: false,
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
            new_artifacts: vec![artifact(5, "create_file([\"/x/a.js\", \"…\"])", json!({}))],
            advise_attachments: true,
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
            stack: Vec::new(),
            console: Vec::new(),
            artifacts: Vec::new(),
            resume: ResumeKind::No,
        };
        let rendered = report.render();
        assert!(!rendered.contains("- resume(value)"));
        assert!(rendered.contains("not resumable"));
        assert!(rendered.contains("- run_program(source)"));
    }
}
