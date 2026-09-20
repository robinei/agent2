//! A reply goes in, what it did comes out.
//!
//! The shape `interp`'s tests already have — `eval("1 + 2 * 3")` is a
//! source string in and a value out, one line per fact — applied to the
//! layer above. A test says what the model wrote and then asks what
//! came of it; the drive loop, the tool answers and the log archaeology
//! are this file's problem.
//!
//! **Why this exists.** Most tests at this layer feed a scripted reply
//! and then read the log back with a hand-written scan:
//!
//! ```ignore
//! let said: Vec<String> = tree.events.values().filter_map(|e| match &e.payload {
//!     EventPayload::Call(Call::Send { text, .. }) => Some(text.clone()),
//!     _ => None,
//! }).collect();
//! ```
//!
//! Two things are wrong with that, and they cost a morning on
//! 2026-09-20. It scans the **whole log**, so it silently means "the
//! only `Send` there has ever been" — and the moment `finish(text)`
//! started sending the finishing word, four such tests began matching
//! the wrong event. And `Tree::events` is a `HashMap`, so `.find(...)`
//! over it is not deterministic: *which* wrong event they matched
//! varied between runs. Nine of that day's twenty-four failures were
//! this one idiom, none of them about the change being made.
//!
//! [`Said`] is scoped to the reply that produced it. `r.tells` is what
//! *this* reply told someone, in log order. Nothing outside the reply
//! can redirect an assertion, and nothing inside it is reached for by
//! guessing.
//!
//! **What it does not replace.** Order is a real subject here — the
//! rule that a `Call` lands between its reply and its outcome is about
//! placement, and only a sequence can test placement. So [`Said::kinds`]
//! hands back the raw event-kind sequence rather than hiding it: the
//! doubled-reply bug this file's own [`Invariant::OneReply`] now catches
//! was originally caught by a test asserting exactly that list.
//!
//! **Invariants ride along.** Every [`Conversation::reply`] checks a
//! small set of properties that should hold of *any* reply
//! ([`Invariant`]), so a test written about one thing still fails when
//! something else breaks underneath it. That is where the doubled reply
//! would have been caught in eighty tests instead of one.
//!
//! **Three places hold replies to these rules, and they are the same
//! rules.** This file checks them on a reply a test wrote;
//! [`crate::replay`] checks them on a reply a live run wrote, fed back
//! through the harness; and `evals/invariants.py` checks them on a log
//! without running anything, which is what still works after a schema
//! change stops `agent` opening the file at all. A rule worth adding
//! here is usually worth adding there.
//!
//! **It pays on the way in, not only afterwards.** Porting the existing
//! tests to this shape found two live bugs on the first afternoon, both
//! because the harness drives what the host drives rather than what the
//! old test happened to drive:
//!
//! - A cell ending in a fire-and-forget `tell` ended the whole reply.
//!   The old streaming tests pumped ticks but never handed back the
//!   settlements the host hands back, so the reply never reached the
//!   state that breaks
//!   ([`tests::settling_a_cells_last_tell_does_not_end_the_reply`]).
//! - Writing [`Invariant::SitesAreReplyAbsolute`] turned up the case it
//!   does not hold in — a handler's reply and the program it wakes
//!   produce events at once, and a span can only be read against the
//!   reply it came from. Which is a fact about the design that was
//!   nowhere written down.

use std::collections::HashMap;

use serde_json::json;

use crate::machine::{OutCall, Runner, StepInput, StepOutput, ToolResult};
use crate::types::{
    Address, Author, Call, Event, EventId, EventPayload, Handback, Origin, Outcome, Tree,
};

/// Fuel per tick. Large: a test wants the program to run, not to
/// measure slicing.
const FUEL: u64 = 1_000_000;

/// Ticks before the drive loop calls it a hang. A real reply settles in
/// a handful; a thousand means something is spinning.
const MAX_ROUNDS: usize = 1_000;

/// The budget `document()` renders under — a fixed test constant,
/// because no session tracks one in a test harness.
const TEST_BUDGET: usize = 64 * 1024;

type ToolFn = Box<dyn Fn(&serde_json::Value) -> Result<serde_json::Value, String>>;

/// Properties that should hold of any reply, checked after every one.
///
/// **These are not assertions a test opts into.** A test that opts in
/// is a test that remembered, and the bugs worth catching this way are
/// the ones nobody was looking for. Each is a rule already written down
/// somewhere in `types.rs` or `docs/28`; this is that rule made to fail
/// a test rather than sit in a comment.
///
/// A test that deliberately builds a state violating one says so with
/// [`Conversation::allow`], which reads as a claim about the test
/// rather than as a hole in the rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Invariant {
    /// **One completion, one `Reply`.** The event is logged before a
    /// byte of the reply arrives and never again for the same
    /// completion.
    ///
    /// Broken once, on 2026-09-20: `finish` began halting the VM
    /// part-way through a notebook, the reply was closed out there, and
    /// the completion arriving afterwards was mistaken for a fresh turn
    /// — so one scripted reply logged two `Reply` events and ran two
    /// programs. One test noticed, by luck, because it happened to
    /// assert on the whole event sequence.
    OneReply,
    /// **Every reply ends**, exactly once, naming itself. A reply with
    /// no `ReplyEnd` is one every per-reply measure divides by wrongly,
    /// and the figures it carries — what the completion cost — have
    /// nowhere else to go.
    ReplyEnds,
    /// **The parts put the reply back together** (`docs/28`). Prose and
    /// cells are logged as they arrive, each carrying its own bytes
    /// including the fences, so concatenating them in order yields the
    /// text the model actually wrote. This is what makes the log a
    /// record of the reply rather than a summary of it.
    PartsConcatenate,
    /// **Every `Handback` names a reply on this branch.** An outcome
    /// that names something else, or nothing, cannot be attributed —
    /// and "completed ⇒ a terminal `Handback`" is what makes recovery
    /// decidable from the log alone.
    HandbackNamesTheReply,
    /// **A prose send is synthetic.** No instruction issued it — the
    /// model wrote a paragraph, not a call — so there is no source
    /// expression to point a cursor at, and zero width is the
    /// convention `span.rs` names for exactly that. A real-looking
    /// site here would send the debugger to an offset that means
    /// nothing.
    ProseIsSynthetic,
    /// **A site is an offset into the reply, not into a parse buffer.**
    /// Every span a reply logs — a call's, a row's, a trap's — is
    /// reply-absolute and half-open, so cutting the reply with it
    /// yields the expression it names. A cell-local offset is usually
    /// still *in range*, which is why being in bounds is not the test:
    /// the cut has to land on a character boundary and not be empty.
    ///
    /// The bug this is written against had traps reporting an offset
    /// into the compiler's own parse buffer, which pointed the debugger
    /// at whatever happened to be at that index in the reply.
    ///
    /// **Not checked on a reply that resumes a suspended one.** A
    /// handler's reply wakes the program underneath it, and that
    /// program's own calls land in this reply's scope carrying spans
    /// into the reply *it* was written in — correctly. Two replies are
    /// producing events at once there, and a span can only be read
    /// against the one it came from.
    SitesAreReplyAbsolute,
    /// **Every call settles**, or is still honestly waiting. A `tell`
    /// gets its delivery receipt, a tool call gets its result; an `ask`
    /// may sit open, because an answer is someone else's to give, and a
    /// suspended or abandoned run may leave calls hanging on purpose
    /// (`Handback::Abandoned`'s own doc). Anything else is a promise
    /// nobody will ever resolve.
    CallsSettle,
}

