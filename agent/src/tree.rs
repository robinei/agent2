use crate::types::*;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

use jiff::Timestamp;

impl Tree {
    pub fn new(file: Option<File>) -> Self {
        Self {
            id_counter: 0,
            leaf_id: EventId::new(1),
            events: HashMap::new(),
            frames: Vec::new(),
            file,
        }
    }

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

        let (leaf_id, frames) = if events.is_empty() {
            (EventId::new(1), Vec::new())
        } else {
            let leaf_id = EventId::new(max_id);
            let frames = Self::reconstruct_frames(&events, leaf_id);
            (leaf_id, frames)
        };

        Ok(Self {
            id_counter: max_id,
            leaf_id,
            events,
            frames,
            file: Some(file),
        })
    }

    fn reconstruct_frames(events: &HashMap<EventId, Event>, leaf_id: EventId) -> Vec<Frame> {
        // Trace backward from leaf to root via parent_id,
        // then walk forward replaying the frame stack.
        let mut path: Vec<EventId> = Vec::new();
        let mut current = leaf_id;
        let mut visited: HashSet<EventId> = HashSet::new();
        loop {
            if !visited.insert(current) {
                break; // cycle detected
            }
            let Some(event) = events.get(&current) else {
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
        for &id in &path {
            let event = &events[&id];
            Self::replay_event(&mut frames, &event.payload, id);
        }
        frames
    }

    fn replay_event(frames: &mut Vec<Frame>, payload: &EventPayload, id: EventId) {
        match payload {
            EventPayload::PushFrame(prompt) => {
                frames.push(Frame {
                    prompt: prompt.clone(),
                    leaf_id: id,
                    messages: Vec::new(),
                });
            }
            EventPayload::PopFrame => {
                frames.pop().expect("PopFrame on empty frame stack");
            }
            EventPayload::Message(msg) => {
                if let Some(frame) = frames.last_mut() {
                    frame.messages.push(msg.clone());
                }
            }
            EventPayload::Label(_)
            | EventPayload::TextChunk(_)
            | EventPayload::ThinkingChunk(_) => {}
        }

        // Every event that leaves a frame active updates the top frame's leaf.
        if let Some(frame) = frames.last_mut() {
            frame.leaf_id = id;
        }
    }

    pub fn list_leaves(&self) -> Vec<(EventId, Option<String>)> {
        // A leaf is an event that no other event points to as parent.
        let mut child_counts: HashMap<EventId, usize> = HashMap::new();
        for event in self.events.values() {
            if let Some(parent) = event.parent_id {
                *child_counts.entry(parent).or_default() += 1;
            }
        }

        self.events
            .keys()
            .filter(|id| child_counts.get(id).copied().unwrap_or(0) == 0)
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

    pub fn append(&mut self, payload: EventPayload) -> io::Result<EventId> {
        self.id_counter += 1;
        let id = EventId::new(self.id_counter);

        // Bootstrap: if no events yet, this must be a PushFrame.
        // The root event has no parent.
        let parent_id = if self.events.is_empty() {
            assert!(
                matches!(payload, EventPayload::PushFrame(_)),
                "first event must be PushFrame, got {:?}",
                payload
            );
            None
        } else {
            Some(
                self.frames
                    .last()
                    .expect("no active frame on the current spine")
                    .leaf_id,
            )
        };

        Self::replay_event(&mut self.frames, &payload, id);
        self.leaf_id = id;

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff;
    use std::collections::HashMap;
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
    fn test_bootstrap_root_self_referential() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let id = tree.append(EventPayload::PushFrame("hello".into()))?;
        assert_eq!(id.as_u64(), 1);
        assert!(tree.events[&id].is_root());
        assert_eq!(tree.frames.len(), 1);
        assert_eq!(tree.frames[0].prompt, "hello");
        Ok(())
    }

    #[test]
    fn test_linear_conversation() -> io::Result<()> {
        let mut tree = Tree::new(None);
        tree.append(EventPayload::PushFrame("root".into()))?;
        tree.append(user_msg("hello"))?;
        tree.append(assistant_msg("hi there"))?;

        assert_eq!(tree.frames.len(), 1);
        assert_eq!(tree.frames[0].messages.len(), 2);
        assert_eq!(tree.frames[0].messages[0].text(), "hello");
        assert_eq!(tree.frames[0].messages[1].text(), "hi there");
        Ok(())
    }

    #[test]
    fn test_event_ids_monotonic() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let a = tree.append(EventPayload::PushFrame("root".into()))?;
        let b = tree.append(user_msg("hi"))?;
        let c = tree.append(assistant_msg("hey"))?;
        assert!(a.as_u64() < b.as_u64() && b.as_u64() < c.as_u64());
        Ok(())
    }

    #[test]
    #[should_panic(expected = "first event must be PushFrame")]
    fn test_first_event_must_be_push_frame() {
        let mut tree = Tree::new(None);
        let _ = tree.append(user_msg("oops"));
    }

    // --- PushFrame / PopFrame ---

    #[test]
    fn test_sub_frame() -> io::Result<()> {
        let mut tree = Tree::new(None);
        tree.append(EventPayload::PushFrame("root".into()))?;
        tree.append(user_msg("q1"))?;

        tree.append(EventPayload::PushFrame("sub".into()))?;
        tree.append(assistant_msg("answer"))?;
        assert_eq!(tree.frames.len(), 2);

        tree.append(EventPayload::PopFrame)?;
        assert_eq!(tree.frames.len(), 1);
        assert_eq!(tree.frames[0].messages.len(), 1);
        assert_eq!(tree.frames[0].messages[0].text(), "q1");

        tree.append(user_msg("q2"))?;
        assert_eq!(tree.frames[0].messages.len(), 2);
        assert_eq!(tree.frames[0].messages[1].text(), "q2");
        Ok(())
    }

    #[test]
    #[should_panic(expected = "no active frame on the current spine")]
    fn test_pop_frame_on_empty_stack() {
        let mut tree = Tree::new(None);
        tree.append(EventPayload::PushFrame("root".into())).unwrap();
        tree.append(EventPayload::PopFrame).unwrap();
        tree.append(EventPayload::PopFrame).unwrap();
    }

    // --- Labels ---

    #[test]
    fn test_label_on_leaf() -> io::Result<()> {
        let mut tree = Tree::new(None);
        tree.append(EventPayload::PushFrame("root".into()))?;
        let _msg = tree.append(user_msg("hi"))?;
        tree.append(EventPayload::Label("my branch".into()))?;

        let leaves = tree.list_leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].1, Some("my branch".to_string()));
        Ok(())
    }

    #[test]
    fn test_label_earlier_on_spine() -> io::Result<()> {
        let mut tree = Tree::new(None);
        tree.append(EventPayload::PushFrame("root".into()))?;
        tree.append(EventPayload::Label("my branch".into()))?;
        tree.append(user_msg("hello"))?;

        let leaves = tree.list_leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].1, Some("my branch".to_string()));
        Ok(())
    }

    #[test]
    fn test_unlabeled_leaf() -> io::Result<()> {
        let mut tree = Tree::new(None);
        tree.append(EventPayload::PushFrame("root".into()))?;
        tree.append(user_msg("hello"))?;

        let leaves = tree.list_leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].1, None);
        Ok(())
    }

    // --- list_leaves ---

    #[test]
    fn test_list_leaves_on_empty_tree() -> io::Result<()> {
        let tree = Tree::new(None);
        assert!(tree.list_leaves().is_empty());
        Ok(())
    }

    #[test]
    fn test_list_leaves_single_spine() -> io::Result<()> {
        let mut tree = Tree::new(None);
        tree.append(EventPayload::PushFrame("root".into()))?;
        tree.append(user_msg("hello"))?;

        let leaves = tree.list_leaves();
        assert_eq!(leaves.len(), 1);
        Ok(())
    }

    // --- Branching ---

    #[test]
    fn test_reconstruct_from_different_leaves() -> io::Result<()> {
        let tmp = NamedTempFile::new()?;
        let path = tmp.path().to_path_buf();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;
        let mut tree = Tree::open(file)?;

        tree.append(EventPayload::PushFrame("root".into()))?;
        tree.append(user_msg("m1"))?;
        tree.append(EventPayload::PushFrame("sub".into()))?;
        tree.append(assistant_msg("sub answer"))?;
        tree.append(EventPayload::PopFrame)?;
        tree.append(user_msg("follow-up"))?;

        let e2 = EventId::new(2);
        tree.id_counter = 9;
        tree.events.insert(
            EventId::new(7),
            Event {
                id: EventId::new(7),
                parent_id: Some(e2),
                timestamp: jiff::Timestamp::now(),
                payload: EventPayload::Label("branch".into()),
            },
        );
        tree.events.insert(
            EventId::new(8),
            Event {
                id: EventId::new(8),
                parent_id: Some(EventId::new(7)),
                timestamp: jiff::Timestamp::now(),
                payload: EventPayload::PushFrame("branch frame".into()),
            },
        );
        tree.events.insert(
            EventId::new(9),
            Event {
                id: EventId::new(9),
                parent_id: Some(EventId::new(8)),
                timestamp: jiff::Timestamp::now(),
                payload: assistant_msg("b1"),
            },
        );

        let branch_frames = Tree::reconstruct_frames(&tree.events, EventId::new(9));
        assert_eq!(branch_frames.len(), 2);
        assert_eq!(branch_frames[0].prompt, "root");
        assert_eq!(branch_frames[0].messages.len(), 1);
        assert_eq!(branch_frames[0].messages[0].text(), "m1");
        assert_eq!(branch_frames[1].prompt, "branch frame");
        assert_eq!(branch_frames[1].messages.len(), 1);
        assert_eq!(branch_frames[1].messages[0].text(), "b1");

        let orig_frames = Tree::reconstruct_frames(&tree.events, EventId::new(6));
        assert_eq!(orig_frames.len(), 1);
        assert_eq!(orig_frames[0].prompt, "root");
        assert_eq!(orig_frames[0].messages.len(), 2);
        assert_eq!(orig_frames[0].messages[0].text(), "m1");
        assert_eq!(orig_frames[0].messages[1].text(), "follow-up");

        let leaves = tree.list_leaves();
        assert_eq!(leaves.len(), 2);
        Ok(())
    }

    // --- File round-trip ---

    #[test]
    fn test_file_round_trip() -> io::Result<()> {
        let tmp = NamedTempFile::new()?;
        let path = tmp.path().to_path_buf();

        {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&path)?;
            let mut tree = Tree::open(file)?;
            tree.append(EventPayload::PushFrame("root".into()))?;
            tree.append(user_msg("alive"))?;
        }

        {
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let tree = Tree::open(file)?;
            assert_eq!(tree.frames.len(), 1);
            assert_eq!(tree.frames[0].prompt, "root");
            assert_eq!(tree.frames[0].messages.len(), 1);
            assert_eq!(tree.frames[0].messages[0].text(), "alive");

            let leaves = tree.list_leaves();
            assert_eq!(leaves.len(), 1);
        }
        Ok(())
    }

    #[test]
    fn test_file_append_resumes_from_last_leaf() -> io::Result<()> {
        let tmp = NamedTempFile::new()?;
        let path = tmp.path().to_path_buf();

        {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&path)?;
            let mut tree = Tree::open(file)?;
            tree.append(EventPayload::PushFrame("root".into()))?;
            tree.append(user_msg("first msg"))?;
        }

        {
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let mut tree = Tree::open(file)?;
            assert_eq!(tree.frames[0].messages.len(), 1);

            tree.append(assistant_msg("second msg"))?;
            assert_eq!(tree.frames[0].messages.len(), 2);
        }

        {
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let tree = Tree::open(file)?;
            assert_eq!(tree.frames[0].messages.len(), 2);
            assert_eq!(tree.frames[0].messages[0].text(), "first msg");
            assert_eq!(tree.frames[0].messages[1].text(), "second msg");
        }
        Ok(())
    }

    #[test]
    fn test_open_empty_file_append_event() -> io::Result<()> {
        let tmp = NamedTempFile::new()?;
        let path = tmp.path().to_path_buf();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;
        let mut tree = Tree::open(file)?;
        assert!(tree.frames.is_empty());

        tree.append(EventPayload::PushFrame("first".into()))?;
        assert_eq!(tree.frames.len(), 1);
        assert_eq!(tree.frames[0].prompt, "first");
        Ok(())
    }

    // --- Event.is_root ---

    #[test]
    fn test_root_event_is_root() -> io::Result<()> {
        let mut tree = Tree::new(None);
        let id = tree.append(EventPayload::PushFrame("root".into()))?;
        assert!(tree.events[&id].is_root());
        Ok(())
    }

    #[test]
    fn test_non_root_event_is_not_root() -> io::Result<()> {
        let mut tree = Tree::new(None);
        tree.append(EventPayload::PushFrame("root".into()))?;
        let id = tree.append(user_msg("hello"))?;
        assert!(!tree.events[&id].is_root());
        Ok(())
    }

    #[test]
    fn test_reconstruct_frames_empty_returns_empty() {
        let events = HashMap::new();
        let frames = Tree::reconstruct_frames(&events, EventId::new(42));
        assert!(frames.is_empty());
    }
}
