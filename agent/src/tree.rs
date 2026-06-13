use crate::types::*;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

use jiff::Timestamp;

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

    /// Start a new frame: append a `FrameStart` rooting a new spine.
    /// `parent_id` is the call-site event on the caller's spine (`None`
    /// only for the tree's root frame). The caller's own spine handle is
    /// untouched — its leaf does not advance past the call site.
    pub fn start_frame(
        &mut self,
        parent_id: Option<EventId>,
        prompt: impl Into<String>,
        input: serde_json::Value,
    ) -> io::Result<Spine> {
        match parent_id {
            None => assert!(
                self.events.is_empty(),
                "root FrameStart on a non-empty tree"
            ),
            Some(parent) => assert!(
                self.events.contains_key(&parent),
                "FrameStart parent {parent:?} not in tree"
            ),
        }

        let payload = EventPayload::FrameStart {
            prompt: prompt.into(),
            input,
        };
        let id = self.log_event(parent_id, payload)?;
        Ok(self.spine_at(id))
    }

    /// Fork from any event: reconstruct the spine at `from` and return
    /// an appendable handle. The first `append` on the returned spine
    /// creates a *sibling* of `from`'s existing spine child — a
    /// divergent branch within the same frame (user-driven retry /
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
    /// innermost frame absorbs chat messages. `FrameStart` must go
    /// through `start_frame`; nothing may follow a `FrameResult`.
    pub fn append(&mut self, spine: &mut Spine, payload: EventPayload) -> io::Result<EventId> {
        assert!(
            payload.is_storable(),
            "live-only chunk appended to log: {payload:?}"
        );
        assert!(
            !matches!(payload, EventPayload::FrameStart { .. }),
            "FrameStart must go through start_frame"
        );
        assert!(
            !spine.is_complete(),
            "append after FrameResult on a completed spine"
        );

        Self::replay_event(&mut spine.frames, &payload);
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
    /// `parent_id`, then replay forward. The frame chain is the
    /// `FrameStart` ancestors of the leaf, innermost last.
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

        let mut frames: Vec<Frame> = Vec::new();
        for id in &path {
            Self::replay_event(&mut frames, &self.events[id].payload);
        }
        Spine { leaf_id, frames }
    }

    fn replay_event(frames: &mut Vec<Frame>, payload: &EventPayload) {
        match payload {
            EventPayload::FrameStart { prompt, input } => {
                frames.push(Frame {
                    prompt: prompt.clone(),
                    input: input.clone(),
                    messages: Vec::new(),
                    result: None,
                });
            }
            EventPayload::Message(msg) => {
                frames
                    .last_mut()
                    .expect("Message event with no enclosing frame")
                    .messages
                    .push(msg.clone());
            }
            EventPayload::FrameResult { result } => {
                frames
                    .last_mut()
                    .expect("FrameResult event with no enclosing frame")
                    .result = Some(result.clone());
            }
            // Execution/marker events carry no frame-visible state; they
            // are queried from `events` by id (artifacts, replay, UI).
            EventPayload::Invoke { .. }
            | EventPayload::ProgramResult { .. }
            | EventPayload::Label(_) => {}
            // Never stored (asserted in append).
            EventPayload::TextChunk(_) | EventPayload::ThinkingChunk(_) => {}
        }
    }

    /// The set of spine leaves. A leaf is an event no *spine* event
    /// follows: `FrameStart` children don't count — they root child
    /// branches, so a call-site event stays its caller's leaf while a
    /// subagent is in flight.
    pub fn list_leaves(&self) -> Vec<(EventId, Option<String>)> {
        let mut spine_child_counts: HashMap<EventId, usize> = HashMap::new();
        for event in self.events.values() {
            if matches!(event.payload, EventPayload::FrameStart { .. }) {
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
            match event.parent_id {
                Some(parent) => current = parent,
                None => return None,
            };
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

    // --- Bootstrap & linear flow ---

    #[test]
    fn test_bootstrap_root_frame() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let spine = tree.start_frame(None, "hello", json!(null))?;
        assert_eq!(spine.leaf_id.as_u64(), 1);
        assert!(tree.events[&spine.leaf_id].is_root());
        assert_eq!(spine.frames.len(), 1);
        assert_eq!(spine.frame().prompt, "hello");
        Ok(())
    }

    #[test]
    #[should_panic(expected = "root FrameStart on a non-empty tree")]
    fn test_second_root_frame_panics() {
        let mut tree = Tree::new(None);
        tree.start_frame(None, "root", json!(null)).unwrap();
        let _ = tree.start_frame(None, "another root", json!(null));
    }

    #[test]
    fn test_linear_conversation() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_frame(None, "root", json!(null))?;
        tree.append(&mut spine, user_msg("hello"))?;
        tree.append(&mut spine, assistant_msg("hi there"))?;

        assert_eq!(spine.frames.len(), 1);
        assert_eq!(spine.frame().messages.len(), 2);
        assert_eq!(spine.frame().messages[0].text(), "hello");
        assert_eq!(spine.frame().messages[1].text(), "hi there");
        Ok(())
    }

    #[test]
    #[should_panic(expected = "FrameStart must go through start_frame")]
    fn test_append_frame_start_panics() {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_frame(None, "root", json!(null)).unwrap();
        let _ = tree.append(
            &mut spine,
            EventPayload::FrameStart {
                prompt: "child".into(),
                input: json!(null),
            },
        );
    }

    #[test]
    #[should_panic(expected = "live-only chunk")]
    fn test_append_chunk_panics() {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_frame(None, "root", json!(null)).unwrap();
        let _ = tree.append(&mut spine, EventPayload::TextChunk("hi".into()));
    }

    // --- Frame completion ---

    #[test]
    fn test_frame_result_completes_spine() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_frame(None, "root", json!(null))?;
        tree.append(&mut spine, assistant_msg("done"))?;
        assert!(!spine.is_complete());

        tree.append(
            &mut spine,
            EventPayload::FrameResult {
                result: json!({"ok": true}),
            },
        )?;
        assert!(spine.is_complete());
        assert_eq!(spine.frame().result, Some(json!({"ok": true})));
        Ok(())
    }

    #[test]
    #[should_panic(expected = "append after FrameResult")]
    fn test_append_after_frame_result_panics() {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_frame(None, "root", json!(null)).unwrap();
        tree.append(&mut spine, EventPayload::FrameResult { result: json!(42) })
            .unwrap();
        let _ = tree.append(&mut spine, user_msg("too late"));
    }

    // --- Execution events ---

    #[test]
    fn test_invoke_and_program_result_are_artifacts_not_messages() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_frame(None, "root", json!(null))?;
        let invoke_id = tree.append(
            &mut spine,
            EventPayload::Invoke {
                name: "fetch".into(),
                args: json!({"url": "http://x"}),
                result: json!("body"),
            },
        )?;
        let result_id = tree.append(
            &mut spine,
            EventPayload::ProgramResult {
                value: json!([1, 2]),
            },
        )?;

        // Spine leaf advanced past both, but the frame's chat transcript
        // is untouched — they're id-addressable artifacts.
        assert_eq!(spine.leaf_id, result_id);
        assert!(spine.frame().messages.is_empty());
        assert!(matches!(
            tree.events[&invoke_id].payload,
            EventPayload::Invoke { .. }
        ));
        assert!(matches!(
            tree.events[&result_id].payload,
            EventPayload::ProgramResult { .. }
        ));
        Ok(())
    }

    // --- Branching: subagent frames ---

    /// Caller spine + child frame branched at a call-site event, appends
    /// interleaved between the two spines.
    fn build_branched_tree(tree: &mut Tree) -> io::Result<(Spine, Spine)> {
        let mut caller = tree.start_frame(None, "root", json!(null))?;
        tree.append(&mut caller, user_msg("m1"))?;
        let call_site = tree.append(&mut caller, assistant_msg("spawning"))?;

        let mut child = tree.start_frame(Some(call_site), "child prompt", json!({"task": 1}))?;
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
            assert_eq!(spine.frames.len(), 1);
            let msgs: Vec<&str> = spine.frame().messages.iter().map(|m| m.text()).collect();
            assert_eq!(
                msgs,
                ["m1", "spawning", "caller continues", "caller answer"]
            );
        }
        for spine in [&child, &tree.spine_at(child.leaf_id)] {
            assert_eq!(spine.frames.len(), 2, "child sits under the root frame");
            assert_eq!(spine.frame().prompt, "child prompt");
            assert_eq!(spine.frame().input, json!({"task": 1}));
            let msgs: Vec<&str> = spine.frame().messages.iter().map(|m| m.text()).collect();
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
        // A FrameStart child must not swallow the caller's leaf: with no
        // caller activity after the call site, the call-site event is
        // still the caller's resumable leaf.
        let mut tree = Tree::new(None);
        let mut caller = tree.start_frame(None, "root", json!(null))?;
        let call_site = tree.append(&mut caller, assistant_msg("spawning"))?;
        let child = tree.start_frame(Some(call_site), "child", json!(null))?;

        let mut leaves: Vec<EventId> = tree.list_leaves().into_iter().map(|(id, _)| id).collect();
        leaves.sort_by_key(|id| id.as_u64());
        assert_eq!(leaves, vec![call_site, child.leaf_id]);
        Ok(())
    }

    // --- Forking ---

    #[test]
    fn test_fork_mid_spine_diverges_leaving_original_intact() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_frame(None, "root", json!(null))?;
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
                .frame()
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
        let mut spine = tree.start_frame(None, "root", json!(null))?;
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
        let mut spine = tree.start_frame(None, "root", json!(null))?;
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
        let mut spine = tree.start_frame(None, "root", json!(null))?;
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
        let mut spine = tree.start_frame(None, "root", json!(null))?;
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
                .open(&path)?;
            let mut tree = Tree::open(file)?;
            let (caller, child) = build_branched_tree(&mut tree)?;
            // Child is in flight: FrameStart logged, no FrameResult yet.
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
            assert_eq!(child.frame().prompt, "child prompt");
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
            assert_eq!(child.frame().result, Some(json!("done")));
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
                .open(&path)?;
            let mut tree = Tree::open(file)?;
            let mut spine = tree.start_frame(None, "root", json!(null))?;
            tree.append(&mut spine, user_msg("first msg"))?;
        }

        {
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let mut tree = Tree::open(file)?;
            let leaves = tree.list_leaves();
            assert_eq!(leaves.len(), 1);
            let mut spine = tree.spine_at(leaves[0].0);
            assert_eq!(spine.frame().messages.len(), 1);

            tree.append(&mut spine, assistant_msg("second msg"))?;
            assert_eq!(spine.frame().messages.len(), 2);
        }

        {
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let tree = Tree::open(file)?;
            let leaves = tree.list_leaves();
            assert_eq!(leaves.len(), 1);
            let spine = tree.spine_at(leaves[0].0);
            assert_eq!(spine.frame().messages.len(), 2);
            assert_eq!(spine.frame().messages[0].text(), "first msg");
            assert_eq!(spine.frame().messages[1].text(), "second msg");
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
            .open(&path)?;
        let mut tree = Tree::open(file)?;
        assert!(tree.list_leaves().is_empty());

        let spine = tree.start_frame(None, "first", json!(null))?;
        assert_eq!(spine.frames.len(), 1);
        assert_eq!(spine.frame().prompt, "first");
        Ok(())
    }

    // --- spine_at edges ---

    #[test]
    fn test_spine_at_unknown_id_is_empty() {
        let tree = Tree::new(None);
        let spine = tree.spine_at(EventId::new(42));
        assert!(spine.frames.is_empty());
    }
}
