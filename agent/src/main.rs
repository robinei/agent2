mod card;
mod compaction;
mod document;
mod host;
mod machine;
mod notebook;
mod report;
// The prompt lab: capture a request, edit it, resample it. A binary
// path reaches this one, so it is not test-gated.
mod lab;
// Fixtures and scripted end-to-end runs: test-only, and compiled only
// for `cargo test` now that no binary path reaches them.
#[cfg(test)]
mod exemplar_gen;
#[cfg(test)]
mod replay;
mod score;
#[cfg(test)]
mod scripted;
#[cfg(test)]
mod testkit;
mod transcript;
mod tree;
mod types;

// Pass D (`23_ONE_AGENT.md`): the TUI is back in the build, rebuilt on
// the settled vocabulary — one document row per event, addressed by its
// id, rather than the old tool-call block model.
mod debug;

pub use machine::*;
pub use types::*;

use host::{BranchId, SessionEvent};

const USAGE: &str = "usage: agent <command>
  debug <file.js>                   standalone debugger TUI: compile and
                                    step one program under the fuel
                                    slicer, no session, no LLM.
  session [options] [log.jsonl]     agent session (attached TUI by default)
    --headless                      print events instead of the TUI
    --stdin                         keep the session alive and take one
                                    turn per line of stdin, printing
                                    `--- quiet` when each exchange
                                    settles. Implies --headless. The
                                    only way to answer a program that
                                    is parked on ask(): its VM lives in
                                    this process, so a turn-per-process
                                    driver arrives after it is gone.
    --real                          use the real provider (needs
                                    AGENT2_API_KEY, or the older
                                    DEEPSEEK_API_KEY); the TUI picks it
                                    automatically when the key is set —
                                    --headless stays scripted unless
                                    --real is given
    --turn <text>                   say this on the conversation branch
                                    before the TUI takes over, so a
                                    session can start — or carry on —
                                    with nobody there to type. If the
                                    branch is waiting on an ask(), this
                                    answers it; otherwise it is a new
                                    turn.
    --list-leaves                   print the log's leaf set and exit
    --list-branches                 print the log's branch set and exit
    --resume <id>                   open the branch leaf <id> sits on (else
                                    the lowest leaf that owes work)
    --fork <id>                     fork a divergent branch from event <id>
                                    and print its id
    --name <text>                   name the branch (with --fork), else rename
                                    the conversation branch
  transcript <log.jsonl> [id]       what happened, for the person who
                                    was not watching: what it said, what
                                    it ran, how it ended, and — the last
                                    line — whether it is waiting on you.
  document <log.jsonl> [id]         print the exact document the log
                                    would be sent as — every message,
                                    role and size. The prompt is
                                    otherwise the one thing you cannot
                                    look at. With an event id, the
                                    document as of that point rather
                                    than the newest leaf: what a reply
                                    was answering, not what the run
                                    ended on.
  score <log.jsonl ...>             fold each finished log into the
                                    numbers a change is argued from:
                                    programs, calls per program, tokens
                                    in and out, reasoning, and the time
                                    inside completions split from the
                                    time everywhere else. JSON per log,
                                    so a before/after is diff or jq.
  login                             sign in to OpenAI with a ChatGPT
                                    subscription (PKCE in the browser).
                                    The token it stores is what a
                                    chatgpt.com/backend-api endpoint
                                    authenticates with.
  capture <log.jsonl> [id] [-o f]   the same document, written so it
                                    reads back byte-identical. Edit any
                                    part of it — the card, one turn, the
                                    ephemeral tail — and sample the
                                    result. Prints to stdout with no -o.
  sample <file> [-n N] [-j C]       ask the provider for the next reply
          [-o out.jsonl]            N times against that fixed document,
                                    one JSON row each: source, thinking,
                                    usage, ms. Nothing runs and nothing
                                    is logged. Holding the context still
                                    is what makes the difference between
                                    two prompts measurable at all.";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("score") => {
            if let Err(e) = score::run_cli(&args[2..]) {
                eprintln!("{e}");
                std::process::exit(2);
            }
        }
        Some("debug") => {
            let Some(path) = args.get(2) else {
                eprintln!("usage: agent debug <file.js>");
                std::process::exit(2);
            };
            if let Err(e) = debug::run(path) {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Some("session") => {
            let mut headless = false;
            let mut real = false;
            let mut stdin_turns = false;
            let mut turn: Option<String> = None;
            let mut log_path: Option<String> = None;
            let mut list_leaves = false;
            let mut list_branches = false;
            let mut resume: Option<u64> = None;
            let mut fork: Option<u64> = None;
            let mut name: Option<String> = None;
            let mut card_dir: Option<String> = None;
            let mut rest = args[2..].iter();
            let next_val = |rest: &mut std::slice::Iter<String>, flag: &str| -> String {
                match rest.next() {
                    Some(v) => v.clone(),
                    None => {
                        eprintln!("{flag} needs a value");
                        std::process::exit(2);
                    }
                }
            };
            let parse_id = |raw: String, flag: &str| -> u64 {
                match raw.trim_start_matches('#').parse::<u64>() {
                    Ok(n) if n > 0 => n,
                    _ => {
                        eprintln!("{flag} needs a positive event id, got `{raw}`");
                        std::process::exit(2);
                    }
                }
            };
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "--headless" => headless = true,
                    "--stdin" => {
                        stdin_turns = true;
                        headless = true;
                    }
                    "--real" => real = true,
                    "--turn" => turn = Some(next_val(&mut rest, "--turn")),
                    "--list-leaves" => list_leaves = true,
                    "--list-branches" => list_branches = true,
                    "--resume" => {
                        resume = Some(parse_id(next_val(&mut rest, "--resume"), "--resume"))
                    }
                    "--fork" => fork = Some(parse_id(next_val(&mut rest, "--fork"), "--fork")),
                    "--name" => name = Some(next_val(&mut rest, "--name")),
                    // Installed before the session opens, because the
                    // system prompt is snapshotted at the agent's root.
                    "--card" => card_dir = Some(next_val(&mut rest, "--card")),
                    other => log_path = Some(other.to_string()),
                }
            }
            if let Some(dir) = &card_dir {
                let card = match card::load_from(std::path::Path::new(dir)) {
                    Ok(card) => card,
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(2);
                    }
                };
                eprintln!(
                    "agent: card from {dir} ({} bytes, {} exemplar(s))",
                    card.text.len(),
                    card.exemplars.len()
                );
                if let Err(e) = card::set_active(card) {
                    eprintln!("{e}");
                    std::process::exit(2);
                }
            }
            // The attached TUI is the M1 driving seat: it picks the
            // real client automatically when a key is present.
            // Headless stays the scripted M0 demo unless --real.
            let use_real = real || (!headless && std::env::var("DEEPSEEK_API_KEY").is_ok());
            let nav = SessionNav {
                list_leaves,
                list_branches,
                resume,
                fork,
                name,
                turn,
            };
            let result = if headless {
                run_session_headless(log_path, use_real, nav, stdin_turns)
            } else {
                run_session_tui(log_path, use_real, &nav)
            };
            if let Err(e) = result {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Some("transcript") => {
            if let Err(e) = transcript::run_cli(&args[2..]) {
                eprintln!("{e}");
                std::process::exit(2);
            }
        }
        Some("document") => {
            if let Err(e) = print_document(args.get(2).map(String::as_str), args.get(3)) {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Some("login") => match host::openai_oauth::login() {
            Ok(_) => {
                let path = host::openai_oauth::token_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                eprintln!("Signed in. Token saved to {path} (mode 0600).");
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
        Some("capture") => {
            if let Err(e) = capture_document(&args[2..]) {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Some("sample") => {
            if let Err(e) = sample_document(&args[2..]) {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

/// Print the exact document a log's newest leaf would be sent as —
/// every message, its role, and its size.
///
/// This exists because the prompt was, for a long time, the one thing
/// nobody could look at. The card was edited a fragment at a time and
/// reasoned about as prose, while the thing the model actually receives
/// — a system message, then N worked-example turns, then the
/// conversation — was never laid out end to end. Two findings came
/// within minutes of finally printing it, both invisible from the
/// source: the exemplars occupied most of the context, and their
/// assistant halves carried no marker at all, so once a provider's chat
/// template flattened the list the model saw them as its own prior
/// output.
///
/// A prompt you cannot read is a prompt you will reason about wrongly.
///
/// The optional `at` names an event id and renders the document as of
/// that point instead of the newest leaf. Without it only the end of a
/// finished run can be looked at, which is the least interesting
/// moment: what a reply was answering is the spine *before* it, and a
/// run that went wrong went wrong in the middle. Reading the document
/// the model held when it wrote a particular block is how the
/// rendering gets checked against the replies it produced.
fn print_document(log: Option<&str>, at: Option<&String>) -> Result<(), String> {
    let path = log.ok_or("usage: agent document <log.jsonl> [event-id]")?;
    let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let tree = types::Tree::open(file).map_err(|e| format!("{path}: {e}"))?;
    let leaf = match at {
        Some(raw) => {
            let n: u64 = raw
                .trim_start_matches('#')
                .parse()
                .map_err(|_| format!("event id must be a positive number, got `{raw}`"))?;
            *tree
                .events
                .keys()
                .find(|id| id.as_u64() == n)
                .ok_or_else(|| format!("no event #{n} in {path}"))?
        }
        None => *tree
            .events
            .keys()
            .max_by_key(|id| id.as_u64())
            .ok_or("the log is empty")?,
    };
    // **With the tail on**, which this printed without for as long as
    // it has existed. The tail is the last thing in the request — it
    // rides the end of the final `User` message and is never logged —
    // so the one tool for reading the prompt was omitting the part in
    // the strongest position, which is exactly the part you cannot
    // recover by reading the log.
    //
    // `attached` is false on a `Runner` rebuilt from a log, so the
    // presence line reads as an unattached request. Everything else
    // is what a real one would carry.
    let spine = tree.spine_at(leaf);
    let state = machine::Runner::with_spine(&tree, tree.spine_at(leaf));
    let doc = document::render(&tree, &spine, 64 * 1024);
    let doc = match state.request_tail(&tree) {
        Some(tail) => doc.with_tail(&tail),
        None => doc,
    };

    // Printed, not refused: reading a broken prompt is how you find out
    // it is broken, so `document` always shows it — and says so first,
    // where it will be read before the 23 KB below it.
    if let Some(why) = document_contradiction(&doc) {
        eprintln!("warning: {why}");
    }
    let total: usize = doc.messages.iter().map(|m| m.content.len()).sum();
    println!("{} messages, {total} bytes\n", doc.messages.len());
    for (i, m) in doc.messages.iter().enumerate() {
        println!(
            "──── [{i}] {:?} · {} bytes {}",
            m.role,
            m.content.len(),
            "─".repeat(28)
        );
        println!("{}\n", m.content);
    }
    Ok(())
}

/// The complaint a self-contradicting document deserves, or `None`.
///
/// **A document whose manifest and whose worked examples disagree is
/// not measuring what it was built to measure.** Three eval documents
/// here were captured from `agent session --headless` logs — the
/// scripted M0 demo, whose registry holds exactly one tool — so each
/// declared `tools.echo` and nothing else, in front of the card's
/// examples calling `tools.bash` and `tools.read_file`. The model
/// noticed, as it should: "the tools available in this session are only
/// `tools.echo`! ... So bash/read_file may not exist". A day of arms was
/// invalidated by a contradiction nothing in the harness was looking
/// for, and which is visible only to someone reading 23 KB of prompt.
///
/// It is checked over the rendered document rather than at the registry
/// because that is where the two halves finally meet: `full_card` sees
/// the registry but the exemplars ride beside it on the `Agent` event,
/// and a narrowed spawn (`spawn(..., { tools: [...] })`) reaches the
/// same state legitimately at runtime. A capture is the point where it
/// stops being a runtime fact and becomes an experiment.
fn document_contradiction(doc: &document::Document) -> Option<String> {
    let system = doc.messages.first()?;
    let examples: Vec<&str> = doc.messages[1..doc.preamble]
        .iter()
        .filter(|m| matches!(m.role, document::ChatRole::Assistant))
        .map(|m| m.content.as_str())
        .collect();
    let missing = card::undeclared_example_tools(&system.content, &examples);
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "this document's worked examples call {} — which its own tool manifest \
         does not declare. A scripted (`agent session` without `--real`) or \
         allowlist-narrowed session declares only the tools it holds, and the \
         card's examples were written against the real registry; the model reads \
         both and spends the turn deciding which to believe. Re-capture from a \
         `--real` log, or narrow the exemplars to match.",
        missing.join(", ")
    ))
}

/// A tiny `--flag value` reader. Not worth a dependency: these two
/// commands have four options between them.
fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// **Capture the request at one point of one session, as an editable
/// file** (see `lab.rs`).
///
/// The same rendering `agent document` prints, written in a form that
/// reads back byte-identical — including the ephemeral tail, which is
/// the part no log holds and the only part that has yet produced a
/// measurable effect.
fn capture_document(args: &[String]) -> Result<(), String> {
    let path = args
        .first()
        .filter(|a| !a.starts_with('-'))
        .ok_or("usage: agent capture <log.jsonl> [event-id] [-o file]")?;
    let at = args.get(1).filter(|a| !a.starts_with('-'));
    let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let tree = types::Tree::open(file).map_err(|e| format!("{path}: {e}"))?;
    let leaf = match at {
        Some(raw) => {
            let n: u64 = raw
                .trim_start_matches('#')
                .parse()
                .map_err(|_| format!("event id must be a positive number, got `{raw}`"))?;
            *tree
                .events
                .keys()
                .find(|id| id.as_u64() == n)
                .ok_or_else(|| format!("no event #{n} in {path}"))?
        }
        None => *tree
            .events
            .keys()
            .max_by_key(|id| id.as_u64())
            .ok_or("the log is empty")?,
    };
    let spine = tree.spine_at(leaf);
    let state = machine::Runner::with_spine(&tree, tree.spine_at(leaf));
    let doc = document::render(&tree, &spine, 64 * 1024);
    let doc = match state.request_tail(&tree) {
        Some(tail) => doc.with_tail(&tail),
        None => doc,
    };
    // Refused, not warned: a capture is written to be measured, often
    // in a batch whose stderr nobody reads until the numbers look odd.
    if let Some(why) = document_contradiction(&doc) {
        return Err(format!("refusing to capture #{}: {why}", leaf.as_u64()));
    }
    let text = lab::write(&doc);
    match opt(args, "-o") {
        Some(out) => {
            std::fs::write(out, &text).map_err(|e| format!("{out}: {e}"))?;
            let bytes: usize = doc.messages.iter().map(|m| m.content.len()).sum();
            eprintln!(
                "{out}: {} messages, {bytes} bytes, at #{}",
                doc.messages.len(),
                leaf.as_u64()
            );
        }
        None => print!("{text}"),
    }
    Ok(())
}

/// **Sample the same request many times and write one line per
/// completion.**
///
/// Nothing is executed and nothing is logged: this asks the provider
/// for the next reply and records it. That is the whole point — the
/// observation is one completion against a fixed context, so the
/// between-run variance that has swamped every task-level A/B here is
/// not merely reduced but absent.
///
/// Errors are written as rows too rather than aborting the batch. A
/// provider that fails one request in forty should cost one sample, and
/// a run that dies at sample 38 having written nothing is how an
/// afternoon gets lost.
fn sample_document(args: &[String]) -> Result<(), String> {
    use host::{Cancel, LlmClient};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let path = args
        .first()
        .filter(|a| !a.starts_with('-'))
        .ok_or("usage: agent sample <file.doc> [-n 40] [-j 4] [-o out.jsonl]")?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let doc = lab::parse(&text).map_err(|e| format!("{path}: {e}"))?;
    let n: usize = opt(args, "-n").unwrap_or("40").parse().map_err(|_| "-n")?;
    let jobs: usize = opt(args, "-j").unwrap_or("4").parse().map_err(|_| "-j")?;
    let jobs = jobs.max(1).min(n.max(1));

    // Whichever provider the environment names; this command has no
    // business knowing which wire format it got.
    let client = host::provider::from_env()?;
    let client = ArcLlm(client);
    let bytes: usize = doc.messages.iter().map(|m| m.content.len()).sum();
    eprintln!(
        "{n} samples, {jobs} at a time — {} messages, {bytes} bytes each  [this bills]",
        doc.messages.len()
    );

    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let out = std::sync::Mutex::new(Vec::<lab::Sample>::with_capacity(n));
    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= n {
                        return;
                    }
                    let started = std::time::Instant::now();
                    let mut thinking = String::new();
                    let cancel = Cancel::new();
                    let turn = client.complete(&doc, &cancel, &mut |chunk| {
                        if let host::LlmChunk::Thinking(t) = chunk {
                            thinking.push_str(&t);
                        }
                    });
                    let ms = started.elapsed().as_millis();
                    let sample = match turn {
                        Ok(t) => lab::Sample {
                            i,
                            ms,
                            source: t.source,
                            // The client reports thinking both ways; the
                            // accumulated chunks are the fallback for a
                            // transport that only streams it.
                            thinking: t.thinking.unwrap_or(thinking),
                            truncated: t.truncated,
                            usage: t.usage,
                            error: None,
                        },
                        Err(e) => lab::Sample {
                            i,
                            ms,
                            source: String::new(),
                            thinking,
                            truncated: false,
                            usage: None,
                            error: Some(e),
                        },
                    };
                    out.lock().expect("samples").push(sample);
                    let d = done.fetch_add(1, Ordering::SeqCst) + 1;
                    eprint!("\r{d}/{n}");
                }
            });
        }
    });
    eprintln!();

    let mut samples = out.into_inner().expect("samples");
    samples.sort_by_key(|s| s.i);
    let failed = samples.iter().filter(|s| s.error.is_some()).count();
    let mut body = String::new();
    for s in &samples {
        body.push_str(&serde_json::to_string(s).map_err(|e| e.to_string())?);
        body.push('\n');
    }
    match opt(args, "-o") {
        Some(dest) => {
            std::fs::write(dest, &body).map_err(|e| format!("{dest}: {e}"))?;
            eprintln!("{dest}: {} samples, {failed} failed", samples.len());
        }
        None => print!("{body}"),
    }
    Ok(())
}

/// The state directory: `$AGENT2_STATE_DIR`, else `$HOME/.agent2`. Trees,
/// and later other durable session state, live under it.
fn state_dir() -> Result<std::path::PathBuf, String> {
    if let Ok(dir) = std::env::var("AGENT2_STATE_DIR") {
        return Ok(std::path::PathBuf::from(dir));
    }
    let home = std::env::var("HOME")
        .map_err(|_| "neither AGENT2_STATE_DIR nor HOME is set".to_string())?;
    Ok(std::path::PathBuf::from(home).join(".agent2"))
}

/// A fresh tree log for a new session: `<state>/trees/<uuid>/tree.jsonl`.
/// The per-session directory leaves room for sidecar artifacts later.
fn new_tree_path() -> Result<std::path::PathBuf, String> {
    let dir = state_dir()?
        .join("trees")
        .join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir.join("tree.jsonl"))
}

