//! **The prompt lab: capture a request, edit it, resample it.**
//!
//! Every prompt question this project has asked so far was answered by
//! running whole tasks and comparing medians — six runs an arm, each a
//! fresh sample of a heavy-tailed distribution, and the answer almost
//! always "no signal". The card-length sweep put p = 0.937 on its
//! primary metric. The one result that did separate
//! (`docs/evidence/2026-09-21-tail-position-result.md`) needed twelve
//! runs against six and landed at p = 0.0054, which is not much clear
//! of a Bonferroni threshold.
//!
//! The variance is not in the change being measured. It is in the
//! *path*: a run wanders, and where it wanders decides the tokens. So
//! this holds the path still. Capture the document at one point of one
//! session, change the one thing under test, and sample the **next
//! completion** many times. Between-run path variance is gone by
//! construction, the observation is one completion rather than one
//! task, and the unit is cheap — no tools run, nothing is written, and
//! nothing has to converge.
//!
//! Two verbs, and a file between them that is just text:
//!
//! ```text
//! agent capture session.jsonl 27 -o base.doc
//! cp base.doc arm.doc && $EDITOR arm.doc
//! agent sample base.doc -n 40 -o base.jsonl
//! agent sample arm.doc  -n 40 -o arm.jsonl
//! python3 evals/labstats.py base.jsonl arm.jsonl
//! ```
//!
//! **The file is the interface.** It round-trips a [`Document`]
//! exactly, and anything in it can be edited: the card in the system
//! message, a single turn of the conversation, the ephemeral tail on
//! the end of the last user message — which is the part no log holds
//! and the part that has produced the only effect we have measured.

use crate::document::{ChatMessage, ChatRole, Document};

/// First line of a captured file. The version is here so a format
/// change is a readable error rather than a mis-parse.
const MAGIC: &str = "#!agent2-doc 1";

/// The shortest separator tried. Lengthened at capture time until no
/// line of any message starts with it, so a card that itself discusses
/// this format cannot break the file it is captured into.
const SEP_SEED: &str = "@@@@@@@@";

fn role_name(role: ChatRole) -> &'static str {
    match role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
    }
}

fn role_of(name: &str) -> Result<ChatRole, String> {
    match name {
        "system" => Ok(ChatRole::System),
        "user" => Ok(ChatRole::User),
        "assistant" => Ok(ChatRole::Assistant),
        other => Err(format!(
            "unknown role `{other}` — expected system, user or assistant"
        )),
    }
}

/// A separator no message begins a line with. Collision is close to
/// impossible at eight characters, but "close to impossible" is how the
/// forgery guard and the console line cap were both written, and both
/// were wrong in the end.
fn separator(doc: &Document) -> String {
    let mut sep = SEP_SEED.to_owned();
    while doc
        .messages
        .iter()
        .any(|m| m.content.lines().any(|l| l.starts_with(&sep)))
    {
        sep.push('@');
    }
    sep
}

/// Render a document as an editable file.
pub fn write(doc: &Document) -> String {
    let sep = separator(doc);
    let mut out = format!("{MAGIC} sep={sep} preamble={}\n", doc.preamble);
    for m in &doc.messages {
        out.push_str(&format!("{sep} {}\n{}\n", role_name(m.role), m.content));
    }
    out
}

/// Read one back. Whitespace inside a message is preserved to the byte;
/// the only thing consumed is the single newline this format adds
/// before each separator.
pub fn parse(text: &str) -> Result<Document, String> {
    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default();
    if !header.starts_with(MAGIC) {
        return Err(format!(
            "not a captured document: expected a first line starting `{MAGIC}`"
        ));
    }
    let field = |key: &str| -> Option<&str> {
        header
            .split_whitespace()
            .find_map(|f| f.strip_prefix(key))
            .filter(|v| !v.is_empty())
    };
    let sep = field("sep=").ok_or("the header has no `sep=`")?;
    let preamble: usize = field("preamble=")
        .and_then(|v| v.parse().ok())
        .ok_or("the header has no readable `preamble=`")?;

    let mut messages: Vec<ChatMessage> = Vec::new();
    let mut open: Option<(ChatRole, Vec<&str>)> = None;
    for line in lines {
        if let Some(rest) = line.strip_prefix(sep)
            && let Some(name) = rest.strip_prefix(' ')
        {
            if let Some((role, body)) = open.take() {
                messages.push(ChatMessage {
                    role,
                    content: body.join("\n"),
                });
            }
            open = Some((role_of(name.trim())?, Vec::new()));
            continue;
        }
        match open.as_mut() {
            Some((_, body)) => body.push(line),
            None => return Err("content before the first separator".into()),
        }
    }
    if let Some((role, body)) = open {
        messages.push(ChatMessage {
            role,
            content: body.join("\n"),
        });
    }
    if messages.is_empty() {
        return Err("no messages in the file".into());
    }
    // No trailing-newline fixup here, and that is deliberate. `write`
    // puts one newline after each message, `lines()` renders it as a
    // final empty element, and `join("\n")` puts it back — so content
    // that ended in a newline round-trips and content that did not is
    // left alone. Stripping one "to undo the separator" ate a real
    // newline off every message that had one, which the card does.
    Ok(Document {
        preamble: preamble.min(messages.len()),
        messages,
    })
}

