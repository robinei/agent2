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
    --turn <text>                   queue a first user turn (headless)";

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
            let mut rest = args[2..].iter();
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "--headless" => headless = true,
                    "--real" => real = true,
                    "--turn" => match rest.next() {
                        Some(text) => turn = Some(text.clone()),
                        None => {
                            eprintln!("--turn needs a message");
                            std::process::exit(2);
                        }
                    },
                    other => log_path = Some(other.to_string()),
                }
            }
            // The attached TUI is the M1 driving seat: it picks the
            // real client automatically when a key is present.
            // Headless stays the scripted M0 demo unless --real.
            let use_real = real || (!headless && std::env::var("DEEPSEEK_API_KEY").is_ok());
            let result = if headless {
                run_session_headless(log_path, use_real, turn)
            } else {
                run_session_tui(log_path, use_real)
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

fn open_tree(log_path: Option<String>) -> Result<Tree, String> {
    match log_path {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false) // an existing log is resumed, not wiped
                .open(&path)
                .map_err(|e| format!("{path}: {e}"))?;
            Tree::open(file).map_err(|e| format!("{path}: {e}"))
        }
        None => Ok(Tree::new(None)),
    }
}

/// Frame prompt for real (M1) sessions; the dialect card carries the
/// mechanics, this carries the role.
const REAL_PROMPT: &str = "You are a capable general-purpose agent. Solve the user's task; reply with \
     your final answer when done.";

/// Registry + LLM client + frame prompt for a session: the real
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

/// The attached TUI (9_TUI Step 4) — the harness's primary frontend.
/// Type a message to kick it off.
fn run_session_tui(log_path: Option<String>, real: bool) -> Result<(), String> {
    let tree = open_tree(log_path)?;
    let (registry, llm, prompt) = build_brain(real)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let session = host::Session::new(tree, prompt, serde_json::Value::Null, registry, llm, tx)
        .map_err(|e| e.to_string())?;
    debug::run_attached(session, rx)
}

/// The headless session: print every `SessionEvent` from the channel —
/// the CLI is just another consumer of the serializable UI boundary.
/// Scripted (default) runs the M0 demo; `--real --turn <text>` drives
/// one real conversation to completion.
fn run_session_headless(
    log_path: Option<String>,
    real: bool,
    turn: Option<String>,
) -> Result<(), String> {
    let tree = open_tree(log_path)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let printer = std::thread::spawn(move || {
        for event in rx {
            print_session_event(&event);
        }
    });
    let session = if real {
        let (registry, llm, prompt) = build_brain(true)?;
        let session = host::Session::new(tree, prompt, serde_json::Value::Null, registry, llm, tx)
            .map_err(|e| e.to_string())?;
        let turn = turn.ok_or("a real headless session needs --turn <text>")?;
        session.handle().send(host::SessionCommand::UserTurn(turn));
        session.run()
    } else {
        host::run_demo(tree, tx).map_err(|e| e.to_string())?
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
        SessionEvent::Error { frame, message } => match frame {
            Some(f) => eprintln!("!! [frame {}] {message}", f.as_u64()),
            None => eprintln!("!! {message}"),
        },
        SessionEvent::Leaves(leaves) => {
            println!("leaves ({}):", leaves.len());
            for leaf in leaves {
                let mark = if leaf.active { "*" } else { " " };
                let state = if leaf.complete { "done" } else { "open" };
                let label = leaf
                    .label
                    .as_deref()
                    .map(|l| format!(" «{l}»"))
                    .unwrap_or_default();
                println!(
                    "  {mark} #{} [frame {} · {state}]{label}  {}",
                    leaf.leaf.as_u64(),
                    leaf.frame.as_u64(),
                    leaf.summary,
                );
            }
        }
        SessionEvent::Event { frame, event } => {
            let head = format!("[frame {} · #{}]", frame.as_u64(), event.id.as_u64());
            match &event.payload {
                EventPayload::FrameStart { prompt, input } => {
                    println!("{head} frame start: {prompt} (input: {input})");
                }
                EventPayload::FrameResult { result } => {
                    println!("{head} frame result: {result}");
                }
                EventPayload::Message(Message::User { text }) => {
                    println!("{head} user: {text}");
                }
                EventPayload::Message(Message::Assistant {
                    text, tool_calls, ..
                }) => {
                    let calls: Vec<String> = tool_calls
                        .iter()
                        .map(|c| format!("⚙ {}({})", c.name, c.arguments))
                        .collect();
                    println!("{head} assistant: {}{}", text, calls.join(" "));
                }
                EventPayload::Message(Message::System { text }) => {
                    println!("{head} system: {text}");
                }
                EventPayload::Message(Message::Tool { name, text, .. }) => {
                    println!("{head} tool result ({name}):");
                    for line in text.lines() {
                        println!("    {line}");
                    }
                }
                EventPayload::Invoke { name, args, result } => {
                    println!("{head} invoke: {name}({args}) → {result}");
                }
                EventPayload::ProgramResult { value } => {
                    println!("{head} program result: {value}");
                }
                EventPayload::Label(label) => println!("{head} label: {label}"),
                EventPayload::TextChunk(_) | EventPayload::ThinkingChunk(_) => {}
            }
        }
    }
}
