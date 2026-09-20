mod card;
mod compaction;
mod document;
mod host;
mod machine;
mod notebook;
mod report;
// Fixtures and scripted end-to-end runs: test-only, and compiled only
// for `cargo test` now that no binary path reaches them.
mod score;
#[cfg(test)]
mod scripted;
mod tree;
mod types;

// Pass D (`23_ONE_AGENT.md`): the TUI is back in the build, rebuilt on
// the settled vocabulary — one document row per event, addressed by its
// id, rather than the old tool-call block model.
mod debug;

pub use machine::*;
pub use types::*;

use host::SessionEvent;

const USAGE: &str = "usage: agent <command>
  debug <file.js>                   standalone debugger TUI: compile and
                                    step one program under the fuel
                                    slicer, no session, no LLM.
  session [options] [log.jsonl]     agent session (attached TUI by default)
    --headless                      print events instead of the TUI
    --real                          use DeepSeek (needs DEEPSEEK_API_KEY);
                                    the TUI picks it automatically when the
                                    key is set — --headless stays scripted
                                    unless --real is given
    --turn <text>                   queue a first user turn on the
                                    conversation branch, before the TUI
                                    takes over — so a session can start
                                    with nobody there to type one
    --list-leaves                   print the log's leaf set and exit
    --list-branches                 print the log's branch set and exit
    --resume <id>                   open the branch leaf <id> sits on (else
                                    the lowest leaf that owes work)
    --fork <id>                     fork a divergent branch from event <id>
                                    and print its id
    --name <text>                   name the branch (with --fork), else rename
                                    the conversation branch
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
                                    so a before/after is diff or jq.";

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
                run_session_headless(log_path, use_real, nav)
            } else {
                run_session_tui(log_path, use_real, &nav)
            };
            if let Err(e) = result {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Some("document") => {
            if let Err(e) = print_document(args.get(2).map(String::as_str), args.get(3)) {
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
    let doc = document::render(
        &tree,
        &tree.spine_at(leaf),
        64 * 1024,
    );

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
const REAL_PROMPT: &str = "You are a capable general-purpose agent. Solve the user's task; reply with \
     your final answer when done.";

/// Registry + LLM client + agent prompt for a session: the real
/// DeepSeek setup, or the scripted M0 demo.
fn build_brain(
    real: bool,
) -> Result<(host::ToolRegistry, Box<dyn host::LlmClient>, &'static str), String> {
    if real {
        let client = host::DeepSeekClient::from_env()?;
        Ok((host::real_registry(), Box::new(client), REAL_PROMPT))
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
        });
    } else if let Some(name) = nav.name.clone() {
        h.send(host::SessionCommand::Rename { branch, name });
    }
    if let Some(text) = nav.turn.clone() {
        // A kickoff line is a task instruction, not a question, and the
        // agent's reply reaches the client either way (18_TARGETING).
        h.send(host::SessionCommand::UserTurn {
            branch,
            text,
            expects_reply: false,
        });
    }
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
                && matches!(
                    event.payload,
                    types::EventPayload::Reply
                )
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
    } else if real || driven || nav.resume.is_some() {
        let mut session = build_session(log_path, real, nav.resume, tx)?;
        if real && !driven {
            return Err("a real headless session needs --turn <text>".into());
        }
        // Presence is per-request and honest about its limit: a headless
        // run with a queued turn has someone waiting on the other end; one
        // without does not, and its agents are told so.
        session.set_attached(nav.turn.is_some());
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
                EventPayload::Compacted { of, text } => match text {
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