/// One sampled completion, as written to the results file.
///
/// `source` and `thinking` are kept whole rather than pre-measured: the
/// metric that mattered on 2026-09-21 (fenced drafts inside the
/// reasoning stream) was not one anybody had thought to record in
/// advance, and the run that would have answered it had already been
/// paid for. Store the text; decide what to count afterwards.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Sample {
    pub i: usize,
    pub ms: u128,
    pub source: String,
    pub thinking: String,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::host::Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: ChatRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_owned(),
        }
    }

    /// **The whole point of the format.** A capture that does not come
    /// back byte-identical is an arm that differs from its control by
    /// something nobody chose, which is the failure this tool exists to
    /// remove from the measurement.
    #[test]
    fn a_document_round_trips_exactly() {
        let doc = Document {
            messages: vec![
                msg(ChatRole::System, "the card\n\nwith ```js fences\n"),
                msg(ChatRole::User, "do it"),
                msg(ChatRole::Assistant, "```js\nreturn 1;\n```"),
                msg(ChatRole::User, "## program completed\n- a tail line."),
            ],
            preamble: 3,
        };
        let text = write(&doc);
        assert_eq!(parse(&text).expect("parses"), doc);
    }

    /// An empty message is a real shape — a turn whose whole content
    /// was clipped away — and the naive "join the lines" reader turns
    /// it into a lost message rather than an empty one.
    #[test]
    fn an_empty_message_survives_the_round_trip() {
        let doc = Document {
            messages: vec![msg(ChatRole::System, ""), msg(ChatRole::User, "hi")],
            preamble: 1,
        };
        assert_eq!(parse(&write(&doc)).expect("parses"), doc);
    }

    /// The card can talk about this format — this file's own doc
    /// comment does — so the separator has to get out of the way rather
    /// than assume it will not be written.
    #[test]
    fn the_separator_lengthens_past_a_collision() {
        let doc = Document {
            messages: vec![msg(
                ChatRole::System,
                &format!("{SEP_SEED} system\nnot really a separator"),
            )],
            preamble: 1,
        };
        let text = write(&doc);
        assert!(
            text.contains(&format!("{SEP_SEED}@ system")),
            "the separator did not lengthen: {text}"
        );
        assert_eq!(parse(&text).expect("parses"), doc);
    }

    /// **The shipped card, which is the content most likely to break
    /// this.** It carries fenced blocks, a markdown table, `↓`/`←` and
    /// a trailing newline — and it is the message an experiment is most
    /// likely to be editing. A synthetic fixture that happens to avoid
    /// all four would pass while the only capture anyone makes fails.
    #[test]
    fn the_shipped_card_round_trips() {
        let card = crate::card::active();
        let mut messages = vec![msg(ChatRole::System, &card.text)];
        for ex in &card.exemplars {
            messages.push(msg(ChatRole::User, &ex.user));
            messages.push(msg(ChatRole::Assistant, &ex.assistant));
        }
        let doc = Document {
            preamble: messages.len(),
            messages,
        };
        assert_eq!(parse(&write(&doc)).expect("parses"), doc);
    }

    #[test]
    fn a_file_that_is_not_a_capture_says_so() {
        let e = parse("just some text\nand more").unwrap_err();
        assert!(e.contains("not a captured document"), "{e}");
    }

    /// Editing is the point, so the parser must accept a hand-written
    /// file — one where the editor reflowed nothing but the content.
    #[test]
    fn a_hand_edited_file_parses() {
        let text = format!("{MAGIC} sep=### preamble=1\n### system\nrules\n### user\nask\n");
        let doc = parse(&text).expect("parses");
        assert_eq!(doc.messages.len(), 2);
        assert_eq!(doc.messages[0].content, "rules");
        assert_eq!(doc.messages[1].content, "ask");
    }
}
