//! Inline markdown → styled spans for one chat row (`**bold**`/
//! `__bold__`, `*italic*`/`_italic_`, `~~strikethrough~~`, and
//! `` `inline code` ``), plus a style-aware word-wrap to lay the result
//! out at a pane width. Ported from the predecessor project's
//! `agent-cli/src/markdown.rs`, scoped down to inline emphasis only:
//! `chat.rs` already splits a multi-line message into one row per
//! source line before a row ever reaches here, so there is no
//! multi-line context left to recognize a fenced code block or a table
//! from — a block-aware pass would have to move upstream into
//! `chat.rs`'s row model instead, not live here.
//!
//! Every span is built by patching modifiers/color onto the row's own
//! `base` style (never `Style::default()`), so emphasis composes with
//! whatever the row already carries — the even/odd alternation, a
//! selection's `REVERSED`, and so on — instead of overwriting it.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Bold,
    Italic,
    Strikethrough,
}

impl Format {
    fn open_marker(self) -> &'static str {
        match self {
            Format::Bold => "**",
            Format::Italic => "*",
            Format::Strikethrough => "~~",
        }
    }

    fn modifier(self) -> Modifier {
        match self {
            Format::Bold => Modifier::BOLD,
            Format::Italic => Modifier::ITALIC,
            Format::Strikethrough => Modifier::CROSSED_OUT,
        }
    }
}

struct Frame {
    kind: Format,
    /// The characters that opened this frame, for the end-of-line
    /// recovery. `Format::open_marker` can only guess `*`, and guessing
    /// puts a character in front of the person that the model never
    /// wrote — which for `_italic_` is the same class of mistake as
    /// eating the underscores of a snake_case name.
    marker: String,
    buf: String,
}

enum State {
    Normal,
    /// A run of `*`/`_`. `underscore` is true only while every character
    /// of the run has been `_`, and `after_word` records whether the
    /// character before the run was part of a word — together they are
    /// what [`Parser::feed`] needs to apply CommonMark's intraword rule.
    Stars {
        count: usize,
        underscore: bool,
        after_word: bool,
    },
    Code(String),
    Tildes(usize),
}

/// What counts as "inside a word" for the intraword-underscore rule.
/// CommonMark words it as flanking by a non-punctuation, non-whitespace
/// character; alphanumeric is that, for the identifiers this is here to
/// protect.
fn is_word(c: char) -> bool {
    c.is_alphanumeric()
}

struct Parser {
    state: State,
    formats: Vec<Frame>,
    spans: Vec<Span<'static>>,
    segment: String,
    base: Style,
    /// The character fed before this one, so a `_` can tell whether it
    /// sits inside a word. `None` at the start of the line.
    prev: Option<char>,
}

impl Parser {
    fn new(base: Style) -> Self {
        Parser {
            state: State::Normal,
            formats: Vec::new(),
            spans: Vec::new(),
            segment: String::new(),
            base,
            prev: None,
        }
    }

    fn push_char(&mut self, c: char) {
        match self.formats.last_mut() {
            Some(f) => f.buf.push(c),
            None => self.segment.push(c),
        }
    }

    fn flush_segment(&mut self) {
        if !self.segment.is_empty() {
            let s = std::mem::take(&mut self.segment);
            self.spans.push(Span::styled(s, self.base));
        }
    }

    fn active_modifiers(&self) -> Modifier {
        self.formats
            .iter()
            .fold(Modifier::empty(), |m, f| m | f.kind.modifier())
    }

    fn text_style(&self) -> Style {
        self.base.add_modifier(self.active_modifiers())
    }

    /// Cyan, like the predecessor's own inline-code color — kept distinct
    /// from the row's own foreground so code stands out inside colored
    /// prose too, while still inheriting active emphasis and any
    /// selection `REVERSED`.
    fn code_style(&self) -> Style {
        self.base
            .patch(Style::new().fg(Color::Cyan))
            .add_modifier(self.active_modifiers())
    }

    fn open_format(&mut self, kind: Format) {
        self.open_format_with(kind, kind.open_marker().to_owned());
    }

    fn open_format_with(&mut self, kind: Format, marker: String) {
        if let Some(parent) = self.formats.last_mut() {
            let text = std::mem::take(&mut parent.buf);
            if !text.is_empty() {
                let style = self.text_style();
                self.spans.push(Span::styled(text, style));
            }
        }
        self.flush_segment();
        self.formats.push(Frame {
            kind,
            marker,
            buf: String::new(),
        });
    }