/// How a reply's program ended.
///
/// One value, not the four booleans a test would otherwise assemble out
/// of the log — and exhaustive, so `assert_eq!(r.ended, Ending::…)`
/// says which ending *and* rules out the others in one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// `finish(text)` — the task is over and the branch rests.
    Finished,
    /// The program ran off the end. Under this transport that is a
    /// handover, not an ending: the next reply carries on.
    Completed,
    /// `stop(reason)` — this reply cannot finish, and says why.
    Stopped(String),
    /// `raise(name, payload)` — parked for a judgement.
    Raised {
        name: String,
        payload: Option<serde_json::Value>,
    },
    /// A runtime error nobody caught, and where in the reply it
    /// happened.
    Trapped { message: String, site: u32 },
    /// A cell that did not compile.
    CellFailed(String),
    /// A handler's `abandon()` discarded the run beneath it.
    Abandoned,
    /// A post arrived and parked the run.
    Posted,
    /// Still running when the drive loop went quiet — which means it is
    /// blocked on something this harness did not answer.
    Running,
}

/// A row this reply appended.
#[derive(Debug, Clone)]
pub struct Row {
    /// The `Note`'s own id — what a later `history.fetch(id)` names.
    pub id: EventId,
    pub value: serde_json::Value,
    /// Where in the reply the `history.append(...)` call was written.
    /// Half-open, reply-absolute (not cell-local), which is what lets
    /// the debugger put a cursor on it.
    pub site: (u32, u32),
}

impl Row {
    /// The source that wrote this row, cut out of `reply` by its own
    /// span — how a test says "the site is the whole call" without
    /// counting characters.
    pub fn source_in<'a>(&self, reply: &'a str) -> &'a str {
        &reply[self.site.0 as usize..self.site.1 as usize]
    }
}

/// An `ask` this reply issued and nobody has answered.
#[derive(Debug, Clone)]
#[allow(
    dead_code,
    reason = "a harness's shape is its API; `to` is read by tests not yet written"
)]
pub struct Ask {
    /// The logged `Call::Send`, to answer with
    /// [`Conversation::answer`].
    pub call: EventId,
    pub to: Address,
    pub text: String,
    pub options: Vec<String>,
}

/// What one reply did — scoped to the events that reply produced, and
/// nothing else in the log.
#[derive(Debug, Clone)]
pub struct Said {
    /// Prose segments, in order, **as they reached the person** — the
    /// `Send` each one became, so trimmed of the blank lines that
    /// separated it from the fences. The verbatim bytes are what
    /// [`Invariant::PartsConcatenate`] checks; what a test wants to
    /// read is what was said.
    pub prose: Vec<String>,
    /// This reply's own `Reply` event — the id `history.fetch` takes to
    /// read the whole reply back as the markdown the model wrote.
    pub reply: EventId,
    /// Each part of the reply, prose and cells alike, in source order.
    /// Thinking is not one: it is not a piece of the reply.
    pub parts: Vec<EventId>,
    /// The outcome, when the program reached one.
    pub handback: Option<EventId>,
    /// The `finish(text)` send, if the reply ended itself with one.
    pub finished_by: Option<EventId>,
    /// The source of each ```js cell, fences included — what `Part::Cell`
    /// carries, so a test can assert on what ran as well as on what it
    /// did.
    pub cells: Vec<String>,
    /// `tell(text)` and `finish(text)`, in log order: everything this
    /// reply said that expected no answer.
    pub tells: Vec<String>,
    /// The `Send` behind each of `tells`, same order — for the tests
    /// that are about the call rather than the words.
    pub told: Vec<EventId>,
    /// `ask` / `choose` — the sends still waiting on someone.
    pub asks: Vec<Ask>,
    /// `history.append(value)`, in order.
    pub rows: Vec<Row>,
    /// `tools.*` calls, in dispatch order, as `(name, args)`.
    pub calls: Vec<(String, serde_json::Value)>,
    /// Lines the program printed.
    pub printed: Vec<String>,
    /// What another agent said to this branch — a `Send` from
    /// elsewhere, landing here as a post. Where `notices` is the
    /// harness speaking, this is the other half of the conversation.
    pub heard: Vec<String>,
    /// Harness notices logged on this branch during the reply — the
    /// channel the model demonstrably reads, used to tell it that its
    /// last reply stopped short or wrote a call in a channel that does
    /// not exist.
    pub notices: Vec<String>,
    /// Rows this reply compacted — `history.remove`/`replace`/`slice`.
    pub compacted: Vec<EventId>,
    /// Settlements logged in this reply's scope, `(call, outcome)`.
    pub settled: Vec<(EventId, Outcome)>,
    /// Questions this reply discharged with `answer(id, …)`.
    pub answered: Vec<(EventId, serde_json::Value)>,
    /// How the program ended.
    pub ended: Ending,
    /// How the *reply* ended, which is a fact about the text and not
    /// about the program: `Finished`, `Truncated`, `Interrupted`.
    pub reply_ended: Option<crate::types::ReplyEnd>,
    /// What the completion cost, as the provider reported it — one
    /// figure per reply, which is the whole reason `ReplyEnd` exists.
    pub usage: Vec<crate::host::Usage>,
    /// Whether the branch rested: no further request went out after
    /// this reply. The other half of `finish`'s contract, and the one
    /// no scan of the log can see — resting is the *absence* of an
    /// event.
    pub rests: bool,
    /// The raw event-kind sequence this reply added, for the tests that
    /// are about placement. See the module doc: hidden order is how the
    /// bug that motivated this file stayed hidden.
    pub kinds: Vec<&'static str>,
    /// Ids this reply produced — what [`Said::owns`] answers from.
    span: (u64, u64),
}

#[allow(
    dead_code,
    reason = "the projection is the API; not every field has a test yet"
)]
impl Said {
    /// Whether `id` is an event this reply produced.
    pub fn owns(&self, id: EventId) -> bool {
        let n = id.as_u64();
        n > self.span.0 && n <= self.span.1
    }

    /// Whether anything this reply said contains `needle`.
    pub fn said(&self, needle: &str) -> bool {
        self.tells
            .iter()
            .chain(&self.prose)
            .any(|t| t.contains(needle))
    }

    /// The single row this reply appended. Panics naming what it found
    /// instead, because "the row" is the commonest thing to want and
    /// `rows[0]` on an empty vector says nothing about why.
    pub fn row(&self) -> &Row {
        match self.rows.as_slice() {
            [r] => r,
            other => panic!(
                "expected exactly one appended row, got {}: {other:?}",
                other.len()
            ),
        }
    }

    /// The values this reply appended, in order — the commonest thing
    /// to compare a whole run against.
    pub fn values(&self) -> Vec<&serde_json::Value> {
        self.rows.iter().map(|r| &r.value).collect()
    }

    /// The single `ask` still open, same bargain as [`Said::row`].
    pub fn ask(&self) -> &Ask {
        match self.asks.as_slice() {
            [a] => a,
            other => panic!(
                "expected exactly one open ask, got {}: {other:?}",
                other.len()
            ),
        }
    }
}

