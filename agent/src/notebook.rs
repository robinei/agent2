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
//! exactly `js`, `javascript`, `ts` or `typescript` in lower case (the
//! compiler parses TypeScript and erases the types, so all four are the
//! same program); a closing fence is at
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

/// The cell driver: a reply's pieces, split as they arrive and its cells fed
/// to one paused compilation in turn (25.4, 25.5).
///
/// **A reply is one run** (D7). The cells share a frame that is never unwound
/// between them, so the driver's whole job is: feed a cell, let the VM run it
/// to its `Pause`, feed the next — and when the reply ends, close the unit so
/// the ordinary root `Return(0)` ends the run on exactly the path a one-shot
/// program takes. No cell boundary ever reaches `finish_program`; the reply's
/// end does, once.
///
/// **The reply grows underneath it** (D11): a cell runs the moment its fence
/// closes, with the completion still streaming. That is why the prelude is
/// compiled *first* rather than appended per cell — see
/// [`interp::ReplCore::prime_prelude`] — and why every cell's parse buffer is
/// rebuilt over the reply as it stands, with the earlier cells' offsets
/// unchanged beneath it.
///
/// It holds the compile half of the evaluation ([`interp::ReplCore`]) rather
/// than a whole [`interp::Repl`], because the host already keeps the VM in its
/// own run state and one VM in two places is one too many.
pub struct Notebook {
    core: interp::ReplCore,
    /// Where the reply's own coordinates start: past the primed prelude.
    /// Every cell span the compiler emits is `base + markdown offset`.
    base: usize,
    /// The prelude region, blanked — spaces except newlines — so a cell's
    /// parse buffer keeps the same line structure the unit's source has.
    base_blank: String,
    /// The completion so far.
    reply: String,
    stream: Stream,
    /// Pieces that are complete but not yet acted on, in source order.
    ///
    /// A cell can close while the previous one is still suspended on an
    /// await, so it waits here until the VM reaches the previous cell's
    /// `Pause` (D11's sequential rule). Prose waits in the same queue rather
    /// than being logged the moment it is recognised, because the log has to
    /// read in source order: a paragraph written between two cells belongs
    /// after the first cell's calls, not before them.
    queued: std::collections::VecDeque<Piece>,
    /// Whether the completion has ended, so the run can be closed once the
    /// queue drains.
    ended: bool,
    /// The completion was cut off by the token budget. The cells that closed
    /// still ran (D11's partial progress); the run is reported as truncated
    /// rather than completed, so the next completion knows.
    truncated: bool,
}

impl Notebook {
    /// Begin a reply. Compiles the prelude into `vm` as the unit's first
    /// fragment, which is what lets the reply's own region grow afterwards.
    pub fn new(vm: &mut interp::VM) -> Result<Self, String> {
        // **A top-level `return` ends the program**, which under this
        // transport is the whole reply: the cells share one scope and
        // one frame, so returning from that frame is the least
        // surprising reading of the primitive and is what `stop(reason)`
        // used to spell. The rejection that stood here sent a model
        // writing `return` to `history.append` and `finish(text)`,
        // neither of which ends anything.
        let mut core = interp::ReplCore::new();
        core.prime_prelude(vm)
            .map_err(|diags| render_cell_diags("", &diags))?;
        let base = core.source_base();
        let base_blank = core.source()[..base]
            .bytes()
            .map(|b| if b == b'\n' { '\n' } else { ' ' })
            .collect();
        Ok(Self {
            core,
            base,
            base_blank,
            reply: String::new(),
            stream: Stream::new(),
            queued: std::collections::VecDeque::new(),
            ended: false,
            truncated: false,
        })
    }

    /// Append streamed text and hand back whatever pieces are now complete.
    /// Cells named by the returned pieces are queued for compilation.
    pub fn push_text(&mut self, text: &str) -> Vec<Piece> {
        self.reply.push_str(text);
        self.drop_leaked_reasoning();
        let pieces = self.stream.advance(&self.reply);
        self.queue(&pieces);
        pieces
    }

