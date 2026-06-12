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
  session [--headless] [log.jsonl]  scripted-LLM demo session
                                    (attached TUI; --headless prints events)";

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
            let mut log_path: Option<String> = None;
            for arg in &args[2..] {
                match arg.as_str() {
                    "--headless" => headless = true,
                    other => log_path = Some(other.to_string()),
                }
            }
            let result = if headless {
                run_session_headless(log_path)
            } else {
                run_session_tui(log_path)
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

/// The attached TUI (9_TUI Step 4) over the scripted demo session —
/// the harness's primary frontend. Type a message to kick it off.
fn run_session_tui(log_path: Option<String>) -> Result<(), String> {
    let tree = open_tree(log_path)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let session = host::Session::new(
        tree,
        host::DEMO_PROMPT,
        serde_json::Value::Null,
        host::demo_registry(),
        Box::new(host::demo_script()),
        tx,
    )
    .map_err(|e| e.to_string())?;
    debug::run_attached(session, rx)
}

/// The headless M0 session: run the scripted demo, printing every
/// `SessionEvent` from the channel — the CLI is just another consumer
/// of the serializable UI boundary.
fn run_session_headless(log_path: Option<String>) -> Result<(), String> {
    let tree = open_tree(log_path)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let printer = std::thread::spawn(move || {
        for event in rx {
            print_session_event(&event);
        }
    });
    let session = host::run_demo(tree, tx).map_err(|e| e.to_string())?;
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