/// A branch, its log, and the drive loop that keeps them moving.
///
/// One agent and no threads: this is `Runner` driven directly, which is
/// what the great majority of tests at this layer actually need. Tests
/// that are *about* the session loop — routing between agents, the
/// client's chunk callback — belong in `host`'s own suite, which drives
/// a real `Session`.
pub struct Conversation {
    tree: Tree,
    runner: Runner,
    tools: HashMap<String, ToolFn>,
    allowed: Vec<Invariant>,
    /// Requests that went out since the last reply — how `rests` is
    /// decided.
    requests: usize,
    /// Sends left open on purpose (asks), so the settle loop does not
    /// keep rediscovering them.
    open_asks: Vec<EventId>,
    /// A fresh generation per reply — what tells `notebook_stream` that
    /// the last reply is over and a new one is arriving.
    epoch: u64,
    /// The reply being streamed a chunk at a time, if one is open:
    /// where its scope starts, and the text fed so far.
    open_reply: Option<(u64, String)>,
    /// What the next completion to close will report having cost.
    usage: Option<crate::host::Usage>,
    /// What the next completion to close will report having thought.
    thinking: Option<String>,
    /// Whether the reply just fed woke a program suspended beneath it,
    /// in which case two replies were producing events at once — see
    /// [`Invariant::SitesAreReplyAbsolute`].
    resumed: bool,
}

// **Unused here is not unused.** This is the harness's surface, and
// it is written to be complete rather than to be exactly what today's
// twelve tests happen to reach for: `tree`/`runner` are the escape
// hatch that makes adopting it safe (a test that needs something the
// projection does not carry can still get at the log), and the rest
// pair with methods that are used. A convention worth keeping: when a
// method here has no caller for long enough to be forgotten, delete it
// rather than widening this.
#[allow(dead_code, reason = "test-harness API, deliberately complete")]
impl Conversation {
    pub fn new() -> Self {
        Self::with_charter("you are a test agent")
    }

    pub fn with_charter(charter: &str) -> Self {
        let mut tree = Tree::new(None);
        let runner = Runner::new_root(&mut tree, charter, "").expect("a root runner");
        Self {
            tree,
            runner,
            tools: HashMap::new(),
            allowed: Vec::new(),
            requests: 0,
            open_asks: Vec::new(),
            epoch: 0,
            open_reply: None,
            usage: None,
            thinking: None,
            resumed: false,
        }
    }

    /// Answer `tools.<name>(...)` with `f(args)`. Args arrive as a JSON
    /// array, positionally, exactly as the log records them.
    ///
    /// A call to a tool with no handler is answered with an error
    /// naming it, rather than left hanging: a program blocked forever
    /// on an unregistered tool reads as a harness hang, and the test
    /// that forgot to register it learns nothing.
    pub fn tool<F>(&mut self, name: &str, f: F) -> &mut Self
    where
        F: Fn(&serde_json::Value) -> Result<serde_json::Value, String> + 'static,
    {
        self.tools.insert(name.to_owned(), Box::new(f));
        self
    }

    /// Answer `tools.<name>(...)` with a fixed value.
    pub fn answers(&mut self, name: &str, value: serde_json::Value) -> &mut Self {
        self.tool(name, move |_| Ok(value.clone()))
    }

    /// Reject `tools.<name>(...)` with `message` — a tool that failed,
    /// which is a different thing from one that is missing.
    pub fn rejects(&mut self, name: &str, message: &str) -> &mut Self {
        let message = message.to_owned();
        self.tool(name, move |_| Err(message.clone()))
    }

    /// Stop enforcing `inv` on this conversation. For a test that
    /// deliberately builds the state the invariant forbids — say so
    /// here and the exemption is visible, rather than the rule being
    /// weakened for everyone.
    pub fn allow(&mut self, inv: Invariant) -> &mut Self {
        self.allowed.push(inv);
        self
    }

    /// The user speaks. Returns the `Post`'s id, which is what a
    /// program names to `answer(...)` or `history.remove(...)`.
    pub fn user(&mut self, text: &str) -> EventId {
        self.post(Author::User, text, true)
    }

    /// The harness speaks — a notice, which owes no answer.
    pub fn harness(&mut self, text: &str) -> EventId {
        self.post(Author::Harness, text, false)
    }

    fn post(&mut self, from: Author, text: &str, expects_reply: bool) -> EventId {
        let origin = Origin::Direct {
            text: text.to_owned(),
            input: serde_json::Value::Null,
            options: Vec::new(),
            expects_reply,
        };
        let (post, out) = self
            .runner
            .deliver(&mut self.tree, from, origin)
            .expect("deliver");
        self.settle(out);
        post
    }

    /// The model replies. `markdown` is the whole reply — prose and
    /// ```js blocks — fed through the streaming path, which is the one
    /// production uses.
    ///
    /// Everything it sets off runs before this returns: tools answered,
    /// sends delivered, ticks pumped, until the branch is quiet.
    pub fn reply(&mut self, markdown: &str) -> Said {
        self.deliver_reply(markdown, true)
    }

    /// As [`reply`](Self::reply), but arriving in pieces — one chunk
    /// per element, then the end. For the tests that are about *when*
    /// the text lands: a program that halts in the first cell while the
    /// second is still being written, a fence closing across a seam.
    pub fn reply_in_chunks(&mut self, chunks: &[&str]) -> Said {
        self.resumed = self.runner.status() == "suspended";
        let before = self.tree.id_counter;
        self.requests = 0;
        self.epoch += 1;
        let mut out = Vec::new();
        for chunk in chunks {
            let more = self
                .runner
                .notebook_stream(&mut self.tree, self.epoch, chunk)
                .expect("stream");
            out.extend(self.settle(more));
        }
        let end = self
            .runner
            .step(
                &mut self.tree,
                StepInput::LlmResponse(crate::host::scripted_program("")),
            )
            .expect("stream end");
        self.settle(end);
        let said = self.project(before);
        self.check(&said, &chunks.concat());
        said
    }

    /// Feed one chunk of a reply and stop there, leaving the
    /// completion open. What came of *that* chunk comes back.
    ///
    /// For the tests that are about a reply being acted on while it is
    /// still being written — a cell running before the next one's fence
    /// has arrived, which is the whole point of the transport and is
    /// invisible to any test that hands the reply over whole. End it
    /// with [`end_reply`](Self::end_reply) or
    /// [`truncate_reply`](Self::truncate_reply).
    ///
    /// No invariants: the reply is not over, so most of them are not
    /// yet true of it. [`end_reply`] checks them against the whole
    /// text.
    pub fn chunk(&mut self, text: &str) -> Said {
        if self.open_reply.is_none() {
            self.resumed = self.runner.status() == "suspended";
            self.requests = 0;
            self.epoch += 1;
            self.open_reply = Some((self.tree.id_counter, String::new()));
        }
        let before = self.tree.id_counter;
        let out = self
            .runner
            .notebook_stream(&mut self.tree, self.epoch, text)
            .expect("stream");
        self.settle(out);
        if let Some((_, written)) = self.open_reply.as_mut() {
            written.push_str(text);
        }
        self.project(before)
    }

    /// The completion is over. Returns the whole reply's [`Said`], not
    /// just the tail — what the reply did, all of it.
    pub fn end_reply(&mut self) -> Said {
        self.close_reply(false)
    }