    /// **Reasoning the provider put in the wrong channel.**
    ///
    /// The API has two: `reasoning_content` is thinking, `content` is the
    /// reply. Some providers leak the first into the second and close it
    /// with a bare `</think>`. Measured 2026-09-18 at 1 completion in 74
    /// against `opencode.ai/zen`, and it is not cosmetic — the leak in that
    /// run carried a stray ` ``` `, which opened a quote block, which
    /// swallowed the reply's third ```js block whole. The model wrote that
    /// cell, the person saw it, and nothing ran it.
    ///
    /// So on seeing the closer, everything from the last piece handed out up
    /// to and including the tag is dropped, and the scanner re-reads what is
    /// left. The reply is rebuilt without the leak, which is what makes the
    /// fence state come good.
    ///
    /// **Two things it deliberately will not do.** It never reaches behind
    /// `consumed`: a cell already dispatched has run, and a prose segment
    /// already emitted has reached the person, so neither can be unsaid — if
    /// the tag turns up before that line the leak is kept and the reply is
    /// merely ugly. And it leaves a *matched* `<think>`/`</think>` pair
    /// alone — a model quoting the tags, not a provider emitting one. See
    /// [`strip_leaked_reasoning`] for why the opener, and not the fence
    /// around it, is what tells those apart.
    fn drop_leaked_reasoning(&mut self) {
        strip_leaked_reasoning(&mut self.reply, self.stream.consumed());
    }

    /// The completion is over: hand back the trailing prose, and let the run
    /// close once the queue drains.
    pub fn end(&mut self) -> Vec<Piece> {
        self.end_truncated(false)
    }

    /// The completion is over because the token budget ran out. The cells
    /// that closed stand; the half-written one after them was never a cell.
    pub fn end_truncated(&mut self, truncated: bool) -> Vec<Piece> {
        let pieces = self.stream.finish(&self.reply);
        self.queue(&pieces);
        self.ended = true;
        self.truncated = truncated;
        pieces
    }

    /// Whether the completion was cut off.
    pub fn was_truncated(&self) -> bool {
        self.truncated
    }

    fn queue(&mut self, pieces: &[Piece]) {
        self.queued.extend(pieces.iter().cloned());
    }

    /// The next piece to act on, in source order. `None` means the queue has
    /// drained — which is "wait for more" while the reply is still arriving
    /// ([`is_ended`](Self::is_ended) says which).
    pub fn take_piece(&mut self) -> Option<Piece> {
        self.queued.pop_front()
    }

    /// Whether the completion has finished arriving.
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    /// The completion so far.
    pub fn reply(&self) -> &str {
        &self.reply
    }

    /// How many executable cells the reply has held so far. Zero at the end
    /// is the cell-less reply D4 is about, and the drift metric 25.8 counts.
    pub fn cell_count(&self) -> usize {
        self.stream.cells().len()
    }

    /// The source of cell `i` — what its `Turn` records (D15).
    /// Cell `i` **with its fences** — the bytes that go on the log as a
    /// `Part::Cell`, so that the parts of a reply concatenate back to it
    /// (28).
    pub fn cell_outer(&self, i: usize) -> String {
        let c = self.stream.cells()[i];
        self.reply[c.outer_start..c.outer_end].to_owned()
    }

    pub fn cell_source(&self, i: usize) -> String {
        self.stream.cells()[i].slice(&self.reply).to_string()
    }

    /// A raw VM span, as an offset into the **reply** (28).
    ///
    /// It used to return a *cell-local* offset, and everything that
    /// rendered a site then had to put the cell's own start back — a
    /// `Cut` shift on every call, in `document.rs`, computed by
    /// re-splitting the reply. A site is an offset into the reply now,
    /// which is the text a diagnostic quotes and the text a snip cuts,
    /// so there is nothing left to rebase between.
    ///
    /// The one subtraction that survives is the prelude's: the VM's
    /// buffer is the prelude and then the reply, and only the reply is
    /// anything the model wrote.
    pub fn rebase_site(&self, raw: u32) -> u32 {
        (raw as usize).saturating_sub(self.base) as u32
    }

    /// Compile cell `i` into `vm`.
    ///
    /// Each cell is compiled from a buffer as long as the reply so far, with
    /// only its own bytes live (D2), so the spans it emits are offsets into
    /// the unit and the analysis tables accumulate across cells instead of
    /// colliding.
    pub fn feed_cell(&mut self, vm: &mut interp::VM, i: usize) -> Result<(), String> {
        let buffer = self.buffer_for(self.stream.cells()[i]);
        self.core
            .push(vm, &buffer)
            .map_err(|diags| render_cell_diags(&buffer, &diags))
    }

