//! The notebook split: markdown in, executable cell spans out
//! (`25_NOTEBOOK.md`, step 25.1).
//!
//! Under `Transport::Notebook` a completion is markdown containing
//! fenced code blocks rather than a bare JavaScript program. This
//! module is the whole of the markdown side of that: it finds the
//! executable cells ([`split_cells`]) and it owns the single parse
//! buffer the compiler is handed for each one ([`ParseBuffer`]).
//!
//! **Pure, and deliberately small.** No IO, no JS parsing, no
//! knowledge of the VM. Per D16, markdown never enters `interp` —
//! `interp` receives a `&str` and knows nothing about fences — so the
//! fence rules live here and nowhere else.
//!
//! **Recognition is strict, and the asymmetry is the reason** (25.1).
//! A fence is three or more backticks at column 0; the info string is
//! exactly `js` or `javascript` in lower case; a closing fence is at
//! least as long as the one it closes. No tildes, no indented fences,
//! nothing inside a list item or a block quote — all of which
//! CommonMark permits and this does not. A strict subset is safe
//! because a *missed* cell is not silent: the reply still speaks, the
//! turn rests (D4), and the person's next message gets it moving
//! again, so a miss costs one exchange. A *wrongly* recognised cell
//! runs code the model did not mean to run, and has no such backstop.
//!
//! This is narrower than `document.rs`'s `extract_program`, which
//! strips a bare ` ``` ` fence and matches the tag case-insensitively.
//! Giving that leniency up is deliberate (D3): a bare fence is also how
//! prose quotes anything at all.
#![allow(dead_code)] // wired into the compile path in 25.4; until then
// the only callers are this module's own tests.

/// The cell driver: a reply's cells, fed to one paused compilation in turn
/// (25.4).
///
/// **A reply is one run** (D7). The cells share a frame that is never unwound
/// between them, so the driver's whole job is: feed a cell, let the VM run it
/// to its `Pause`, feed the next — and when they run out, close the unit so the
/// ordinary root `Return(0)` ends the run on exactly the path a one-shot
/// program takes. No cell boundary ever reaches `finish_program`; the reply's
/// end does, once.
///
/// It holds the compile half of the evaluation ([`interp::ReplCore`]) rather
/// than a whole [`interp::Repl`], because the host already keeps the VM in its
/// own run state and one VM in two places is one too many.
pub struct Notebook {
    core: interp::ReplCore,
    buffer: ParseBuffer,
    cells: Vec<CellSpan>,
    /// The next cell to feed. Equal to `cells.len()` once they are exhausted
    /// and only the epilogue is left.
    next: usize,
}

/// What [`Notebook::feed_next`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Fed {
    /// A cell's instructions were appended; step the VM to run them.
    Cell(usize),
    /// The cells ran out and the run's epilogue was appended; the next step
    /// reports `Done`.
    Closed,
}

impl Notebook {
    /// Split `markdown` into cells and prepare to feed them.
    ///
    /// Nothing is compiled here — a notebook with no cells is a perfectly good
    /// notebook that simply has nothing to run (D4), and the caller decides
    /// what that means before any VM exists.
    pub fn new(markdown: &str) -> Self {
        let mut core = interp::ReplCore::new();
        core.reject_top_level_return(NO_TOP_LEVEL_RETURN);
        Self {
            cells: split_cells(markdown),
            buffer: ParseBuffer::new(markdown),
            core,
            next: 0,
        }
    }

    /// How many executable cells the reply held. Zero is the cell-less reply
    /// D4 is about, and the drift metric 25.8 counts.
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// The cell at `i`, as a range into the markdown.
    pub fn cell(&self, i: usize) -> CellSpan {
        self.cells[i]
    }

    /// Compile the next cell into `vm` — or, once they are exhausted, the
    /// run's `Return(0)` epilogue.
    ///
    /// Each cell is compiled from the shared buffer with only its own bytes
    /// live (D2), so the spans it emits are offsets into the markdown and the
    /// analysis tables accumulate across cells instead of colliding.
    pub fn feed_next(&mut self, vm: &mut interp::VM) -> Result<Fed, String> {
        if self.next < self.cells.len() {
            let i = self.next;
            let live = self.buffer.focus(self.cells[i]);
            self.core
                .push(vm, live)
                .map_err(|diags| render_cell_diags(live, &diags))?;
            self.next += 1;
            Ok(Fed::Cell(i))
        } else {
            // Nothing live: the epilogue is the compiler's own, with no
            // source of its own to point at.
            let blank = self.buffer.clear();
            self.core
                .close(vm, blank)
                .map_err(|diags| render_cell_diags(blank, &diags))?;
            Ok(Fed::Closed)
        }
    }
}

