use std::collections::HashMap;
use std::num::NonZeroU64;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EventId(NonZeroU64);

/// A half-open byte range of a row's value, as `history.slice` names it.
///
/// Bytes rather than lines, so the offsets a program computes with
/// `content.slice(a, b)` are the offsets it passes here — `.length` in
/// this dialect counts UTF-8 bytes, and two units for one idea is how
/// an off-by-one becomes a mystery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub from: u32,
    pub to: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Event {
    pub id: EventId,
    pub parent_id: Option<EventId>,
    #[serde(with = "jiff::fmt::serde::timestamp::millisecond::required")]
    pub timestamp: Timestamp,
    pub payload: EventPayload,
}

impl Event {
    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }
}

/// Event vocabulary (8_HARNESS Step 1). Two classes:
///
/// - **Chat events** render into LLM requests for their agent.
/// - **Execution events** are harness-internal: queried for replay,
///   artifacts, and UI — never sent to the LLM as messages.
///
/// Each variant documents its parent rule, which spine it lands on, and
/// whether it renders to chat.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum EventPayload {
    /// **A completion begins** (28.A). Parent: the owning agent's spine.
    /// Renders to chat: as the assistant message its parts concatenate
    /// to.
    ///
    /// Allocated *before* a byte arrives, which is what lets everything
    /// in the reply name it and what records a generation that produced
    /// nothing at all — an HTTP 530 used to leave no trace that anything
    /// was attempted.
    ///
    /// It carries no author. A `Reply` is by definition the branch's
    /// LLM, and the branch identifies the agent; a person handing the
    /// branch a cell is [`Restart`](EventPayload::Restart), which is a
    /// different act with none of the completion vocabulary attached.
    Reply,

    /// **One piece of a reply, verbatim** (28.A). Parent: the owning
    /// agent's spine. Renders to chat: as part of the assistant message.
    ///
    /// The parts of a reply concatenate, byte for byte, to the
    /// completion that produced them. That is the invariant the whole
    /// design rests on: nothing has to be reassembled, because nothing
    /// was taken apart.
    Part {
        /// The [`Reply`](EventPayload::Reply) this belongs to.
        reply: EventId,
        part: Part,
    },

    /// **The completion stopped arriving** (28.A), and why. Parent: the
    /// owning agent's spine. Renders to chat: as a marker on the end of
    /// the assistant message when `how` is not `Finished`.
    ///
    /// Distinct from the reply's [`Handback`](EventPayload::Handback):
    /// the text stops when the provider is done, the *work* stops when
    /// the last cell finishes — which can be much later, after a raise
    /// that another reply answered.
    ReplyEnd {
        reply: EventId,
        how: ReplyEnd,
        #[serde(default)]
        usage: crate::host::Usage,
    },

    /// **The conversation is full and a compaction was asked for**
    /// (28). Parent: the owning agent's spine. Renders to chat: no — the
    /// directive rides the ephemeral request tail, because an expired
    /// one left in the history reads as a standing instruction and was
    /// obeyed twice.
    ///
    /// It was `Cause::Compaction`, which made it a *condition*: a
    /// post-mortem of a program that stopped. Nothing stopped. This is a
    /// thing the harness did, and counting the attempts — which is all
    /// anyone reads it for — is not a reason to call it something it is
    /// not.
    ///
    /// Carries the measurement **in the currency that decided**, so the
    /// directive the model reads names the constraint that is actually
    /// full. A token-triggered compaction reporting bytes would be
    /// telling it a true number about the wrong thing.
    Compaction {
        measured: usize,
        limit: usize,
        unit: Measure,
    },

    /// **A person hands the branch one cell** (28.A) — the `e`, `v` and
    /// answer gestures, `SessionCommand::Restart`. Parent: the owning
    /// agent's spine. Renders to chat: as an assistant message, fenced.
    ///
    /// Not a `Reply`, because none of the completion vocabulary
    /// applies: it does not stream, cannot be truncated, has no usage
    /// and no reasoning. Modelling it as a reply with an *author* is
    /// what let the two be confused — and they were, until 2026-09-19.
    ///
    /// It carries no source. What the person handed over arrives as
    /// `Part`s exactly as a model's reply does, so everything that reads
    /// a reply reads this the same way; the only thing this variant says
    /// is who wrote it.
    Restart,

    /// **The reply paused, or ended** (28.A). Parent: the owning agent's
    /// spine. Renders to chat: as the harness's report.
    ///
    /// Effects — `Call`, `Result`, `Note`, `Answer`, `Console` — carry no
    /// reply of their own, because a report is delimited *handback to
    /// handback*: each shows the rows since the previous one, which puts
    /// a resumed tail's rows in the right report with nothing stored.
    ///
    /// A reply hands back **N times and ends once**, which
    /// `Cause`'s own doc said and the type contradicted by calling them
    /// all outcomes. `how` says which this is.
    Handback {
        reply: EventId,
        how: Handback,
        /// Byte offset into the reply where it happened (28.A: sites are
        /// reply-absolute).
        #[serde(default)]
        site: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        stack: Vec<String>,
    },

    /// Chat event. Parent: the previous event on the owning agent's
    /// spine. Renders to chat: yes — a `Turn` is the assistant message,
    /// its `source` the bare program the model ran; the harness's report
    /// on what that program did comes back as a `Post` from
    /// `Author::Harness`, not as a distinguished reply kind of its own.
    Post { from: Author, origin: Origin },

    /// Chat event. Parent: the previous event on the owning agent's
    /// spine. Renders to chat: yes — as a marker in the branch's own
    /// history, nothing more.
    ///
    /// `append_history` is what **I** should remember; a `tell`/`Post`
    /// is what **someone else** needs to know. A `Note` has no
    /// recipient and wakes no branch — where every `tell` is heard by
    /// someone, a note is heard by no one but the branch's own future
    /// self, reading its own history back on a later turn. It exists for
    /// the program that has worked something out and wants it on the
    /// record for its *next* program to see rendered, not merely held in
    /// a variable this run's VM is about to drop — not to read something
    /// back later in the *same* turn, since if you need the value now
    /// you are already holding it.
    Note {
        /// **What was appended, not a rendering of it.** This was a
        /// `String` holding `value.to_string()`, so `history.append(obj)`
        /// came back from `history.fetch` as the JSON *text* of `obj` —
        /// against a card that promises "read any entry back, **whole**".
        /// A program had to know to `JSON.parse` a value it had just
        /// handed over intact, and nothing said so.
        ///
        /// `#[serde(alias = "text")]` reads the old logs unchanged: a
        /// JSON string there deserialises to `Value::String`, which is
        /// exactly what the old field meant. The row's display text is
        /// derived with `note_text` at render, so there is one source of
        /// truth rather than two fields that can disagree.
        #[serde(alias = "text")]
        value: serde_json::Value,
        /// Source byte range of the `history.append(...)` that wrote it,
        /// so the document can point the call at the row it produced.
        /// Zero for notes written before this existed, and for any the
        /// harness itself appends.
        #[serde(default)]
        site: u32,
        #[serde(default)]
        site_end: u32,
    },

    /// Structural event; roots an agent's first branch. Parent: the
    /// call-site event on the caller's spine (`None` for the tree
    /// root). Starts a new spine: the caller's spine continues past the
    /// call site independently. Renders to chat: no — but `context()`
    /// **resets** here, which is clean-room isolation (decision 3) in the
    /// type rather than in an `is_some()`.
    ///
    /// `system` is the deliberate exception to "nothing regenerable is
    /// stored": the system prompt is assembled from the registry — state
    /// outside the log — and sits at the very front of the prompt, where
    /// churn is most expensive. Snapshotting it is what keeps a later card
    /// edit or a new registry tool from altering an existing
    /// conversation's cached prefix.
    Agent {
        /// The branch's name at birth; `None` shows a derived label until
        /// someone names it. A branch's name is the last [`Rename`] at or
        /// after its root, else this.
        ///
        /// [`Rename`]: EventPayload::Rename
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// What this agent is for — the role its system prompt states.
        charter: String,
        /// The agent's tool allowlist, enforced by the registry from the
        /// agent's own root; `None` inherits the spawner's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools: Option<Vec<String>>,
        /// The assembled system prompt, snapshotted at creation.
        system: String,
        /// The worked examples, snapshotted at creation for the same
        /// reason `system` is — **and they were not, until it turned
        /// out that half a snapshot protects nothing.**
        ///
        /// The cache-immutable prefix is the system message *and* the
        /// worked-example turns that follow it. `document::render` read
        /// `system` from here and the exemplars from
        /// `card::seed_exemplars()` — the *current process's* active
        /// card — so a conversation started under `--card X` rendered X's
        /// prose in front of the embedded card's examples the moment
        /// anything read the log without passing `--card X` again. That
        /// is `agent document`, `agent score` (which reconstructs
        /// `prompt_bytes` by re-rendering), and any resume.
        ///
        /// `card.rs`'s own header claimed the opposite — "editing the
        /// files cannot disturb a conversation already underway" — and
        /// was half right for a year.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exemplars: Vec<Exemplar>,
    },

    /// Structural event; roots a **divergent** branch. Parent: the event
    /// forked from. Renders to chat: a harness line (C1).
    ///
    /// `context()` **carries through** a `Fork` — history and artifacts
    /// cross it — but obligations do not: `replay_event` clears `open`
    /// here, so pre-fork posts stay the original branch's to answer and
    /// there is exactly one owner for every open post.
    Fork {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },

    /// Settlement event; **this branch answered** the `Post` named by
    /// `question`. Parent: the answering branch's spine. Renders to chat:
    /// no — the answer's text is already this branch's `Turn`.
    ///
    /// The other half of the exchange is a `Result` on the *asker's*
    /// branch, which names the `Send`. Neither copies the other: the
    /// `Answer` holds the value and the `Result` names it.
    Answer {
        question: EventId,
        value: serde_json::Value,
    },

    /// Execution event; one per call a program issues, logged at
    /// **dispatch** (17_BRANCHES A2). Parent: the owning agent's spine,
    /// between the program's `Turn` and its eventual `Return`/`Condition`.
    /// Renders to chat: no — queried for replay, the artifact menu, and
    /// UI. Addressable via `fetch_history(id)`, which resolves a call id
    /// through to its `Result`.
    ///
    /// Logging at issue rather than at resolution is what distinguishes a
    /// call that **definitively did not work** (a `Failed` `Result`) from
    /// one that was **in flight when the process died** (no `Result` at
    /// all) — a `send_email` issued a millisecond before `kill -9` used to
    /// be invisible in the log.
    Call(Call),

    /// Execution event; a call settled — the artifact. Parent: the owning
    /// agent's spine, in **resolution order** (decision 7). `call` names
    /// the `Call` event this settles; every call gets exactly one
    /// `Result`. Renders to chat: no.
    Result { call: EventId, outcome: Outcome },

    /// This branch is called this from here on. Parent: the owning
    /// branch's spine. Renders to chat: **no** — a `Rename` is a record,
    /// so renaming a branch never wakes it (the driving rule counts only
    /// `Message`s).
    ///
    /// A branch's name is the last `Rename` **at or after its root**, else
    /// its root's name. That is per-*path*, so renaming an original leaves
    /// its forks alone — which is what you want when a fork was named for
    /// how it differs.
    Rename { name: String },

    /// Structural event; one per compaction operation. Parent: the owning
    /// agent's spine. Renders to chat: replaces its target's row — `of`
    /// names the event being compacted, `text` is what is shown in its
    /// place, and `None` means **nothing is shown**: the entry stops
    /// contributing to the document entirely.
    ///
    /// `None` used to mean a short stub, `[19] note: (removed)`, and a
    /// compacted program a 62-byte comment. Those were a *floor*:
    /// unremovable by construction, one per compacted entry, rising
    /// monotonically for the life of a session. At the default 64 KB
    /// budget with a 12.7 KB card that floor reaches the compaction
    /// threshold after a few hundred programs, and a session that
    /// arrives there can never get under budget again however well it
    /// compacts — it stops producing `Compacted` events, so
    /// `COMPACTION_ATTEMPTS` is never reset and compaction stops firing.
    /// Nothing recovers from that state.
    ///
    /// Removing the stub costs only the reminder: the id is no longer
    /// advertised anywhere. Nothing is *lost* — the event is still in
    /// the log and `history.fetch(id)` still answers for it, so a
    /// program that noted the id elsewhere can still read it back. A
    /// program that wants the reminder kept asks for it, with
    /// `history.replace(id, "…")`.
    ///
    /// **Never removes the target row from the log.** Replay builds a
    /// lookup (target id → this event) that the renderer consults instead
    /// of re-deriving the row from `of` directly — the log stays
    /// append-only, and a branch forked before the compaction still sees
    /// the original event exactly as it was, because nothing was ever
    /// deleted, only shadowed for later renders.
    ///
    /// A **replaced** program (a `Turn` with `Some` text) still renders
    /// as a comment-only assistant turn — valid JavaScript, carrying its
    /// own id — so whatever occupies the assistant's slot is still an
    /// assistant turn. A **removed** program occupies no slot at all,
    /// which the fold handles without a special case: `render` only
    /// flushes the pending user lines when it meets a `Turn`, so a turn
    /// that renders nothing simply lets the lines before and after it
    /// merge into one user message. No empty message, and no two
    /// assistant turns in a row.
    Compacted {
        of: EventId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// A window into the entry's own value, in bytes, instead of
        /// text standing in for it. Carries no bytes of its own —
        /// which is the point: paging a long row by `replace` writes
        /// the same text to the log once per window, and the log is
        /// the one thing in this system that is never rationed but
        /// also never wrong.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<Window>,
    },

    /// Execution event; the full, unclipped console output of one program
    /// run, logged at its terminal (success/suspend/abandon). Parent: the
    /// owning agent's spine, by the run's `Tool` result. Renders to chat:
    /// no — it is the faithful console for the log and the debugger panes
    /// (the completion/condition report carries only a clipped tail), so
    /// a finished program's console survives reload. Never sent to the LLM.
    Console {
        /// **One entry per line, and it means it.** Until 2026-09-19
        /// this held one entry per `console.log` *call* — a real
        /// `sweep-200` entry carried 323 newlines in 4,094 bytes — so
        /// every bound over it counted calls while calling them lines:
        /// a 256-"line" ring that a file dump spent one slot on, and a
        /// report tail whose stated line count could be off by a
        /// hundredfold.
        /// The split happens where the entry is formed
        /// (`interp`'s `console_write`), so the VM buffer, this event,
        /// the report's tail and `history.fetch`'s array all count the
        /// same thing the reader does.
        lines: Vec<String>,
    },
}