    /// The completion is over because the token budget ran out: the
    /// cells that closed stand, the half-written one after them was
    /// never a cell.
    pub fn truncate_reply(&mut self) -> Said {
        self.close_reply(true)
    }

    /// What the provider said it was thinking. A real completion
    /// carries it, and it is logged as a part of the reply — so a
    /// replay that leaves it out logs one event fewer and every id
    /// after it shifts, which is enough to break a program that named
    /// one.
    pub fn thinking(&mut self, text: &str) -> &mut Self {
        self.thinking = Some(text.to_owned());
        self
    }

    /// The completion is over and cost this many output tokens — for
    /// the tests that are about the figure riding on `ReplyEnd` rather
    /// than about what the reply did.
    pub fn end_reply_costing(&mut self, completion: u64) -> Said {
        self.usage = Some(crate::host::Usage {
            prompt: 10,
            cached: 0,
            completion,
            reasoning: 0,
        });
        self.close_reply(false)
    }

    fn close_reply(&mut self, truncated: bool) -> Said {
        let (before, written) = self
            .open_reply
            .take()
            .expect("end_reply with no reply open — call chunk() first");
        let out = self
            .runner
            .step(
                &mut self.tree,
                StepInput::LlmResponse(crate::machine::LlmTurn {
                    truncated,
                    usage: self.usage.take(),
                    thinking: self.thinking.take(),
                    ..crate::host::scripted_markdown("")
                }),
            )
            .expect("stream end");
        self.settle(out);
        let said = self.project(before);
        // A truncated reply is one the log cannot put back together by
        // construction: the text stops mid-token and the half-cell is
        // dropped. Everything else still holds.
        if !truncated {
            self.check(&said, &written);
        }
        said
    }

    /// As [`reply`](Self::reply), but the completion arrives whole
    /// rather than streaming — a client that does not stream, or a user
    /// taking the branch's turn by hand. The ordering differs (the
    /// reply's end is known before its first cell runs), so a test
    /// about ordering should say which it means.
    pub fn reply_whole(&mut self, markdown: &str) -> Said {
        self.deliver_reply(markdown, false)
    }

    fn deliver_reply(&mut self, markdown: &str, streamed: bool) -> Said {
        self.resumed = self.runner.status() == "suspended";
        let before = self.tree.id_counter;
        self.requests = 0;
        let thinking = self.thinking.take();
        let out = if streamed {
            self.epoch += 1;
            let mut out = self
                .runner
                .notebook_stream(&mut self.tree, self.epoch, markdown)
                .expect("stream");
            out.extend(
                self.runner
                    .step(
                        &mut self.tree,
                        StepInput::LlmResponse(crate::machine::LlmTurn {
                            thinking,
                            ..crate::host::scripted_markdown("")
                        }),
                    )
                    .expect("stream end"),
            );
            out
        } else {
            self.runner
                .step(
                    &mut self.tree,
                    StepInput::LlmResponse(crate::machine::LlmTurn {
                        thinking,
                        ..crate::host::scripted_markdown(markdown)
                    }),
                )
                .expect("reply")
        };
        self.settle(out);
        let said = self.project(before);
        self.check(&said, markdown);
        said
    }