/// Open the session log. An explicit path is opened/created in place and
/// resumed; with no path a fresh persistent log is minted under the state
/// directory (the default is durable now, not in-memory) and its location
/// is announced so the session can be resumed later.
fn open_tree(log_path: Option<String>) -> Result<Tree, String> {
    let path = match log_path {
        Some(path) => std::path::PathBuf::from(path),
        None => {
            let path = new_tree_path()?;
            eprintln!("agent: new session log at {}", path.display());
            path
        }
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        // `append`, not plain `write`: with O_APPEND the kernel places
        // every write at the current end of file, so a second process
        // holding the same log cannot land a line inside one this
        // process is writing. Without it both seek to *their* idea of
        // the end — observed 2026-09-16, two sessions on one log, and
        // the result was a truncated event with the next one's JSON
        // beginning inside it, which neither `Tree::open` nor
        // `agent score` could read back.
        //
        // This makes corruption impossible, not concurrency safe: two
        // writers still interleave whole lines and mint colliding ids.
        // An advisory lock is the fix for that, and wants a dependency
        // this crate does not have yet.
        .append(true)
        .create(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Tree::open(file).map_err(|e| format!("{}: {e}", path.display()))
}

/// Open a log without taking a write handle to it — `agent score` reads
/// a finished run, including one still being written by another
/// process, and must never resume or truncate it.
fn open_tree_read_only(path: &str) -> Result<Tree, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    Tree::open(file).map_err(|e| format!("{path}: {e}"))
}

/// Context prompt for real (M1) sessions; the dialect card carries the
/// mechanics, this carries the role.
///
/// **And only the role.** It used to end "reply with your final answer
/// when done", which is chat idiom and contradicts the card twice over
/// in the same clause: there is no final-answer reply here — the
/// answer is a `tell` and the ending is `finish(text)` — and "when done"
/// reads as the name of the function that does something else. It sits
/// last in the system message, after the card and the listing, which is
/// the most recency-weighted position in the prompt; a sentence there
/// that disagrees with 23 KB above it is the sentence that wins.
const REAL_PROMPT: &str = "You are a capable general-purpose agent. Solve the user's task.";

/// Registry + LLM client + agent prompt for a session: the real
/// DeepSeek setup, or the scripted M0 demo.
fn build_brain(
    real: bool,
) -> Result<(host::ToolRegistry, Box<dyn host::LlmClient>, &'static str), String> {
    if real {
        let client = host::provider::from_env()?;
        Ok((host::real_registry(), Box::new(ArcLlm(client)), REAL_PROMPT))
    } else {
        Ok((
            host::demo_registry(),
            Box::new(host::demo_script()),
            host::DEMO_PROMPT,
        ))
    }
}

/// Fork/rename/resume navigation (M4), shared by the CLI front-ends.
struct SessionNav {
    list_leaves: bool,
    list_branches: bool,
    resume: Option<u64>,
    fork: Option<u64>,
    name: Option<String>,
    turn: Option<String>,
}

/// Build a session over the log, anchoring at `--resume <id>` when given
/// (else `Session::new`'s auto-pick).
fn build_session(
    log_path: Option<String>,
    real: bool,
    resume: Option<u64>,
    tx: std::sync::mpsc::Sender<SessionEvent>,
) -> Result<host::Session, String> {
    let tree = open_tree(log_path)?;
    let (registry, llm, prompt) = build_brain(real)?;
    let session = match resume {
        Some(id) => host::Session::open_at(tree, EventId::new(id), registry, llm, tx),
        None => host::Session::new(tree, prompt, registry, llm, tx),
    };
    session.map_err(|e| e.to_string())
}

/// Queue the navigation commands (fork/rename) ahead of an optional
/// first user turn — all FIFO on the one inbox, so order is preserved.
///
/// A fork **adds** a branch and moves nothing, so a `--turn` beside a
/// `--fork` still lands on the conversation branch; the fork's id is
/// printed (`BranchOpened`) for the next invocation to address.
fn queue_nav(session: &host::Session, nav: &SessionNav) {
    let h = session.handle();
    let branch = session.conversation_branch();
    if let Some(from) = nav.fork {
        h.send(host::SessionCommand::Fork {
            from: EventId::new(from),
            name: nav.name.clone(),
            text: None,
        });
    } else if let Some(name) = nav.name.clone() {
        h.send(host::SessionCommand::Rename { branch, name });
    }
    if let Some(text) = nav.turn.clone() {
        submit(
            session,
            turn_target(session.tree(), nav.resume, session.conversation_branch()),
            text,
        );
    }
}

/// Where a `--turn` lands.
///
/// The conversation branch, unless `--resume <id>` named a leaf — then
/// that leaf's branch, because naming one is the whole point of the
/// flag.
///
/// **This is the fix for a silent wrong-branch turn.** `open_at`
/// anchors the *runner* at the named leaf, but `submit` addressed
/// `conversation_branch()`, which is `branches().first()` and so is
/// always the root agent's first branch. A fork of the conversation
/// branch is a second branch of the *same* agent, so
/// `--resume <fork> --turn ...` put the message on the original and
/// reported it there, with nothing to say it had gone somewhere else.
/// There was no way to speak to a fork from the CLI at all.
fn turn_target(tree: &types::Tree, resume: Option<u64>, conversation: BranchId) -> BranchId {
    resume
        .and_then(|id| tree.branch_of(EventId::new(id)))
        .unwrap_or(conversation)
}

/// **One gesture: the person typed something.**
///
/// Whether that is an answer or a new instruction is not theirs to
/// declare — it depends on whether the branch is holding a question
/// open, and the branch is the thing that knows. This is
/// `resolve_submit` (`debug/attach.rs`), which the TUI has always used;
/// without it a headless driver could start a conversation and never
/// continue one, because a `UserTurn` sent to a branch parked on
/// `ask()` leaves the question open forever and the reply it was
/// waiting for never arrives.
///
/// A kickoff line is a task instruction, not a question, and the
/// agent's reply reaches the client either way (18_TARGETING).
fn submit(session: &host::Session, branch: BranchId, text: String) {
    session
        .handle()
        .send(host::SessionCommand::Submit { branch, text });
}

/// The attached TUI (9_TUI Step 4) — the harness's primary frontend.
/// Type a message to kick it off.
/// The attached TUI. Takes the whole [`SessionNav`], not just
/// `--resume`: the navigation commands are queued **before** the TUI
/// takes over, exactly as the headless path does, so `--turn` lands a
/// first user message on the conversation branch with nobody present to
/// type one.
///
/// That is what makes an unattended run possible at all, and the eval's
/// end state depends on it (`23_ONE_AGENT.md`, Pass D): the real TUI,
/// launched with a first message and an answer-as-the-user flag, needs
/// no human. Threading only `resume` here silently dropped `--turn` in
/// TUI mode, which left the documented gesture with no code path.
fn run_session_tui(log_path: Option<String>, real: bool, nav: &SessionNav) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut session = build_session(log_path, real, nav.resume, tx)?;
    // The TUI *is* a client, so it is presence.
    session.set_attached(true);
    queue_nav(&session, nav);
    debug::run_attached(session, rx)
}