/// A message that landed on this branch, with its body materialised.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Post {
    pub from: Author,
    pub origin: Origin,
}

impl Post {
    /// The words themselves, when the body is inline. Empty while it is
    /// still by-reference (`Origin::Sent`) — resolve it through the
    /// `Tree` first, which is what `replay_event` does.
    pub fn text(&self) -> &str {
        self.origin.direct().map(|(t, _, _)| t).unwrap_or("")
    }
}

/// One piece of a reply, exactly as it arrived (28.A).
/// What a size was measured in — see [`EventPayload::Compaction`].
///
/// Both are real limits and neither converts to the other: bytes are
/// what this crate can count for itself, tokens are what the provider
/// counts and the window is expressed in. Which one applies is a fact
/// about the session's configuration, so it is recorded rather than
/// inferred when the log is read back.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Measure {
    Bytes,
    Tokens,
}

impl Measure {
    /// The word to put after a number, and before "budget".
    pub fn noun(self) -> &'static str {
        match self {
            Measure::Bytes => "byte",
            Measure::Tokens => "token",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Part {
    /// Reasoning. On the log because it arrived; never replayed to the
    /// model, which is the renderer's business and not the log's.
    ///
    /// It is a part rather than a field so that a provider leaking its
    /// thinking into the reply channel can be *recorded* as misrouted
    /// rather than deleted — see `notebook::strip_leaked_reasoning`.
    Thinking(String),
    /// Everything outside a fence. Verbatim, whitespace included: the
    /// concatenation invariant is false the moment this is trimmed.
    Prose(String),
    /// A fenced block that executes, **including its fences** — so the
    /// parts still concatenate, and so a cell's offset in the reply is
    /// the sum of the lengths before it.
    Cell(String),
}

/// Why a completion stopped arriving (28.A).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ReplyEnd {
    /// The provider finished.
    Finished,
    /// It hit the token budget mid-reply. The parts that arrived stand;
    /// the model is shown them and told they were cut off, rather than
    /// having the whole reply discarded unread.
    Truncated,
    /// We stopped it — a trap in an earlier cell, or the person.
    Interrupted,
    /// It never arrived. A provider error, recorded so that a run which
    /// reached no completion says so instead of looking like a run that
    /// chose to do nothing.
    Failed(String),
}

/// What a reply's run did when it handed back (28.A).
///
/// The first four are **pauses**: the frame is standing, the reply
/// resumes once something decides. The last three end it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Handback {
    Raised {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },
    Trapped {
        kind: String,
        message: String,
        resumable: bool,
    },
    Posted {
        ids: Vec<EventId>,
    },
    /// The program stopped itself with `stop("reason")` — it found out
    /// it could not finish correctly and said so.
    ///
    /// **Not a fault.** A `Trapped` is something going wrong that the
    /// program did not foresee; this is the program foreseeing it. They
    /// render differently for that reason, and the reply is paused
    /// rather than over: the reason goes in front of the next one,
    /// which carries on with every row and every printed line still in
    /// hand.
    Stopped {
        reason: String,
    },
    /// A cell that would not compile. The reply is paused, not over: the
    /// next one repairs it and the run carries on in the same frame.
    CellFailed {
        message: String,
    },

