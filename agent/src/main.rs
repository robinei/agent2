mod debug;
mod host;
mod machine;
mod tree;
mod types;

pub use machine::*;
pub use types::*;

use host::SessionEvent;

const USAGE: &str = "usage: agent <command>
  debug <file.js>                 standalone debugger TUI
  session [--headless] [log.jsonl]  M0 scripted-LLM demo session";

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
            let mut log_path: Option<String> = None;
            for arg in &args[2..] {
                match arg.as_str() {
                    // The attached TUI becomes the default with 9_TUI
                    // Step 4; today both forms run headless.
                    "--headless" => {}
                    other => log_path = Some(other.to_string()),
                }
            }
            if let Err(e) = run_session(log_path) {
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

/// The headless M0 session: run the scripted demo, printing every
/// `SessionEvent` from the channel — the CLI is just another consumer
/// of the serializable UI boundary.
fn run_session(log_path: Option<String>) -> Result<(), String> {
    let tree = match log_path {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false) // an existing log is resumed, not wiped
                .open(&path)
                .map_err(|e| format!("{path}: {e}"))?;
            Tree::open(file).map_err(|e| format!("{path}: {e}"))?
        }
        None => Tree::new(None),
    };

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