    /// Append the run's `Return(0)` epilogue: the reply is over, so the next
    /// step unwinds the frame and reports `Done` on exactly the path a
    /// one-shot program takes.
    pub fn close(&mut self, vm: &mut interp::VM) -> Result<(), String> {
        let buffer = self.blank_buffer();
        self.core
            .close(vm, &buffer)
            .map_err(|diags| render_cell_diags(&buffer, &diags))
    }

    /// The parse buffer with `cell` live: the prelude region blanked, the
    /// reply blanked but for this cell, and every newline in place.
    fn buffer_for(&self, cell: CellSpan) -> String {
        let mut buf = self.blank_buffer();
        buf.replace_range(
            self.base + cell.start..self.base + cell.end,
            cell.slice(&self.reply),
        );
        buf
    }

    /// The same buffer with nothing live at all.
    fn blank_buffer(&self) -> String {
        let mut buf = String::with_capacity(self.base + self.reply.len());
        buf.push_str(&self.base_blank);
        buf.extend(
            self.reply
                .bytes()
                .map(|b| if b == b'\n' { '\n' } else { ' ' }),
        );
        buf
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
/// One piece of a reply, in source order (D1).
///
/// A reply decomposes into prose segments and cells. Each prose segment is a
/// message to the person; each cell is JavaScript to run. The fences between
/// them belong to neither and are stored nowhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    /// Text between two fences, **verbatim** — every byte, including the
    /// blank lines that separated it from them.
    ///
    /// It used to arrive trimmed, which is fine for sending to a person
    /// and fatal for the log: 28's whole design rests on the parts of a
    /// reply concatenating, byte for byte, back to the completion that
    /// produced them, and a trim makes that false on the first blank
    /// line. What to *show* someone is the renderer's business;
    /// [`Piece::visible`] is where the trim lives now.
    Prose(String),
    /// An executable cell, by index into the reply's cells.
    Cell(usize),
}

impl Piece {
    /// A prose piece as a person should see it: trimmed. `None` when
    /// there is nothing but whitespace, which is a piece worth logging
    /// and not worth sending.
    pub fn visible(&self) -> Option<&str> {
        match self {
            Piece::Prose(text) => {
                let t = text.trim();
                (!t.is_empty()).then_some(t)
            }
            Piece::Cell(_) => None,
        }
    }
}

/// The streaming splitter: hands back each piece of a reply **the moment it
/// is complete** (D11).
///
/// A cell is complete when its closing fence arrives, which is decidable at
/// the line level with no parsing — three or more backticks at column 0. That
/// is the property phase 24 could never get from a JS expression, where
/// `tell("a")` might still become `tell("a").then(...)`.
///
/// A prose segment is complete when the cell after it opens, or when the reply
/// ends. So prose is delivered a little behind the person's reading — it lands
/// when the model starts the next code block — while the *rendering* of it is
/// already live through the TUI's own chunk buffer. What this produces is the
/// log, not the screen.
///
/// Re-scanning the accumulated reply on each advance is quadratic over the
/// whole completion. That is the trade phase 24 already accepted in writing:
/// a few KB against a scanner that does no parsing at all.
pub struct Stream {
    /// Bytes of the reply already handed out as pieces.
    consumed: usize,
    /// Cells already handed out.
    emitted_cells: usize,
    /// Every cell seen so far, refreshed on each advance.
    cells: Vec<CellSpan>,
}

impl Default for Stream {
    fn default() -> Self {
        Self::new()
    }
}

impl Stream {
    pub fn new() -> Self {
        Self {
            consumed: 0,
            emitted_cells: 0,
            cells: Vec::new(),
        }
    }

    /// Every cell recognised in the reply so far.
    pub fn cells(&self) -> &[CellSpan] {
        &self.cells
    }

    /// Bytes already handed out as pieces — the line behind which nothing
    /// can be taken back (see `Notebook::drop_leaked_reasoning`).
    pub fn consumed(&self) -> usize {
        self.consumed
    }