    Completed,
    Abandoned,
    Interrupted,
}

impl Handback {
    /// Whether the reply can still continue. Replaces
    /// `Disposition`, which encoded the same fact in a second place.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Handback::Completed | Handback::Abandoned | Handback::Interrupted
        )
    }
}

/// Who authored a message. The user is an author, not an agent: they have
/// no branch of their own and speak *inside* branches.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Author {
    User,
    Agent(EventId),
    Harness,
}

/// Where a delivered message's body lives.
///
/// A body is stored **once**: the `Send` holds the question and the `Post`
/// names it. The card's own pattern hands the same plan to every worker,
/// so copying would write twenty bodies for a ten-way fan-out.
///
/// The in-memory `Context` is the other side of that trade: `replay_event`
/// resolves `Sent` through the `Tree` and stores the resolved body inline,
/// so only the *log* is free of copies.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Origin {
    /// The `Send` that dispatched this delivery; the body is read there.
    /// This is also what routes an answer back to the sender's branch.
    Sent(EventId),
    /// The body inline — for the user and harness posts that have no send
    /// side.
    Direct {
        text: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        input: serde_json::Value,
        expects_reply: bool,
        /// The offered options, when the `Send` behind this was a
        /// `choose` — the recipient cannot answer within a set it
        /// cannot see, so the body carries them the same way it carries
        /// the text.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
    },
}