/// The headless session: print every `SessionEvent` from the channel —
/// the CLI is just another consumer of the serializable UI boundary.
/// With no navigation flags, scripted runs the M0 demo; otherwise the
/// session is driven by the queued `--list-leaves`/`--fork`/`--name`/
/// `--turn` commands.
fn run_session_headless(
    log_path: Option<String>,
    real: bool,
    nav: SessionNav,
    stdin_turns: bool,
) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    // **Whether anything ever ran**, decided by the printer because it
    // is the one thing that sees every event. A headless `--real` run
    // that produced no `Turn` did not do the job it was started for,
    // and until now it said so only on stderr and then exited 0.
    //
    // Live 2026-09-19: the eval driver reported `agent exited 0 without
    // writing a program` on four separate suites and the runs were read
    // as a harness regression for an hour. The cause was a provider
    // 530 — `Upstream response was not valid JSON` — printed to stderr,
    // which the driver did not capture, under an exit code that said
    // success. Two layers each dropped the one line that explained it.
    let wrote_a_program = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw_a_program = std::sync::Arc::clone(&wrote_a_program);
    let printer = std::thread::spawn(move || {
        for event in rx {
            if let SessionEvent::Event { event, .. } = &event
                && matches!(event.payload, types::EventPayload::Reply)
            {
                saw_a_program.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            print_session_event(&event);
        }
    });

    let listing = nav.list_leaves || nav.list_branches;
    let driven = listing || nav.fork.is_some() || nav.name.is_some() || nav.turn.is_some();
    let session = if listing {
        // Open, ask for the projection, exit — no LLM contact.
        let session = build_session(log_path, real, nav.resume, tx)?;
        if nav.list_leaves {
            session.handle().send(host::SessionCommand::ListLeaves);
        }
        if nav.list_branches {
            session.handle().send(host::SessionCommand::ListBranches);
        }
        session.handle().send(host::SessionCommand::Shutdown);
        session.run()
    } else if stdin_turns {
        // **The session outlives the exchange, and stdin is its own
        // thread.** A program parked on `ask()` holds its VM in this
        // process; a driver that starts a process per turn arrives
        // after that VM is gone, and the answer lands as a settlement
        // nobody was waiting for.
        //
        // The reader is a thread rather than a step between runs so a
        // line typed *during* a run reaches the branch at its next fuel
        // slice (rule B) instead of after the run finishes — which is
        // what a person at the TUI gets, and the only way to interrupt
        // from out here.
        let session = build_session(log_path, real, nav.resume, tx)?;
        // Someone is typing, by construction.
        let mut session = session;
        session.set_attached(true);
        queue_nav(&session, &nav);
        let handle = session.handle();
        let branch = session.conversation_branch();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            for line in stdin.lock().lines().map_while(Result::ok) {
                let text = line.trim();
                if text.is_empty() {
                    continue;
                }
                handle.send(host::SessionCommand::Submit {
                    branch,
                    text: text.to_owned(),
                });
            }
            handle.send(host::SessionCommand::Shutdown);
        });
        session.serve(|| {
            // The exchange is over. A driver reads until this line,
            // looks at the log, and decides what to say next.
            use std::io::Write;
            println!("--- quiet");
            let _ = std::io::stdout().flush();
        })
    } else if real || driven || nav.resume.is_some() {
        let mut session = build_session(log_path, real, nav.resume, tx)?;
        if real && !driven {
            return Err("a real headless session needs --turn <text>".into());
        }
        // **Nobody is attached to a one-shot run, whoever started it.**
        // This said `nav.turn.is_some()` — a queued turn means someone
        // is waiting on the other end — and that is true of a person
        // watching and false of every driver, which is what actually
        // runs this shape. The process exits the moment the branch
        // settles, so an `ask()` here can never be answered in this
        // session: answering means a *new* process, which arrives after
        // the VM is gone and lands as a settlement nobody was waiting
        // for. `--stdin` is the mode where someone really is typing,
        // and it sets this itself.
        //
        // Live on 2026-09-25: `dead-code-sweep` did careful work — a
        // scratch copy, every allow stripped, `cargo check` as the
        // judge — then asked which way to take one judgement call. The
        // tail had told it "a client is attached; an ask() may be
        // answered promptly". The suite scored the run as a failure,
        // because the edit it was holding back never happened.
        //
        // The two mistakes are not the same size. Claiming presence
        // that is not there parks a run and kills it; denying presence
        // that is there costs a reply handed back in prose, which the
        // person then reads.
        session.set_attached(false);
        queue_nav(&session, &nav);
        session.run()
    } else {
        host::run_demo(open_tree(log_path)?, tx).map_err(|e| e.to_string())?
    };
    drop(session); // closes the event channel; the printer drains and exits
    printer.join().map_err(|_| "printer thread panicked")?;
    // Only a `--real` run is held to this. A listing or a demo run is
    // *supposed* to write no program, and `--turn`-less navigation has
    // no completion to wait for.
    if real && !listing && !wrote_a_program.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(
            "no program was ever written — the run reached no completion. The reason is on \
             stderr above, if the provider gave one."
                .to_owned(),
        );
    }
    Ok(())
}

