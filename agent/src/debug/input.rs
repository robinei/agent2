//! `InputBuffer` — a cursor-navigable, multi-line text buffer for the
//! attached TUI's message/rewrite box (19_UX Part A). Self-contained
//! editing logic with no `ratatui`/`Session` dependency, so it is unit-
//! tested entirely on its own.

/// Lines are char-indexed (`Vec<char>`, not byte offsets into a
/// `String`) so a cursor position is always a valid UTF-8 boundary —
/// there is no byte-offset arithmetic anywhere in this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputBuffer {
    lines: Vec<Vec<char>>,
    cursor: (usize, usize),
    /// The column `up`/`down` are trying to hold, set by every
    /// horizontal move and consulted — never overwritten — by `up`/
    /// `down` themselves. Without it, moving down through a short line
    /// and back onto a long one would lose the column a vertical move
    /// was heading back to. The one piece of state past `lines` +
    /// `cursor` this buffer needs to remember.
    sticky_col: usize,
}

impl InputBuffer {
    pub fn new() -> Self {
        InputBuffer {
            lines: vec![Vec::new()],
            cursor: (0, 0),
            sticky_col: 0,
        }
    }

    /// Cursor at the top — reviewing a prefilled program from the start
    /// is the common case, not resuming where someone else left off.
    // Not yet consumed outside its own tests — 19_UX Part B's Step B1
    // wires this in.
    #[allow(dead_code)]
    pub fn prefilled(text: &str) -> Self {
        InputBuffer {
            lines: text
                .split('\n')
                .map(|line| line.chars().collect())
                .collect(),
            cursor: (0, 0),
            sticky_col: 0,
        }
    }

    // ── mutation ────────────────────────────────────────────────────

    pub fn insert_char(&mut self, c: char) {
        let (row, col) = self.cursor;
        self.lines[row].insert(col, c);
        self.cursor.1 += 1;
        self.sticky_col = self.cursor.1;
    }

    pub fn insert_newline(&mut self) {
        let (row, col) = self.cursor;
        let rest = self.lines[row].split_off(col);
        self.lines.insert(row + 1, rest);
        self.cursor = (row + 1, 0);
        self.sticky_col = 0;
    }