impl Origin {
    /// The inline body, when this origin carries one. A `Sent` origin
    /// resolves through the `Tree` instead (`Tree::resolve_origin`).
    pub fn direct(&self) -> Option<(&str, &serde_json::Value, bool)> {
        match self {
            Origin::Direct {
                text,
                input,
                expects_reply,
                ..
            } => Some((text, input, *expects_reply)),
            Origin::Sent(_) => None,
        }
    }

    /// The options a `choose` offered, empty for everything else.
    pub fn options(&self) -> &[String] {
        match self {
            Origin::Direct { options, .. } => options,
            Origin::Sent(_) => &[],
        }
    }
}

/// What a program's branch waits on. Four **typed** kinds, not one
/// `Invoke` with a magic `name`: from a program's view they are all
/// `tools.*` calls ("subagents are tools", 8_HARNESS dec. 2, holds at the
/// API), but `dispatch_calls` interprets the name exactly once, at
/// dispatch, and everything downstream — the artifact menu,
/// reconciliation, re-attach, routing an answer home — matches on the
/// variant instead of re-parsing a string.
///
/// Every variant carries `site`, the source byte offset of its `Invoke`
/// instruction, so a report can annotate the program source per call site
/// from the log alone (`InvokeCall::site`). `Send` additionally carries
/// `site_end` (`InvokeCall::site_end`), the end of that same call's source
/// span, so a host can underline the whole `tools.ask`/`tools.tell` call
/// rather than caret its first byte.
///
/// **One `Call` per call a program issues, logged at dispatch — with one
/// exception.** Under `Transport::Notebook` a *prose* segment of the model's
/// reply is logged as a `Send` too (`25_NOTEBOOK.md` D15), and no program
/// issued it: it is text the model wrote beside its code, logged as the
/// reply streams rather than at execution time. Such a send carries a
/// synthetic zero-width `site`/`site_end`, because there is no instruction
/// and no source expression behind it, and the reply's *opening* prose is
/// logged before any `Turn` exists at all — so the placement rule ("between
/// the program's `Turn` and its eventual `Return`") describes program-issued
/// calls and no longer describes every `Call` on the spine.
///
/// Using `Send` anyway is deliberate. A prose segment *is* a message to the
/// person: it renders in history exactly as a `tell` does, so the model
/// re-reads its own reply in a shape it already knows, and `agent score`'s
/// `tells`/`silent` fields fold off it without learning a second payload.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Call {
    /// This branch's program messaged an agent or the user — the mirror of
    /// `Post`. `tools.ask` and `tools.tell` both log one, differing only in
    /// `expects_reply`; the body lives here and the `Post` names it, so it
    /// is never copied.
    ///
    /// **Or this branch's model wrote prose** (`Transport::Notebook`, D15):
    /// the reply's own text, addressed to the user with
    /// `expects_reply: false` and a synthetic site. See [`Call`]'s own
    /// comment for why that case shares this variant rather than adding one.
    /// It has no VM promise behind it, so the host writes its `Result` at
    /// delivery instead of settling a program's await — the same path an
    /// unawaited `tell` already takes.
    Send {
        /// **This send is a prose segment of the model's own reply**, not
        /// a call a program made (`Transport::Notebook`, D15).
        ///
        /// Marked rather than inferred. The only other way to tell the
        /// two apart is the synthetic zero-width `site`/`site_end`, and a
        /// `tell` written as the first thing in a cell has `site: 0` too
        /// — so the discriminator would be `site_end`, which is a
        /// coincidence of layout standing in for a fact about origin.
        ///
        /// What it decides: a prose send leaves **no history row**,
        /// because the reply it belongs to is already rendered whole as
        /// the assistant turn and a row would print it twice. A `tell`
        /// keeps its row; it is not in the reply's text, only its call
        /// is.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        prose: bool,
        to: Address,
        text: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        input: serde_json::Value,
        expects_reply: bool,
        /// `choose(who, question, options)`'s options — empty for every
        /// `tell` and every free-form `ask`.
        ///
        /// Non-empty is the whole difference between the two asking
        /// verbs, and it is a **promise to the asking program**: the
        /// value that settles this call is one of these strings, `===`
        /// -equal, so `await choose(...)` is safe to compare and switch
        /// on. Nothing else in this vocabulary hands a program a value
        /// with a shape it can rely on without checking.
        ///
        /// The promise is kept by refusing replies rather than by
        /// coercing them: `Runner::pick_option` normalises a human's
        /// "2" or "leave IT" onto the canonical spelling, and anything
        /// it cannot place leaves the call open (`Host::cmd_reply`) or
        /// rejects the `answer` (`machine.rs`'s `TOOL_ANSWER`). A
        /// settled `choose` therefore never carries prose.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
        site: u32,
        /// End of the `tools.ask`/`tools.tell` call's source span
        /// (`site` is its start), so a host can log/underline the whole
        /// call rather than caret its first byte. `#[serde(default)]`:
        /// logs written before this field existed carry none, and this
        /// repo never migrates logs — an old log just defaults it to `0`
        /// rather than refusing to load.
        #[serde(default)]
        site_end: u32,
    },
    /// This branch's program created an agent. Settled with the agent
    /// handle; the `Agent` event it roots is a child of this `Spawn`.
    Spawn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        charter: String,
        /// The child's tool allowlist; `None` inherits the caller's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools: Option<Vec<String>>,
        site: u32,
    },
    /// This branch's program forked itself — a divergent branch that
    /// inherits the caller's history rather than starting clean. Settled
    /// with the fork's handle; the `Fork` event it roots is a child of
    /// this `Call::Fork`.
    ///
    /// **Creating is not messaging** (`22_ONE_VOCABULARY.md`). `fork()`
    /// takes nothing and carries no first task: it creates a branch and
    /// hands back a handle, and whatever the child should do is said
    /// afterwards, with `tell(agent, …)` to delegate and not wait or
    /// `await ask(agent, …)` to delegate and use the result. `spawn`'s
    /// single string is an identity — a charter — for the same reason,
    /// not a task. An earlier design had both verbs kick their child off
    /// implicitly, which needed a `task` field here and a paragraph
    /// explaining why the two verbs' strings meant different things;
    /// both are gone.
    Fork {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        site: u32,
    },
    /// This branch's program called a host tool.
    Invoke {
        name: String,
        args: serde_json::Value,
        site: u32,
    },
}