fn print_session_event(event: &SessionEvent) {
    match event {
        // Chunks stream live; the headless printer shows the logged
        // message instead of interleaving partial text.
        SessionEvent::Chunk { .. } => {}
        SessionEvent::ProgramStatus {
            branch,
            program,
            status,
            ..
        } => {
            println!(
                "[branch {} · #{}] program status: {status:?}",
                branch.as_u64(),
                program.as_u64(),
            );
        }
        SessionEvent::Error { branch, message } => match branch {
            Some(b) => eprintln!("!! [branch {}] {message}", b.as_u64()),
            None => eprintln!("!! {message}"),
        },
        SessionEvent::BranchOpened { branch } => {
            println!("branch #{} is live", branch.as_u64());
        }
        SessionEvent::Branches(branches) => {
            println!("branches ({}):", branches.len());
            for b in branches {
                let name = b
                    .name
                    .as_deref()
                    .map(|n| format!(" «{n}»"))
                    .unwrap_or_default();
                let asking = b
                    .asking_user
                    .map(|q| format!(" · asking you #{}", q.as_u64()))
                    .unwrap_or_default();
                println!(
                    "  #{}{name} [agent {} · leaf #{} · {} · {} open{asking}]",
                    b.branch.as_u64(),
                    b.agent.as_u64(),
                    b.leaf.as_u64(),
                    b.status,
                    b.open,
                );
            }
        }
        SessionEvent::Answered {
            branch,
            question,
            value,
            ..
        } => {
            println!(
                "[branch {}] answered #{}: {value}",
                branch.as_u64(),
                question.as_u64()
            );
        }
        SessionEvent::Leaves(leaves) => {
            println!("leaves ({}):", leaves.len());
            for leaf in leaves {
                let state = match leaf.open {
                    0 => "idle".to_owned(),
                    n => format!("{n} open"),
                };
                let name = leaf
                    .name
                    .as_deref()
                    .map(|n| format!(" «{n}»"))
                    .unwrap_or_default();
                println!(
                    "  #{} [agent {} · {state}]{name}  {}",
                    leaf.leaf.as_u64(),
                    leaf.agent.as_u64(),
                    leaf.summary,
                );
            }
        }
        SessionEvent::Event { branch, event, .. } => {
            let head = format!("[branch {} · #{}]", branch.as_u64(), event.id.as_u64());
            match &event.payload {
                EventPayload::Agent { name, charter, .. } => {
                    let name = name
                        .as_deref()
                        .map(|n| format!(" «{n}»"))
                        .unwrap_or_default();
                    println!("{head} agent{name}: {charter}");
                }
                EventPayload::RequestFailed { message } => {
                    println!("{head} request failed: {message}");
                }
                EventPayload::Render { of, mode, .. } => {
                    println!("{head} {mode:?} #{}", of.as_u64());
                }
                EventPayload::Fork { name } => {
                    let name = name
                        .as_deref()
                        .map(|n| format!(" «{n}»"))
                        .unwrap_or_default();
                    println!("{head} fork{name}");
                }
                EventPayload::Answer { question, value } => {
                    println!("{head} answer to #{}: {value}", question.as_u64());
                }
                EventPayload::Post { from, origin } => {
                    println!(
                        "{head} post: {}",
                        report::render_post(event.id, *from, origin)
                    );
                }
                // The whole turn *is* a program now — no separate prose
                // channel and no tool-call list beside it (23_ONE_AGENT.md's
                // substitution table).
                EventPayload::Reply => println!("{head} reply"),
                EventPayload::Compaction {
                    measured,
                    limit,
                    unit,
                } => {
                    println!("{head} compaction: {measured} of {limit} {}s", unit.noun())
                }
                EventPayload::Restart => println!("{head} restart"),
                EventPayload::Part { part, .. } => match part {
                    types::Part::Thinking(t) => println!("{head} thinking: {} bytes", t.len()),
                    types::Part::Prose(t) => println!("{head} prose: {t}"),
                    types::Part::Cell(t) => println!("{head} cell: {t}"),
                },
                EventPayload::ReplyEnd { how, usage, .. } => {
                    println!("{head} reply end: {how:?}, {} out", usage.completion)
                }
                // `wait_until` is exempt: a polling loop calls it repeatedly
                // for no reason worth printing, and it never has interesting
                // output. The log itself still records it in full — only
                // this printer's view is thinned. Its `Result` line still
                // prints on its own; not worth tracking ids to suppress that
                // too.
                EventPayload::Call(call) => {
                    if !matches!(call, Call::Invoke { name, .. } if name.as_str() == "wait_until") {
                        println!("{head} call: {}", describe_call(call));
                    }
                }
                EventPayload::Result { call, outcome } => match outcome {
                    Outcome::Delivered(v) => {
                        println!("{head} result of #{}: {v}", call.as_u64())
                    }
                    Outcome::Failed(msg) => {
                        println!("{head} result of #{}: failed: {msg}", call.as_u64())
                    }
                },
                EventPayload::Handback { how, site, .. } => {
                    println!("{head} handback at {site}: {how:?}");
                }
                EventPayload::Rename { name } => println!("{head} rename: {name}"),
                EventPayload::Console { lines } => {
                    println!("{head} console: {} lines", lines.len());
                }
                EventPayload::Note { value, .. } => {
                    println!("{head} note: {}", crate::machine::note_text(value))
                }
                EventPayload::Compacted { of, text, .. } => match text {
                    Some(t) => println!("{head} compacted #{}: {t}", of.as_u64()),
                    None => println!("{head} compacted #{}: removed", of.as_u64()),
                },
            }
        }
    }
}