/// Render a cell's diagnostics against the parse buffer.
///
/// The buffer, not the raw markdown, because that is the coordinate system the
/// spans were taken in — and outside the live cell it is blank, which is
/// exactly right: a diagnostic points into the cell that failed.
fn render_cell_diags(buffer: &str, diags: &[interp::Diagnostic]) -> String {
    diags
        .iter()
        .map(|d| d.render(buffer))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a cell should write instead of a top-level `return` (D5, 25.3).
///
/// **The rule lives in `interp`; this sentence lives here** (D16). `interp`
/// carries a flag — the inverse of `oxc`'s `allow_return_outside_function` —
/// and `Repl::reject_top_level_return` takes the message from whoever set it,
/// because `history.append` and `done()` are this harness's vocabulary and not
/// the language's.
///
/// **The diagnostic matters more than the rule**, because ending a program
/// with `return {…}` is the *trained* habit — exemplar 02 teaches it — so this
/// has to say what to write instead rather than merely that it is disallowed.
/// Under this transport `return` did two things and something else already
/// does each: falling off the last cell ends the run, `done()` rests the
/// branch, and `history.append` is what leaves a row the next turn reads.
pub const NO_TOP_LEVEL_RETURN: &str =
    "a cell cannot `return`: use `history.append(...)` to leave a row the next \
     turn reads, and `done()` to rest once the work is finished";

/// One executable cell, as a byte range into the markdown it came from.
///
/// The range covers the cell's *content* — everything between the
/// opening fence's newline and the start of the closing fence line —
/// so `&markdown[cell.start..cell.end]` is exactly the JavaScript to
/// compile, with the trailing newline of its last line included and
/// neither fence line in it.
///
/// These are the markdown's own offsets, not cell-local ones, which is
/// the whole of D2: the analyzer's tables and the debug table are
/// span-keyed, so two cells compiled from their own substrings would
/// both start at zero and collide. Cell-local `site` values are
/// recovered at *log* time by subtracting [`CellSpan::start`] (D1), so
/// nothing downstream sees an absolute offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellSpan {
    pub start: usize,
    pub end: usize,
}

impl CellSpan {
    /// The cell's source text, sliced out of the markdown it indexes.
    pub fn slice<'a>(&self, markdown: &'a str) -> &'a str {
        &markdown[self.start..self.end]
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// An open fence, while scanning.
struct OpenFence {
    /// How many backticks opened it. A closing fence must have at
    /// least this many — which is what lets a 4-backtick fence quote a
    /// 3-backtick one without the inner fence closing the outer block.
    ticks: usize,
    /// Whether this block's info string marks it executable. Blocks
    /// that are *not* executable are still tracked, because their
    /// content must not be scanned for fences.
    executable: bool,
    /// Byte offset just past the opening fence line's newline.
    content_start: usize,
}

/// Is this info string one that executes? Exactly `js` or
/// `javascript`, lower case (D3). Everything else — `text`, `rust`,
/// `JS`, and a bare fence with no info string at all — is prose
/// quoting code, and is left for the person to read.
fn executes(info: &str) -> bool {
    info == "js" || info == "javascript"
}

