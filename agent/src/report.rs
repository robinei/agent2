//! Condition + completion reports (8_HARNESS Step 4).
//!
//! The `run_program` tool result is the product surface of the whole
//! project: it is what the LLM reads to decide how to restart a failed
//! program. The renderers here are pure — structured input in, string
//! out, no `Tree`/`VM` access — so golden tests can assert exact bytes,
//! and every section carries a hard size bound (the context-growth
//! mitigation locked in the plan: bounded reports, menu pruned to
//! recent entries, full data always fetchable by id).

/// Max bytes of the "what happened" section (diagnostic + payload).
pub const WHAT_MAX_BYTES: usize = 2048;
/// Max bytes of a rendered condition payload (within the what section).
pub const PAYLOAD_MAX_BYTES: usize = 1024;
/// Max call-stack frames named in the where section (innermost kept).
pub const STACK_MAX_FRAMES: usize = 8;
/// Console lines quoted (tail — the latest output before the stop).
pub const CONSOLE_TAIL_LINES: usize = 20;
/// Per-line clip for quoted console output.
pub const CONSOLE_LINE_MAX_BYTES: usize = 200;
/// Artifact-menu entries shown (most recent kept; older ids stay valid).
pub const MENU_MAX_ENTRIES: usize = 20;
/// Per-entry preview bytes in the artifact menu.
pub const PREVIEW_MAX_BYTES: usize = 256;

/// One artifact-menu entry: an `Invoke` or `ProgramResult` event,
/// fetchable in full via `tools.tool_result(id)`.
pub struct Artifact {
    pub id: u64,
    /// `name(args-preview)` for tool calls, `program result` otherwise.
    pub label: String,
    pub result: serde_json::Value,
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
        return "in (no live contexts)".into();
    }
    if stack.len() > STACK_MAX_FRAMES {
        let omitted = stack.len() - STACK_MAX_FRAMES;
        format!(
            "in … ({omitted} outer contexts omitted) → {}",
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
        out.push_str(&format!(
            "\n[#{}] {} → {}",
            a.id,
            a.label,
            preview(&a.result)
        ));
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn artifact(id: u64, label: &str, result: serde_json::Value) -> Artifact {
        Artifact {
            id,
            label: label.into(),
            result,
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

    #[test]
    fn stack_keeps_innermost_frames() {
        let stack: Vec<String> = (0..12).map(|i| format!("f{i}")).collect();
        let rendered = render_stack(&stack);
        assert!(rendered.contains("(4 outer contexts omitted)"));
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