    /// Answer an open `ask` — what a person, or another branch, would
    /// send back. Returns what the waking program did with it, so the
    /// round trip reads as one expression.
    ///
    /// No invariants are checked here: this is the *tail* of a reply
    /// already checked, and the events it adds carry no `Reply` of
    /// their own to be one of.
    pub fn answer(&mut self, ask: EventId, value: serde_json::Value) -> Said {
        self.open_asks.retain(|id| *id != ask);
        let before = self.tree.id_counter;
        let out = self
            .runner
            .step(
                &mut self.tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: ask,
                    result: Ok(value),
                }]),
            )
            .expect("answer");
        self.settle(out);
        self.project(before)
    }

    /// Continue a suspended reply — the live half of a handler's
    /// `resume(value)`. Same bargain as [`answer`](Self::answer).
    pub fn resume(&mut self, value: serde_json::Value) -> Said {
        let before = self.tree.id_counter;
        let out = self.runner.resume(&mut self.tree, value).expect("resume");
        self.settle(out);
        self.project(before)
    }

    /// The menu row the model is shown for `id` — what the *document*
    /// says about a row, as distinct from what `history.fetch` would
    /// hand a program.
    ///
    /// Those two diverging is the whole design of the history bound: a
    /// long row renders clipped and fetches whole, and a window moves
    /// what is rendered without touching what is stored. A test about
    /// that reads both sides, and this is the rendered one.
    ///
    /// Panics if `id` is not a row of this branch, or if it does not
    /// render as a self-contained value — both are the test being
    /// wrong about what it appended.
    pub fn row_shown(&self, id: EventId) -> String {
        let segment = self.runner.agent_segment(&self.tree);
        let compacted = self.tree.compacted_lookup(self.runner.spine.leaf_id);
        let rows = crate::machine::menu_rows(&segment, 0, &compacted);
        let found: Vec<&crate::report::Artifact> =
            rows.iter().filter(|a| a.id == id.as_u64()).collect();
        match found.as_slice() {
            [a] => match &a.state {
                crate::report::ArtifactState::Whole(t) => t.clone(),
                other => panic!("#{} does not render whole: {other:?}", id.as_u64()),
            },
            [] => panic!("#{} is not a row of this branch", id.as_u64()),
            many => panic!("#{} renders as {} rows, not one", id.as_u64(), many.len()),
        }
    }

    /// How many menu rows name `id` — one, normally. A window or a
    /// replacement *moves* a row; it never adds one, and that is worth
    /// saying out loud in the tests about paging.
    pub fn rows_named(&self, id: EventId) -> usize {
        let segment = self.runner.agent_segment(&self.tree);
        let compacted = self.tree.compacted_lookup(self.runner.spine.leaf_id);
        crate::machine::menu_rows(&segment, 0, &compacted)
            .iter()
            .filter(|a| a.id == id.as_u64())
            .count()
    }

    /// `history.fetch(id)` as a program would see it — the value
    /// behind a row, which is a different thing from what the document
    /// renders for it (see [`row_shown`](Self::row_shown)).
    pub fn fetch(&self, id: EventId) -> serde_json::Value {
        self.runner
            .fetch_history(&self.tree, &[interp::Value::PosInt(id.as_u64())])
            .unwrap_or_else(|e| panic!("fetch #{} : {e}", id.as_u64()))
    }

    /// The question this branch is waiting on a person to answer, if
    /// it is waiting on one. A branch holds at most one at a time.
    pub fn open_ask(&self) -> Option<EventId> {
        self.open_asks.last().copied()
    }

    /// The posts on this branch still owed an answer — what a program
    /// names to `answer(id, …)`.
    pub fn open(&self) -> &[EventId] {
        self.runner.open()
    }

    /// What the branch is: `idle`, `thinking`, `running`, `suspended`.
    /// The same word the session's own branch list reports.
    pub fn status(&self) -> &'static str {
        self.runner.status()
    }

    /// The source span an event logged — where in the reply the
    /// expression that produced it was written. `(0, 0)` for anything
    /// synthetic.
    pub fn site_of(&self, id: EventId) -> (u32, u32) {
        match self.tree.events.get(&id).map(|e| &e.payload) {
            Some(EventPayload::Call(Call::Send { site, site_end, .. })) => (*site, *site_end),
            Some(EventPayload::Call(Call::Invoke { site, .. })) => (*site, 0),
            Some(EventPayload::Note { site, site_end, .. }) => (*site, *site_end),
            Some(EventPayload::Handback { site, .. }) => (*site, 0),
            _ => panic!("#{} has no site", id.as_u64()),
        }
    }

    /// The document the next request would carry — what the model is
    /// about to be looking at. Renders without advancing anything, so a
    /// test may read it as often as it likes.
    pub fn document(&self) -> String {
        self.runner
            .document(&self.tree, TEST_BUDGET)
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether a request went out after the last reply — the negation
    /// of `Said::rests`, spelled for the tests that read it directly.
    pub fn asked(&self) -> bool {
        self.requests > 0
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    pub fn runner(&self) -> &Runner {
        &self.runner
    }

    // ── the drive loop ──────────────────────────────────────────────

    /// Answer everything the machine asks for until it stops asking.
    ///
    /// The host's own loop in miniature, and deliberately the same
    /// shape: each `StepOutput` is a request for something, and this
    /// answers it the way `host::Session::dispatch` does — a tool with
    /// its registered handler, a `tell` with the delivery receipt a
    /// landed post produces, a `spawn` with the child's id. What it
    /// does *not* answer is an `ask`, because an answer is someone
    /// else's to give; those wait for [`Conversation::answer`].
    fn settle(&mut self, out: Vec<StepOutput>) -> Vec<StepOutput> {
        let mut seen = Vec::new();
        let mut queue = out;
        for _ in 0..MAX_ROUNDS {
            let mut working = false;
            let mut results: Vec<ToolResult> = Vec::new();
            for o in queue {
                match o {
                    StepOutput::Working => working = true,
                    // **Counted, not answered.** A test says what the
                    // model replies; the harness never invents one. The
                    // count is how `Said::rests` knows whether the
                    // branch was asked again.
                    StepOutput::LlmRequest(_) => self.requests += 1,
                    StepOutput::ToolCalls(calls) => {
                        for OutCall { call, name, args } in calls {
                            let result = match self.tools.get(&name) {
                                Some(f) => f(&args),
                                None => Err(format!(
                                    "no tool `{name}` in this test — register it with \
                                     `c.answers(\"{name}\", …)`"
                                )),
                            };
                            results.push(ToolResult { call, result });
                        }
                    }
                    StepOutput::Sends(ids) => {
                        for id in ids {
                            match self.send_shape(id) {
                                // An ask waits for an answer.
                                Some(true) => self.open_asks.push(id),
                                Some(false) => results.push(ToolResult {
                                    call: id,
                                    result: Ok(json!({ "post": serde_json::Value::Null })),
                                }),
                                None => {}
                            }
                        }
                    }
                    StepOutput::Spawns(ids) | StepOutput::Forks(ids) => {
                        for id in ids {
                            results.push(ToolResult {
                                call: id,
                                result: Ok(json!({ "agent": id.as_u64() })),
                            });
                        }
                    }
                    other => seen.push(other),
                }
            }
            if !results.is_empty() {
                queue = self
                    .runner
                    .step(&mut self.tree, StepInput::ToolResults(results))
                    .expect("tool results");
                continue;
            }
            if !working {
                return seen;
            }
            queue = self
                .runner
                .step(&mut self.tree, StepInput::Tick { fuel: FUEL })
                .expect("tick");
        }
        panic!("the machine never settled — {MAX_ROUNDS} rounds and still asking");
    }

    /// `Some(expects_reply)` for a logged `Send`, `None` for anything
    /// else.
    fn send_shape(&self, id: EventId) -> Option<bool> {
        match self.tree.events.get(&id).map(|e| &e.payload) {
            Some(EventPayload::Call(Call::Send { expects_reply, .. })) => Some(*expects_reply),
            _ => None,
        }
    }

    // ── the projection ──────────────────────────────────────────────

    /// Everything on this branch newer than `before` — the events this
    /// reply produced, and only those.
    fn scope(&self, before: u64) -> Vec<&Event> {
        self.runner
            .agent_segment(&self.tree)
            .into_iter()
            .filter(|e| e.id.as_u64() > before)
            .collect()
    }

    fn project(&self, before: u64) -> Said {
        let events = self.scope(before);
        let mut s = fold(&self.tree, &events, (before, self.tree.id_counter));
        s.reply = self.runner.reply_id;
        s.rests = self.requests == 0 && self.runner.is_idle();
        s
    }
}

/// The same projection over a whole branch of a finished log — what a
/// session test reads instead of walking the tree by hand.
///
/// **`rests` is not knowable here.** Resting is the absence of a
/// request, which a log does not record; from a finished log the
/// honest reading is "nothing is open and the last program ended", and
/// that is what this reports. Everything else means exactly what it
/// means from a live reply, because it is the same fold.
impl Said {
    pub fn of_branch(tree: &Tree, leaf: EventId) -> Said {
        let owned = tree.path_events(leaf);
        let events: Vec<&Event> = owned.to_vec();
        let mut s = fold(tree, &events, (0, leaf.as_u64()));
        s.reply = events
            .iter()
            .rev()
            .find(|e| matches!(e.payload, EventPayload::Reply | EventPayload::Restart))
            .map(|e| e.id)
            .unwrap_or(leaf);
        s.rests = s.asks.is_empty() && matches!(s.ended, Ending::Finished | Ending::Completed);
        s
    }
}

fn fold(tree: &Tree, events: &[&Event], span: (u64, u64)) -> Said {
    let mut s = Said {
        reply: EventId::new(1),
        parts: Vec::new(),
        handback: None,
        finished_by: None,
        prose: Vec::new(),
        cells: Vec::new(),
        tells: Vec::new(),
        told: Vec::new(),
        asks: Vec::new(),
        rows: Vec::new(),
        calls: Vec::new(),
        printed: Vec::new(),
        heard: Vec::new(),
        notices: Vec::new(),
        compacted: Vec::new(),
        settled: Vec::new(),
        answered: Vec::new(),
        ended: Ending::Running,
        reply_ended: None,
        usage: Vec::new(),
        // **Rested, not merely quiet.** A reply that parked — on a
        // raise, on a `stop` — also leaves no request behind at
        // this layer, because the one that wakes it is the host's
        // to send. Resting is the branch going *idle* owing
        // nothing, which is a different state and the only one
        // `finish` produces.
        rests: false,
        kinds: Vec::new(),
        span,
    };
    for e in events {
        s.kinds.push(kind_of(&e.payload));
        // Thinking is not a piece of the reply, so it is not a part.
        if let EventPayload::Part { part, .. } = &e.payload
            && !matches!(part, crate::types::Part::Thinking(_))
        {
            s.parts.push(e.id);
        }
        match &e.payload {
            EventPayload::Part { part, .. } => match part {
                crate::types::Part::Cell(t) => s.cells.push(t.clone()),
                // The verbatim prose is the invariant's business,
                // not a test's: what a test reads is the `Send`
                // below, which is what the person actually got.
                crate::types::Part::Prose(_) | crate::types::Part::Thinking(_) => {}
            },
            // **Prose is a `Send` too** — a paragraph between cells
            // reaches the person as a message like any other. It is
            // its own field rather than a `tell`, because counting
            // it as one would make every narrating reply look like
            // it said everything twice.
            EventPayload::Call(Call::Send {
                prose: true, text, ..
            }) => s.prose.push(text.clone()),
            EventPayload::Call(Call::Send {
                text,
                expects_reply,
                to,
                options,
                site,
                site_end,
                ..
            }) => {
                if *expects_reply {
                    s.asks.push(Ask {
                        call: e.id,
                        to: *to,
                        text: text.clone(),
                        options: options.clone(),
                    });
                } else {
                    s.tells.push(text.clone());
                    s.told.push(e.id);
                    // No call expression behind it: `finish(text)`
                    // is an effect, not a call, so its send has the
                    // synthetic zero-width site.
                    if (*site, *site_end) == (0, 0) {
                        s.finished_by = Some(e.id);
                    }
                }
            }
            EventPayload::Call(Call::Invoke { name, args, .. }) => {
                s.calls.push((name.clone(), args.clone()))
            }
            EventPayload::Note {
                value,
                site,
                site_end,
                ..
            } => s.rows.push(Row {
                id: e.id,
                value: value.clone(),
                site: (*site, *site_end),
            }),
            EventPayload::Post { from, origin } => {
                if let Some((text, _, _)) = tree.resolve(origin).direct() {
                    match from {
                        Author::Harness => s.notices.push(text.to_owned()),
                        Author::Agent(_) => s.heard.push(text.to_owned()),
                        Author::User => {}
                    }
                }
            }
            EventPayload::Compacted { of, .. } => s.compacted.push(*of),
            EventPayload::Console { lines } => s.printed.extend(lines.iter().cloned()),
            EventPayload::Result { call, outcome } => s.settled.push((*call, outcome.clone())),
            EventPayload::Answer { question, value } => s.answered.push((*question, value.clone())),
            EventPayload::ReplyEnd { how, usage, .. } => {
                s.reply_ended = Some(how.clone());
                s.usage.push(*usage);
            }
            EventPayload::Handback { how, site, .. } => {
                s.ended = ending_of(how, *site);
                s.handback = Some(e.id);
            }
            _ => {}
        }
    }
    finish_up(&mut s);
    s
}

// ── the invariants ──────────────────────────────────────────────

impl Conversation {
    fn enforced(&self, inv: Invariant) -> bool {
        !self.allowed.contains(&inv)
    }

    fn check(&self, s: &Said, markdown: &str) {
        let events = self.scope(s.span.0);
        if self.enforced(Invariant::OneReply) {
            let replies = s
                .kinds
                .iter()
                .filter(|k| **k == "Reply" || **k == "Restart")
                .count();
            assert_eq!(
                replies, 1,
                "one completion must log exactly one Reply, got {replies}: {:?}",
                s.kinds
            );
        }
        if self.enforced(Invariant::ReplyEnds) {
            let ends = s.kinds.iter().filter(|k| **k == "ReplyEnd").count();
            assert_eq!(
                ends, 1,
                "a reply must end exactly once, got {ends}: {:?}",
                s.kinds
            );
        }
        if self.enforced(Invariant::PartsConcatenate) {
            let rebuilt: String = events
                .iter()
                .filter_map(|e| match &e.payload {
                    EventPayload::Part {
                        part: crate::types::Part::Prose(t) | crate::types::Part::Cell(t),
                        ..
                    } => Some(t.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                rebuilt, markdown,
                "the parts must put the reply back together (docs/28)"
            );
        }
        if self.enforced(Invariant::HandbackNamesTheReply) {
            let reply = events
                .iter()
                .find(|e| matches!(e.payload, EventPayload::Reply | EventPayload::Restart))
                .map(|e| e.id);
            for e in &events {
                if let EventPayload::Handback { reply: named, .. } = &e.payload {
                    assert_eq!(
                        Some(*named),
                        reply,
                        "a Handback names a reply that is not this one: {named:?}"
                    );
                }
            }
        }
        if self.enforced(Invariant::ProseIsSynthetic) {
            for e in &events {
                if let EventPayload::Call(Call::Send {
                    prose: true,
                    site,
                    site_end,
                    text,
                    ..
                }) = &e.payload
                {
                    assert_eq!(
                        (*site, *site_end),
                        (0, 0),
                        "prose {text:?} was given a real site"
                    );
                }
            }
        }
        if self.enforced(Invariant::SitesAreReplyAbsolute) && !self.resumed {
            for e in &events {
                let (site, site_end) = match &e.payload {
                    EventPayload::Call(Call::Send { site, site_end, .. }) => (*site, *site_end),
                    EventPayload::Note { site, site_end, .. } => (*site, *site_end),
                    // A handback carries a point, not a span.
                    EventPayload::Handback { site, .. } if *site > 0 => (*site, *site + 1),
                    _ => continue,
                };
                if site_end == 0 {
                    continue; // synthetic; `ProseIsSynthetic` has that one
                }
                assert!(
                    site < site_end && (site_end as usize) <= markdown.len(),
                    "#{} spans {site}..{site_end} of a {}-byte reply",
                    e.id.as_u64(),
                    markdown.len()
                );
                assert!(
                    markdown.is_char_boundary(site as usize)
                        && markdown.is_char_boundary(site_end as usize),
                    "#{} spans {site}..{site_end}, which is not a character boundary",
                    e.id.as_u64()
                );
            }
        }
        if self.enforced(Invariant::CallsSettle) {
            // Not while something is parked: a suspended or abandoned
            // run leaves calls open by design.
            let parked = !matches!(
                s.ended,
                Ending::Completed | Ending::Finished | Ending::Stopped(_)
            );
            if !parked {
                for e in &events {
                    let EventPayload::Call(call) = &e.payload else {
                        continue;
                    };
                    if matches!(
                        call,
                        Call::Send {
                            expects_reply: true,
                            ..
                        }
                    ) {
                        continue; // an answer is someone else's to give
                    }
                    let settled = events.iter().any(|x| {
                        matches!(&x.payload, EventPayload::Result { call, .. } if *call == e.id)
                    });
                    assert!(
                        settled,
                        "#{} ({}) was issued and never settled",
                        e.id.as_u64(),
                        kind_of(&e.payload)
                    );
                }
            }
        }
    }
}

/// **Finishing and handing on are one `Completed` on the log.** The
/// difference is whether the reply *said* something on its way out, and
/// `finish(text)` is the only thing that sends with no call expression
/// behind it — the synthetic zero-width site the verb gives its send,
/// which an ordinary `tell` never has.
///
/// Read from the events rather than from whether the branch rested,
/// because resting is a live fact and this has to be decidable from a
/// finished log too.
fn finish_up(s: &mut Said) {
    if s.ended == Ending::Completed && s.finished_by.is_some() {
        s.ended = Ending::Finished;
    }
}

fn ending_of(how: &Handback, site: u32) -> Ending {
    match how {
        Handback::Completed => Ending::Completed,
        Handback::Stopped { reason } => Ending::Stopped(reason.clone()),
        Handback::Raised { name, payload, .. } => Ending::Raised {
            name: name.clone(),
            payload: payload.clone(),
        },
        Handback::Trapped { message, .. } => Ending::Trapped {
            message: message.clone(),
            site,
        },
        Handback::CellFailed { message } => Ending::CellFailed(message.clone()),
        Handback::Abandoned => Ending::Abandoned,
        Handback::Posted { .. } => Ending::Posted,
        _ => Ending::Running,
    }
}

fn kind_of(p: &EventPayload) -> &'static str {
    match p {
        EventPayload::Agent { .. } => "Agent",
        EventPayload::Fork { .. } => "Fork",
        EventPayload::Answer { .. } => "Answer",
        EventPayload::Post { .. } => "Post",
        EventPayload::Reply => "Reply",
        EventPayload::Compaction { .. } => "Compaction",
        EventPayload::Part { .. } => "Part",
        EventPayload::ReplyEnd { .. } => "ReplyEnd",
        EventPayload::Restart => "Restart",
        EventPayload::Handback { .. } => "Handback",
        EventPayload::Call(_) => "Call",
        EventPayload::Result { .. } => "Result",
        EventPayload::Console { .. } => "Console",
        EventPayload::Note { .. } => "Note",
        EventPayload::Compacted { .. } => "Compacted",
        EventPayload::Rename { .. } => "Rename",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape, end to end: a reply that reads a file, says what it
    /// found and finishes. Everything this asserts is a fact about the
    /// reply, named — there is no scan of the log here at all.
    #[test]
    fn a_reply_reads_says_and_finishes() {
        let mut c = Conversation::new();
        c.answers("read_file", json!({ "content": "NEW", "version": 1 }));
        c.user("what does PATH say?");

        let r = c.reply(
            "Reading it.\n\n```js\nconst f = await tools.read_file(\"PATH\");\nfinish(`PATH says ${f.content}.`);\n```\n",
        );

        assert_eq!(r.prose, ["Reading it."]);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].0, "read_file");
        assert_eq!(r.tells, ["PATH says NEW."]);
        assert_eq!(r.ended, Ending::Finished);
        assert!(r.rests, "finish rests the branch");
    }

    /// The other ending. `stop` halts and does **not** rest: the reason
    /// goes in front of the next reply, which is the whole point of it
    /// being a separate verb.
    #[test]
    fn a_reply_that_stops_says_why_and_does_not_rest() {
        let mut c = Conversation::new();
        c.answers("bash", json!({ "status": 1, "stdout": "2 failed" }));
        c.user("is it green?");

        let r = c.reply(
            "```js\nconst check = await tools.bash(\"make check\");\nif (check.status !== 0) stop(`CHECK fails:\\n${check.stdout}`);\nfinish(\"green.\");\n```\n",
        );

        assert_eq!(r.ended, Ending::Stopped("CHECK fails:\n2 failed".into()));
        assert!(
            r.tells.is_empty(),
            "it stopped before it could claim success"
        );
        assert!(
            !r.rests,
            "a stop is not an ending — the branch is asked again"
        );
    }

    /// **`finish(text)` halts, and the cells after it never run** (D8,
    /// D11) — which is the whole difference from the verb that used to
    /// settle and carry on. What it does *not* do is cancel the
    /// generation: the reply keeps arriving, every part of it is logged
    /// (28 — the record of what was written stays whole), and the turn
    /// closes on the `ReplyEnd` that carries what the completion cost.
    /// Written but not run is a state the log can say, and this is it.
    ///
    /// Chunked deliberately: the halt has to happen while the second
    /// cell is still being written, which is the case that broke.
    #[test]
    fn finish_in_a_cell_skips_the_cells_after_it() {
        let mut c = Conversation::new();
        c.user("go");

        let r = c.reply_in_chunks(&[
            "```js\nfinish(\"ok\");\n```\n",
            "\n```js\ntell(\"after finish\");\n```\n",
        ]);

        assert_eq!(
            r.cells.len(),
            2,
            "the cell it wrote after `finish` is on the log"
        );
        assert_eq!(r.tells, ["ok"], "and nothing after `finish(text)` ran");
        assert_eq!(r.ended, Ending::Finished);
        assert!(r.rests, "`finish(text)` rests the branch");
        // The turn is a whole one. `ReplyEnds` and `OneReply` already
        // hold — they are checked on every reply — so this only has to
        // say where the end sits: after the word `finish` sent, because
        // the reply was still arriving when the program ended itself.
        assert_eq!(
            r.kinds,
            [
                "Reply", "Part", "Call", "Result", "Part", "Part", "ReplyEnd", "Handback",
                "Console"
            ],
            "the word `finish` sent goes out while the second cell is still arriving, \
             and the reply's end lands after both"
        );
    }

    /// A reply that hands on rather than ending: no `finish`, so the
    /// branch is asked again with the row in front of it.
    #[test]
    fn a_reply_that_appends_a_row_hands_on() {
        let mut c = Conversation::new();
        c.answers("bash", json!({ "status": 0, "stdout": "a.rs\nb.rs\n" }));
        c.user("which files mention it?");

        let r = c.reply(
            "```js\nconst hits = (await tools.bash(\"grep -rl OLD .\")).stdout.split(\"\\n\").filter(Boolean);\nhistory.append({ hits });\n```\n",
        );

        assert_eq!(r.row().value, json!({ "hits": ["a.rs", "b.rs"] }));
        assert_eq!(r.ended, Ending::Completed);
        assert!(!r.rests, "completing is not, by itself, a reason to rest");
    }

    /// An `ask` waits. The harness answers nothing on the model's
    /// behalf — that is a person's to give — so the reply parks and the
    /// answer is a second step the test writes out.
    #[test]
    fn an_ask_parks_until_it_is_answered() {
        let mut c = Conversation::new();
        c.user("set it to whatever I say");

        let r = c.reply(
            "```js\nconst n = await ask(\"user\", \"how many?\");\nfinish(`set to ${n}.`);\n```\n",
        );

        assert_eq!(r.ask().text, "how many?");
        assert!(r.tells.is_empty(), "nothing said yet — it is waiting");

        let call = r.ask().call;
        let after = c.answer(call, json!(7));
        assert_eq!(
            after.tells,
            ["set to 7."],
            "the answer reached the expression that asked"
        );
        assert_eq!(after.ended, Ending::Finished);
    }

    /// The history verbs work in any program, not only a compaction
    /// one, and the edit lands when that program finishes.
    ///
    /// They used to be refused outside a compaction program, on the
    /// reasoning that history is not a thing an ordinary program edits.
    /// But the program that made an entry is the one that knows what it
    /// was worth: having read a listing and picked four paths out of
    /// it, it knows right then that the listing is not worth carrying,
    /// and knows it better than a compaction program will later with
    /// less to go on.
    #[test]
    fn a_history_edit_applies_from_any_program() {
        let mut c = Conversation::new();
        let post = c.user("go");

        let r = c.reply(&format!(
            "```js\nhistory.remove({}); tell(\"done\");\n```\n",
            post.as_u64()
        ));

        assert_eq!(
            r.tells,
            ["done"],
            "no refusal, the program ran straight through"
        );
        assert_eq!(
            r.compacted,
            [post],
            "and the edit landed when the program finished"
        );
        // **And it does not compact itself.** That rule is for the
        // reply the harness *asked* for, which is work in a document it
        // asked to be made smaller. An ordinary program that happens to
        // use the same verb is the conversation, and stays in it.
        assert!(
            !r.compacted.iter().any(|id| r.owns(*id)),
            "an ordinary program compacted its own block: {:?}",
            r.compacted
        );
    }

    /// A tool that fails is not a tool that is missing, and the
    /// difference reaches the program: the promise rejects, and an
    /// uncaught rejection traps the reply rather than ending it.
    #[test]
    fn a_failing_tool_rejects_and_the_trap_carries_its_message() {
        let mut c = Conversation::new();
        c.rejects("bash", "no such command");
        c.user("run it");

        let r = c.reply("```js\nawait tools.bash(\"nope\");\nfinish(\"ran it.\");\n```\n");

        assert!(
            matches!(&r.ended, Ending::Trapped { message, .. } if message.contains("no such command")),
            "the tool's own words reach the trap: {:?}",
            r.ended
        );
        assert!(r.tells.is_empty(), "nothing after the rejection ran");
    }

    /// A `choose` carries the options with it — the recipient sees what
    /// it may answer, which is what makes the answer comparable with
    /// `===` instead of parsed out of prose.
    #[test]
    fn a_choose_carries_its_options() {
        let mut c = Conversation::new();
        c.user("A or B?");

        let r = c.reply(
            "```js\nconst pick = await choose(\"user\", \"which?\", [\"A\", \"B\"]);\nfinish(`picked ${pick}.`);\n```\n",
        );

        assert_eq!(r.ask().options, ["A", "B"]);
        let after = c.answer(r.ask().call, json!("B"));
        assert_eq!(after.tells, ["picked B."]);
    }

    /// `raise` parks for a judgement and the expression carries on from
    /// where it stopped, every binding still alive.
    #[test]
    fn a_raise_parks_and_resumes_into_the_expression() {
        let mut c = Conversation::new();
        c.user("go");

        let r = c.reply(
            "```js\nconst n = 41;\nconst which = raise(\"pick_one\", { n });\nfinish(`took ${which}, n was ${n}.`);\n```\n",
        );

        assert_eq!(
            r.ended,
            Ending::Raised {
                name: "pick_one".into(),
                payload: Some(json!({ "n": 41 }))
            }
        );
        assert!(!r.rests, "a parked branch is not a rested one");

        let after = c.resume(json!("b2"));
        assert_eq!(
            after.tells,
            ["took b2, n was 41."],
            "the frame was still standing"
        );
    }

    /// What the reply left behind is what the next request carries —
    /// the one thing no assertion about the log can see, because it is
    /// about the document rather than about the events.
    #[test]
    fn the_row_a_reply_appends_is_in_the_next_request() {
        let mut c = Conversation::new();
        c.user("what is here?");
        c.reply("```js\nhistory.append({ found: \"two config files\" });\n```\n");

        assert!(
            c.document().contains("two config files"),
            "the next reply reads what this one appended"
        );
    }

    /// **The harness's own annotations do not reach the person.** The
    /// model reads `↓ history[N]` above each of its blocks and writes
    /// them back; the document render has stripped those from its next
    /// prompt for a while, but prose leaves by a second door — the
    /// `Send` a paragraph becomes — and that one carried them through.
    ///
    /// Found live against `Qwen3.8-27B` on 2026-09-20, driving a real
    /// session as the person at the keyboard: the answer that reached
    /// the screen opened with an annotation the person never saw the
    /// original of. Then counted across the kept corpus — 36 of 373
    /// runs delivered one, 52 of 1,640 prose messages.
    #[test]
    fn a_marker_the_model_copied_does_not_reach_the_person() {
        let mut c = Conversation::new();
        c.user("go");
        let r = c.reply(
            "↓ history[17]\nThe check is simple, so I will just run it.\n\n\
             ↓ history[18]\n```js\nfinish(\"ran it.\");\n```\n",
        );

        assert_eq!(
            r.prose,
            ["The check is simple, so I will just run it."],
            "the annotation is ours; the person reads the sentence"
        );
        // And the log still has every byte — that is what makes the
        // parts concatenate back to the reply, which the harness
        // checks on every reply.
        assert!(
            c.fetch(r.parts[0])
                .as_str()
                .is_some_and(|t| t.contains("history[17]")),
            "the part keeps what the model wrote"
        );
    }

    /// A `↓` in a sentence is something the model meant, and stays.
    #[test]
    fn only_a_bare_marker_line_is_dropped() {
        let mut c = Conversation::new();
        c.user("go");
        let r =
            c.reply("The count ↓ history[3] is the one I want.\n\n```js\nfinish(\"ok\");\n```\n");
        assert_eq!(r.prose, ["The count ↓ history[3] is the one I want."]);
    }

    /// **A cell that ends with a fire-and-forget call does not end the
    /// reply.** The VM reaches that cell's `Pause` with the call still
    /// outstanding; the settlement the host hands back a moment later
    /// used to pump the VM off the end of the code it had, report
    /// `Done`, and hand the run back — while the reply was still
    /// arriving. Every cell after it was then dropped in silence,
    /// because `notebook_feed` finds no run to feed.
    ///
    /// `tell("…")` as the last statement of a cell is the shape three
    /// exemplars teach, so this was reachable by the most ordinary
    /// reply there is. Found on 2026-09-20 by porting a streaming test
    /// to this harness: the old one drove ticks but never handed back
    /// the settlements, so the reply never reached the state that
    /// breaks.
    #[test]
    fn settling_a_cells_last_tell_does_not_end_the_reply() {
        let mut c = Conversation::new();
        c.user("go");

        let so_far = c.chunk("```js\ntell(\"cell 0 ran\");\n```\n");
        assert_eq!(so_far.tells, ["cell 0 ran"]);
        assert_eq!(so_far.ended, Ending::Running, "the reply is still arriving");
        assert_eq!(c.status(), "running");

        c.chunk("\n```js\ntell(\"cell 1 ran\");\n```\n");
        let whole = c.end_reply();
        assert_eq!(
            whole.tells,
            ["cell 0 ran", "cell 1 ran"],
            "the cell after it ran too"
        );
    }

    // ── the harness's own guarantees ────────────────────────────────

    /// The invariants are not decoration: each one fails a test when it
    /// is violated. Checked by violating them on purpose, because an
    /// invariant that cannot be made to fail is not being evaluated.
    #[test]
    #[should_panic(expected = "must put the reply back together")]
    fn parts_concatenate_is_enforced() {
        let mut c = Conversation::new();
        c.user("go");
        // A reply whose text the log deliberately cannot reproduce:
        // the provider leaked its reasoning into the reply channel and
        // closed it with a bare `</think>`, which the notebook strips
        // (`Notebook::drop_leaked_reasoning`). Right to strip — the
        // leak has swallowed a whole cell before now — and the
        // invariant is right to notice the parts no longer add up.
        c.reply("weighing it up</think>\n\n```js\ntell(\"hi\");\n```\n");
    }

    /// And `allow` is how a test that means to break one says so.
    #[test]
    fn an_exemption_is_spelled_out() {
        let mut c = Conversation::new();
        c.allow(Invariant::PartsConcatenate);
        c.user("go");
        let r = c.reply("weighing it up</think>\n\n```js\ntell(\"hi\");\n```\n");
        assert_eq!(r.tells, ["hi"]);
    }
}