impl Call {
    pub fn site(&self) -> u32 {
        match self {
            Call::Send { site, .. }
            | Call::Spawn { site, .. }
            | Call::Fork { site, .. }
            | Call::Invoke { site, .. } => *site,
        }
    }
}

/// Where a `Send` is addressed. `tools.ask` with `to` omitted resolves to
/// the author of the question being answered *before* the call is logged,
/// so nothing unresolved ever reaches the log.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Address {
    /// The human driving the session. They have no branch — they speak
    /// inside branches — so a question to them is pending until their
    /// reply produces this `Send`'s `Result`.
    User,
    /// A branch, named by its root event id (an agent's `Agent`, or a
    /// `Fork`).
    Branch(EventId),
}

/// How a call settled. `Failed` is load-bearing, not a convenience: it is
/// what separates a call that **definitively did not work** from one that
/// was merely **issued** (no `Result` at all), which reconciliation must
/// read as "may have happened".
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Outcome {
    /// An answer, a delivery receipt, an agent handle, or a tool result.
    Delivered(serde_json::Value),
    /// It definitively did not happen; the message is the reason.
    Failed(String),
}

impl Outcome {
    /// The delivered value, or `None` for a failure.
    pub fn value(&self) -> Option<&serde_json::Value> {
        match self {
            Outcome::Delivered(v) => Some(v),
            Outcome::Failed(_) => None,
        }
    }
}

