//! Display formatting for widget names.

#[allow(dead_code)]
fn prefix() -> &'static str {
    "w"
}

#[allow(dead_code)]
fn suffix() -> &'static str {
    "!"
}

#[allow(dead_code)]
fn pad(s: &str) -> String {
    format!(" {s} ")
}

#[allow(dead_code)]
fn trim(s: &str) -> String {
    s.trim().to_owned()
}

pub fn render(n: usize) -> String {
    pad(&format!("{}{n}", prefix()))
}