/// Split a markdown reply into its executable cells, in source order.
///
/// An unterminated final fence yields **no cell**. D11 is explicit
/// that a cell executes the moment its fence *closes*, and a fence
/// that never closes is a completion truncated mid-cell: the bytes are
/// half a statement the model was still writing. Under D11 truncation
/// is partial progress — the cells that closed stand — and this is the
/// boundary of "closed". Running a half-written cell is precisely the
/// wrongly-recognised case the strictness rule above exists to avoid.
pub fn split_cells(markdown: &str) -> Vec<CellSpan> {
    let mut cells = Vec::new();
    let mut open: Option<OpenFence> = None;
    let mut offset = 0usize;

    for line in markdown.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        // `trim_end` takes the newline and any `\r` with it, so CRLF
        // needs no special case: a closing fence is all backticks once
        // trailing whitespace is gone.
        let text = line.trim_end();

        match &open {
            Some(fence) => {
                let ticks = leading_backticks(text);
                // A closing fence is backticks and nothing else. An
                // info string is not permitted on a close, so a line
                // like ```js inside a 4-backtick block is content.
                if ticks >= fence.ticks && ticks == text.len() {
                    if fence.executable {
                        cells.push(CellSpan {
                            start: fence.content_start,
                            end: line_start,
                        });
                    }
                    open = None;
                }
            }
            None => {
                let ticks = leading_backticks(text);
                if ticks < 3 {
                    continue;
                }
                let info = text[ticks..].trim();
                // CommonMark forbids a backtick in a backtick fence's
                // info string, because ```` ``` ` ``` ```` is a code
                // span. Treating it as no fence at all is the safe
                // direction: a missed cell, never a spurious one.
                if info.contains('`') {
                    continue;
                }
                open = Some(OpenFence {
                    ticks,
                    executable: executes(info),
                    content_start: offset,
                });
            }
        }
    }

    cells
}

fn leading_backticks(text: &str) -> usize {
    text.bytes().take_while(|&b| b == b'`').count()
}

/// Blank a byte for the parse buffer: everything becomes a space
/// except `\n`, which is kept (D2).
///
/// Byte-exactness alone would satisfy every consumer that goes through
/// a span, but line and column are computed from a source, so
/// preserving the newlines makes the buffer answer those identically
/// to the markdown too. The fill is never read as text, only skipped,
/// so a multi-byte character becoming that many spaces is fine — and
/// it is what keeps the length exact.
fn blanked(b: u8) -> u8 {
    if b == b'\n' { b'\n' } else { b' ' }
}

/// The one buffer every cell is compiled from (D2).
///
/// Byte-for-byte as long as the reply and with the same newlines, with
/// **only the cell being compiled live** and every other byte blanked.
/// The parser is handed this, so the spans it emits are already
/// offsets into the markdown — which is what lets the analyzer simply
/// accumulate across cells (D12) instead of being rebuilt and seeded
/// by hand.
///
/// `oxc_parser` has no offset option, so the alternatives were to line
/// the bytes up like this or to walk the AST afterwards adding an
/// offset to every node's span. Lining up is free by comparison.
///
/// One buffer, not one per cell: [`focus`](Self::focus) blanks the
/// previously live cell before writing the next, so the cost is one
/// allocation amortized and O(cell) of filling per cell. What it buys
/// in exchange is that the parser re-lexes the whitespace prefix each
/// time, making lexing quadratic over the reply — the trade phase 24
/// already accepted in writing, against a parser that runs at MB/s.
pub struct ParseBuffer {
    source: String,
    buf: Vec<u8>,
    live: Option<CellSpan>,
}

impl ParseBuffer {
    /// A buffer over `markdown` with nothing live yet — the same
    /// length and the same newlines, and otherwise all spaces.
    pub fn new(markdown: &str) -> Self {
        let buf = markdown.bytes().map(blanked).collect();
        Self {
            source: markdown.to_owned(),
            buf,
            live: None,
        }
    }

    /// The markdown this buffer was built over.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Make `cell` the live one, blanking whichever was live before,
    /// and return the whole buffer for the parser.
    pub fn focus(&mut self, cell: CellSpan) -> &str {
        if let Some(prev) = self.live.take() {
            for i in prev.start..prev.end {
                self.buf[i] = blanked(self.source.as_bytes()[i]);
            }
        }
        self.buf[cell.start..cell.end]
            .copy_from_slice(&self.source.as_bytes()[cell.start..cell.end]);
        self.live = Some(cell);
        self.as_str()
    }

    /// Blank the live cell, leaving nothing live.
    pub fn clear(&mut self) -> &str {
        if let Some(prev) = self.live.take() {
            for i in prev.start..prev.end {
                self.buf[i] = blanked(self.source.as_bytes()[i]);
            }
        }
        self.as_str()
    }