/// One agent's reconstructed conversation along a spine — its slice of
/// the `Agent`-ancestor chain: what it is for, the system prompt it was
/// rooted with, the rendered messages on its segment of the path, and
/// what it still owes an answer to.
///
/// Bodies here are **resolved**: a `Post` that names its `Send` in the log
/// carries the body inline once it reaches a `Context`.
/// One worked example: the user turn that opens it and the program
/// that answers it. Part of the log vocabulary rather than `card.rs`'s
/// own type, because an agent's exemplars are snapshotted into its
/// root `Agent` event — see that variant's `exemplars` field.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Exemplar {
    pub user: String,
    pub assistant: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Context {
    /// What this agent is for (`Agent.charter`).
    pub charter: String,
    /// The system prompt snapshotted at the agent's root (`Agent.system`),
    /// rebuilt into every request verbatim.
    pub system: String,
    /// The worked examples snapshotted alongside it — the rest of the
    /// immutable prefix (`EventPayload::Agent.exemplars`).
    pub exemplars: Vec<Exemplar>,
    /// Every post that landed on this branch, bodies materialised.
    ///
    /// It was `Vec<Message>` when a `Message` could also be a `Turn`.
    /// Only posts were ever pushed — a turn is the branch's own reply,
    /// not something said to it — so the element type says so now.
    pub messages: Vec<Post>,
    /// Posts open on this branch, oldest first: unanswered, expecting a
    /// reply, and at or after this branch's root. **Agents never close** —
    /// a branch that has answered is `Idle`, not done — so this is what
    /// "owes something" means, and it is the only such state.
    pub open: Vec<EventId>,
}