    /// Pieces that have become complete since the last call, given the reply
    /// as it stands now.
    ///
    /// `reply` must be the accumulated completion from the start, not just the
    /// newest chunk: a fence can straddle a chunk boundary, and the only way
    /// to be sure of a piece is to look at the whole thing.
    pub fn advance(&mut self, reply: &str) -> Vec<Piece> {
        // **Only complete lines.** A fence is a line, and
        // `split_inclusive` hands back the last one *without* its
        // newline when the buffer stops mid-line — so a closing fence
        // seen before its newline arrives ends the cell one byte early,
        // and that byte then turns up at the head of the next prose
        // piece. Streamed and batched then decompose the same reply into
        // different bytes, which the concatenation invariant (28)
        // forbids and the old `trim` used to hide.
        let committed = match reply.rfind('\n') {
            Some(i) => &reply[..i + 1],
            None => "",
        };
        self.scan(reply, committed)
    }

    /// [`advance`](Self::advance) over an explicit committed prefix.
    fn scan(&mut self, reply: &str, committed: &str) -> Vec<Piece> {
        self.cells = split_cells(committed);
        let mut out = Vec::new();
        while self.emitted_cells < self.cells.len() {
            let cell = self.cells[self.emitted_cells];
            // The prose between wherever we stopped and this cell's
            // opening fence.
            if let Some(text) = prose_between(reply, self.consumed, cell.outer_start) {
                out.push(Piece::Prose(text));
            }
            out.push(Piece::Cell(self.emitted_cells));
            self.consumed = cell.outer_end;
            self.emitted_cells += 1;
        }
        out
    }

    /// The reply is over: hand back the trailing prose, if any.
    ///
    /// Separate from [`advance`](Self::advance) because a trailing segment is
    /// only known to be complete once the completion ends — until then the
    /// model may still be part-way through a sentence, or about to open
    /// another fence.
    pub fn finish(&mut self, reply: &str) -> Vec<Piece> {
        // **The whole reply is committed now**, last line included.
        // `advance` stops at the final newline because a line still
        // being written may yet turn into a fence; here there is no
        // "yet". A reply whose closing ``` is the last thing the
        // provider sent — no trailing newline — used to come back as
        // one prose piece spanning the entire reply, fences and all:
        // the cell was never recognised, so the code never ran and the
        // person was shown the source as if the model had only talked
        // about it. Seen live on 2026-09-19.
        //
        // Safe because `split_cells` closes a cell only on a closing
        // fence, so a reply truncated mid-block still yields no cell.
        let mut out = self.scan(reply, reply);
        if let Some(text) = prose_between(reply, self.consumed, reply.len()) {
            out.push(Piece::Prose(text));
        }
        self.consumed = reply.len();
        out
    }
}

/// The prose in `reply[start..end]`, or `None` when there is nothing but
/// whitespace there.
///
/// Trimmed, because the blank line separating a paragraph from the fence below
/// it is markdown punctuation rather than part of the message. An all-blank
/// gap between two adjacent cells is not a message at all.
fn prose_between(reply: &str, start: usize, end: usize) -> Option<String> {
    (start < end).then(|| reply[start..end].to_owned())
}

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
    /// Start of the opening fence *line*, and one past the end of the
    /// closing fence line (including its newline, when it has one).
    ///
    /// The cell's outer extent, which is what the prose around it is
    /// measured against: a prose segment runs from one cell's
    /// `outer_end` to the next one's `outer_start`. The fences
    /// themselves belong to neither piece and are stored nowhere — the
    /// reply is recoverable in content and order, not byte-for-byte
    /// (D1).
    pub outer_start: usize,
    pub outer_end: usize,
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
    /// Byte offset of the opening fence line's first character.
    outer_start: usize,
}

/// What a provider writes when it has been leaking its thinking into the
/// reply channel and has stopped.
const CLOSE_THINK: &str = "</think>";
/// Its opener. Present only when the **model** wrote both: a provider
/// leak has no opener to leak, the thinking before it having gone out on
/// `reasoning_content` where it belonged.
const OPEN_THINK: &str = "<think>";

/// [`Notebook::drop_leaked_reasoning`]'s rule, over the two values it
/// actually depends on: the reply so far, and the line behind which
/// nothing can be taken back. A free function because that is the whole
/// of it — no VM, no compiler, nothing a test has to stand up first.
///
/// **The discriminator is a missing opener, not a fence.** The first cut
/// of this refused to strip a tag inside a fenced block, on the grounds
/// that a model quoting `</think>` in a quoted block must not lose its
/// reply. That guard defeated the fix outright, and the test said so:
/// the live leak *contained a stray fence*, so the tag it closed with
/// looked quoted by the very thing that made it worth repairing.
///
/// What separates the two is the opener. A model writing about these
/// tags writes both; a provider emits only the closer. So a `<think>`
/// earlier in the reply means quoting and nothing is touched, and a lone
/// closer is an artefact.
///
/// Returns whether anything was dropped.
fn strip_leaked_reasoning(reply: &mut String, from: usize) -> bool {
    let Some(rel) = reply[from..].find(CLOSE_THINK) else {
        return false;
    };
    let at = from + rel;
    if reply[..at].contains(OPEN_THINK) {
        return false; // a matched pair: the model is quoting them
    }
    reply.replace_range(from..at + CLOSE_THINK.len(), "");
    true
}