    /// The buffer as text.
    ///
    /// Always valid UTF-8, and cheap to prove: the fill is ASCII, and
    /// a cell's bounds are line boundaries in the source, so the live
    /// region is a whole UTF-8 substring copied verbatim. The check is
    /// a linear scan over a few KB, run once per focus, and it is the
    /// invariant D2 rests on — worth asserting rather than assuming.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf)
            .expect("parse buffer stays UTF-8: the fill is ASCII and cells are line-aligned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gate's core assertion: every span slices the markdown back
    /// to exactly the cell's text.
    fn cells_of(md: &str) -> Vec<&str> {
        split_cells(md).iter().map(|c| c.slice(md)).collect()
    }

    #[test]
    fn a_js_fence_is_a_cell() {
        let md = "prose\n\n```js\nconst x = 1;\n```\n\nmore prose\n";
        assert_eq!(cells_of(md), vec!["const x = 1;\n"]);
    }

    #[test]
    fn javascript_spells_the_same_thing() {
        let md = "```javascript\ntell(\"hi\");\n```\n";
        assert_eq!(cells_of(md), vec!["tell(\"hi\");\n"]);
    }

    /// D3: executing is what every turn does, so it is untagged;
    /// quoting is the marked case. Anything that is not exactly `js`
    /// or `javascript` is for the person to read.
    #[test]
    fn a_non_executable_tag_is_not_a_cell() {
        for tag in ["text", "rust", "python", "ts", "jsx"] {
            let md = format!("```{tag}\nconst x = 1;\n```\n");
            assert!(
                split_cells(&md).is_empty(),
                "```{tag} must not execute, but it produced a cell"
            );
        }
    }

    /// Strict means strict: `JS` is not `js`. `extract_program`
    /// matched the tag case-insensitively; this does not, and the
    /// failure is a rested turn rather than a surprise run.
    #[test]
    fn the_tag_is_lower_case_only() {
        assert!(split_cells("```JS\nconst x = 1;\n```\n").is_empty());
        assert!(split_cells("```JavaScript\nconst x = 1;\n```\n").is_empty());
    }

    /// The one place this is deliberately narrower than today's
    /// `extract_program`, which strips a bare fence too (D3). A bare
    /// fence is how prose quotes anything at all.
    #[test]
    fn a_bare_fence_does_not_execute() {
        assert!(split_cells("```\nconst x = 1;\n```\n").is_empty());
    }

    /// D11: a cell executes the moment its fence *closes*. An
    /// unterminated fence is a completion truncated mid-cell — half a
    /// statement the model was still writing — and running it is
    /// exactly the wrongly-recognised case strictness exists to avoid.
    #[test]
    fn an_unterminated_final_fence_is_not_a_cell() {
        let md = "prose\n\n```js\nconst x = 1;\nawait tools.read_fi";
        assert!(split_cells(md).is_empty());
    }

    /// And the cells that *did* close still stand — truncation is
    /// partial progress (D11), not a discarded reply.
    #[test]
    fn a_closed_cell_before_an_unterminated_one_still_stands() {
        let md = "```js\nconst a = 1;\n```\n\nprose\n\n```js\nconst b = 2";
        assert_eq!(cells_of(md), vec!["const a = 1;\n"]);
    }

    /// The notebook prior for quoting code: a longer fence wraps a
    /// shorter one. The inner ```js is content, not a cell, because a
    /// closing fence must be at least as long as the one it closes and
    /// may carry no info string.
    #[test]
    fn a_four_backtick_fence_wraps_a_three_backtick_one() {
        let md = "here is what I would write:\n\n````\n```js\nconst x = 1;\n```\n````\n";
        assert!(
            split_cells(md).is_empty(),
            "the inner fence is quoted, not executable"
        );
    }

    /// And the same wrapping with the outer fence tagged, which is how
    /// a markdown sample is quoted.
    #[test]
    fn a_four_backtick_markdown_fence_hides_its_inner_cell() {
        let md = "````markdown\n```js\ndone();\n```\n````\n";
        assert!(split_cells(md).is_empty());
    }

    /// The outer fence being executable does not make its content
    /// cells — it makes the content *its own* text.
    #[test]
    fn a_four_backtick_js_fence_is_one_cell_containing_a_fence() {
        let md = "````js\nconst s = \"```\";\n````\n";
        assert_eq!(cells_of(md), vec!["const s = \"```\";\n"]);
    }

    #[test]
    fn zero_cells() {
        assert!(split_cells("").is_empty());
        assert!(split_cells("just prose, no code at all.\n").is_empty());
        assert!(split_cells("a line with ``` backticks mid-line\n").is_empty());
    }

    /// D1: a reply decomposes into its pieces in source order, and
    /// several cells is the ordinary case.
    #[test]
    fn several_cells_come_back_in_source_order() {
        let md = "\
one\n\
\n\
```js\n\
const a = 1;\n\
```\n\
\n\
two\n\
\n\
```js\n\
const b = a + 1;\n\
```\n\
\n\
three\n";
        assert_eq!(cells_of(md), vec!["const a = 1;\n", "const b = a + 1;\n"]);
    }

    /// Non-executable blocks are still *tracked*, or a fence inside
    /// one would be scanned as a cell.
    #[test]
    fn a_fence_inside_a_quoted_block_of_equal_length_still_closes_it() {
        // ```text ... ``` closes at the first bare fence; the ```js
        // after it is then a real cell.
        let md = "```text\nnot code\n```\n\n```js\nreal();\n```\n";
        assert_eq!(cells_of(md), vec!["real();\n"]);
    }

    /// Indented fences, list items and block quotes are all out (25.1).
    #[test]
    fn a_fence_must_be_at_column_zero() {
        assert!(split_cells("  ```js\nconst x = 1;\n  ```\n").is_empty());
        assert!(split_cells("- ```js\n  const x = 1;\n  ```\n").is_empty());
        assert!(split_cells("> ```js\n> const x = 1;\n> ```\n").is_empty());
    }

    /// No tildes (25.1). CommonMark permits them; this does not.
    #[test]
    fn tildes_are_not_fences() {
        assert!(split_cells("~~~js\nconst x = 1;\n~~~\n").is_empty());
    }

    /// A closing fence carries no info string, so a longer run of
    /// backticks closes a shorter block but a tagged line does not.
    #[test]
    fn a_longer_closing_fence_closes_a_shorter_block() {
        let md = "```js\nconst a = 1;\n`````\n";
        assert_eq!(cells_of(md), vec!["const a = 1;\n"]);
    }

    #[test]
    fn a_closing_fence_may_have_trailing_whitespace() {
        let md = "```js\nconst a = 1;\n```   \n";
        assert_eq!(cells_of(md), vec!["const a = 1;\n"]);
    }

    #[test]
    fn an_empty_cell_is_a_cell_with_nothing_in_it() {
        let md = "```js\n```\n";
        assert_eq!(cells_of(md), vec![""]);
    }

    /// A cell whose closing fence is the last line with no trailing
    /// newline still closes.
    #[test]
    fn a_closing_fence_at_eof_without_a_newline_closes() {
        let md = "```js\ndone();\n```";
        assert_eq!(cells_of(md), vec!["done();\n"]);
    }

    // --- the `return` diagnostic (D5, 25.3) ---

    /// The gate: the message names both replacements, because a model that is
    /// only told `return` is disallowed has nowhere to go. It is carried into
    /// `interp` by `Repl::reject_top_level_return`, which supplies no wording
    /// of its own.
    #[test]
    fn the_return_diagnostic_names_what_to_use_instead() {
        assert!(
            NO_TOP_LEVEL_RETURN.contains("history.append"),
            "the message must name what leaves a row for the next turn"
        );
        assert!(
            NO_TOP_LEVEL_RETURN.contains("done()"),
            "the message must name what rests the branch"
        );
        assert!(
            NO_TOP_LEVEL_RETURN.contains("return"),
            "and it must name the thing being refused"
        );
    }

    /// End to end through the REPL: a cell's top-level `return` is refused
    /// with this harness's sentence, while a `return` inside a function in a
    /// cell is left alone.
    #[test]
    fn a_cell_cannot_return_but_a_function_in_one_can() {
        let md = "```js\nfunction f() { return 1; }\nconsole.log(f());\n```\n\n\
                  ```js\nreturn { done: true };\n```\n";
        let cells = split_cells(md);
        assert_eq!(cells.len(), 2);

        let mut pb = ParseBuffer::new(md);
        let mut repl =
            interp::Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
        repl.reject_top_level_return(NO_TOP_LEVEL_RETURN);

        repl.push(pb.focus(cells[0]))
            .expect("a function inside a cell may return");
        repl.vm.step(u64::MAX).unwrap();
        assert_eq!(repl.vm.console_lines, vec!["1"]);

        let errs = repl
            .push(pb.focus(cells[1]))
            .expect_err("the cell's own `return` is refused");
        assert!(
            errs.iter().any(|d| d.message == NO_TOP_LEVEL_RETURN),
            "expected this harness's message, got {errs:?}"
        );
    }

    // --- the parse buffer (D2) ---

    /// The gate, stated directly: the buffer's length and every
    /// newline position match the markdown, for each cell in turn.
    fn assert_shape_matches(buf: &str, md: &str) {
        assert_eq!(buf.len(), md.len(), "buffer length must equal the markdown");
        let buf_nl: Vec<usize> = buf
            .bytes()
            .enumerate()
            .filter(|(_, b)| *b == b'\n')
            .map(|(i, _)| i)
            .collect();
        let md_nl: Vec<usize> = md
            .bytes()
            .enumerate()
            .filter(|(_, b)| *b == b'\n')
            .map(|(i, _)| i)
            .collect();
        assert_eq!(buf_nl, md_nl, "every newline must sit at the same offset");
    }

    #[test]
    fn the_buffer_matches_the_markdown_for_each_cell_in_turn() {
        let md = "\
opening prose\n\
\n\
```js\n\
const a = 1;\n\
```\n\
\n\
middle prose, with a multi-byte character: \u{2014}\n\
\n\
```js\n\
const b = a + 1;\n\
```\n\
\n\
closing prose\n";
        let cells = split_cells(md);
        assert_eq!(cells.len(), 2);

        let mut pb = ParseBuffer::new(md);
        assert_shape_matches(pb.as_str(), md);
        assert!(
            pb.as_str().trim().is_empty(),
            "nothing is live before the first focus"
        );

        for cell in &cells {
            let live = pb.focus(*cell);
            assert_shape_matches(live, md);
            // Exactly this cell is live...
            assert_eq!(&live[cell.start..cell.end], cell.slice(md));
            // ...and nothing else is.
            let mut rest = live.to_owned();
            rest.replace_range(cell.start..cell.end, &" ".repeat(cell.len()));
            assert!(
                rest.chars().all(|c| c == ' ' || c == '\n'),
                "a second cell or some prose is still live in the buffer"
            );
        }
    }

    /// The em-dash in the prose above is three bytes; blanking is
    /// byte-wise, so it becomes three spaces and the length stays
    /// exact. The fill is never read as text, only skipped.
    #[test]
    fn a_multi_byte_character_blanks_to_its_own_width_in_spaces() {
        let md = "\u{2014}\n```js\nx();\n```\n";
        let pb = ParseBuffer::new(md);
        assert_shape_matches(pb.as_str(), md);
        assert_eq!(&pb.as_str()[..3], "   ");
    }

    /// One buffer, reused: focusing cell 1 blanks cell 0, so only ever
    /// one cell is live.
    #[test]
    fn focusing_a_cell_blanks_the_previous_one() {
        let md = "```js\nfirst();\n```\n```js\nsecond();\n```\n";
        let cells = split_cells(md);
        let mut pb = ParseBuffer::new(md);

        let live = pb.focus(cells[0]).to_owned();
        assert!(live.contains("first();"));
        assert!(!live.contains("second();"));

        let live = pb.focus(cells[1]).to_owned();
        assert!(live.contains("second();"));
        assert!(
            !live.contains("first();"),
            "cell 0 must be blanked when cell 1 is focused"
        );
        assert_shape_matches(&live, md);

        let cleared = pb.clear();
        assert!(cleared.trim().is_empty());
        assert_shape_matches(cleared, md);
    }

    /// Why D2 exists at all: the spans the parser sees are the
    /// markdown's own, so cell 1's text sits at cell 1's offset rather
    /// than at zero.
    #[test]
    fn a_cells_text_sits_at_its_markdown_offset_not_at_zero() {
        let md = "prose\n\n```js\nsecond_cell_offset();\n```\n";
        let cells = split_cells(md);
        let mut pb = ParseBuffer::new(md);
        let live = pb.focus(cells[0]);
        let at = live.find("second_cell_offset").unwrap();
        assert_eq!(at, md.find("second_cell_offset").unwrap());
        assert!(at > 0, "the cell must not have been rebased to zero");
    }
}