impl Context {
    /// The `input` const a program binds: the machine-bound data of the
    /// **oldest still-open post**, whole. The context sees only a
    /// bounded preview of the same value, so a caller passing a large
    /// `input` never dumps it into the callee's context.
    ///
    /// `open` is exactly this branch's obligations (18_TARGETING: only
    /// `answer(question, value)` closes one), so `open.first()` — not a
    /// scan of `messages` for the first post that merely *expected* a
    /// reply — is the post a fresh program should bind to; after the
    /// first is answered, the next program sees the next one.
    /// `Value::Null` when nothing is open.
    pub fn input(&self, tree: &Tree) -> serde_json::Value {
        let Some(&question) = self.open.first() else {
            return serde_json::Value::Null;
        };
        let Some(EventPayload::Post { origin, .. }) =
            tree.events.get(&question).map(|e| &e.payload)
        else {
            return serde_json::Value::Null;
        };
        tree.resolve(origin)
            .direct()
            .map(|(_, input, _)| input.clone())
            .unwrap_or(serde_json::Value::Null)
    }

    /// The options a still-open post offered, when it came from a
    /// `choose` — empty for every `ask`. What `answer(question, …)` is
    /// held to, and what a recipient reads off the rendered post.
    pub fn options(tree: &Tree, question: EventId) -> Vec<String> {
        let Some(EventPayload::Post { origin, .. }) =
            tree.events.get(&question).map(|e| &e.payload)
        else {
            return Vec::new();
        };
        tree.resolve(origin).options().to_vec()
    }
}