/// One-line rendering of a logged call for the headless printer.
fn describe_call(call: &Call) -> String {
    match call {
        Call::Send {
            to,
            text,
            expects_reply,
            options,
            ..
        } => {
            let verb = match (*expects_reply, options.is_empty()) {
                (false, _) => "tell",
                (true, true) => "ask",
                (true, false) => "choose",
            };
            let to = match to {
                Address::User => "user".to_owned(),
                Address::Branch(id) => format!("#{}", id.as_u64()),
            };
            let offered = if options.is_empty() {
                String::new()
            } else {
                format!(" [{}]", options.join(" / "))
            };
            format!("{verb} {to}: {text}{offered}")
        }
        Call::Spawn { name, charter, .. } => {
            format!(
                "spawn {}: {charter}",
                name.as_deref().unwrap_or("<unnamed>")
            )
        }
        Call::Fork { name, .. } => {
            format!("fork {}", name.as_deref().unwrap_or("<unnamed>"))
        }
        Call::Invoke { name, args, .. } => format!("invoke {name}({args})"),
    }
}

/// `Arc<dyn LlmClient>` where a `Box` is wanted.
///
/// `host::provider::from_env` hands back an `Arc` because the session
/// loop shares one client across worker threads; the two callers that
/// want a `Box` get this rather than a second constructor per provider.
struct ArcLlm(std::sync::Arc<dyn host::LlmClient>);

