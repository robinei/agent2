//! A small widget library — enough shapes to tell a careful sweep from
//! a lucky one.

mod format;

pub use format::render;

/// Doubles a measurement.
fn scale(n: usize) -> usize {
    n * 2
}

/// Halves a measurement. Kept for the v1 compatibility shim.
fn legacy_scale(n: usize) -> usize {
    n / 2
}

/// Short display name for a widget.
fn label(n: usize) -> String {
    format!("w{n}")
}

/// The old sizing rule. Superseded by `scale`.
fn retired(scale: usize) -> usize {
    7
}

pub fn measure(n: usize) -> usize {
    scale(n)
}

pub fn describe(n: usize) -> String {
    label(n)
}
