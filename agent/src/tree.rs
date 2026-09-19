use crate::types::*;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// One inner call a program made, with its settlement if one landed.
/// `outcome` is `None` while the call is still in flight — the same
/// distinction the artifact menu draws.
#[derive(Debug, Clone, PartialEq)]
pub struct InvokeView {
    pub id: EventId,
    pub name: String,
    pub args: serde_json::Value,
    pub outcome: Option<Outcome>,
    /// The call's dispatch site (a byte offset into the program source)
    /// and whether it is a `Send` — what `report::annotate_calls` needs
    /// to annotate a running program's source live, from this same
    /// log-derived projection rather than the VM (17_BRANCHES Part D).
    pub site: u32,
    pub is_send: bool,
}

/// One program execution, projected from the log: everything its panes
/// need without a live VM (decision 8). `id` is the `Turn` event's own
/// id — under code mode every `Turn` **is** a program, so there is no
/// `run_program`/`resume` split to fold into one entry the way the old
/// tool-call protocol needed: a handler program (the mind's answer to a
/// `raise()`) is a `Turn` in its own right, with its own id and its own
/// `ProgramView`, distinguished from the program it is deliberating for
/// only by `depth`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramView {
    pub id: EventId,
    pub source: String,
    pub invokes: Vec<InvokeView>,
    /// The top-level `return` value (`Some` ⇒ ran to completion).
    pub result: Option<serde_json::Value>,
    /// The id of this program's most recent outcome event — what its
    /// report is derived from.
    pub outcome: Option<EventId>,
    /// The cause of that outcome, when it was a `Condition`.
    pub condition: Option<crate::types::Handback>,
    /// Console (from the `Console` event, already capped at logging).
    pub console: Vec<String>,
    /// The handler-nesting depth this program ran at — **derived** from
    /// `Condition::disposition` on replay, never stored (see
    /// [`depth_after`]). `0` is the branch's own document: a root
    /// program, or a handover's continuation ("literally the branch's
    /// current program", doc 22), which renders as a full turn with no
    /// nesting. `> 0` is a deliberating handler's interior — invisible
    /// to `document::render`'s request, but not to this projection:
    /// `programs_for` is exactly the debug/TUI surface doc 22's
    /// render-axes table calls a "nested handler block", and a caller
    /// can only draw that nesting if the depth survives into the view.
    pub depth: usize,
}

impl ProgramView {
    /// The program's status, **derived from the log**. `protocol.rs` used
    /// to note that suspended-vs-failed "is not inferable from the report
    /// text"; with one outcome event per handback it now is, so a reopened
    /// log can say how a program ended.
    pub fn status(&self) -> crate::host::ProgramStatus {
        use crate::host::ProgramStatus;
        match &self.condition {
            // A `Return` clears the condition, so this is the last word.
            None if self.result.is_some() => ProgramStatus::Completed,
            // Issued, no outcome: in flight, or lost with the process.
            None => ProgramStatus::Running,
            // A raise or a trap is a suspension the LLM can restart.
            // A pause the next reply can answer.
            Some(h) if !h.is_terminal() => ProgramStatus::Suspended,
            Some(crate::types::Handback::Completed) => ProgramStatus::Completed,
            // The VM is gone.
            Some(_) => ProgramStatus::Failed,
        }
    }

    /// The report the LLM read for this program's latest handback,
    /// derived from the log (`None` while the run has no outcome).
    pub fn report(&self, tree: &Tree, leaf: EventId, budget: usize) -> Option<String> {
        self.outcome
            .map(|o| crate::report::derive_report(tree, leaf, o, budget))
    }
}

/// The log's format version. Bump it when the event vocabulary changes
/// in a way an older build would misread; nothing migrates, because a
/// misread log is worse than a refused one.
pub const LOG_VERSION: u64 = 2;

/// The log's first line: a version header, never an event.
#[derive(Serialize, Deserialize)]
struct LogHeader {
    version: u64,
}

/// Update the derived handler-nesting counter for one path event — the
/// single fold both `programs_for`'s attach-target stack and
/// `document::render`'s depth-0 filter apply, so the two consumers can
/// never silently disagree about what depth an event ran at. Doc 22 is
/// explicit about the stakes: "if the log doesn't say [which raise
/// pushed a handler and which handed over], every subsequent depth is
/// wrong — and with it the document's `depth > 0` filter and the
/// decision/completion reading of `Return`."
///
/// - `Condition{disposition: Pushed}`: the raising frame is still on
///   the stack, suspended, waiting on the handler that runs next —
///   depth increases by one for whatever follows.
/// - `Condition{disposition: Handover}`: the raising frame was popped
///   *before* the handler was built (a real tail call — `stack.rs`'s
///   old `ProgramStack::push`/`apply_decision` semantics, now derived
///   instead of stored), so depth is unchanged: the handler that
///   follows opens at exactly the depth the raise happened at, not one
///   deeper.
/// - `Return`: settles exactly one frame — the frame that just decided,
///   however many chained handovers it took to get there (`stack.rs`'s
///   `apply_decision` does exactly one `frames.pop()` per decision) —
///   so depth decreases by one. Saturating: a depth-0 program's own
///   ordinary `return`, with no raise anywhere in its history, is the
///   common case and must not underflow.
/// - `Condition{cause: Abandoned}`: `return abandon()` discarded the
///   suspended run, which settles exactly one frame the same way a
///   `Return` does — so depth decreases by one, **whatever the
///   condition's own disposition says.** The disposition describes the
///   raise that opened a scope; this cause describes a decision that
///   closes one, so it is matched first. Without this arm an abandoned
///   run never decrements and every later event on the branch renders
///   as though still inside a scope nothing will ever close.
/// - `Condition{cause: Interrupted}`: the process died and the VM went
///   with it — reconciliation's repair (`host/mod.rs`'s `reconcile`)
///   gives the run an outcome "like any other" precisely so it settles
///   the same way a `Return` or an `Abandoned` does, **whatever
///   disposition the repair stamped on it** (`Disposition::Pushed`,
///   the type's own safe default — nothing is actually about to run at
///   a deeper level, there is no handler "that runs next" for a crash).
///   Matched before the disposition arms for the identical reason
///   `Abandoned` is: without this, a crash below depth 0 would leave
///   every later event on the branch permanently misrendered as still
///   inside a scope nothing will ever close.
/// - anything else leaves depth unaffected.
pub fn depth_after(depth: usize, payload: &EventPayload) -> usize {
    // **Derived, where `Disposition` used to be stored.** A handback
    // that pauses opens a scope, because a reply is about to run inside
    // it; a handback that ends closes one. The two were encoded in two
    // places and could disagree — 28 keeps one.
    match payload {
        EventPayload::Handback { how, .. } if how.is_terminal() => depth.saturating_sub(1),
        EventPayload::Handback { .. } => depth + 1,
        _ => depth,
    }
}

/// A `Compacted` event, resolved: the label always shown in the
/// target's place, and the rewritten text when the op was a rewrite
/// rather than a removal. Returned by [`Tree::compacted_lookup`]; see
/// `EventPayload::Compacted`'s own doc comment for why the target row
/// is never actually removed from the log.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactedView {
    /// `None` ⇒ the entry renders nothing at all; `Some` ⇒ this is shown
    /// in its place.
    pub text: Option<String>,
}

impl Tree {
    /// An **in-memory** tree, or one over a file whose header the caller
    /// has already written. Production creates through [`Tree::open`],
    /// which writes the header for an empty file, so this is only ever
    /// handed `None`.
    pub fn new(file: Option<File>) -> Self {
        Self {
            id_counter: 0,
            events: HashMap::new(),
            file,
            reports: Default::default(),
        }
    }