impl host::LlmClient for ArcLlm {
    fn complete(
        &self,
        request: &document::Document,
        cancel: &host::Cancel,
        chunk: &mut dyn FnMut(host::LlmChunk),
    ) -> Result<machine::LlmTurn, String> {
        self.0.complete(request, cancel, chunk)
    }
}

#[cfg(test)]
mod turn_target_tests {
    use super::*;

    fn post(text: &str) -> EventPayload {
        EventPayload::Post {
            from: Author::User,
            origin: Origin::Direct {
                text: text.into(),
                input: serde_json::json!(null),
                options: Vec::new(),
                expects_reply: true,
            },
        }
    }

    /// **`--resume` names a branch; `--turn` must honour it.**
    ///
    /// A fork of the conversation branch belongs to the *same* agent, so
    /// `conversation_branch()` — `branches().first()` — returns the
    /// original for both. Addressing a turn that way put the message on
    /// the branch the person had just navigated away from, and said
    /// nothing about it. There was no way to speak to a fork from the
    /// CLI at all.
    #[test]
    fn a_turn_beside_resume_lands_on_the_resumed_branch() {
        let mut tree = types::Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "", Vec::new())
            .unwrap();
        let at = tree.append(&mut spine, post("q")).unwrap();
        let conversation = tree.branch_of(at).expect("the root branch");