    fn try_close_format(&mut self, kind: Format) -> bool {
        if self.formats.last().map(|f| f.kind == kind) != Some(true) {
            return false;
        }
        let style = self.text_style();
        let frame = self.formats.pop().unwrap();
        if !frame.buf.is_empty() {
            self.spans.push(Span::styled(frame.buf, style));
        }
        true
    }

    /// Close matching formats from the innermost out, then open whatever
    /// count is left — a run of `*`/`_` is a toggle, not an open or a
    /// close by itself, so which one it means depends entirely on what's
    /// already active.
    fn resolve_stars(&mut self, count: usize, underscore: bool) {
        let marker = if underscore { '_' } else { '*' };
        let mut n = count;
        if n % 2 == 1 && self.formats.last().map(|f| f.kind == Format::Italic) == Some(true) {
            self.try_close_format(Format::Italic);
            n -= 1;
        }
        while n >= 2 {
            if self.try_close_format(Format::Bold) {
                n -= 2;
            } else if self.formats.len() >= 2
                && self.formats.last().map(|f| f.kind == Format::Italic) == Some(true)
                && self.formats[self.formats.len() - 2].kind == Format::Bold
            {
                // `**bold *text**`: the inner `*` never found its own
                // closer, so fold it back into the Bold frame as literal
                // text instead of losing the run.
                let italic = self.formats.pop().unwrap();
                let parent = self.formats.last_mut().unwrap();
                parent.buf.push_str(&italic.marker);
                parent.buf.push_str(&italic.buf);
            } else {
                break;
            }
        }
        while n >= 2 {
            self.open_format_with(Format::Bold, format!("{marker}{marker}"));
            n -= 2;
        }
        if n >= 1 {
            self.open_format_with(Format::Italic, marker.to_string());
        }
    }

    fn resolve_tildes(&mut self, count: usize) {
        let mut n = count;
        while n >= 2 {
            if !self.try_close_format(Format::Strikethrough) {
                self.open_format(Format::Strikethrough);
            }
            n -= 2;
        }
        if n > 0 {
            self.push_char('~');
        }
    }

    fn feed(&mut self, c: char) {
        let prev = self.prev;
        self.prev = Some(c);
        let state = std::mem::replace(&mut self.state, State::Normal);
        match state {
            State::Normal => match c {
                '*' | '_' => {
                    self.state = State::Stars {
                        count: 1,
                        underscore: c == '_',
                        after_word: prev.is_some_and(is_word),
                    }
                }
                '`' => {
                    self.flush_segment();
                    self.state = State::Code(String::new());
                }
                '~' => self.state = State::Tildes(1),
                _ => self.push_char(c),
            },
            State::Stars {
                count,
                underscore,
                after_word,
            } => match c {
                '*' | '_' => {
                    self.state = State::Stars {
                        count: count + 1,
                        underscore: underscore && c == '_',
                        after_word,
                    }
                }
                _ => {
                    // **`invoice_total` is an identifier, not emphasis.**
                    // CommonMark's intraword rule exists for exactly
                    // this: a run of `_` flanked by word characters on
                    // both sides can neither open nor close, so it is
                    // literal text. Without it the chat pane ate the
                    // underscores of a snake_case name and italicised
                    // what lay between two of them — live on
                    // 2026-09-21 the person read `invoicetotal` and
                    // `invoice*total` for a function the model had
                    // correctly called `invoice_total`. Asterisks keep
                    // the old behaviour: nobody writes `a*b*c` meaning
                    // multiplication in prose as often as they write
                    // snake_case, and CommonMark agrees.
                    if underscore && after_word && is_word(c) {
                        for _ in 0..count {
                            self.push_char('_');
                        }
                    } else {
                        self.resolve_stars(count, underscore);
                    }
                    self.feed(c);
                }
            },
            State::Code(mut buf) => match c {
                '`' => {
                    let style = self.code_style();
                    self.spans.push(Span::styled(buf, style));
                }
                _ => {
                    buf.push(c);
                    self.state = State::Code(buf);
                }
            },
            State::Tildes(count) => match c {
                '~' => self.state = State::Tildes(count + 1),
                _ => {
                    self.resolve_tildes(count);
                    self.feed(c);
                }
            },
        }
    }

