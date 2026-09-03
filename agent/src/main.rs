mod debug;
mod host;
mod machine;
mod report;
mod tree;
mod types;

pub use machine::*;
pub use types::*;

use host::SessionEvent;

const USAGE: &str = "usage: agent <command>
  debug <file.js>                   standalone debugger TUI
  session [options] [log.jsonl]     agent session (attached TUI by default)
    --headless                      print events instead of the TUI
    --real                          use DeepSeek (needs DEEPSEEK_API_KEY);
                                    the TUI picks it automatically when the
                                    key is set — --headless stays scripted
                                    unless --real is given
    --turn <text>                   queue a first user turn (headless)
    --list-leaves                   print the log's leaf set and exit
    --resume <id>                   open anchored at leaf <id> (else the
                                    lowest incomplete leaf)
    --fork <id>                     fork a divergent branch from event <id>
    --name <text>                   name the branch (with --fork), else rename
                                    the active leaf";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
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
            let mut resume: Option<u64> = None;
            let mut fork: Option<u64> = None;
            let mut name: Option<String> = None;
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
                    "--resume" => {
                        resume = Some(parse_id(next_val(&mut rest, "--resume"), "--resume"))
                    }
                    "--fork" => fork = Some(parse_id(next_val(&mut rest, "--fork"), "--fork")),
                    "--name" => name = Some(next_val(&mut rest, "--name")),
                    other => log_path = Some(other.to_string()),
                }
            }
            // The attached TUI is the M1 driving seat: it picks the
            // real client automatically when a key is present.
            // Headless stays the scripted M0 demo unless --real.
            let use_real = real || (!headless && std::env::var("DEEPSEEK_API_KEY").is_ok());
            let nav = SessionNav {
                list_leaves,
                resume,
                fork,
                name,
                turn,
            };
            let result = if headless {
                run_session_headless(log_path, use_real, nav)
            } else {
                run_session_tui(log_path, use_real, nav.resume)
            };
            if let Err(e) = result {
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
        .write(true)
        .create(true)
        .truncate(false) // an existing log is resumed, not wiped
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Tree::open(file).map_err(|e| format!("{}: {e}", path.display()))
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

/// Queue the M4 navigation commands (fork/rename) ahead of an optional
/// first user turn — all FIFO on the one inbox, so order is preserved.
fn queue_nav(session: &host::Session, nav: &SessionNav) {
    let h = session.handle();
    if let Some(from) = nav.fork {
        h.send(host::SessionCommand::Fork {
            from: EventId::new(from),
            name: nav.name.clone(),
        });
    } else if let Some(text) = nav.name.clone() {
        h.send(host::SessionCommand::Rename(text));
    }
    if let Some(text) = nav.turn.clone() {
        h.send(host::SessionCommand::UserTurn(text));
    }
}

/// The attached TUI (9_TUI Step 4) — the harness's primary frontend.
/// Type a message to kick it off.
fn run_session_tui(
    log_path: Option<String>,
    real: bool,
    resume: Option<u64>,
) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let session = build_session(log_path, real, resume, tx)?;
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
    let printer = std::thread::spawn(move || {
        for event in rx {
            print_session_event(&event);
        }
    });

    let driven = nav.list_leaves || nav.fork.is_some() || nav.name.is_some() || nav.turn.is_some();
    let session = if nav.list_leaves {
        // Open, ask for the leaf set, exit — no LLM contact.
        let session = build_session(log_path, real, nav.resume, tx)?;
        session.handle().send(host::SessionCommand::ListLeaves);
        session.handle().send(host::SessionCommand::Shutdown);
        session.run()
    } else if real || driven || nav.resume.is_some() {
        let session = build_session(log_path, real, nav.resume, tx)?;
        if real && !driven {
            return Err("a real headless session needs --turn <text>".into());
        }
        queue_nav(&session, &nav);
        session.run()
    } else {
        host::run_demo(open_tree(log_path)?, tx).map_err(|e| e.to_string())?
    };
    drop(session); // closes the event channel; the printer drains and exits
    printer.join().map_err(|_| "printer thread panicked")?;
    Ok(())
}

fn print_session_event(event: &SessionEvent) {
    match event {
        // Chunks stream live; the headless printer shows the logged
        // message instead of interleaving partial text.
        SessionEvent::Chunk { .. } => {}
        SessionEvent::ProgramStatus {
            agent,
            program,
            status,
        } => {
            println!(
                "[agent {} · #{}] program status: {status:?}",
                agent.as_u64(),
                program.as_u64(),
            );
        }
        SessionEvent::Error { agent, message } => match agent {
            Some(f) => eprintln!("!! [agent {}] {message}", f.as_u64()),
            None => eprintln!("!! {message}"),
        },
        SessionEvent::Leaves(leaves) => {
            println!("leaves ({}):", leaves.len());
            for leaf in leaves {
                let mark = if leaf.active { "*" } else { " " };
                let state = if leaf.complete { "done" } else { "open" };
                let name = leaf
                    .name
                    .as_deref()
                    .map(|n| format!(" «{n}»"))
                    .unwrap_or_default();
                println!(
                    "  {mark} #{} [agent {} · {state}]{name}  {}",
                    leaf.leaf.as_u64(),
                    leaf.agent.as_u64(),
                    leaf.summary,
                );
            }
        }
        SessionEvent::Event { agent, event } => {
            let head = format!("[agent {} · #{}]", agent.as_u64(), event.id.as_u64());
            match &event.payload {
                EventPayload::Agent { name, charter, .. } => {
                    let name = name
                        .as_deref()
                        .map(|n| format!(" «{n}»"))
                        .unwrap_or_default();
                    println!("{head} agent{name}: {charter}");
                }
                EventPayload::FrameResult { result } => {
                    println!("{head} agent result: {result}");
                }
                EventPayload::Message(Message::Post { from, origin }) => {
                    println!("{head} post: {}", report::render_post(*from, origin));
                }
                EventPayload::Message(Message::Turn {
                    text, tool_calls, ..
                }) => {
                    let calls: Vec<String> = tool_calls
                        .iter()
                        .map(|c| format!("⚙ {}({})", c.name, c.arguments))
                        .collect();
                    println!("{head} turn: {}{}", text, calls.join(" "));
                }
                EventPayload::Message(Message::Tool { name, text, .. }) => {
                    println!("{head} tool result ({name}):");
                    for line in text.lines() {
                        println!("    {line}");
                    }
                }
                EventPayload::Call(call) => {
                    println!("{head} call: {}", describe_call(call));
                }
                EventPayload::Result { call, outcome } => match outcome {
                    Outcome::Delivered(v) => {
                        println!("{head} result of #{}: {v}", call.as_u64())
                    }
                    Outcome::Failed(msg) => {
                        println!("{head} result of #{}: failed: {msg}", call.as_u64())
                    }
                },
                EventPayload::ProgramResult { value } => {
                    println!("{head} program result: {value}");
                }
                EventPayload::Rename { name } => println!("{head} rename: {name}"),
                EventPayload::Console { lines } => {
                    println!("{head} console: {} lines", lines.len());
                }
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
            ..
        } => {
            let verb = if *expects_reply { "ask" } else { "tell" };
            let to = match to {
                Address::User => "user".to_owned(),
                Address::Branch(id) => format!("#{}", id.as_u64()),
            };
            format!("{verb} {to}: {text}")
        }
        Call::Spawn { name, charter, .. } => {
            format!(
                "spawn {}: {charter}",
                name.as_deref().unwrap_or("<unnamed>")
            )
        }
        Call::Invoke { name, args, .. } => format!("invoke {name}({args})"),
    }
}