/// Is this info string one that executes? Exactly `js`, `javascript`,
/// `ts` or `typescript`, lower case (D3). Everything else — `text`,
/// `rust`, `JS`, and a bare fence with no info string at all — is prose
/// quoting code, and is left for the person to read.
///
/// **`ts` runs because the compiler reads it.** `interp::compile`
/// parses TypeScript and erases what erases, so a block tagged `ts` is
/// the same program as the one tagged `js`; refusing it would mean a
/// reply that silently did nothing, on a distinction the model cannot
/// see the consequences of. The card says plainly that all four run and
/// that JavaScript is preferred — nothing here checks a type, so an
/// annotation is a comment that costs tokens.
///
/// The cost is real and accepted: a model working in a TypeScript
/// codebase has an honest reason to *quote* TypeScript, and that quote
/// now runs. The mitigation is the card, not the parser — a quoted
/// block is `text` and always was.
fn executes(info: &str) -> bool {
    matches!(info, "js" | "javascript" | "ts" | "typescript")
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
                            outer_start: fence.outer_start,
                            outer_end: offset,
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
                    outer_start: line_start,
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

    /// A reply as the streaming notebook sees it: text accumulates, the
    /// leak repair runs on every push, and the scanner re-reads what is
    /// left. Exactly `Notebook::push_text` without the VM under it.
    struct Reply {
        text: String,
        stream: Stream,
    }

    impl Reply {
        fn new() -> Self {
            Reply {
                text: String::new(),
                stream: Stream::new(),
            }
        }
        fn push(&mut self, chunk: &str) {
            self.text.push_str(chunk);
            strip_leaked_reasoning(&mut self.text, self.stream.consumed());
            self.stream.advance(&self.text);
        }
        fn cells(&self) -> Vec<&str> {
            self.stream
                .cells()
                .iter()
                .map(|c| c.slice(&self.text))
                .collect()
        }
    }

    /// **The live shape, from the run it cost a cell.** A reply streams
    /// two cells, then the provider leaks its thinking into the reply
    /// channel — carrying a stray fence with it — and closes with
    /// `</think>`. That stray fence opened a quote block, so the third
    /// `js` block became its contents and never ran. Dropping the leak
    /// puts the fence state back and the third cell is a cell again.
    #[test]
    fn a_leaked_reasoning_span_does_not_swallow_the_cell_after_it() {
        let mut r = Reply::new();
        r.push("First.\n\n```js\nconst a = 1;\n```\n");
        r.push("Second.\n\n```js\nconst b = 2;\n```\n");
        assert_eq!(r.cells().len(), 2, "two cells before the leak");
        r.push("\n```\n\nI see output. We're writing, need continue.\n\nLet's run.</think>\n\n");
        r.push("```js\nconst c = 3;\n```\n\nDone.\n");
        assert_eq!(
            r.cells(),
            vec!["const a = 1;\n", "const b = 2;\n", "const c = 3;\n"],
            "the cell after the leak is still a cell: {:?}",
            r.text
        );
        assert!(
            !r.text.contains("</think>") && !r.text.contains("I see output"),
            "and the leak is gone from what gets logged and replayed: {:?}",
            r.text
        );
    }

    /// **A model quoting the tags is not a provider emitting one**, and
    /// this is the false positive that would cost a whole reply. The
    /// opener says which: a model writing about these tags writes both,
    /// and a matched pair is left exactly alone.
    #[test]
    fn a_matched_pair_is_the_model_quoting_and_is_kept() {
        let mut r = Reply::new();
        r.push("The provider sends this:\n\n```text\n<think>…</think>\n```\n\n");
        assert!(
            r.text.contains("</think>"),
            "quoted, not stripped: {:?}",
            r.text
        );
        r.push("```js\nconst a = 1;\n```\n");
        assert_eq!(
            r.cells(),
            vec!["const a = 1;\n"],
            "and the cell after it still runs"
        );
    }

    /// Nothing already handed out is taken back. A cell that has been
    /// dispatched has run and a prose segment that has been emitted has
    /// reached the person, so a tag arriving behind that line leaves the
    /// reply ugly rather than rewriting history.
    #[test]
    fn the_repair_never_reaches_behind_what_was_already_emitted() {
        let mut r = Reply::new();
        r.push("Before.\n\n```js\nconst a = 1;\n```\n");
        let before = r.text.clone();
        r.push("leaked thinking</think>\nafter\n");
        assert!(
            r.text.starts_with(&before),
            "everything already emitted is untouched: {:?}",
            r.text
        );
        assert!(!r.text.contains("leaked thinking"), "{:?}", r.text);
        assert!(r.text.contains("after"), "{:?}", r.text);
    }

    /// No tag, no repair — the overwhelmingly common case pays nothing
    /// and is changed in no way.
    #[test]
    fn an_ordinary_reply_is_untouched() {
        let md = "Prose.\n\n```js\nconst a = 1;\n```\n\nMore.\n";
        let mut text = md.to_owned();
        assert!(!strip_leaked_reasoning(&mut text, 0));
        assert_eq!(text, md);
    }

    /// D3: executing is what every turn does, so it is untagged;
    /// quoting is the marked case. Anything that is not one of the four
    /// dialect tags is for the person to read.
    #[test]
    fn a_non_executable_tag_is_not_a_cell() {
        for tag in ["text", "rust", "python", "jsx", "JS", "tsx"] {
            let md = format!("```{tag}\nconst x = 1;\n```\n");
            assert!(
                split_cells(&md).is_empty(),
                "```{tag} must not execute, but it produced a cell"
            );
        }
    }

    /// All four dialect tags run. `ts` and `typescript` reach the same
    /// compiler as `js` — it parses TypeScript and erases the types —
    /// so refusing them would have meant a reply that silently did
    /// nothing on a distinction with no visible consequence.
    #[test]
    fn every_dialect_tag_is_a_cell() {
        for tag in ["js", "javascript", "ts", "typescript"] {
            let md = format!("```{tag}\nconst x: number = 1;\n```\n");
            assert_eq!(
                split_cells(&md).len(),
                1,
                "```{tag} must execute, but it produced no cell"
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
        let md = "````markdown\n```js\ntell(\"ok\"); finish();\n```\n````\n";
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
        let md = "```js\ntell(\"ok\"); finish();\n```";
        assert_eq!(cells_of(md), vec!["tell(\"ok\"); finish();\n"]);
    }

    // --- the streaming splitter (D11, D15) ---

    /// Feed a reply one byte at a time and collect the pieces, which is the
    /// worst case a real stream can produce: every fence straddles a chunk.
    fn pieces_byte_by_byte(reply: &str) -> Vec<Piece> {
        let mut stream = Stream::new();
        let mut out = Vec::new();
        for (i, _) in reply.char_indices() {
            out.extend(stream.advance(&reply[..i]));
        }
        out.extend(stream.finish(reply));
        out
    }

    /// The shape from the doc: prose, cell, prose, cell — in source order.
    #[test]
    fn a_reply_decomposes_into_its_pieces_in_source_order() {
        let reply = "Both files claim to own the retry policy.\n\n\
                     ```js\nconst a = 1;\n```\n\n\
                     `retry.rs` is the newer of the two.\n\n\
                     ```js\ntell(\"ok\"); finish();\n```\n";
        let mut stream = Stream::new();
        let pieces = stream.finish(reply);
        assert_eq!(
            pieces,
            vec![
                Piece::Prose("Both files claim to own the retry policy.\n\n".into()),
                Piece::Cell(0),
                Piece::Prose("\n`retry.rs` is the newer of the two.\n\n".into()),
                Piece::Cell(1),
            ]
        );
    }

    /// **A cell is complete the moment its fence closes**, not when the
    /// completion ends (D11) — so it comes back from `advance`, with the
    /// reply still arriving.
    #[test]
    fn a_cell_lands_as_soon_as_its_closing_fence_arrives() {
        let mut stream = Stream::new();
        assert!(
            stream
                .advance("Some prose first.\n\n```js\nconst a = 1;")
                .is_empty()
        );
        // The closing fence completes both the prose before it and the cell.
        let pieces = stream.advance("Some prose first.\n\n```js\nconst a = 1;\n```\n");
        assert_eq!(
            pieces,
            vec![Piece::Prose("Some prose first.\n\n".into()), Piece::Cell(0)]
        );
        // And it is not handed out a second time.
        assert!(
            stream
                .advance("Some prose first.\n\n```js\nconst a = 1;\n```\n\nmore")
                .is_empty()
        );
    }

    /// Trailing prose is only complete when the reply is: until then the
    /// model may still be mid-sentence, or about to open another fence.
    #[test]
    fn trailing_prose_waits_for_the_end_of_the_reply() {
        let reply = "```js\ntell(\"ok\"); finish();\n```\n\nThat is everything.\n";
        let mut stream = Stream::new();
        assert_eq!(stream.advance(reply), vec![Piece::Cell(0)]);
        assert_eq!(
            stream.finish(reply),
            vec![Piece::Prose("\nThat is everything.\n".into())]
        );
    }

    /// **The invariant 28 rests on: the pieces of a reply concatenate,
    /// byte for byte, back to the reply.**
    ///
    /// Nothing has to be reassembled because nothing was taken apart —
    /// but only while this holds. It was false twice on the way here: a
    /// `trim` on every prose piece, and a closing fence recognised
    /// before its newline arrived, which made a *streamed* reply
    /// decompose differently from the same bytes handed over whole.
    #[test]
    fn the_pieces_concatenate_back_to_the_reply() {
        let replies = [
            "\n\n  Leading blank lines.\n\n```js\nlet a = 1;\n```\n\n\nTrailing.   \n\n",
            "```js\nlet a = 1;\n```\n```js\nlet b = 2;\n```\n",
            "No cells at all.\n",
            "```js\nonly();\n```\n",
            "One.\n\n```js\nlet a = 1;\n```\n\nTwo.\n\n```js\na = 2;\n```\n\nThree.\n",
            // No trailing newline: the provider's last token is the
            // closing fence itself. See
            // `a_reply_ending_on_its_closing_fence_still_has_a_cell`.
            "Looking.\n\n```js\nlet a = 1;\n```",
        ];
        for reply in replies {
            let cells = split_cells(reply);
            let rebuilt = |pieces: &[Piece]| -> String {
                pieces
                    .iter()
                    .map(|p| match p {
                        Piece::Prose(t) => t.clone(),
                        Piece::Cell(i) => {
                            reply[cells[*i].outer_start..cells[*i].outer_end].to_owned()
                        }
                    })
                    .collect()
            };
            let mut whole = Stream::new();
            assert_eq!(rebuilt(&whole.finish(reply)), reply, "whole: {reply:?}");
            assert_eq!(
                rebuilt(&pieces_byte_by_byte(reply)),
                reply,
                "byte by byte: {reply:?}"
            );
        }
    }

    /// **A reply whose last token is its closing fence still has a
    /// cell.** `advance` commits only to the final newline, because a
    /// line still being written may yet turn into a fence — but when
    /// the reply *ends*, there is no "yet", and `finish` has to say so.
    ///
    /// Seen live on 2026-09-19: the model wrote a paragraph and one
    /// `js` block, the provider stopped on the closing ``` with no
    /// newline after it, and the whole reply came back as a single
    /// prose piece. The code never ran; the person was shown the
    /// source as though the model had only talked about it.
    #[test]
    fn a_reply_ending_on_its_closing_fence_still_has_a_cell() {
        let reply = "Looking first.\n\n```js\nconst x = await tools.bash(\"ls\");\n```";
        for pieces in [Stream::new().finish(reply), pieces_byte_by_byte(reply)] {
            assert!(
                pieces.iter().any(|p| matches!(p, Piece::Cell(_))),
                "the block is a cell, newline or not: {pieces:?}"
            );
        }
    }

    /// And a reply cut off *inside* a block is not one: the fence never
    /// closed, so there is nothing to run, only text.
    #[test]
    fn a_reply_cut_off_inside_a_block_has_no_cell() {
        let reply = "Looking first.\n\n```js\nconst x = await tools.bash(\"l";
        let pieces = Stream::new().finish(reply);
        assert!(
            !pieces.iter().any(|p| matches!(p, Piece::Cell(_))),
            "{pieces:?}"
        );
    }

    /// A fence straddling chunk boundaries is still recognised exactly once,
    /// which is why `advance` takes the whole accumulated reply.
    #[test]
    fn pieces_are_the_same_however_the_chunks_fall() {
        let reply = "One.\n\n```js\nlet a = 1;\n```\n\nTwo.\n\n```js\na = 2;\n```\n\nThree.\n";
        let mut whole = Stream::new();
        assert_eq!(pieces_byte_by_byte(reply), whole.finish(reply));
    }

    /// Two adjacent cells with only blank space between them still
    /// produce a prose piece — the bytes are on the log, because the
    /// parts must concatenate — but it is not a *message*: `visible()`
    /// is `None` and nobody is sent an empty line.
    #[test]
    fn nothing_but_whitespace_between_cells_is_not_a_message() {
        let reply = "```js\nlet a = 1;\n```\n\n```js\na = 2;\n```\n";
        let mut stream = Stream::new();
        let pieces = stream.finish(reply);
        assert_eq!(
            pieces,
            vec![Piece::Cell(0), Piece::Prose("\n".into()), Piece::Cell(1)]
        );
        assert_eq!(pieces[1].visible(), None, "whitespace is not a message");
    }

    /// A reply with no cells is all prose, delivered when it ends (D4).
    #[test]
    fn a_cell_less_reply_is_one_prose_piece() {
        let reply = "The retry policy already lives in `retry.rs`.\n";
        let mut stream = Stream::new();
        assert!(stream.advance(reply).is_empty());
        assert_eq!(
            stream.finish(reply),
            vec![Piece::Prose(
                "The retry policy already lives in `retry.rs`.\n".into()
            )]
        );
    }

    /// A truncated reply: the cell that closed stands, and the half-written
    /// one after it is not a cell at all (D11's partial progress).
    #[test]
    fn a_truncated_reply_keeps_the_cells_that_closed() {
        let reply =
            "```js\nconsole.log(\"ran\");\n```\n\nNext I will\n\n```js\nawait tools.read_fi";
        let mut stream = Stream::new();
        let pieces = stream.finish(reply);
        assert_eq!(
            pieces,
            vec![
                Piece::Cell(0),
                // The prose after the closed cell still lands; the
                // unterminated fence and its contents do not.
                Piece::Prose("\nNext I will\n\n```js\nawait tools.read_fi".into()),
            ]
        );
        assert_eq!(stream.cells().len(), 1);
    }

    /// A multi-paragraph report comes back as one message, with its internal
    /// blank lines intact — this is the case the whole phase exists for.
    #[test]
    fn a_multi_paragraph_report_is_one_prose_piece() {
        let reply = "# Findings\n\nThe first thing I noticed.\n\n\
                     - one\n- two\n\nAnd the conclusion.\n";
        let mut stream = Stream::new();
        let pieces = stream.finish(reply);
        let Piece::Prose(text) = &pieces[0] else {
            panic!("expected prose, got {pieces:?}");
        };
        assert!(text.starts_with("# Findings"));
        assert!(text.contains("\n\n- one\n- two\n\n"));
        assert!(
            text.ends_with("And the conclusion.\n"),
            "verbatim: {text:?}"
        );
        assert_eq!(pieces.len(), 1);
    }

    /// **A cell's top-level `return` ends the program**, and a
    /// function inside a cell returns from itself as it always did.
    /// The reply is one program across its cells — one scope, one
    /// frame — so returning from that frame ends the reply, which is
    /// the least surprising reading of the primitive and what
    /// `stop(reason)` used to spell.
    #[test]
    fn a_cell_can_return_and_so_can_a_function_in_one() {
        let md = "```js\nfunction f() { return 1; }\nconsole.log(f());\n```\n\n\
                  ```js\nreturn { done: true };\n```\n";
        let cells = split_cells(md);
        assert_eq!(cells.len(), 2);

        let mut pb = ParseBuffer::new(md);
        let mut repl = interp::Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();

        repl.push(pb.focus(cells[0]))
            .expect("a function inside a cell may return");
        repl.vm.step(u64::MAX).unwrap();
        assert_eq!(repl.vm.console_lines, vec!["1"]);

        repl.push(pb.focus(cells[1]))
            .expect("and so may the cell itself");
        assert!(
            matches!(repl.vm.step(u64::MAX), Ok(interp::StepResult::Done { .. })),
            "a top-level return ends the program"
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