    /// Unterminated syntax at end-of-line recovers as its literal
    /// marker text, never a dropped or silently-eaten character — this
    /// is a renderer, not a validator, so `**oops` must still read
    /// `**oops`, not `oops`.
    fn finish(mut self) -> Vec<Span<'static>> {
        match std::mem::replace(&mut self.state, State::Normal) {
            State::Stars {
                count,
                underscore,
                after_word,
            } => {
                // End of line: a run can still *close* what is open —
                // that is how `__bold__` ends — but an underscore run
                // at the end of a word with nothing to close is the
                // `trailing_` case, and is literal text.
                let closes = self
                    .formats
                    .last()
                    .is_some_and(|f| matches!(f.kind, Format::Bold | Format::Italic));
                if underscore && after_word && !closes {
                    for _ in 0..count {
                        self.push_char('_');
                    }
                } else {
                    self.resolve_stars(count, underscore)
                }
            }
            State::Tildes(count) => self.resolve_tildes(count),
            State::Code(buf) => {
                self.segment.push('`');
                self.segment.push_str(&buf);
            }
            State::Normal => {}
        }
        while let Some(frame) = self.formats.pop() {
            self.segment.push_str(&frame.marker);
            self.segment.push_str(&frame.buf);
        }
        self.flush_segment();
        self.spans
    }
}

/// Parse one line of text for inline emphasis, styled on top of `base`.
/// Text with no markdown in it comes back as a single span equal to the
/// input — the common case costs nothing extra downstream.
pub fn inline_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut parser = Parser::new(base);
    for c in text.chars() {
        parser.feed(c);
    }
    parser.finish()
}

