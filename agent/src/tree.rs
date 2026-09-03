use crate::types::*;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

use jiff::Timestamp;

/// One agent for the navigator pane, projected from the log (decision 8).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentView {
    pub id: EventId,
    /// The enclosing agent of this agent's call site (`None` for root).
    pub parent: Option<EventId>,
    pub prompt: String,
    /// A `FrameResult` was logged on this agent's spine.
    pub complete: bool,
}

/// One inner call a program made, with its settlement if one landed.
/// `outcome` is `None` while the call is still in flight — the same
/// distinction the artifact menu draws.
#[derive(Debug, Clone, PartialEq)]
pub struct InvokeView {
    pub id: EventId,
    pub name: String,
    pub args: serde_json::Value,
    pub outcome: Option<Outcome>,
}

/// One program execution, projected from the log: everything its panes
/// need without a live VM (decision 8). `id` is the `run_program`
/// Assistant event id; a `resume` folds into the same view.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramView {
    pub id: EventId,
    pub source: String,
    /// Attachment content: name → content string (from run_program args).
    pub attachments: HashMap<String, String>,
    pub invokes: Vec<InvokeView>,
    /// The top-level `return` value (`Some` ⇒ ran to completion).
    pub result: Option<serde_json::Value>,
    /// The run's `Tool` result text (completion or condition report).
    pub report: Option<String>,
    /// Full, unclipped console (from the `Console` event).
    pub console: Vec<String>,
}

impl ProgramView {
    /// Log-derived status for the program-list display. The precise
    /// suspended-vs-failed split for a *live* program comes from
    /// `SessionEvent::ProgramStatus`; this is what the log alone shows.
    pub fn status_label(&self) -> &'static str {
        if self.result.is_some() {
            "completed"
        } else if self.report.is_some() {
            "condition" // ended on a raise/trap (suspended or failed)
        } else {
            "running" // no tool result yet — in flight / interrupted
        }
    }
}

impl Tree {
    pub fn new(file: Option<File>) -> Self {
        Self {
            id_counter: 0,
            events: HashMap::new(),
            file,
        }
    }

    /// Load the event log. Reconstructing spines is the caller's move:
    /// `list_leaves()` for the set, `spine_at(leaf)` for a handle.
    pub fn open(mut file: File) -> Result<Self, io::Error> {
        // Rewind to start in case the file was opened with append(true),
        // which positions the cursor at the end initially.
        file.seek(SeekFrom::Start(0))?;

        let mut content = String::new();
        file.read_to_string(&mut content)?;
        // Cursor is now at end of existing content — subsequent writes
        // will land there regardless of whether append(true) was used.

        let mut max_id: u64 = 0;
        let mut events = HashMap::<EventId, Event>::new();
        for line in content.lines() {
            let event: Event = serde_json::from_str(line)?;
            max_id = std::cmp::max(max_id, event.id.as_u64());
            events.insert(event.id, event);
        }

        Ok(Self {
            id_counter: max_id,
            events,
            file: Some(file),
        })
    }

    /// Start a new agent: append a `Agent` rooting a new spine.
    /// `parent_id` is the call-site event on the caller's spine (`None`
    /// only for the tree's root agent). The caller's own spine handle is
    /// untouched — its leaf does not advance past the call site.
    pub fn start_agent(
        &mut self,
        parent_id: Option<EventId>,
        prompt: impl Into<String>,
        input: serde_json::Value,
    ) -> io::Result<Spine> {
        match parent_id {
            None => assert!(self.events.is_empty(), "root Agent on a non-empty tree"),
            Some(parent) => assert!(
                self.events.contains_key(&parent),
                "Agent parent {parent:?} not in tree"
            ),
        }

        let payload = EventPayload::Agent {
            prompt: prompt.into(),
            input,
        };
        let id = self.log_event(parent_id, payload)?;
        Ok(self.spine_at(id))
    }

