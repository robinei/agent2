//! Display formatting for widget names.

fn prefix() -> &'static str {
    "w"
}

fn suffix() -> &'static str {
    "!"
}

fn pad(s: &str) -> String {
    format!(" {s} ")
}

fn trim(s: &str) -> String {
    s.trim().to_owned()
}

pub fn render(n: usize) -> String {
    pad(&format!("{}{n}", prefix()))
}