    pub fn backspace(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            self.lines[row].remove(col - 1);
            self.cursor.1 -= 1;
        } else if row > 0 {
            let joined_at = self.lines[row - 1].len();
            let current = self.lines.remove(row);
            self.lines[row - 1].extend(current);
            self.cursor = (row - 1, joined_at);
        }
        self.sticky_col = self.cursor.1;
    }

    pub fn delete_forward(&mut self) {
        let (row, col) = self.cursor;
        if col < self.lines[row].len() {
            self.lines[row].remove(col);
        } else if row + 1 < self.lines.len() {
            let next = self.lines.remove(row + 1);
            self.lines[row].extend(next);
        }
        self.sticky_col = self.cursor.1;
    }

    /// Ctrl-K — the current line only; readline has no multi-line case
    /// to translate, and merging into the line below at end-of-line
    /// would be a corner readline itself never defines.
    pub fn kill_to_end(&mut self) {
        let (row, col) = self.cursor;
        self.lines[row].truncate(col);
        self.sticky_col = self.cursor.1;
    }

    /// Ctrl-U — same one-line scope as `kill_to_end`.
    pub fn kill_to_start(&mut self) {
        let (row, col) = self.cursor;
        self.lines[row].drain(0..col);
        self.cursor.1 = 0;
        self.sticky_col = 0;
    }

    /// Ctrl-W — back to where `word_left` would land.
    pub fn delete_word_backward(&mut self) {
        let to = self.cursor;
        self.word_left();
        let from = self.cursor;
        self.delete_range(from, to);
    }

    /// Alt-D — forward to where `word_right` would land.
    pub fn delete_word_forward(&mut self) {
        let from = self.cursor;
        self.word_right();
        let to = self.cursor;
        self.cursor = from;
        self.delete_range(from, to);
    }

    /// Deletes every character from `from` up to (not including) `to` —
    /// both `(row, col)` positions, `from` at or before `to` in
    /// document order — joining lines as needed, and leaves the cursor
    /// at `from`. The one range-deleting primitive `kill_to_end`/`_start`
    /// don't need (they never cross a line) but the word deletions do.
    fn delete_range(&mut self, from: (usize, usize), to: (usize, usize)) {
        let (from_row, from_col) = from;
        let (to_row, to_col) = to;
        if from_row == to_row {
            self.lines[from_row].drain(from_col..to_col);
        } else {
            let tail = self.lines[to_row].split_off(to_col);
            self.lines[from_row].truncate(from_col);
            self.lines[from_row].extend(tail);
            self.lines.drain(from_row + 1..=to_row);
        }
        self.cursor = from;
        self.sticky_col = from_col;
    }

    // ── movement ────────────────────────────────────────────────────

    pub fn left(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            self.cursor.1 -= 1;
        } else if row > 0 {
            self.cursor = (row - 1, self.lines[row - 1].len());
        }
        self.sticky_col = self.cursor.1;
    }

    pub fn right(&mut self) {
        let (row, col) = self.cursor;
        if col < self.lines[row].len() {
            self.cursor.1 += 1;
        } else if row + 1 < self.lines.len() {
            self.cursor = (row + 1, 0);
        }
        self.sticky_col = self.cursor.1;
    }

    pub fn up(&mut self) {
        let row = self.cursor.0;
        if row > 0 {
            let new_row = row - 1;
            let col = self.sticky_col.min(self.lines[new_row].len());
            self.cursor = (new_row, col);
        }
    }

    pub fn down(&mut self) {
        let row = self.cursor.0;
        if row + 1 < self.lines.len() {
            let new_row = row + 1;
            let col = self.sticky_col.min(self.lines[new_row].len());
            self.cursor = (new_row, col);
        }
    }

    pub fn home(&mut self) {
        self.cursor.1 = 0;
        self.sticky_col = 0;
    }

    pub fn end(&mut self) {
        self.cursor.1 = self.lines[self.cursor.0].len();
        self.sticky_col = self.cursor.1;
    }

    /// Cursor to the end of the *last* line, regardless of the current
    /// row — unlike `end`, which stays on the current line. Used when
    /// recalling history forward (input.rs callers): landing on the
    /// bottom row is what lets a repeated `Down` keep walking forward
    /// instead of just moving within the recalled text.
    pub fn bottom(&mut self) {
        let row = self.lines.len() - 1;
        self.cursor = (row, self.lines[row].len());
        self.sticky_col = self.cursor.1;
    }

    /// Skips any whitespace immediately before the cursor, then the
    /// word behind that. A line boundary counts as whitespace (`'\n'`
    /// `is_whitespace`), which is what makes this cross lines the same
    /// way `left` does, for free.
    pub fn word_left(&mut self) {
        while self.prev_char().is_some_and(char::is_whitespace) {
            self.left();
        }
        while self.prev_char().is_some_and(|c| !c.is_whitespace()) {
            self.left();
        }
    }

    /// Skips any whitespace the cursor is currently sitting in (line
    /// boundary included, as in `word_left`), then the word that
    /// follows — landing just past its last character, readline's
    /// "end of the next word," never past the whitespace after it.
    pub fn word_right(&mut self) {
        while self.cur_char().is_some_and(char::is_whitespace) {
            self.right();
        }
        while self.cur_char().is_some_and(|c| !c.is_whitespace()) {
            self.right();
        }
    }

    fn prev_char(&self) -> Option<char> {
        let (row, col) = self.cursor;
        if col > 0 {
            Some(self.lines[row][col - 1])
        } else if row > 0 {
            Some('\n')
        } else {
            None
        }
    }

    fn cur_char(&self) -> Option<char> {
        let (row, col) = self.cursor;
        if col < self.lines[row].len() {
            Some(self.lines[row][col])
        } else if row + 1 < self.lines.len() {
            Some('\n')
        } else {
            None
        }
    }

    // ── whole-buffer ────────────────────────────────────────────────

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn clear(&mut self) {
        self.lines = vec![Vec::new()];
        self.cursor = (0, 0);
        self.sticky_col = 0;
    }

    /// The line count and cursor position, for rendering.
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn cursor(&self) -> (usize, usize) {
        self.cursor
    }

    /// The chars of one line, for rendering — `row` must be a valid
    /// line index (`< line_count()`).
    pub fn line(&self, row: usize) -> &[char] {
        &self.lines[row]
    }
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for InputBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let joined = self
            .lines
            .iter()
            .map(|line| line.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        write!(f, "{joined}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_delete_at_start_middle_end() {
        let mut b = InputBuffer::new();
        for c in "hello".chars() {
            b.insert_char(c);
        }
        assert_eq!(b.to_string(), "hello");
        assert_eq!(b.cursor(), (0, 5));

        b.home();
        b.insert_char('!');
        assert_eq!(b.to_string(), "!hello");
        assert_eq!(b.cursor(), (0, 1));

        b.end();
        b.backspace();
        assert_eq!(b.to_string(), "!hell");

        // Middle: delete the 'e'.
        b.home();
        b.right();
        b.right();
        b.delete_forward();
        assert_eq!(b.to_string(), "!hll");
    }

    #[test]
    fn backspace_and_delete_join_lines() {
        let mut b = InputBuffer::prefilled("one\ntwo");
        b.cursor = (1, 0);
        b.backspace();
        assert_eq!(b.to_string(), "onetwo");
        assert_eq!(b.cursor(), (0, 3));

        let mut b = InputBuffer::prefilled("one\ntwo");
        b.cursor = (0, 3);
        b.delete_forward();
        assert_eq!(b.to_string(), "onetwo");
        assert_eq!(b.cursor(), (0, 3));
    }

    #[test]
    fn bottom_moves_cursor_to_the_end_of_the_last_line() {
        let mut b = InputBuffer::prefilled("one\ntwo\nthree");
        assert_eq!(b.cursor(), (0, 0));
        b.bottom();
        assert_eq!(b.cursor(), (2, 5));
    }

    #[test]
    fn up_down_hold_sticky_column_across_a_short_line() {
        let mut b = InputBuffer::prefilled("longer line\nhi\nanother long one");
        b.cursor = (0, 0);
        for _ in 0..5 {
            b.right();
        }
        assert_eq!(b.cursor(), (0, 5));
        b.down();
        // "hi" is only 2 chars: column clamps, but the target (5) is
        // remembered, not overwritten to 2.
        assert_eq!(b.cursor(), (1, 2));
        b.down();
        assert_eq!(
            b.cursor(),
            (2, 5),
            "sticky column restored on a longer line"
        );
        // Symmetric going back up: still 5, through the short line again.
        b.up();
        assert_eq!(b.cursor(), (1, 2));
        b.up();
        assert_eq!(b.cursor(), (0, 5), "sticky column restored going up too");
        b.up();
        assert_eq!(b.cursor(), (0, 5), "clamped at the first line");
    }

    #[test]
    fn left_right_wrap_at_line_boundaries() {
        let mut b = InputBuffer::prefilled("ab\ncd");
        b.cursor = (0, 0);
        b.left();
        assert_eq!(b.cursor(), (0, 0), "clamped at the very start");

        b.cursor = (0, 2);
        b.right();
        assert_eq!(b.cursor(), (1, 0), "wraps to the start of the next line");
        b.left();
        assert_eq!(
            b.cursor(),
            (0, 2),
            "wraps back to the end of the previous line"
        );

        b.cursor = (1, 2);
        b.right();
        assert_eq!(b.cursor(), (1, 2), "clamped at the very end");
    }

    #[test]
    fn word_left_and_right_cross_whitespace_and_lines() {
        let mut b = InputBuffer::prefilled("foo  bar\nbaz");
        b.cursor = (0, 8); // end of "bar"
        b.word_left();
        assert_eq!(b.cursor(), (0, 5), "start of \"bar\"");
        b.word_left();
        assert_eq!(
            b.cursor(),
            (0, 0),
            "start of \"foo\", skipping the run of spaces"
        );

        b.cursor = (0, 8);
        b.word_right();
        assert_eq!(
            b.cursor(),
            (1, 3),
            "crosses the line boundary to the end of \"baz\""
        );
    }

    #[test]
    fn kill_to_end_and_start_stay_within_one_line() {
        let mut b = InputBuffer::prefilled("hello\nworld");
        b.cursor = (0, 5); // end of "hello"
        b.kill_to_end();
        assert_eq!(
            b.to_string(),
            "hello\nworld",
            "nothing to kill at end-of-line"
        );

        b.cursor = (0, 2);
        b.kill_to_end();
        assert_eq!(b.to_string(), "he\nworld");

        let mut b = InputBuffer::prefilled("hello\nworld");
        b.cursor = (1, 0);
        b.kill_to_start();
        assert_eq!(
            b.to_string(),
            "hello\nworld",
            "nothing to kill at start-of-line"
        );

        b.cursor = (1, 3);
        b.kill_to_start();
        assert_eq!(b.to_string(), "hello\nld");
        assert_eq!(b.cursor(), (1, 0));
    }

    #[test]
    fn delete_word_backward_and_forward() {
        let mut b = InputBuffer::prefilled("foo bar baz");
        b.cursor = (0, 11); // end
        b.delete_word_backward();
        assert_eq!(b.to_string(), "foo bar ");
        // Mid-word: cursor inside "bar".
        b.cursor = (0, 6);
        b.delete_word_backward();
        assert_eq!(b.to_string(), "foo r ");

        let mut b = InputBuffer::prefilled("foo bar baz");
        b.cursor = (0, 0);
        b.delete_word_forward();
        assert_eq!(b.to_string(), " bar baz");
        // Mid-word.
        let mut b = InputBuffer::prefilled("foo bar baz");
        b.cursor = (0, 5);
        b.delete_word_forward();
        assert_eq!(b.to_string(), "foo b baz");
    }

    #[test]
    fn prefilled_round_trips_through_display() {
        let text = "line one\nline two\nline three";
        let b = InputBuffer::prefilled(text);
        assert_eq!(b.to_string(), text);
        assert_eq!(b.cursor(), (0, 0));
    }

    #[test]
    fn is_empty_and_clear() {
        let mut b = InputBuffer::new();
        assert!(b.is_empty());
        b.insert_char('x');
        assert!(!b.is_empty());
        b.insert_newline();
        assert!(!b.is_empty());
        b.clear();
        assert!(b.is_empty());
        assert_eq!(b.cursor(), (0, 0));
        assert_eq!(b.line_count(), 1);
    }
}