    /// Fork from any event: reconstruct the spine at `from` and return
    /// an appendable handle. The first `append` on the returned spine
    /// creates a *sibling* of `from`'s existing spine child — a
    /// divergent branch within the same agent (user-driven retry /
    /// exploration, decision 4). Errors if `from` is unknown or its
    /// spine is already complete (a `FrameResult` is on the path, so
    /// nothing may follow it).
    pub fn fork(&self, from: EventId) -> io::Result<Spine> {
        if !self.events.contains_key(&from) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("cannot fork: event {from:?} not in tree"),
            ));
        }
        let spine = self.spine_at(from);
        if spine.is_complete() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cannot fork from a completed spine at {from:?}"),
            ));
        }
        Ok(spine)
    }

    /// Append an event to a spine. The spine's leaf advances; the
    /// innermost agent absorbs chat messages. `Agent` must go
    /// through `start_agent`; nothing may follow a `FrameResult`.
    pub fn append(&mut self, spine: &mut Spine, payload: EventPayload) -> io::Result<EventId> {
        assert!(
            !matches!(payload, EventPayload::Agent { .. }),
            "Agent must go through start_agent"
        );
        assert!(
            !spine.is_complete(),
            "append after FrameResult on a completed spine"
        );

        Self::replay_event(&mut spine.contexts, &payload);
        let id = self.log_event(Some(spine.leaf_id), payload)?;
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
            file.flush()?;
            file.sync_all()?;
        }

        self.events.insert(id, event);
        Ok(id)
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
            Self::replay_event(&mut contexts, &self.events[id].payload);
        }
        Spine { leaf_id, contexts }
    }

    fn replay_event(contexts: &mut Vec<Context>, payload: &EventPayload) {
        match payload {
            EventPayload::Agent { prompt, input } => {
                contexts.push(Context {
                    prompt: prompt.clone(),
                    input: input.clone(),
                    messages: Vec::new(),
                    result: None,
                });
            }
            EventPayload::Message(msg) => {
                contexts
                    .last_mut()
                    .expect("Message event with no enclosing agent")
                    .messages
                    .push(msg.clone());
            }
            EventPayload::FrameResult { result } => {
                contexts
                    .last_mut()
                    .expect("FrameResult event with no enclosing agent")
                    .result = Some(result.clone());
            }
            // Execution/marker events carry no agent-visible state; they
            // are queried from `events` by id (artifacts, replay, UI).
            EventPayload::Call(_)
            | EventPayload::Result { .. }
            | EventPayload::ProgramResult { .. }
            | EventPayload::Console { .. }
            | EventPayload::Label(_) => {}
        }
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

    /// Every agent in the log, in DFS tree order (root-first, children
    /// grouped under their parent and sorted by id): the agent-navigator
    /// projection (decision 8). `complete` is whether a `FrameResult`
    /// was logged on the agent's spine.
    pub fn agent_list(&self) -> Vec<AgentView> {
        let mut completed: HashSet<EventId> = HashSet::new();
        for event in self.events.values() {
            if let EventPayload::FrameResult { .. } = event.payload
                && let Some(agent) = event.parent_id.and_then(|p| self.enclosing_agent(p))
            {
                completed.insert(agent);
            }
        }
        let contexts: Vec<AgentView> = self
            .events
            .values()
            .filter_map(|event| match &event.payload {
                EventPayload::Agent { prompt, .. } => Some(AgentView {
                    id: event.id,
                    parent: event.parent_id.and_then(|p| self.enclosing_agent(p)),
                    prompt: prompt.clone(),
                    complete: completed.contains(&event.id),
                }),
                _ => None,
            })
            .collect();

        let mut children: HashMap<Option<EventId>, Vec<&AgentView>> = HashMap::new();
        for fv in &contexts {
            children.entry(fv.parent).or_default().push(fv);
        }
        for list in children.values_mut() {
            list.sort_by_key(|fv| fv.id.as_u64());
        }

        let mut ordered = Vec::with_capacity(contexts.len());
        let mut stack: Vec<&AgentView> =
            children.get(&None).into_iter().flatten().copied().collect();
        stack.reverse();
        while let Some(fv) = stack.pop() {
            ordered.push(fv.clone());
            if let Some(kids) = children.get(&Some(fv.id)) {
                for kid in kids.iter().rev() {
                    stack.push(kid);
                }
            }
        }
        ordered
    }

    /// `agent`'s programs along `leaf`'s path, in order — the program-list
    /// projection (decision 8). Each `run_program` opens a program; a
    /// `resume` folds into the open one (same VM, one entry); `Invoke`,
    /// `ProgramResult`, `Console`, and the run's `Tool` result attach to
    /// it. Everything a finished program's panes need, no live VM.
    pub fn programs_for(&self, agent: EventId, leaf: EventId) -> Vec<ProgramView> {
        let mut cur_agent: Option<EventId> = None;
        let mut programs: Vec<ProgramView> = Vec::new();
        for ev in self.path_events(leaf) {
            if let EventPayload::Agent { .. } = ev.payload {
                cur_agent = Some(ev.id);
                continue;
            }
            if cur_agent != Some(agent) {
                continue;
            }
            match &ev.payload {
                EventPayload::Message(Message::Assistant { tool_calls, .. }) => {
                    for call in tool_calls {
                        if call.name == crate::machine::TOOL_RUN_PROGRAM {
                            let source = call
                                .arguments
                                .get("source")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let attachments: std::collections::HashMap<String, String> = call
                                .arguments
                                .get("attachments")
                                .and_then(|v| v.as_object())
                                .map(|obj| {
                                    obj.iter()
                                        .filter_map(|(k, v)| {
                                            v.as_str().map(|s| (k.clone(), s.to_string()))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            programs.push(ProgramView {
                                id: ev.id,
                                source,
                                attachments,
                                invokes: Vec::new(),
                                result: None,
                                report: None,
                                console: Vec::new(),
                            });
                        }
                        // `resume` continues the open program — no new entry.
                    }
                }
                EventPayload::Call(call) => {
                    if let Some(p) = programs.last_mut() {
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
                        };
                        p.invokes.push(InvokeView {
                            id: ev.id,
                            name,
                            args,
                            outcome: None,
                        });
                    }
                }
                EventPayload::Result { call, outcome } => {
                    if let Some(p) = programs.last_mut()
                        && let Some(iv) = p.invokes.iter_mut().find(|iv| iv.id == *call)
                    {
                        iv.outcome = Some(outcome.clone());
                    }
                }
                EventPayload::ProgramResult { value } => {
                    if let Some(p) = programs.last_mut() {
                        p.result = Some(value.clone());
                    }
                }
                EventPayload::Console { lines } => {
                    if let Some(p) = programs.last_mut() {
                        p.console = lines.clone();
                    }
                }
                EventPayload::Message(Message::Tool { text, .. }) => {
                    if let Some(p) = programs.last_mut() {
                        p.report = Some(text.clone());
                    }
                }
                _ => {}
            }
        }
        programs
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
                let label = self.label_for_leaf(id);
                (id, label)
            })
            .collect()
    }

    fn label_for_leaf(&self, leaf_id: EventId) -> Option<String> {
        let mut current = leaf_id;
        loop {
            let event = self.events.get(&current)?;
            if let EventPayload::Label(label) = &event.payload {
                return Some(label.clone());
            }
            current = event.parent_id?;
        }
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
        EventPayload::Message(Message::User { text: text.into() })
    }

    fn assistant_msg(text: &str) -> EventPayload {
        EventPayload::Message(Message::Assistant {
            text: text.into(),
            thinking: None,
            tool_calls: Vec::new(),
        })
    }

    fn run_program_call(id: &str, source: &str) -> EventPayload {
        EventPayload::Message(Message::Assistant {
            text: String::new(),
            thinking: None,
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "run_program".into(),
                arguments: json!({ "source": source }),
            }],
        })
    }

    fn resume_call(id: &str) -> EventPayload {
        EventPayload::Message(Message::Assistant {
            text: String::new(),
            thinking: None,
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "resume".into(),
                arguments: json!({ "value": null }),
            }],
        })
    }

    fn tool_result(call_id: &str, text: &str) -> EventPayload {
        EventPayload::Message(Message::Tool {
            name: "run_program".into(),
            call_id: call_id.into(),
            text: text.into(),
        })
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
            let mut spine = tree.start_agent(None, "root", json!(null))?;
            agent = spine.leaf_id; // the Agent id is the agent id
            tree.append(
                &mut spine,
                run_program_call("c1", "console.log('hi'); return 42;"),
            )?;
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
            tree.append(&mut spine, EventPayload::ProgramResult { value: json!(42) })?;
            tree.append(
                &mut spine,
                tool_result("c1", "completed: 42 (clipped console…)"),
            )?;
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
        let contexts = tree.agent_list();
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].id, agent);
        assert_eq!(contexts[0].prompt, "root");
        assert!(!contexts[0].complete, "root never logs a FrameResult");

        let progs = tree.programs_for(agent, leaf);
        assert_eq!(progs.len(), 1);
        let p = &progs[0];
        assert_eq!(p.id, EventId::new(agent.as_u64() + 1)); // the run_program assistant event
        assert!(p.source.contains("return 42"));
        assert_eq!(p.invokes.len(), 1);
        assert_eq!(p.invokes[0].name, "bash");
        assert_eq!(p.result, Some(json!(42)));
        assert_eq!(
            p.console,
            vec!["hi".to_string()],
            "full console, not the clipped report"
        );
        assert_eq!(p.status_label(), "completed");
        Ok(())
    }

    /// A raise that is later resumed to completion is one program; its
    /// console spans both segments and the latest result wins.
    #[test]
    fn raise_then_resume_is_one_program() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, "root", json!(null))?;
        let agent = spine.leaf_id;
        tree.append(&mut spine, run_program_call("c1", "raise('x');"))?;
        tree.append(&mut spine, tool_result("c1", "condition: x"))?; // suspend
        tree.append(&mut spine, resume_call("c2"))?; // continues the same program
        tree.append(
            &mut spine,
            EventPayload::ProgramResult {
                value: json!("done"),
            },
        )?;
        tree.append(&mut spine, tool_result("c2", "completed: done"))?;
        tree.append(
            &mut spine,
            EventPayload::Console {
                lines: vec!["before".into(), "after".into()],
            },
        )?;
        let leaf = spine.leaf_id;

        let progs = tree.programs_for(agent, leaf);
        assert_eq!(progs.len(), 1, "resume folds into one program");
        assert_eq!(progs[0].result, Some(json!("done")));
        assert_eq!(
            progs[0].console,
            vec!["before".to_string(), "after".to_string()]
        );
        assert_eq!(progs[0].status_label(), "completed");
        Ok(())
    }

    // --- Bootstrap & linear flow ---

    #[test]
    fn test_bootstrap_root_agent() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let spine = tree.start_agent(None, "hello", json!(null))?;
        assert_eq!(spine.leaf_id.as_u64(), 1);
        assert!(tree.events[&spine.leaf_id].is_root());
        assert_eq!(spine.contexts.len(), 1);
        assert_eq!(spine.context().prompt, "hello");
        Ok(())
    }

    #[test]
    #[should_panic(expected = "root Agent on a non-empty tree")]
    fn test_second_root_agent_panics() {
        let mut tree = Tree::new(None);
        tree.start_agent(None, "root", json!(null)).unwrap();
        let _ = tree.start_agent(None, "another root", json!(null));
    }

    #[test]
    fn test_linear_conversation() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, "root", json!(null))?;
        tree.append(&mut spine, user_msg("hello"))?;
        tree.append(&mut spine, assistant_msg("hi there"))?;

        assert_eq!(spine.contexts.len(), 1);
        assert_eq!(spine.context().messages.len(), 2);
        assert_eq!(spine.context().messages[0].text(), "hello");
        assert_eq!(spine.context().messages[1].text(), "hi there");
        Ok(())
    }

    #[test]
    #[should_panic(expected = "Agent must go through start_agent")]
    fn test_append_agent_root_panics() {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, "root", json!(null)).unwrap();
        let _ = tree.append(
            &mut spine,
            EventPayload::Agent {
                prompt: "child".into(),
                input: json!(null),
            },
        );
    }

    // --- Context completion ---

    #[test]
    fn test_result_completes_spine() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, "root", json!(null))?;
        tree.append(&mut spine, assistant_msg("done"))?;
        assert!(!spine.is_complete());

        tree.append(
            &mut spine,
            EventPayload::FrameResult {
                result: json!({"ok": true}),
            },
        )?;
        assert!(spine.is_complete());
        assert_eq!(spine.context().result, Some(json!({"ok": true})));
        Ok(())
    }

    #[test]
    #[should_panic(expected = "append after FrameResult")]
    fn test_append_after_result_panics() {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, "root", json!(null)).unwrap();
        tree.append(&mut spine, EventPayload::FrameResult { result: json!(42) })
            .unwrap();
        let _ = tree.append(&mut spine, user_msg("too late"));
    }

    // --- Execution events ---

    #[test]
    fn test_calls_and_program_result_are_artifacts_not_messages() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, "root", json!(null))?;
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
            EventPayload::ProgramResult {
                value: json!([1, 2]),
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
            EventPayload::ProgramResult { .. }
        ));
        Ok(())
    }

    // --- Branching: subagent contexts ---

    /// Caller spine + child agent branched at a call-site event, appends
    /// interleaved between the two spines.
    fn build_branched_tree(tree: &mut Tree) -> io::Result<(Spine, Spine)> {
        let mut caller = tree.start_agent(None, "root", json!(null))?;
        tree.append(&mut caller, user_msg("m1"))?;
        let call_site = tree.append(&mut caller, assistant_msg("spawning"))?;

        let mut child = tree.start_agent(Some(call_site), "child prompt", json!({"task": 1}))?;
        // Interleave appends across the two spines.
        tree.append(&mut caller, user_msg("caller continues"))?;
        tree.append(&mut child, assistant_msg("child working"))?;
        tree.append(&mut caller, assistant_msg("caller answer"))?;
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
            assert_eq!(
                msgs,
                ["m1", "spawning", "caller continues", "caller answer"]
            );
        }
        for spine in [&child, &tree.spine_at(child.leaf_id)] {
            assert_eq!(spine.contexts.len(), 2, "child sits under the root agent");
            assert_eq!(spine.context().prompt, "child prompt");
            assert_eq!(spine.context().input, json!({"task": 1}));
            let msgs: Vec<&str> = spine.context().messages.iter().map(|m| m.text()).collect();
            assert_eq!(msgs, ["child working"]);
        }
        Ok(())
    }

    #[test]
    fn test_event_ids_monotonic_across_spines() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let (caller, child) = build_branched_tree(&mut tree)?;
        // 7 events total, globally monotonic ids regardless of spine.
        assert_eq!(tree.events.len(), 7);
        let mut ids: Vec<u64> = tree.events.keys().map(|id| id.as_u64()).collect();
        ids.sort_unstable();
        assert_eq!(ids, (1..=7).collect::<Vec<_>>());
        assert!(caller.leaf_id != child.leaf_id);
        Ok(())
    }

    #[test]
    fn test_in_flight_branch_keeps_caller_leaf() -> io::Result<()> {
        // A Agent child must not swallow the caller's leaf: with no
        // caller activity after the call site, the call-site event is
        // still the caller's resumable leaf.
        let mut tree = Tree::new(None);
        let mut caller = tree.start_agent(None, "root", json!(null))?;
        let call_site = tree.append(&mut caller, assistant_msg("spawning"))?;
        let child = tree.start_agent(Some(call_site), "child", json!(null))?;

        let mut leaves: Vec<EventId> = tree.list_leaves().into_iter().map(|(id, _)| id).collect();
        leaves.sort_by_key(|id| id.as_u64());
        assert_eq!(leaves, vec![call_site, child.leaf_id]);
        Ok(())
    }

    // --- Forking ---

    #[test]
    fn test_fork_mid_spine_diverges_leaving_original_intact() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, "root", json!(null))?;
        tree.append(&mut spine, user_msg("q"))?;
        let fork_point = tree.append(&mut spine, assistant_msg("first answer"))?;
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
        assert_eq!(texts(original_leaf), ["q", "first answer", "follow-up A"]);
        assert_eq!(texts(forked_leaf), ["q", "first answer", "follow-up B"]);
        Ok(())
    }

    #[test]
    fn test_fork_from_completed_spine_errors() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, "root", json!(null))?;
        tree.append(&mut spine, assistant_msg("done"))?;
        let result_id = tree.append(&mut spine, EventPayload::FrameResult { result: json!(1) })?;
        let err = tree.fork(result_id).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        Ok(())
    }

    #[test]
    fn test_fork_from_unknown_id_errors() {
        let tree = Tree::new(None);
        let err = tree.fork(EventId::new(99)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    // --- Labels ---

    #[test]
    fn test_label_on_leaf() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, "root", json!(null))?;
        tree.append(&mut spine, user_msg("hi"))?;
        tree.append(&mut spine, EventPayload::Label("my branch".into()))?;

        let leaves = tree.list_leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].1, Some("my branch".to_string()));
        Ok(())
    }

    #[test]
    fn test_label_earlier_on_spine() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, "root", json!(null))?;
        tree.append(&mut spine, EventPayload::Label("my branch".into()))?;
        tree.append(&mut spine, user_msg("hello"))?;

        let leaves = tree.list_leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].1, Some("my branch".to_string()));
        Ok(())
    }

    #[test]
    fn test_unlabeled_leaf() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, "root", json!(null))?;
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
            // Child is in flight: Agent logged, no FrameResult yet.
            assert!(!child.is_complete());
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

            // Resume the in-flight child: reconstruct and finish it.
            let mut child = tree.spine_at(child_leaf);
            assert_eq!(child.context().prompt, "child prompt");
            assert!(!child.is_complete());
            tree.append(
                &mut child,
                EventPayload::FrameResult {
                    result: json!("done"),
                },
            )?;
        }

        {
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let tree = Tree::open(file)?;
            let child = tree.spine_at(EventId::new(tree.id_counter));
            assert!(child.is_complete());
            assert_eq!(child.context().result, Some(json!("done")));
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
            let mut spine = tree.start_agent(None, "root", json!(null))?;
            tree.append(&mut spine, user_msg("first msg"))?;
        }

        {
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let mut tree = Tree::open(file)?;
            let leaves = tree.list_leaves();
            assert_eq!(leaves.len(), 1);
            let mut spine = tree.spine_at(leaves[0].0);
            assert_eq!(spine.context().messages.len(), 1);

            tree.append(&mut spine, assistant_msg("second msg"))?;
            assert_eq!(spine.context().messages.len(), 2);
        }

        {
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let tree = Tree::open(file)?;
            let leaves = tree.list_leaves();
            assert_eq!(leaves.len(), 1);
            let spine = tree.spine_at(leaves[0].0);
            assert_eq!(spine.context().messages.len(), 2);
            assert_eq!(spine.context().messages[0].text(), "first msg");
            assert_eq!(spine.context().messages[1].text(), "second msg");
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

        let spine = tree.start_agent(None, "first", json!(null))?;
        assert_eq!(spine.contexts.len(), 1);
        assert_eq!(spine.context().prompt, "first");
        Ok(())
    }

    // --- spine_at edges ---

    #[test]
    fn test_spine_at_unknown_id_is_empty() {
        let tree = Tree::new(None);
        let spine = tree.spine_at(EventId::new(42));
        assert!(spine.contexts.is_empty());
    }
}