/// A handle on one active leaf of the tree: the cursor appends go
/// through, plus the reconstructed agent chain above it. The innermost
/// agent (`contexts.last()`) owns new events.
#[derive(Clone, Debug)]
pub struct Spine {
    pub leaf_id: EventId,
    pub contexts: Vec<Context>,
}

impl Spine {
    /// The context events on this spine belong to — the innermost
    /// agent's slice of the path.
    pub fn context(&self) -> &Context {
        self.contexts.last().expect("spine has no contexts")
    }
}

pub struct Tree {
    pub id_counter: u64,
    pub events: HashMap<EventId, Event>,
    pub file: Option<std::fs::File>,
    /// Derived reports, keyed by their **outcome event id** — never
    /// logged. Reports are pure functions of the log, so without a memo
    /// every request re-derives every report on the path and a session is
    /// quadratic in branch length; with it, re-derivation is amortised
    /// O(1).
    ///
    /// The `Tree` is the right home because renders happen per branch per
    /// request and the memo must outlive any one `Runner`. It is dropped
    /// wholesale when the renderer changes
    /// (`Tree::clear_report_memo`) — a memo is a cache of one renderer's
    /// output, and editing `report.rs` invalidates all of it.
    pub(crate) reports: std::cell::RefCell<ReportMemo>,
}

/// The report cache, with a counter so a test can observe that history is
/// not re-derived on every request.
pub struct ReportMemo {
    pub entries: HashMap<EventId, String>,
    /// How many reports have actually been rendered (memo misses).
    pub derivations: u64,
    /// The renderer this cache belongs to.
    pub version: u32,
}

impl Default for ReportMemo {
    fn default() -> Self {
        ReportMemo {
            entries: HashMap::new(),
            derivations: 0,
            version: crate::report::REPORT_FORMAT_VERSION,
        }
    }
}

impl EventId {
    pub fn new(value: u64) -> Self {
        Self(NonZeroU64::try_from(value).expect("expected non-zero EventId!"))
    }

    /// The same, for a number that came from a program rather than from
    /// the log — `None` where [`EventId::new`] would panic.
    ///
    /// Ids reach us as whatever the model's arithmetic produced, and 0
    /// is both easy to produce and impossible as an id. Every id-taking
    /// entry point filters `> 0` before converting; one did not, and
    /// `history.fetch(0)` killed the agent mid-task in 2 of 294 kept
    /// runs. A constructor that cannot panic is a better guard than
    /// remembering the filter.
    pub fn checked(value: u64) -> Option<Self> {
        NonZeroU64::new(value).map(Self)
    }

    pub fn as_u64(self) -> u64 {
        let EventId(val) = self;
        val.get()
    }
}