/// Word-wrap a run of styled spans to `width` columns — `textwrap`'s
/// contract (never split a word, an overlong word gets its own line
/// regardless), but style-aware so emphasis survives the reflow. `base`
/// styles the inter-word spaces and the empty line for empty input,
/// matching `push_wrapped_width`'s "a blank line still pushes one row."
pub fn wrap_spans(spans: &[Span<'static>], base: Style, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut words: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    for span in spans {
        let style = span.style;
        let mut buf = String::new();
        for ch in span.content.chars() {
            if ch == ' ' {
                if !buf.is_empty() {
                    words
                        .last_mut()
                        .unwrap()
                        .push(Span::styled(std::mem::take(&mut buf), style));
                }
                if !words.last().unwrap().is_empty() {
                    words.push(Vec::new());
                }
            } else {
                buf.push(ch);
            }
        }
        if !buf.is_empty() {
            words.last_mut().unwrap().push(Span::styled(buf, style));
        }
    }
    words.retain(|w| !w.is_empty());

    let word_width = |w: &[Span<'static>]| -> usize { w.iter().map(Span::width).sum() };

    let mut out_lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;
    for word in words {
        let w = word_width(&word);
        if !current.is_empty() && current_width + 1 + w > width {
            out_lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if !current.is_empty() {
            current.push(Span::styled(" ", base));
            current_width += 1;
        }
        current_width += w;
        current.extend(word);
    }
    if !current.is_empty() || out_lines.is_empty() {
        out_lines.push(current);
    }

    out_lines
        .into_iter()
        .map(|spans| {
            if spans.is_empty() {
                Line::from(String::new()).style(base)
            } else {
                Line::from(spans)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(base: Style, text: &str) -> Span<'static> {
        Span::styled(text.to_owned(), base)
    }

    #[test]
    fn text_with_no_markdown_is_one_unstyled_span() {
        let base = Style::new().fg(Color::White);
        let spans = inline_spans("just some prose", base);
        assert_eq!(spans, vec![plain(base, "just some prose")]);
    }

    #[test]
    fn bold_and_italic_and_strikethrough_are_recognized() {
        let base = Style::default();
        let spans = inline_spans("a **bold** b *italic* c ~~gone~~", base);
        assert_eq!(
            spans,
            vec![
                plain(base, "a "),
                Span::styled("bold", base.add_modifier(Modifier::BOLD)),
                plain(base, " b "),
                Span::styled("italic", base.add_modifier(Modifier::ITALIC)),
                plain(base, " c "),
                Span::styled("gone", base.add_modifier(Modifier::CROSSED_OUT)),
            ]
        );
    }

    /// **A snake_case identifier is not emphasis.** Two `_` inside one
    /// line used to open and close an italic run, eating both
    /// underscores: `invoice_total(x) then line_total(y)` reached the
    /// person as `invoicetotal(x) then linetotal(y)`, and an unclosed
    /// one recovered as `*` — a character the model never wrote. For a
    /// harness whose whole subject is code, that is the renderer
    /// telling the person the wrong name.
    #[test]
    fn intraword_underscores_are_literal() {
        let base = Style::default();
        let text = "call invoice_total(x) then line_total(y)";
        assert_eq!(inline_spans(text, base), vec![plain(base, text)]);
    }

    /// The rule is about *position*, not about the character: at a word
    /// boundary `_` still delimits, and a run inside the emphasis stays
    /// literal — CommonMark renders `_foo_bar_` as one emphasized
    /// `foo_bar`.
    #[test]
    fn underscores_at_a_word_boundary_still_emphasize() {
        let base = Style::default();
        assert_eq!(
            inline_spans("_foo_bar_", base),
            vec![Span::styled(
                "foo_bar".to_owned(),
                base.add_modifier(Modifier::ITALIC)
            )]
        );
    }

    /// Recovery keeps the character that was actually written. An
    /// unterminated `_` is `_`, never the `*` that [`Format::open_marker`]
    /// would hand back.
    #[test]
    fn an_unterminated_underscore_recovers_as_itself() {
        let base = Style::default();
        assert_eq!(
            inline_spans("snake_case", base),
            vec![plain(base, "snake_case")]
        );
        assert_eq!(
            inline_spans("trailing_", base),
            vec![plain(base, "trailing_")]
        );
        // An emphasis that opens and never closes recovers as the
        // character that opened it, not as `Format::open_marker`'s `*`.
        assert_eq!(
            inline_spans("_hello world", base),
            vec![plain(base, "_hello world")]
        );
        assert_eq!(
            inline_spans("__hello world", base),
            vec![plain(base, "__hello world")]
        );
    }

    #[test]
    fn underscore_delimiters_work_the_same_as_asterisks() {
        let base = Style::default();
        assert_eq!(
            inline_spans("__bold__", base),
            vec![Span::styled(
                "bold".to_owned(),
                base.add_modifier(Modifier::BOLD)
            )]
        );
    }

    #[test]
    fn triple_star_nests_bold_and_italic() {
        let base = Style::default();
        let spans = inline_spans("***both***", base);
        assert_eq!(
            spans,
            vec![Span::styled(
                "both".to_owned(),
                base.add_modifier(Modifier::BOLD | Modifier::ITALIC)
            )]
        );
    }

    #[test]
    fn inline_code_is_cyan_regardless_of_base_color() {
        let base = Style::new().fg(Color::Magenta);
        let spans = inline_spans("run `tools.tell`", base);
        assert_eq!(
            spans,
            vec![
                plain(base, "run "),
                Span::styled("tools.tell".to_owned(), Style::new().fg(Color::Cyan)),
            ]
        );
    }

    #[test]
    fn base_reversed_modifier_survives_into_every_span() {
        let base = Style::default().add_modifier(Modifier::REVERSED);
        let spans = inline_spans("plain **bold** and `code`", base);
        for span in &spans {
            assert!(
                span.style.add_modifier.contains(Modifier::REVERSED),
                "selection highlight must not be lost under emphasis: {span:?}"
            );
        }
    }

    #[test]
    fn unterminated_emphasis_recovers_as_literal_markers() {
        let base = Style::default();
        assert_eq!(inline_spans("**oops", base), vec![plain(base, "**oops")]);
        assert_eq!(inline_spans("`oops", base), vec![plain(base, "`oops")]);
    }

    #[test]
    fn wrap_spans_never_exceeds_width_and_keeps_every_word() {
        let base = Style::default();
        let text = "hello **world** this is a fairly long sentence to wrap";
        let spans = inline_spans(text, base);
        let lines = wrap_spans(&spans, base, 12);
        assert!(lines.len() > 1, "must actually wrap at width 12");
        for line in &lines {
            assert!(line.width() <= 12, "line exceeds width: {}", line.width());
        }
        let rejoined: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .concat()
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            rejoined.split_whitespace().collect::<Vec<_>>(),
            text.replace("**", "")
                .split_whitespace()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn wrap_spans_keeps_bold_modifier_on_its_own_wrapped_line() {
        let base = Style::default();
        let spans = inline_spans("short **boldword** tail", base);
        let lines = wrap_spans(&spans, base, 6);
        let bold_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref() == "boldword"))
            .expect("boldword lands on some line");
        let bold_span = bold_line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "boldword")
            .unwrap();
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn wrap_spans_of_empty_input_is_one_empty_line() {
        let base = Style::default();
        let lines = wrap_spans(&[], base, 20);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].width(), 0);
    }
}