        // A branch root is an `Agent` or a `Fork` event, so the fork
        // event itself is what makes the new spine a branch — exactly
        // what `SessionCommand::Fork` appends.
        let mut other = tree.fork(at).unwrap();
        tree.append(
            &mut other,
            EventPayload::Fork {
                name: Some("untold".into()),
            },
        )
        .unwrap();
        let forked = tree.append(&mut other, post("f")).unwrap();
        let fork_branch = tree.branch_of(forked).expect("the fork's branch");
        assert_ne!(fork_branch, conversation, "the fork is its own branch");

        assert_eq!(
            turn_target(&tree, Some(forked.as_u64()), conversation),
            fork_branch,
            "--resume <fork> --turn must address the fork"
        );
        // No --resume: the conversation branch, as it always was.
        assert_eq!(turn_target(&tree, None, conversation), conversation);
        // An id that is not in the log falls back rather than panicking.
        assert_eq!(turn_target(&tree, Some(9999), conversation), conversation);
    }
}

#[cfg(test)]
mod document_contradiction_tests {
    use super::*;
    use document::{ChatMessage, ChatRole, Document};

    fn msg(role: ChatRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_owned(),
            call: None,
            result_for: None,
            thinking: None,
        }
    }

    fn doc(system: &str, example: &str) -> Document {
        let messages = vec![
            msg(ChatRole::System, system),
            msg(ChatRole::User, "a task"),
            msg(ChatRole::Assistant, example),
        ];
        Document {
            preamble: messages.len(),
            messages,
        }
    }

    /// **The scripted registry's document, which is the one that got
    /// shipped.** `agent session` without `--real` is the M0 demo: one
    /// tool, `echo`. Capturing from such a log produced three eval
    /// documents whose manifest declared `tools.echo` and whose worked
    /// examples called `tools.bash` and `tools.read_file`, and the
    /// model spent the turn litigating which half to believe rather
    /// than doing the task. Nothing in the harness was looking.
    #[test]
    fn a_scripted_registrys_document_is_refused() {
        let scripted = "card prose\n\ndeclare namespace tools {\n  \
                        function echo(v: unknown): Promise<unknown>;\n}\n";
        let why = document_contradiction(&doc(scripted, "const r = await tools.bash('ls');"))
            .expect("the contradiction is the whole point");
        assert!(why.contains("bash"), "{why}");

        // And the same document with the tool declared is fine — this
        // must not fire on every capture, or it will be turned off.
        let full = "card prose\n\ndeclare namespace tools {\n  \
                    function echo(v: unknown): Promise<unknown>;\n  \
                    function bash(cmd: string): Promise<unknown>;\n}\n";
        assert!(
            document_contradiction(&doc(full, "const r = await tools.bash('ls');")).is_none(),
            "a declared tool must not be reported"
        );
    }

    /// Only the preamble is examined. What the *conversation* called is
    /// history, not a worked example, and a tool withdrawn mid-session
    /// (or a branch narrowed after the fact) would otherwise make every
    /// capture of a finished run unopenable.
    #[test]
    fn a_call_in_the_conversation_is_not_a_worked_example() {
        let scripted = "card prose\n\ndeclare namespace tools {\n  \
                        function echo(v: unknown): Promise<unknown>;\n}\n";
        let mut d = doc(scripted, "await tools.echo(1);");
        d.messages
            .push(msg(ChatRole::Assistant, "await tools.bash('ls');"));
        assert!(document_contradiction(&d).is_none(), "{:?}", d.messages);
    }
}