    /// Load the event log. Reconstructing spines is the caller's move:
    /// `list_leaves()` for the set, `spine_at(leaf)` for a handle.
    ///
    /// An **empty** file is a new log and gets the version header
    /// written; anything else must carry one this build understands.
    /// Nothing migrates a pre-17 log: the vocabulary changed under it, so
    /// the honest answer is to refuse it by name rather than to read it
    /// as something it is not.
    pub fn open(mut file: File) -> Result<Self, io::Error> {
        // Rewind to start: the file is opened with `append(true)`, which
        // positions the cursor at the end.
        file.seek(SeekFrom::Start(0))?;

        let mut content = String::new();
        file.read_to_string(&mut content)?;
        // Where the cursor sits now does not matter: under O_APPEND the
        // kernel moves every write to the end of the file as it makes
        // it. That is what keeps a second process holding the same log
        // from writing *into* a line this one is emitting (see
        // `open_tree`), and it is the only reason reading the whole file
        // through the same handle we later write through is safe.

        if content.trim().is_empty() {
            let mut tree = Tree::new(Some(file));
            tree.write_header()?;
            return Ok(tree);
        }
        let mut max_id: u64 = 0;
        let mut events = HashMap::<EventId, Event>::new();
        // A crash can now fall mid-line: the log syncs once per loop
        // step, so the tail of the step in progress may be torn. A torn
        // **last** line was never acknowledged to anyone, so dropping it
        // loses nothing reconciliation cannot repair. Anywhere else a bad
        // line is real corruption and must not be swallowed.
        let mut lines = content.lines().peekable();
        let header = lines.next().unwrap_or_default();
        match serde_json::from_str::<LogHeader>(header) {
            Ok(LogHeader { version }) if version == LOG_VERSION => {}
            Ok(LogHeader { version }) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "this log is format version {version}; this build reads \
                         version {LOG_VERSION}. Nothing migrates it — the event \
                         vocabulary changed — so open it with a matching build."
                    ),
                ));
            }
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "this log has no format-version header; this build reads \
                         version {LOG_VERSION}. Pre-17 logs are not migrated — the \
                         event vocabulary changed — so open it with a matching build."
                    ),
                ));
            }
        }
        let mut torn = false;
        while let Some(line) = lines.next() {
            let last = lines.peek().is_none();
            let event: Event = match serde_json::from_str(line) {
                Ok(event) => event,
                Err(_) if last && !content.ends_with('\n') => {
                    torn = true;
                    break;
                }
                Err(e) => return Err(e.into()),
            };
            max_id = std::cmp::max(max_id, event.id.as_u64());
            events.insert(event.id, event);
        }
        if torn {
            // Truncate the partial record so the next append starts on a
            // clean line boundary.
            let keep = content.rfind('\n').map(|i| i + 1).unwrap_or(0) as u64;
            file.set_len(keep)?;
            file.seek(SeekFrom::Start(keep))?;
        }

        Ok(Self {
            id_counter: max_id,
            events,
            file: Some(file),
            reports: Default::default(),
        })
    }

    /// Start a new agent: append a `Agent` rooting a new spine.
    /// `parent_id` is the call-site event on the caller's spine (`None`
    /// only for the tree's root agent). The caller's own spine handle is
    /// untouched — its leaf does not advance past the call site.
    /// `tools` is the agent's own allowlist, enforced by the registry
    /// **from this root** rather than from an event on its parent's
    /// branch — which is what makes the root agent, with no `Spawn`
    /// above it, not a special case. `None` means "everything".
    pub fn start_agent(
        &mut self,
        parent_id: Option<EventId>,
        name: Option<String>,
        charter: impl Into<String>,
        tools: Option<Vec<String>>,
        system: impl Into<String>,
        exemplars: Vec<Exemplar>,
    ) -> io::Result<Spine> {
        match parent_id {
            None => assert!(self.events.is_empty(), "root Agent on a non-empty tree"),
            Some(parent) => assert!(
                self.events.contains_key(&parent),
                "Agent parent {parent:?} not in tree"
            ),
        }

        let payload = EventPayload::Agent {
            name,
            charter: charter.into(),
            tools,
            system: system.into(),
            exemplars,
        };
        let id = self.log_event(parent_id, payload)?;
        Ok(self.spine_at(id))
    }

    /// Fork from any event: reconstruct the spine at `from` and return
    /// an appendable handle. The first `append` on the returned spine
    /// creates a *sibling* of `from`'s existing spine child.
    ///
    /// Nothing seals a branch — **agents never close** — so the only way
    /// this fails is an id that is not in the tree.
    pub fn fork(&self, from: EventId) -> io::Result<Spine> {
        if !self.events.contains_key(&from) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("cannot fork: event {from:?} not in tree"),
            ));
        }
        Ok(self.spine_at(from))
    }

    /// Append an event to a spine. The spine's leaf advances and the
    /// innermost context absorbs whatever the event changes. `Agent` must
    /// go through `start_agent`.
    pub fn append(&mut self, spine: &mut Spine, payload: EventPayload) -> io::Result<EventId> {
        assert!(
            !matches!(payload, EventPayload::Agent { .. }),
            "Agent must go through start_agent"
        );
        // Log first: an event's own id is part of what it contributes to
        // a context (an open post is tracked *by id*).
        let id = self.log_event(Some(spine.leaf_id), payload)?;
        Self::replay_event(&mut spine.contexts, &self.events, &self.events[&id]);
        spine.leaf_id = id;
        Ok(id)
    }

    fn log_event(
        &mut self,
        parent_id: Option<EventId>,
        payload: EventPayload,
    ) -> io::Result<EventId> {
        self.id_counter += 1;
        let id = EventId::new(self.id_counter);

        let event = Event {
            id,
            parent_id,
            timestamp: Timestamp::now(),
            payload,
        };

        if let Some(file) = &mut self.file {
            let json = serde_json::to_string(&event)?;
            writeln!(file, "{json}")?;
        }

        self.events.insert(id, event);
        Ok(id)
    }

    /// Write the format-version header — the log's first line, and the
    /// only line that is not an event.
    fn write_header(&mut self) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            writeln!(file, "{}", serde_json::json!({ "version": LOG_VERSION }))?;
            file.flush()?;
            file.sync_all()?;
        }
        Ok(())
    }

    /// Flush and fsync the log. Called **once per loop step**, not once
    /// per event: a 50-call fan-out was 100 fsyncs on the loop thread —
    /// the same thread that owes millisecond post delivery.
    ///
    /// The guarantee this weakens is precise: a crash could only fall
    /// *between* events and can now fall inside a step, losing that
    /// step's tail. That is exactly what reconciliation already
    /// repairs — every unmatched half of an exchange is fixed on open —
    /// so nothing downstream changes.
    pub fn sync(&mut self) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            file.flush()?;
            file.sync_all()?;
        }
        Ok(())
    }

    /// Reconstruct a spine handle for a leaf: trace to the root via
    /// `parent_id`, then replay forward. The agent chain is the
    /// `Agent` ancestors of the leaf, innermost last.
    pub fn spine_at(&self, leaf_id: EventId) -> Spine {
        let mut path: Vec<EventId> = Vec::new();
        let mut current = leaf_id;
        let mut visited: HashSet<EventId> = HashSet::new();
        loop {
            if !visited.insert(current) {
                break; // cycle detected
            }
            let Some(event) = self.events.get(&current) else {
                break;
            };
            path.push(current);
            match event.parent_id {
                Some(parent) => current = parent,
                None => break,
            };
        }
        path.reverse();

        let mut contexts: Vec<Context> = Vec::new();
        for id in &path {
            Self::replay_event(&mut contexts, &self.events, &self.events[id]);
        }
        Spine { leaf_id, contexts }
    }

    /// Fold one event into the reconstructed context chain. Takes the
    /// event map beside the chain because a `Post` **names** its body
    /// rather than copying it — resolving `Origin::Sent` is a map lookup,
    /// and the two borrows are disjoint (`spine_at` already holds
    /// `&self`).
    fn replay_event(contexts: &mut Vec<Context>, events: &HashMap<EventId, Event>, event: &Event) {
        match &event.payload {
            // `context()` **resets** at an `Agent` — clean-room isolation
            // (decision 3) in the type rather than in an `is_some()`.
            EventPayload::Agent {
                charter,
                system,
                exemplars,
                ..
            } => {
                contexts.push(Context {
                    charter: charter.clone(),
                    system: system.clone(),
                    exemplars: exemplars.clone(),
                    messages: Vec::new(),
                    open: Vec::new(),
                });
            }
            // A `Fork` carries history through — it is the same context,
            // diverged — but **obligations do not cross it**. Pre-fork
            // posts stay the original branch's to answer, so there is
            // exactly one owner for every open post and "which branch
            // delivers?" is never a race. This one line is the whole
            // enforcement of that rule at replay time.
            EventPayload::Fork { .. } => {
                if let Some(ctx) = contexts.last_mut() {
                    ctx.open.clear();
                }
            }
            EventPayload::Post { from, origin } => {
                let ctx = contexts
                    .last_mut()
                    .expect("Post event with no enclosing agent");
                let resolved = resolve_origin(events, origin);
                // Only a post that expects a reply is *open* — a `tell`,
                // a harness notice, or the user's FYI lands, wakes the
                // branch, and owes nothing.
                if matches!(resolved.direct(), Some((_, _, true))) {
                    ctx.open.push(event.id);
                }
                ctx.messages.push(crate::types::Post {
                    from: *from,
                    origin: resolved,
                });
            }
            EventPayload::Answer { question, .. } => {
                if let Some(ctx) = contexts.last_mut() {
                    ctx.open.retain(|id| id != question);
                }
            }
            // Execution/record events carry no context-visible state; they
            // are queried from `events` by id (artifacts, replay, UI).
            // A `Rename` is here on purpose: renaming never wakes a branch.
            // `Note` is here for the same reason — `document.rs` derives
            // its rendered line straight from the log at render time
            // (`EventPayload::Note`'s own doc: "a marker in the branch's
            // own history, nothing more"), not from `Context.messages`,
            // which holds only `Message`s. `Compacted` never touches
            // context either: it shadows another row's *rendering*, not
            // this branch's obligations — the row it targets already ran
            // through this fold when it was first replayed.
            EventPayload::Call(_)
            | EventPayload::Result { .. }
            | EventPayload::Handback { .. }
            | EventPayload::Console { .. }
            // Accounting: it changes nothing a later turn can see.
            | EventPayload::ReplyEnd { .. }
            | EventPayload::Rename { .. }
            | EventPayload::Note { .. }
            | EventPayload::Compacted { .. }
            // A reply, its parts, its end, a restart and a compaction
            // request carry no context-visible state either: no
            // obligation opens or closes, and nothing lands in
            // `messages`.
            | EventPayload::Reply
            | EventPayload::Restart
            | EventPayload::Part { .. }
            | EventPayload::Compaction { .. } => {}
        }
    }

    /// A memoised report, if this outcome has been rendered before.
    pub fn memoised_report(&self, outcome: EventId) -> Option<String> {
        self.reports.borrow().entries.get(&outcome).cloned()
    }

    /// Remember a rendered report. Never logged — reports are derived, so
    /// this is a cache, not a record. The memo is dropped wholesale if the
    /// renderer version has moved, because a memo is a cache of *one*
    /// renderer's output.
    pub fn memoise_report(&self, outcome: EventId, text: String) {
        let mut memo = self.reports.borrow_mut();
        if memo.version != crate::report::REPORT_FORMAT_VERSION {
            memo.entries.clear();
            memo.version = crate::report::REPORT_FORMAT_VERSION;
        }
        memo.derivations += 1;
        memo.entries.insert(outcome, text);
    }

    /// How many reports have actually been rendered (memo misses) — the
    /// observable that proves history is not re-derived per request.
    pub fn report_derivations(&self) -> u64 {
        self.reports.borrow().derivations
    }

    /// Drop every memoised report. A memo is a cache of **one renderer's**
    /// output, so editing `report.rs` invalidates all of it at once. The
    /// derivation counter is a lifetime statistic and survives.
    pub fn clear_report_memo(&self) {
        self.reports.borrow_mut().entries.clear();
    }

    /// Resolve a logged `Message` into its context form, materialising a
    /// `Post` whose body lives in a `Send`. Renderers go through this so
    /// the log can stay copy-free.
    pub fn resolve(&self, origin: &Origin) -> Origin {
        resolve_origin(&self.events, origin)
    }

    /// The branch name in force at `leaf`: the last `Rename` at or after
    /// the branch's root, else the root's own `name`.
    ///
    /// The scoping is per-*path*, which is what makes renaming an original
    /// leave its forks alone — they never walk through that `Rename`.
    pub fn branch_name(&self, leaf: EventId) -> Option<String> {
        let mut name = None;
        for event in self.path_events(leaf) {
            match &event.payload {
                // A branch root resets the name: a rename before it named
                // the branch this one came from, not this one.
                EventPayload::Agent { name: n, .. } | EventPayload::Fork { name: n } => {
                    name.clone_from(n)
                }
                EventPayload::Rename { name: n } => name = Some(n.clone()),
                _ => {}
            }
        }
        name
    }

    /// Events on `leaf`'s path, root-first (the ordered spine the UI
    /// projections fold over). Mirrors `spine_at`'s walk but keeps every
    /// event, including the execution events `spine_at` discards.
    pub fn path_events(&self, leaf: EventId) -> Vec<&Event> {
        let mut path = Vec::new();
        let mut current = Some(leaf);
        let mut visited: HashSet<EventId> = HashSet::new();
        while let Some(cur) = current {
            if !visited.insert(cur) {
                break; // cycle guard
            }
            let Some(event) = self.events.get(&cur) else {
                break;
            };
            path.push(event);
            current = event.parent_id;
        }
        path.reverse();
        path
    }

    /// The branch `leaf` is on: the nearest **branch root** at or above
    /// it — an `Agent` for an agent's first branch, a `Fork` for a
    /// divergent one. A branch is identified by its root event, so this
    /// is the addressing function for everything keyed by branch.
    pub fn branch_of(&self, leaf: EventId) -> Option<EventId> {
        let mut current = Some(leaf);
        while let Some(cur) = current {
            let ev = self.events.get(&cur)?;
            if is_branch_root(&ev.payload) {
                return Some(cur);
            }
            current = ev.parent_id;
        }
        None
    }

    /// The branches of one agent: its `Agent` root, plus every `Fork`
    /// under it that has not crossed into another agent. An agent has one
    /// branch until someone forks it; then it has two, both live, both
    /// its own — which is why an agent id is only an address while this
    /// returns exactly one.
    pub fn branches_of_agent(&self, agent: EventId) -> Vec<EventId> {
        let mut out = vec![agent];
        for event in self.events.values() {
            if matches!(event.payload, EventPayload::Fork { .. })
                && self.enclosing_agent(event.id) == Some(agent)
            {
                out.push(event.id);
            }
        }
        out.sort_by_key(|id| id.as_u64());
        out
    }

    /// Every branch in the log, as `(root, leaf)` pairs sorted by root:
    /// the projection `agents()` and the navigator are rows of. The leaf
    /// is found by descending spine children that are not themselves
    /// branch roots — a `Fork` or a spawned `Agent` starts its own row
    /// rather than continuing this one.
    pub fn branches(&self) -> Vec<(EventId, EventId)> {
        let mut children: HashMap<EventId, Vec<EventId>> = HashMap::new();
        for event in self.events.values() {
            if is_branch_root(&event.payload) {
                continue;
            }
            if let Some(parent) = event.parent_id {
                children.entry(parent).or_default().push(event.id);
            }
        }
        let mut roots: Vec<EventId> = self
            .events
            .values()
            .filter(|e| is_branch_root(&e.payload))
            .map(|e| e.id)
            .collect();
        roots.sort_by_key(|id| id.as_u64());
        roots
            .into_iter()
            .map(|root| {
                let mut leaf = root;
                let mut seen: HashSet<EventId> = HashSet::new();
                // Lowest id wins if a branch ever grew two non-root
                // children: the earlier continuation is the branch's own.
                while seen.insert(leaf) {
                    match children
                        .get(&leaf)
                        .and_then(|c| c.iter().min_by_key(|id| id.as_u64()))
                    {
                        Some(next) => leaf = *next,
                        None => break,
                    }
                }
                (root, leaf)
            })
            .collect()
    }

    /// The innermost `Agent` at or above `event` — which agent an
    /// event belongs to.
    pub fn enclosing_agent(&self, event: EventId) -> Option<EventId> {
        let mut current = Some(event);
        while let Some(cur) = current {
            let ev = self.events.get(&cur)?;
            if matches!(ev.payload, EventPayload::Agent { .. }) {
                return Some(cur);
            }
            current = ev.parent_id;
        }
        None
    }

    /// `agent`'s programs along `leaf`'s path, in order — the program-list
    /// projection (decision 8), now over code mode's flat vocabulary: a
    /// `Turn` **is** a program, full stop, whether it is the branch's own
    /// root program or a handler deliberating a `raise()` at some nested
    /// depth. `Invoke`/`Send`/`Spawn`/`Fork` calls, their `Result`s,
    /// `Console`, and the eventual `Return`/`Condition` all attach to
    /// whichever program is currently open — tracked with a small stack
    /// of open-program indices that mirrors [`depth_after`]'s fold
    /// exactly, so a program's `depth` field and "which program a given
    /// event attaches to" can never disagree.
    ///
    /// **Handler programs get their own entry, not a nested field.** A
    /// `Condition{disposition: Pushed}` leaves the raising program open
    /// (suspended, on the stack) and the next `Turn` — the handler —
    /// opens a *new* `ProgramView` one level deeper; its own `Return`
    /// pops it back off, and the raising program (still open beneath it)
    /// resumes absorbing events with no new `Turn` of its own, exactly
    /// as `stack.rs`'s `ProgramStack` ran it. A `Handover` disposition
    /// closes the raising program outright — its VM was popped before
    /// the handler was built — so the handler that follows opens at the
    /// *same* depth, a flat sibling rather than a nested child. This is
    /// the deliberate choice for what handler programs look like here:
    /// every one is independently addressable by its own id (an
    /// `fetch_history(id)` target, a debugger-pane row of its own), and the
    /// `depth` field is what lets a caller draw the nesting doc 22's
    /// render-axes table calls a "nested handler block" — a single flat
    /// `Vec` was rejected because collapsing a deliberation into its
    /// raising program's own entry would hide that the handler is a
    /// separate completion with a separate id, and a caller (`report.rs`,
    /// a debug pane) that wants to fetch or display it on its own would
    /// have nothing to key on.
    pub fn programs_for(&self, agent: EventId, leaf: EventId) -> Vec<ProgramView> {
        let mut cur_agent: Option<EventId> = None;
        let mut programs: Vec<ProgramView> = Vec::new();
        // Indices into `programs`, innermost (currently attaching) last.
        // Its length is always the depth `depth_after` would report —
        // pushed on every `Turn`, popped on every `Return` and every
        // `Handover` `Condition`, left alone on a `Pushed` one.
        let mut stack: Vec<usize> = Vec::new();
        let mut depth: usize = 0;
        // The program a `Console` event should attach to. `machine.rs`
        // logs `Console` immediately *after* the `Return`/`Condition`
        // that ends a run (`suspend`'s own doc: "the console is a
        // diagnostic stream... the report carries only a bounded tail of
        // it"), by which point `Return`/a `Handover` `Condition` has
        // already popped that program off `stack` — so `stack.last()`
        // alone would attach the console to the *next* frame out (or
        // nowhere, at depth 0) instead of the program that actually
        // produced it. Set whenever a program gets its outcome, consumed
        // by the very next `Console`.
        let mut last_outcome_idx: Option<usize> = None;

        for ev in self.path_events(leaf) {
            if let EventPayload::Agent { .. } = ev.payload {
                cur_agent = Some(ev.id);
                continue;
            }
            if cur_agent != Some(agent) {
                continue;
            }
            match &ev.payload {
                EventPayload::Reply | EventPayload::Restart => {
                    // A new program starting invalidates any outcome
                    // still waiting for its `Console` — that pairing is
                    // adjacent-only in the log, never carried across a
                    // later program.
                    last_outcome_idx = None;
                    programs.push(ProgramView {
                        id: ev.id,
                        // Grows as the reply's parts arrive (28).
                        source: String::new(),
                        invokes: Vec::new(),
                        result: None,
                        outcome: None,
                        condition: None,
                        console: Vec::new(),
                        depth,
                    });
                    stack.push(programs.len() - 1);
                }
                EventPayload::Part { part, .. } => {
                    if let Some(&idx) = stack.last() {
                        match part {
                            crate::types::Part::Prose(t) | crate::types::Part::Cell(t) => {
                                programs[idx].source.push_str(t)
                            }
                            crate::types::Part::Thinking(_) => {}
                        }
                    }
                }
                EventPayload::Call(call) => {
                    if let Some(&idx) = stack.last() {
                        let (name, args) = match call {
                            Call::Invoke { name, args, .. } => (name.clone(), args.clone()),
                            Call::Send { to, text, .. } => (
                                "send".to_owned(),
                                serde_json::json!({ "to": to, "text": text }),
                            ),
                            Call::Spawn { name, charter, .. } => (
                                "spawn".to_owned(),
                                serde_json::json!({ "name": name, "charter": charter }),
                            ),
                            Call::Fork { name, .. } => {
                                ("fork".to_owned(), serde_json::json!({ "name": name }))
                            }
                        };
                        programs[idx].invokes.push(InvokeView {
                            id: ev.id,
                            name,
                            args,
                            outcome: None,
                            site: call.site(),
                            is_send: matches!(call, Call::Send { .. }),
                        });
                    }
                }
                EventPayload::Result { call, outcome } => {
                    if let Some(&idx) = stack.last()
                        && let Some(iv) = programs[idx].invokes.iter_mut().find(|iv| iv.id == *call)
                    {
                        iv.outcome = Some(outcome.clone());
                    }
                }
                EventPayload::Handback { how, .. } => {
                    if let Some(&idx) = stack.last() {
                        let p = &mut programs[idx];
                        p.outcome = Some(ev.id);
                        p.condition = Some(how.clone());
                        if matches!(how, crate::types::Handback::Completed) {
                            p.result = Some(serde_json::Value::Null);
                        }
                        last_outcome_idx = Some(idx);
                    }
                    // A terminal handback closes the reply's scope; a
                    // pause leaves it open for whatever decides it.
                    if how.is_terminal() {
                        stack.pop();
                    }
                }
                EventPayload::Console { lines } => {
                    if let Some(idx) = last_outcome_idx.take().or_else(|| stack.last().copied()) {
                        programs[idx].console = lines.clone();
                    }
                }
                _ => {}
            }
            depth = depth_after(depth, &ev.payload);
        }
        programs
    }

    /// The compaction lookup for `leaf`'s path (see
    /// `EventPayload::Compacted`'s own doc comment): target event id →
    /// the op that shadows it. A renderer consults this **instead of**
    /// re-deriving a row from `of` directly — the log never actually
    /// removes the target, so a branch forked before the compaction (its
    /// path holds no `Compacted` event past its own root) resolves this
    /// lookup empty on that id and renders the original event exactly as
    /// logged, with no special case for "was this ever compacted".
    ///
    /// Last-write-wins if a row is compacted more than once on the same
    /// path — Part E's re-fire case, where a batch that didn't free
    /// enough is asked again: `path_events` is root-first, so a plain
    /// insert naturally keeps the latest attempt.
    pub fn compacted_lookup(&self, leaf: EventId) -> HashMap<EventId, CompactedView> {
        let mut lookup = HashMap::new();
        for event in self.path_events(leaf) {
            if let EventPayload::Compacted { of, text } = &event.payload {
                lookup.insert(*of, CompactedView { text: text.clone() });
            }
        }
        lookup
    }

    /// **The reconciliation table, as a scan.** Every unmatched half of
    /// an exchange in the log, with the branch it belongs to.
    ///
    /// No in-memory table is consulted because none is needed: the four
    /// events of an exchange form a closed loop of ids — `Post.origin →
    /// Send`, `Result.call → Send`, `Answer.question → Post` — so the
    /// wait table *is* this walk. That is why `parents` could be deleted
    /// and why nothing session-local has to survive a crash.
    ///
    /// Rows that need no repair are included: the caller needs to know a
    /// branch owes a reply, or that a call may have happened, even though
    /// there is nothing to append for it.
    pub fn unmatched(&self) -> Vec<Unmatched> {
        // The three indexes the closed loop is walked through.
        let mut settled: HashMap<EventId, &Event> = HashMap::new();
        let mut post_of_send: HashMap<EventId, EventId> = HashMap::new();
        let mut answer_of_post: HashMap<EventId, &Event> = HashMap::new();
        let mut agent_of_spawn: HashMap<EventId, EventId> = HashMap::new();
        for event in self.events.values() {
            match &event.payload {
                EventPayload::Result { call, .. } => {
                    settled.insert(*call, event);
                }
                EventPayload::Post {
                    origin: Origin::Sent(send),
                    ..
                } => {
                    post_of_send.insert(*send, event.id);
                }
                EventPayload::Answer { question, .. } => {
                    answer_of_post.insert(*question, event);
                }
                EventPayload::Agent { .. } => {
                    if let Some(parent) = event.parent_id {
                        agent_of_spawn.insert(parent, event.id);
                    }
                }
                _ => {}
            }
        }

        let mut rows = Vec::new();
        let mut calls: Vec<&Event> = self
            .events
            .values()
            .filter(|e| matches!(e.payload, EventPayload::Call(_)))
            .collect();
        calls.sort_by_key(|e| e.id.as_u64());
        for event in calls {
            let EventPayload::Call(call) = &event.payload else {
                unreachable!()
            };
            let (id, Some(branch)) = (event.id, self.branch_of(event.id)) else {
                continue;
            };
            let done = settled.contains_key(&id);
            match call {
                // A `Spawn` whose `Agent` exists but whose caller never
                // got the handle: append the `Result`, or re-execution
                // spawns a second agent and orphans the first. A `Spawn`
                // with no `Agent` needs nothing — nothing was created.
                Call::Spawn { .. } if !done => {
                    if let Some(agent) = agent_of_spawn.get(&id) {
                        rows.push(Unmatched::UndeliveredHandle {
                            branch,
                            spawn: id,
                            agent: *agent,
                        });
                    }
                }
                // Lost in flight. Not repairable and not re-runnable: an
                // effectful tool may have happened, which is exactly what
                // logging at dispatch exists to record.
                Call::Invoke { .. } if !done => {
                    rows.push(Unmatched::LostInvoke { branch, call: id })
                }
                Call::Send {
                    to, expects_reply, ..
                } if !done => {
                    let post = post_of_send.get(&id).copied();
                    match (to, expects_reply, post) {
                        // The human owes a reply: the branch reopens live
                        // and highlighted, the question inline.
                        (Address::User, true, _) => {
                            rows.push(Unmatched::OwedByUser { branch, send: id })
                        }
                        // A tell to the human has no post to name.
                        (Address::User, false, _) => rows.push(Unmatched::MissingReceipt {
                            branch,
                            send: id,
                            post: None,
                        }),
                        // Crashed between the halves of the message. The
                        // `Send` **is** the message, so appending the
                        // `Post` is idempotent.
                        (Address::Branch(to), _, None) => {
                            rows.push(Unmatched::UndeliveredSend { send: id, to: *to })
                        }
                        // Landed, but the receipt never got back.
                        (Address::Branch(_), false, Some(post)) => {
                            rows.push(Unmatched::MissingReceipt {
                                branch,
                                send: id,
                                post: Some(post),
                            })
                        }
                        (Address::Branch(_), true, Some(post)) => {
                            match answer_of_post.get(&post).map(|e| &e.payload) {
                                // Answered, but the delivery was lost.
                                Some(EventPayload::Answer { value, .. }) => {
                                    rows.push(Unmatched::LostDelivery {
                                        branch,
                                        send: id,
                                        value: value.clone(),
                                    })
                                }
                                // The callee still owes it: it is live by
                                // the open-post row, and the sender's menu
                                // lists this one *pending — re-awaitable*.
                                _ => rows.push(Unmatched::PendingAsk { branch, send: id }),
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        for (branch, leaf) in self.branches() {
            // A post open on this branch: it owes a reply, and becomes
            // live. `Context.open` is the same rule `replay_event`
            // maintains, so a fork's inherited posts are already excluded.
            for post in self.spine_at(leaf).context().open.iter().copied() {
                rows.push(Unmatched::OwedAnswer { branch, post });
            }
            // A `Turn` with no outcome: interrupted mid-program, and the
            // VM went with the process. Under code mode a `Turn` is
            // exactly one program, so it owes exactly **one** outcome (a
            // `Return` or a `Condition`) — never the N-tool-calls count
            // the old tool-call protocol compared against. Zero outcomes
            // means the VM never handed back at all.
            //
            // Scoped to this branch's **own agent segment**, not its
            // whole path: a turn above an `Agent` root belongs to the
            // caller, and reading it here would give every spawned
            // worker its parent's interrupted run as an outcome to
            // repair.
            let path = self.path_events(leaf);
            let start = path
                .iter()
                .rposition(|e| matches!(e.payload, EventPayload::Agent { .. }))
                .unwrap_or(0);
            let Some(turn) = path[start..]
                .iter()
                .rev()
                .find(|e| matches!(e.payload, EventPayload::Reply | EventPayload::Restart))
            else {
                continue;
            };
            if crate::report::outcomes_of_turn(self, leaf, turn.id).is_empty() {
                rows.push(Unmatched::InterruptedRun {
                    branch,
                    leaf,
                    turn: turn.id,
                });
            }
        }
        rows
    }

    /// The set of spine leaves. A leaf is an event no *spine* event
    /// follows: `Agent` children don't count — they root child
    /// branches, so a call-site event stays its caller's leaf while a
    /// subagent is in flight.
    pub fn list_leaves(&self) -> Vec<(EventId, Option<String>)> {
        let mut spine_child_counts: HashMap<EventId, usize> = HashMap::new();
        for event in self.events.values() {
            if matches!(event.payload, EventPayload::Agent { .. }) {
                continue;
            }
            if let Some(parent) = event.parent_id {
                *spine_child_counts.entry(parent).or_default() += 1;
            }
        }

        self.events
            .keys()
            .filter(|id| spine_child_counts.get(id).copied().unwrap_or(0) == 0)
            .copied()
            .map(|id| {
                let name = self.branch_name(id);
                (id, name)
            })
            .collect()
    }
}

/// One unmatched half of an exchange, found by [`Tree::unmatched`] — a
/// row of the reconciliation table, with the branch it sits on.
///
/// The guarantee these serve is precise, and it is not determinism —
/// nothing here is ever re-executed (`DESIGN.md`, the dependency
/// spine): **after a resume, no completed work is invisible.**
#[derive(Debug, Clone, PartialEq)]
pub enum Unmatched {
    /// An open `Post` with no `Answer`: this branch owes a reply, so it
    /// becomes live.
    OwedAnswer { branch: EventId, post: EventId },
    /// A `Send { to: user }` with no `Result`: the **human** owes a
    /// reply. Nothing to repair — the branch reopens live and
    /// highlighted, its question inline.
    OwedByUser { branch: EventId, send: EventId },
    /// A `Send` with no `Post`: crashed between the halves of the
    /// message. Repair by appending the `Post` on `to`.
    UndeliveredSend { send: EventId, to: EventId },
    /// A tell whose post landed but whose receipt did not. Repair by
    /// appending the receipt on `branch`.
    MissingReceipt {
        branch: EventId,
        send: EventId,
        /// `None` for a tell to the human, which has no post to name.
        post: Option<EventId>,
    },
    /// An ask whose `Post` was answered but whose `Result` never landed.
    /// Repair by appending it, carrying the answer's value.
    LostDelivery {
        branch: EventId,
        send: EventId,
        value: serde_json::Value,
    },
    /// An ask the callee still owes. Nothing to repair: the callee is
    /// live by [`Unmatched::OwedAnswer`], and the sender's menu lists
    /// this one *pending — re-awaitable*.
    PendingAsk { branch: EventId, send: EventId },
    /// A `Spawn` whose `Agent` exists but whose handle never reached the
    /// caller. Repair by appending `Result { value: { agent } }` —
    /// otherwise re-execution spawns a second agent and orphans this one.
    UndeliveredHandle {
        branch: EventId,
        spawn: EventId,
        agent: EventId,
    },
    /// An `Invoke` with no `Result`: **issued; may have happened.** Not
    /// repairable, and that is the point — it is why calls are logged at
    /// dispatch rather than at resolution.
    LostInvoke { branch: EventId, call: EventId },
    /// A `Turn` whose calls never all produced an outcome: interrupted
    /// mid-program, and the VM went with the process. Repair by appending
    /// `Condition{Interrupted}` so the run has an outcome like any other.
    InterruptedRun {
        branch: EventId,
        leaf: EventId,
        turn: EventId,
    },
}

/// Whether a payload **roots a branch**: an `Agent` (a clean-room
/// context) or a `Fork` (the same context diverged). The two are the
/// structural roots, and a branch is named by whichever one it starts at.
fn is_branch_root(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::Agent { .. } | EventPayload::Fork { .. }
    )
}

/// Resolve a logged `Origin` into its context form: one that references
/// a `Send` gets that body inline. The **log** stays copy-free;
/// the reconstructed `Context` is where bodies are materialised, because
/// that is what a request renders from.
fn resolve_origin(events: &HashMap<EventId, Event>, origin: &Origin) -> Origin {
    let Origin::Sent(send) = origin else {
        return origin.clone();
    };
    match events.get(send).map(|e| &e.payload) {
        Some(EventPayload::Call(Call::Send {
            text,
            input,
            expects_reply,
            options,
            ..
        })) => Origin::Direct {
            text: text.clone(),
            input: input.clone(),
            options: options.clone(),
            expects_reply: *expects_reply,
        },
        // The `Send` is not in this tree (or is not a `Send`): keep the
        // reference rather than inventing a body.
        _ => Origin::Sent(*send),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs::OpenOptions;
    use std::io;
    use tempfile::NamedTempFile;

    fn user_msg(text: &str) -> EventPayload {
        EventPayload::Post {
            from: Author::User,
            origin: Origin::Direct {
                text: text.into(),
                input: json!(null),
                options: Vec::new(),
                expects_reply: true,
            },
        }
    }

    /// An assistant turn: under code mode the whole turn **is** a
    /// program, so this builds the one shape `Message::Turn` has —
    /// `source` is the bare JavaScript text, no separate prose channel
    /// and no tool-call wrapper (23_ONE_AGENT.md's substitution table).
    /// Replaces the POC-era `run_program_call`/`resume_call` pair, which
    /// built a `Turn` around a `run_program`/`resume` `ToolCall` that no
    /// longer exists — a resume is just another program whose source
    /// happens to be `return resume(value);`.
    /// A reply and its one cell: two events now, where a `Turn` was one.
    fn assistant_msg(source: &str) -> [EventPayload; 2] {
        [
            EventPayload::Reply,
            EventPayload::Part {
                reply: EventId::new(1),
                part: crate::types::Part::Cell(source.to_owned()),
            },
        ]
    }

    fn returned(_value: serde_json::Value) -> EventPayload {
        EventPayload::Handback {
            reply: EventId::new(1),
            how: crate::types::Handback::Completed,
            site: 0,
            stack: Vec::new(),
        }
    }

    fn raised(name: &str) -> EventPayload {
        EventPayload::Handback {
                            reply: EventId::new(1),
                            how: crate::types::Handback::Raised {
                name: name.into(),
                payload: None,
            },
                            site: 0,
                            stack: Vec::new(),
                        }
    }

    // --- Log projections (decision 8: reconstructible from the log) ---

    /// A full program — source, inner tool calls, result, console — round
    /// trips through a saved-and-reloaded log with no live VM.
    #[test]
    fn programs_reconstruct_from_reloaded_log() -> io::Result<()> {
        let file = NamedTempFile::new()?;
        let open = || -> io::Result<Tree> {
            Tree::open(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(file.path())?,
            )
        };
        let (agent, leaf);
        {
            let mut tree = open()?;
            let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
            agent = spine.leaf_id; // the Agent id is the agent id
            let [reply, cell] = assistant_msg("console.log('hi'); history.append(42);");
            tree.append(&mut spine, reply)?;
            tree.append(&mut spine, cell)?;
            let bash = tree.append(
                &mut spine,
                EventPayload::Call(Call::Invoke {
                    name: "bash".into(),
                    args: json!(["ls"]),
                    site: 0,
                }),
            )?;
            tree.append(
                &mut spine,
                EventPayload::Result {
                    call: bash,
                    outcome: Outcome::Delivered(json!("file.txt")),
                },
            )?;
            tree.append(&mut spine, returned(json!(42)))?;
            tree.append(
                &mut spine,
                EventPayload::Console {
                    lines: vec!["hi".into()],
                },
            )?;
            leaf = spine.leaf_id;
        }

        // Reload from disk — nothing live survives, only the log.
        let tree = open()?;
        let branches = tree.branches();
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].0, agent);
        match &tree.events[&agent].payload {
            EventPayload::Agent { charter, .. } => assert_eq!(charter, "root"),
            _ => panic!("expected an Agent root"),
        }

        let progs = tree.programs_for(agent, leaf);
        assert_eq!(progs.len(), 1);
        let p = &progs[0];
        assert_eq!(p.id, EventId::new(agent.as_u64() + 1)); // the run_program assistant event
        assert!(p.source.contains("history.append(42)"), "{}", p.source);
        assert_eq!(p.invokes.len(), 1);
        assert_eq!(p.invokes[0].name, "bash");
        // A reply has no `return` (D5): what it produced is the row it
        // appended, and the terminal simply says it completed.
        assert_eq!(p.result, Some(serde_json::Value::Null));
        assert_eq!(
            p.console,
            vec!["hi".to_string()],
            "full console, not the clipped report"
        );
        assert_eq!(p.status(), crate::host::ProgramStatus::Completed);
        Ok(())
    }

    /// A raise that is later resumed to completion is one program, but
    /// **two handbacks**: each logs its own outcome, and the program's
    /// status walks Suspended → Completed as they land. That the split is
    /// readable from a reopened log at all is what one-outcome-per-handback
    /// buys (`protocol.rs` used to say it was not inferable).
    #[test]
    fn program_status_survives_reopen() -> io::Result<()> {
        use crate::host::ProgramStatus;
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
        let agent = spine.leaf_id;
        let [reply, cell] = assistant_msg("raise('x');");
        tree.append(&mut spine, reply)?;
        tree.append(&mut spine, cell)?;
        tree.append(&mut spine, raised("x"))?; // first handback: suspended
        let suspended_leaf = spine.leaf_id;
        assert_eq!(
            tree.programs_for(agent, suspended_leaf)[0].status(),
            ProgramStatus::Suspended,
            "a reopened log says how the run ended"
        );

        let [reply, cell] = assistant_msg("history.append(resume(null));");

        tree.append(&mut spine, reply)?;

        tree.append(&mut spine, cell)?; // continues the same program
        tree.append(&mut spine, returned(json!("done")))?; // second handback
        tree.append(
            &mut spine,
            EventPayload::Console {
                lines: vec!["before".into(), "after".into()],
            },
        )?;
        let leaf = spine.leaf_id;

        // Handler programs get their own entry, not folded into the
        // raiser's (`programs_for`'s own doc: "every one is independently
        // addressable by its own id"). The raiser (`progs[0]`) stays
        // `Suspended` — the log never says anything more about it once
        // the handler's `resume(...)` takes over — and the handler
        // (`progs[1]`, "history.append(resume(null));") is the one that carries
        // the eventual completion: its `Return`/`Console` are the next
        // events on the spine, with no further `Turn` in between.
        let progs = tree.programs_for(agent, leaf);
        assert_eq!(progs.len(), 2, "the handler is its own program entry");
        assert_eq!(progs[0].status(), ProgramStatus::Suspended);
        assert_eq!(progs[1].result, Some(serde_json::Value::Null));
        assert_eq!(
            progs[1].console,
            vec!["before".to_string(), "after".to_string()]
        );
        assert_eq!(progs[1].status(), ProgramStatus::Completed);

        // A compile failure never ran, so it is Failed, not Suspended.
        let mut other = tree.start_agent(Some(agent), None, "child", None, "", Vec::new())?;
        let [reply, cell] = assistant_msg("let = ;");
        tree.append(&mut other, reply)?;
        tree.append(&mut other, cell)?;
        tree.append(
            &mut other,
            EventPayload::Handback {
                            reply: EventId::new(1),
                            how: crate::types::Handback::CellFailed {
                    message: "compile error".into(),
                },
                            site: 0,
                            stack: Vec::new(),
                        },
        )?;
        let child_agent = tree.enclosing_agent(other.leaf_id).unwrap();
        assert_eq!(
            tree.programs_for(child_agent, other.leaf_id)[0].status(),
            ProgramStatus::Suspended
        );
        Ok(())
    }

    // --- Bootstrap & linear flow ---

    #[test]
    fn test_bootstrap_root_agent() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let spine = tree.start_agent(None, None, "hello", None, "", Vec::new())?;
        assert_eq!(spine.leaf_id.as_u64(), 1);
        assert!(tree.events[&spine.leaf_id].is_root());
        assert_eq!(spine.contexts.len(), 1);
        assert_eq!(spine.context().charter, "hello");
        Ok(())
    }

    #[test]
    #[should_panic(expected = "root Agent on a non-empty tree")]
    fn test_second_root_agent_panics() {
        let mut tree = Tree::new(None);
        tree.start_agent(None, None, "root", None, "", Vec::new())
            .unwrap();
        let _ = tree.start_agent(None, None, "another root", None, "", Vec::new());
    }

    #[test]
    fn test_linear_conversation() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
        tree.append(&mut spine, user_msg("hello"))?;
        let [reply, cell] = assistant_msg("hi there");
        tree.append(&mut spine, reply)?;
        tree.append(&mut spine, cell)?;

        assert_eq!(spine.contexts.len(), 1);
        // **Only what was said *to* the branch.** A reply is the
        // branch's own, and was never a message to it — `Context`
        // carried both while `Message` was one type (28).
        assert_eq!(spine.context().messages.len(), 1);
        assert_eq!(spine.context().messages[0].text(), "hello");
        Ok(())
    }

    #[test]
    #[should_panic(expected = "Agent must go through start_agent")]
    fn test_append_agent_root_panics() {
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "", Vec::new())
            .unwrap();
        let _ = tree.append(
            &mut spine,
            EventPayload::Agent {
                name: None,
                charter: "child".into(),
                tools: None,
                system: String::new(),
                exemplars: Vec::new(),
            },
        );
    }

    // --- Open posts, and the fact that nothing closes ---

    /// A post that expects a reply is **open** until an `Answer` names
    /// it; answering does not seal the branch, it just clears the debt.
    #[test]
    fn an_answer_closes_a_post_not_the_branch() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
        let question = tree.append(&mut spine, user_msg("q"))?;
        assert_eq!(spine.context().open, [question]);

        let [reply, cell] = assistant_msg("done");

        tree.append(&mut spine, reply)?;

        tree.append(&mut spine, cell)?;
        tree.append(
            &mut spine,
            EventPayload::Answer {
                question,
                value: json!({"ok": true}),
            },
        )?;
        assert!(spine.context().open.is_empty());

        // …and the branch keeps taking messages afterwards. Agents never
        // close: a later question is just another post.
        let again = tree.append(&mut spine, user_msg("and another thing"))?;
        assert_eq!(spine.context().open, [again]);
        Ok(())
    }

    /// Only a post that expects a reply is open. A `tell` — from an
    /// agent, the harness, or the user — lands, wakes the branch, and
    /// owes nothing.
    #[test]
    fn a_tell_opens_nothing() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
        tree.append(
            &mut spine,
            EventPayload::Post {
                from: Author::Harness,
                origin: Origin::Direct {
                    text: "fyi".into(),
                    input: json!(null),
                    options: Vec::new(),
                    expects_reply: false,
                },
            },
        )?;
        assert!(spine.context().open.is_empty());
        assert_eq!(spine.context().messages.len(), 1, "it still lands");
        Ok(())
    }

    /// **A fork inherits history, not obligations.** A pre-fork post is
    /// before the fork's root, so it stays the original branch's to
    /// answer — which is what makes "which branch delivers?" never a
    /// race. `replay_event` clearing `open` at the `Fork` is the whole
    /// enforcement.
    #[test]
    fn fork_does_not_owe_prefork_posts() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
        let question = tree.append(&mut spine, user_msg("which file?"))?;
        assert_eq!(spine.context().open, [question]);

        // Fork at the leaf — the "ask without pausing" gesture.
        let mut forked = tree.fork(spine.leaf_id)?;
        tree.append(&mut forked, EventPayload::Fork { name: None })?;

        // The fork sees the question as history…
        assert_eq!(
            forked
                .context()
                .messages
                .iter()
                .map(|m| m.text())
                .collect::<Vec<_>>(),
            ["which file?"]
        );
        // …and owes it nothing.
        assert!(forked.context().open.is_empty());
        // The original still owes it.
        assert_eq!(tree.spine_at(spine.leaf_id).context().open, [question]);

        // A post *after* the fork root is the fork's own to answer.
        let mine = tree.append(&mut forked, user_msg("what are you doing?"))?;
        assert_eq!(forked.context().open, [mine]);
        Ok(())
    }

    // --- Execution events ---

    #[test]
    fn test_calls_and_program_result_are_artifacts_not_messages() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
        let call_id = tree.append(
            &mut spine,
            EventPayload::Call(Call::Invoke {
                name: "fetch".into(),
                args: json!({"url": "http://x"}),
                site: 0,
            }),
        )?;
        let settled_id = tree.append(
            &mut spine,
            EventPayload::Result {
                call: call_id,
                outcome: Outcome::Delivered(json!("body")),
            },
        )?;
        let result_id = tree.append(
            &mut spine,
            EventPayload::Handback {
                reply: EventId::new(1),
                how: crate::types::Handback::Completed,
                site: 0,
                stack: Vec::new(),
            },
        )?;

        // Spine leaf advanced past all three, but the agent's chat
        // transcript is untouched — they're id-addressable artifacts.
        assert_eq!(spine.leaf_id, result_id);
        assert!(spine.context().messages.is_empty());
        assert!(matches!(
            tree.events[&call_id].payload,
            EventPayload::Call(Call::Invoke { .. })
        ));
        assert!(matches!(
            tree.events[&settled_id].payload,
            EventPayload::Result { .. }
        ));
        assert!(matches!(
            tree.events[&result_id].payload,
            EventPayload::Handback { .. }
        ));
        Ok(())
    }

    // --- Branching: subagent contexts ---

    /// Caller spine + child agent branched at a call-site event, appends
    /// interleaved between the two spines.
    fn build_branched_tree(tree: &mut Tree) -> io::Result<(Spine, Spine)> {
        let mut caller = tree.start_agent(None, None, "root", None, "", Vec::new())?;
        tree.append(&mut caller, user_msg("m1"))?;
        let [reply, cell] = assistant_msg("spawning");
        let call_site = tree.append(&mut caller, reply)?;
        tree.append(&mut caller, cell)?;

        let mut child =
            tree.start_agent(Some(call_site), None, "child prompt", None, "", Vec::new())?;
        // The first question is a `Post`, and it carries the caller's
        // machine-bound `input`.
        tree.append(
            &mut child,
            EventPayload::Post {
                from: Author::Agent(EventId::new(1)),
                origin: Origin::Direct {
                    text: "child prompt".into(),
                    input: json!({"task": 1}),
                    options: Vec::new(),
                    expects_reply: true,
                },
            },
        )?;
        // Interleave appends across the two spines.
        tree.append(&mut caller, user_msg("caller continues"))?;
        let [reply, cell] = assistant_msg("child working");
        tree.append(&mut child, reply)?;
        tree.append(&mut child, cell)?;
        let [reply, cell] = assistant_msg("caller answer");
        tree.append(&mut caller, reply)?;
        tree.append(&mut caller, cell)?;
        Ok((caller, child))
    }

    #[test]
    fn test_interleaved_spines_reconstruct_independently() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let (caller, child) = build_branched_tree(&mut tree)?;

        // Live handles and from-scratch reconstruction agree.
        for spine in [&caller, &tree.spine_at(caller.leaf_id)] {
            assert_eq!(spine.contexts.len(), 1);
            let msgs: Vec<&str> = spine.context().messages.iter().map(|m| m.text()).collect();
            // Posts only: a reply is the branch's own and never was a
            // message *to* it (28).
            assert_eq!(msgs, ["m1", "caller continues"]);
        }
        for spine in [&child, &tree.spine_at(child.leaf_id)] {
            assert_eq!(spine.contexts.len(), 2, "child sits under the root agent");
            assert_eq!(spine.context().charter, "child prompt");
            assert_eq!(spine.context().input(&tree), json!({"task": 1}));
            let msgs: Vec<&str> = spine.context().messages.iter().map(|m| m.text()).collect();
            assert_eq!(msgs, ["child prompt"]);
        }
        Ok(())
    }

    #[test]
    fn test_event_ids_monotonic_across_spines() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let (caller, child) = build_branched_tree(&mut tree)?;
        // 11 events, globally monotonic ids regardless of spine — a
        // reply is two events now (itself and its cell), where a `Turn`
        // was one.
        assert_eq!(tree.events.len(), 11);
        let mut ids: Vec<u64> = tree.events.keys().map(|id| id.as_u64()).collect();
        ids.sort_unstable();
        assert_eq!(ids, (1..=11).collect::<Vec<_>>());
        assert!(caller.leaf_id != child.leaf_id);
        Ok(())
    }

    #[test]
    fn test_in_flight_branch_keeps_caller_leaf() -> io::Result<()> {
        // A Agent child must not swallow the caller's leaf: with no
        // caller activity after the call site, the call-site event is
        // still the caller's resumable leaf.
        let mut tree = Tree::new(None);
        let mut caller = tree.start_agent(None, None, "root", None, "", Vec::new())?;
        let [reply, cell] = assistant_msg("spawning");
        let call_site = tree.append(&mut caller, reply)?;
        tree.append(&mut caller, cell)?;
        let child = tree.start_agent(Some(call_site), None, "child", None, "", Vec::new())?;

        let mut leaves: Vec<EventId> = tree.list_leaves().into_iter().map(|(id, _)| id).collect();
        leaves.sort_by_key(|id| id.as_u64());
        // The call site is the caller's leaf: the reply's cell is the
        // event after it, so the fork point moved by one.
        assert_eq!(leaves.len(), 2);
        assert!(leaves.contains(&child.leaf_id));
        Ok(())
    }

    // --- Forking ---

    #[test]
    fn test_fork_mid_spine_diverges_leaving_original_intact() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
        tree.append(&mut spine, user_msg("q"))?;
        let [reply, cell] = assistant_msg("first answer");
        let fork_point = tree.append(&mut spine, reply)?;
        tree.append(&mut spine, cell)?;
        let original_leaf = tree.append(&mut spine, user_msg("follow-up A"))?;

        // Fork from the assistant turn and take a different path.
        let mut forked = tree.fork(fork_point)?;
        assert_eq!(forked.leaf_id, fork_point);
        let forked_leaf = tree.append(&mut forked, user_msg("follow-up B"))?;

        // Two leaves now hang off the same fork point; neither path saw
        // the other's append.
        assert_ne!(original_leaf, forked_leaf);
        let texts = |leaf: EventId| -> Vec<String> {
            tree.spine_at(leaf)
                .context()
                .messages
                .iter()
                .map(|m| m.text().to_owned())
                .collect()
        };
        // Posts only (28): the answers were the branch's own replies.
        assert_eq!(texts(original_leaf), ["q", "follow-up A"]);
        assert_eq!(texts(forked_leaf), ["q", "follow-up B"]);
        Ok(())
    }

    #[test]
    fn test_fork_from_an_answered_leaf_is_fine() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
        let question = tree.append(&mut spine, user_msg("q"))?;
        let [reply, cell] = assistant_msg("done");
        tree.append(&mut spine, reply)?;
        tree.append(&mut spine, cell)?;
        let answered = tree.append(
            &mut spine,
            EventPayload::Answer {
                question,
                value: json!(1),
            },
        )?;
        // Nothing seals a branch, so forking past an answer is ordinary.
        let forked = tree.fork(answered)?;
        assert_eq!(forked.leaf_id, answered);
        Ok(())
    }

    #[test]
    fn test_fork_from_unknown_id_errors() {
        let tree = Tree::new(None);
        let err = tree.fork(EventId::new(99)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    // --- Durability granularity ---

    /// The log syncs once per loop **step**, so a crash can fall inside
    /// one and tear the tail. A torn last line was never acknowledged to
    /// anyone: reopening drops it, truncates back to the last clean
    /// record, and keeps appending — which is the state reconciliation
    /// already knows how to repair.
    #[test]
    fn sync_per_step_survives_a_torn_tail() -> io::Result<()> {
        let tmp = NamedTempFile::new()?;
        let path = tmp.path().to_path_buf();
        let open = || -> io::Result<Tree> {
            Tree::open(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&path)?,
            )
        };

        let turn;
        {
            let mut tree = open()?;
            let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
            tree.append(&mut spine, user_msg("go"))?;
            let [reply, cell] = assistant_msg("history.append(1);");
            turn = tree.append(&mut spine, reply)?;
            tree.append(&mut spine, cell)?;
            tree.sync()?;
            // A step in progress: these are written but not yet synced.
            tree.append(&mut spine, returned(json!(1)))?;
        }

        // Cut mid-record, the way a crash inside a step would.
        let full = std::fs::read_to_string(&path)?;
        let last = full[..full.len() - 1].rfind('\n').unwrap() + 1;
        let cut = last + (full.len() - last) / 2;
        std::fs::write(&path, &full[..cut])?;

        // Reopening lands on the step's start — the complete records —
        // and the partial one is gone from the file, not just from memory.
        let mut tree = open()?;
        assert_eq!(tree.events.len(), 4);
        assert_eq!(tree.id_counter, 4);
        assert_eq!(std::fs::read_to_string(&path)?, &full[..last]);

        // The run now has no outcome, which is exactly the reconciliation
        // row for it — and appending continues on a clean boundary.
        let leaf = tree.list_leaves()[0].0;
        let mut spine = tree.spine_at(leaf);
        tree.append(
            &mut spine,
            EventPayload::Handback {
                            reply: EventId::new(1),
                            how: crate::types::Handback::Interrupted,
                            site: 0,
                            stack: Vec::new(),
                        },
        )?;
        tree.sync()?;
        assert_eq!(open()?.events.len(), 5);
        Ok(())
    }

    // --- Bodies are stored once ---

    /// One question with a large `input` fanned to three workers is
    /// stored **once**, in the `Send`; each worker's `Post` names it.
    /// The card's own pattern hands the same plan to every worker, so
    /// copying would write the body once per worker.
    #[test]
    fn fanned_body_is_stored_once() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut caller = tree.start_agent(None, None, "orchestrator", None, "", Vec::new())?;
        let plan = "P".repeat(4096);
        let send = tree.append(
            &mut caller,
            EventPayload::Call(Call::Send {
                prose: false,
                to: Address::Branch(EventId::new(1)),
                text: plan.clone(),
                input: json!({ "big": plan.clone() }),
                options: Vec::new(),
                expects_reply: true,
                site: 0,
                site_end: 0,
            }),
        )?;

        // Three workers, each delivered the *same* body by reference.
        let mut workers = Vec::new();
        for _ in 0..3 {
            let mut w = tree.start_agent(Some(send), None, "worker", None, "", Vec::new())?;
            tree.append(
                &mut w,
                EventPayload::Post {
                    from: Author::Agent(EventId::new(1)),
                    origin: Origin::Sent(send),
                },
            )?;
            workers.push(w.leaf_id);
        }

        // The body appears in exactly one event in the log.
        let holders = tree
            .events
            .values()
            .filter(|e| serde_json::to_string(&e.payload).unwrap().contains(&plan))
            .count();
        assert_eq!(holders, 1, "the body lives only in the Send");

        // …and every worker's *context* still sees it, resolved.
        for leaf in workers {
            let ctx = tree.spine_at(leaf);
            let ctx = ctx.context();
            assert_eq!(ctx.messages.len(), 1);
            assert_eq!(ctx.messages[0].text(), plan);
            assert_eq!(ctx.input(&tree), json!({ "big": plan.clone() }));
        }
        Ok(())
    }

    /// A user post has no send side, so it carries its own body — and
    /// that round-trips through the log unchanged.
    #[test]
    fn direct_post_carries_its_own_body() -> io::Result<()> {
        let tmp = NamedTempFile::new()?;
        let path = tmp.path().to_path_buf();
        let leaf;
        {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            let mut tree = Tree::open(file)?;
            let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
            leaf = tree.append(
                &mut spine,
                EventPayload::Post {
                    from: Author::User,
                    origin: Origin::Direct {
                        text: "read PLAN.md".into(),
                        input: json!({ "n": 7 }),
                        options: Vec::new(),
                        expects_reply: true,
                    },
                },
            )?;
        }
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        let tree = Tree::open(file)?;
        let spine = tree.spine_at(leaf);
        let crate::types::Post { from, origin } = &spine.context().messages[0];
        assert_eq!(*from, Author::User);
        assert_eq!(
            origin.direct(),
            Some(("read PLAN.md", &json!({ "n": 7 }), true))
        );
        Ok(())
    }

    // --- Branch names ---

    #[test]
    fn a_rename_names_the_branch() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
        tree.append(&mut spine, user_msg("hi"))?;
        tree.append(
            &mut spine,
            EventPayload::Rename {
                name: "my branch".into(),
            },
        )?;

        let leaves = tree.list_leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].1, Some("my branch".to_string()));
        Ok(())
    }

    /// A branch's name is the last `Rename` **at or after its root**,
    /// else the root's own `name`. The scoping is per-path, so the later
    /// rename wins on this branch and an earlier one is superseded.
    #[test]
    fn rename_folds_from_the_branch_root() -> io::Result<()> {
        let mut tree = Tree::new(None);
        // A root that was born named.
        let mut spine =
            tree.start_agent(None, Some("at birth".into()), "root", None, "", Vec::new())?;
        assert_eq!(tree.branch_name(spine.leaf_id).as_deref(), Some("at birth"));

        let fork_point = tree.append(&mut spine, user_msg("q"))?;
        tree.append(
            &mut spine,
            EventPayload::Rename {
                name: "the original".into(),
            },
        )?;
        let [reply, cell] = assistant_msg("a");
        let original = tree.append(&mut spine, reply)?;
        tree.append(&mut spine, cell)?;

        // A divergent branch off the shared prefix, named for how it
        // differs.
        let mut forked = tree.fork(fork_point)?;
        tree.append(
            &mut forked,
            EventPayload::Rename {
                name: "the retry".into(),
            },
        )?;
        let [reply, cell] = assistant_msg("b");
        let retry = tree.append(&mut forked, reply)?;
        tree.append(&mut forked, cell)?;

        // Each path carries only the renames on it: renaming one leaves
        // the other alone.
        assert_eq!(tree.branch_name(original).as_deref(), Some("the original"));
        assert_eq!(tree.branch_name(retry).as_deref(), Some("the retry"));
        Ok(())
    }

    /// A nameless branch has no name — the *display* label a UI derives
    /// for it is computed at render time and logged nowhere.
    #[test]
    fn an_unnamed_branch_has_no_name() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
        tree.append(&mut spine, user_msg("hello"))?;

        let leaves = tree.list_leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].1, None);
        Ok(())
    }

    #[test]
    fn test_list_leaves_on_empty_tree() {
        let tree = Tree::new(None);
        assert!(tree.list_leaves().is_empty());
    }

    // --- File round-trip ---

    #[test]
    fn test_file_round_trip_with_in_flight_branch() -> io::Result<()> {
        let tmp = NamedTempFile::new()?;
        let path = tmp.path().to_path_buf();

        let (caller_leaf, child_leaf) = {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            let mut tree = Tree::open(file)?;
            let (caller, child) = build_branched_tree(&mut tree)?;
            // The child is in flight: it was asked and has not answered.
            assert_eq!(child.context().open.len(), 1);
            (caller.leaf_id, child.leaf_id)
        };

        {
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let mut tree = Tree::open(file)?;

            let mut leaves: Vec<EventId> =
                tree.list_leaves().into_iter().map(|(id, _)| id).collect();
            leaves.sort_by_key(|id| id.as_u64());
            let mut expected = vec![caller_leaf, child_leaf];
            expected.sort_by_key(|id| id.as_u64());
            assert_eq!(leaves, expected);

            // Resume the in-flight child: reconstruct and answer it.
            let mut child = tree.spine_at(child_leaf);
            assert_eq!(child.context().charter, "child prompt");
            let question = child.context().open[0];
            tree.append(
                &mut child,
                EventPayload::Answer {
                    question,
                    value: json!("done"),
                },
            )?;
        }

        {
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let tree = Tree::open(file)?;
            let child = tree.spine_at(EventId::new(tree.id_counter));
            // The debt is cleared, and the branch is still appendable.
            assert!(child.context().open.is_empty());
        }
        Ok(())
    }

    #[test]
    fn test_file_append_resumes_from_leaf() -> io::Result<()> {
        let tmp = NamedTempFile::new()?;
        let path = tmp.path().to_path_buf();

        {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            let mut tree = Tree::open(file)?;
            let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
            tree.append(&mut spine, user_msg("first msg"))?;
        }

        {
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let mut tree = Tree::open(file)?;
            let leaves = tree.list_leaves();
            assert_eq!(leaves.len(), 1);
            let mut spine = tree.spine_at(leaves[0].0);
            assert_eq!(spine.context().messages.len(), 1);

            let [reply, cell] = assistant_msg("second msg");

            tree.append(&mut spine, reply)?;

            tree.append(&mut spine, cell)?;
            // One post; the reply is not a message to the branch.
            assert_eq!(spine.context().messages.len(), 1);
        }

        {
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let tree = Tree::open(file)?;
            let leaves = tree.list_leaves();
            assert_eq!(leaves.len(), 1);
            let spine = tree.spine_at(leaves[0].0);
            // Posts only: "second msg" was the branch's own reply.
            assert_eq!(spine.context().messages.len(), 1);
            assert_eq!(spine.context().messages[0].text(), "first msg");
        }
        Ok(())
    }

    #[test]
    fn test_open_empty_file() -> io::Result<()> {
        let tmp = NamedTempFile::new()?;
        let path = tmp.path().to_path_buf();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let mut tree = Tree::open(file)?;
        assert!(tree.list_leaves().is_empty());

        let spine = tree.start_agent(None, None, "first", None, "", Vec::new())?;
        assert_eq!(spine.contexts.len(), 1);
        assert_eq!(spine.context().charter, "first");
        Ok(())
    }

    // --- spine_at edges ---

    #[test]
    fn test_spine_at_unknown_id_is_empty() {
        let tree = Tree::new(None);
        let spine = tree.spine_at(EventId::new(42));
        assert!(spine.contexts.is_empty());
    }
    /// **Log versioning.** A new log gets a `{"version": N}` header; an
    /// older or headerless one is refused by name rather than read as
    /// something it is not. Nothing migrates a pre-17 log: the event
    /// vocabulary changed under it, so a misread is worse than a refusal.
    #[test]
    fn log_version_header_is_written_and_enforced() -> io::Result<()> {
        let file = NamedTempFile::new()?;
        {
            let mut tree = Tree::open(file.reopen()?)?;
            let mut spine = tree.start_agent(None, None, "root", None, "", Vec::new())?;
            tree.append(&mut spine, user_msg("hi"))?;
            tree.sync()?;
        }
        // The header is the first line, and only the first line.
        let content = std::fs::read_to_string(file.path())?;
        let mut lines = content.lines();
        assert_eq!(
            lines.next(),
            Some(format!("{{\"version\":{LOG_VERSION}}}").as_str())
        );
        assert_eq!(lines.count(), 2, "one header, two events");
        // …and a log carrying it reopens with every event intact.
        let tree = Tree::open(file.reopen()?)?;
        assert_eq!(tree.events.len(), 2);
        assert_eq!(tree.id_counter, 2);

        // A pre-17 log — events, no header — is refused, naming both.
        let old = NamedTempFile::new()?;
        std::fs::write(
            old.path(),
            "{\"id\":1,\"parent_id\":null,\"timestamp\":0,\"payload\":{\"Rename\":{\"name\":\"x\"}}}\n",
        )?;
        let Err(err) = Tree::open(old.reopen()?) else {
            panic!("a headerless log is refused");
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("no format-version header"),
            "{err}"
        );
        assert!(
            err.to_string().contains(&LOG_VERSION.to_string()),
            "the message names the version this build reads: {err}"
        );

        // A log from another version is refused naming both numbers.
        let future = NamedTempFile::new()?;
        std::fs::write(future.path(), "{\"version\":99}\n")?;
        let Err(err) = Tree::open(future.reopen()?) else {
            panic!("a log from another version is refused");
        };
        let msg = err.to_string();
        assert!(msg.contains("99"), "{msg}");
        assert!(msg.contains(&LOG_VERSION.to_string()), "{msg}");
        Ok(())
    }
}
