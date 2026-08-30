//! Context-aware escaping for serialized inline text.
//!
//! pulldown-cmark resolves backslash escapes and HTML entities *into*
//! `Event::Text`, so `Inline::Text` holds the literal characters an author
//! wanted, not the spelling they used. Emitting that text verbatim turns
//! `\*not em\*` into live emphasis and `&lt;div&gt;` into an HTML block. The
//! serializer therefore has to put escapes back — but only the ones that are
//! load-bearing at the position the character lands, or every `td fmt` would
//! litter clean prose with backslashes.
//!
//! [`InlineBuf`] collects a serialized inline run as characters tagged with
//! whether they are author text (escapable) or serializer-emitted markup
//! (never escaped). Tagging rather than escaping at push time is what makes the
//! decisions context-aware: by the time [`InlineBuf::finish`] runs, every
//! character knows its real neighbours, including the ones contributed by
//! sibling inlines.

/// Where a serialized inline run lands, which decides what has to be escaped.
#[derive(Clone, Copy, Debug, Default)]
pub struct EscapeCtx {
    /// The run's first character starts an output line, so block openers
    /// (`#`, `-`, `>`, …) would take effect. True for paragraphs and list
    /// items, false for headings and table cells. Characters after a
    /// `SoftBreak` are line-initial regardless.
    pub line_start: bool,
    /// Inside a table cell, where an unescaped `|` splits the row.
    pub table_cell: bool,
    /// Inside an ATX heading, where a trailing `#` run is a closing sequence.
    pub heading: bool,
    /// A wrapped line of this run is a line of the document in its own right,
    /// rather than a *lazy* continuation of the paragraph.
    ///
    /// The distinction decides whether a delimiter row can open a table:
    /// pulldown-cmark doesn't run the table extension over lazy lines. It is
    /// false only where the serializer deliberately pins a wrapped run to
    /// column 0 to keep it lazy — see `push_inlines_indented`.
    pub real_lines: bool,
}

impl EscapeCtx {
    /// Paragraph and list-item content: line-initial, block openers matter.
    pub fn paragraph(real_lines: bool) -> Self {
        Self {
            line_start: true,
            real_lines,
            ..Self::default()
        }
    }

    /// Heading content: follows `# `, so only the closing sequence matters.
    pub fn heading() -> Self {
        Self {
            heading: true,
            ..Self::default()
        }
    }

    /// Table cell content: `|` is the cell delimiter.
    pub fn table_cell() -> Self {
        Self {
            table_cell: true,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy)]
struct OutChar {
    ch: char,
    /// Author text: may be escaped. Markup the serializer emitted may not.
    literal: bool,
    /// Inside link or image text, where `[` and `]` close the label early.
    link_text: bool,
}

/// A serialized inline run, built up before escapes are decided.
#[derive(Default)]
pub struct InlineBuf {
    chars: Vec<OutChar>,
}

impl InlineBuf {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push serializer-generated markup: delimiters, brackets, URLs, code
    /// spans, raw HTML. Emitted verbatim.
    pub fn push_markup(&mut self, s: &str) {
        self.chars.extend(s.chars().map(|ch| OutChar {
            ch,
            literal: false,
            link_text: false,
        }));
    }

    /// Push author text, escaped as needed so it re-parses as literal text.
    pub fn push_text(&mut self, s: &str, link_text: bool) {
        self.chars.extend(s.chars().map(|ch| OutChar {
            ch,
            literal: true,
            link_text,
        }));
    }

    /// Render the run with no escaping at all. The caller keeps this spelling
    /// when it happens to re-parse correctly on its own, which is what keeps
    /// documents that were already stable byte-identical.
    pub fn finish_plain(&self) -> String {
        self.chars.iter().map(|c| c.ch).collect()
    }

    /// Resolve escapes and render the run.
    pub fn finish(&self, ctx: EscapeCtx) -> String {
        let n = self.chars.len();
        let mut esc = vec![false; n];

        self.mark_emphasis_runs(&mut esc);
        for (i, flag) in esc.iter_mut().enumerate() {
            if self.chars[i].literal && self.needs_escape(i, ctx) {
                *flag = true;
            }
        }
        self.mark_line_openers(&mut esc, ctx);
        self.mark_heading_close(&mut esc, ctx);

        let mut out = String::with_capacity(n);
        for (i, c) in self.chars.iter().enumerate() {
            if esc[i] {
                out.push('\\');
            }
            out.push(c.ch);
        }
        out
    }

    fn ch(&self, i: usize) -> Option<char> {
        self.chars.get(i).map(|c| c.ch)
    }

    fn prev_ch(&self, i: usize) -> Option<char> {
        if i == 0 {
            None
        } else {
            self.ch(i - 1)
        }
    }

    /// Whether the character at `i` is the first on its output line.
    fn at_line_start(&self, i: usize, ctx: EscapeCtx) -> bool {
        match self.prev_ch(i) {
            None => ctx.line_start,
            Some(c) => c == '\n',
        }
    }

    /// Escapes that depend only on the character and what follows it.
    fn needs_escape(&self, i: usize, ctx: EscapeCtx) -> bool {
        let c = self.chars[i];
        match c.ch {
            // A backslash before punctuation is itself an escape, and one at
            // end of line is a hard break. At the very end of the run nothing
            // follows it, so it stays literal.
            '\\' => self
                .ch(i + 1)
                .is_some_and(|n| n.is_ascii_punctuation() || n == '\n'),
            // Any backtick can pair with another one and open a code span.
            '`' => true,
            '<' => self.markup_ahead(i),
            // `&` only matters when a full entity reference follows.
            '&' => self.entity_ahead(i),
            '|' => ctx.table_cell,
            '[' => c.link_text || self.label_close_ahead(i),
            ']' => c.link_text,
            _ => false,
        }
    }

    /// Whether the `<` at `i` opens a raw HTML tag or an autolink. Prose like
    /// `a < b` or `generic <K, V> pair` is inert and stays unescaped.
    fn markup_ahead(&self, i: usize) -> bool {
        // Both forms need a closing `>` before the next `<`.
        let mut end = i + 1;
        loop {
            match self.ch(end) {
                None | Some('<') => return false,
                Some('>') => break,
                Some(_) => end += 1,
            }
        }
        let inner: Vec<char> = (i + 1..end).map(|k| self.chars[k].ch).collect();
        let Some(&first) = inner.first() else {
            return false;
        };
        // Comment, processing instruction, declaration, CDATA.
        if matches!(first, '!' | '?') {
            return true;
        }
        // Open or closing tag: a name, then the tag's end or its attributes.
        let name_start = usize::from(first == '/');
        let mut j = name_start;
        while inner
            .get(j)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '-')
        {
            j += 1;
        }
        let named = j > name_start && inner[name_start].is_ascii_alphabetic();
        if named && inner.get(j).is_none_or(|c| *c == '/' || c.is_whitespace()) {
            return true;
        }
        // Autolink: `scheme:rest` or `local@domain`, with no spaces.
        !inner.iter().any(|c| c.is_whitespace()) && inner.iter().any(|c| matches!(c, ':' | '@'))
    }

    /// Whether `&` at `i` opens something pulldown-cmark would decode.
    fn entity_ahead(&self, i: usize) -> bool {
        let mut j = i + 1;
        if self.ch(j) == Some('#') {
            j += 1;
        }
        let start = j;
        while self.ch(j).is_some_and(|c| c.is_ascii_alphanumeric()) {
            j += 1;
        }
        j > start && j - start <= 32 && self.ch(j) == Some(';')
    }

    /// Whether a `]` followed by `(`, `[`, or `:` appears later in the run,
    /// which is what turns a literal `[` back into a link, image, or reference
    /// definition.
    fn label_close_ahead(&self, i: usize) -> bool {
        (i + 1..self.chars.len())
            .any(|j| self.ch(j) == Some(']') && matches!(self.ch(j + 1), Some('(' | '[' | ':')))
    }

    /// Escape `*`/`_` runs that could open or close emphasis. Whole runs are
    /// considered at once — including delimiters the serializer emitted, since
    /// `**bold**` immediately followed by a literal `*` forms one run.
    fn mark_emphasis_runs(&self, esc: &mut [bool]) {
        let n = self.chars.len();
        let mut i = 0;
        while i < n {
            let c = self.chars[i].ch;
            if c != '*' && c != '_' {
                i += 1;
                continue;
            }
            let mut j = i;
            while j < n && self.chars[j].ch == c {
                j += 1;
            }
            if is_delimiter_run(c, self.prev_ch(i), self.ch(j)) {
                for (k, flag) in esc.iter_mut().enumerate().take(j).skip(i) {
                    if self.chars[k].literal {
                        *flag = true;
                    }
                }
            }
            i = j;
        }
    }

    /// Escape line-initial characters that would open a block: headings, list
    /// items, block quotes, thematic breaks, setext underlines, tilde fences.
    fn mark_line_openers(&self, esc: &mut [bool], ctx: EscapeCtx) {
        for i in 0..self.chars.len() {
            if !self.chars[i].literal || !self.at_line_start(i, ctx) {
                continue;
            }
            // An ordered list is defused at its `.`, not at its first digit.
            if self.chars[i].ch.is_ascii_digit() {
                self.mark_ordered_marker(esc, i);
            } else if self.opens_block(i, ctx) {
                esc[i] = true;
            }
        }
    }

    /// Whether the line-initial character at `i` opens a block.
    fn opens_block(&self, i: usize, ctx: EscapeCtx) -> bool {
        let c = self.chars[i].ch;
        let marker_follows =
            |len: usize| matches!(self.ch(i + len), None | Some(' ' | '\t' | '\n'));
        match c {
            '#' => {
                let len = self.run_len(i);
                len <= 6 && marker_follows(len)
            }
            '>' => true,
            '+' => marker_follows(1),
            // A run of `-` under a paragraph line is a setext H2; three or more
            // of any of these alone on a line is a thematic break.
            '-' | '*' | '_' => {
                let rule = self.rule_line(i);
                (c != '_' && marker_follows(1))
                    || (c == '-' && i > 0 && (rule >= 1 || self.starts_table(i, ctx)))
                    || rule >= 3
            }
            // A run of `=` under a paragraph line is a setext H1.
            '=' => i > 0 && self.rule_line(i) >= 1,
            '~' => self.run_len(i) >= 3,
            '|' | ':' => i > 0 && self.starts_table(i, ctx),
            _ => false,
        }
    }

    /// Whether the line at `i` would become a table's delimiter row.
    ///
    /// A GFM table may interrupt a paragraph, so the second line of a
    /// header/delimiter pair inside one has to be defused. Only the second:
    /// the first is a header row only in hindsight, and by itself a line of
    /// pipes is ordinary prose.
    ///
    /// Lazy lines are exempt. The table extension doesn't run over them, which
    /// is the only reason such a paragraph could exist in the first place, and
    /// escaping there would put a backslash into every wrapped table-ish line
    /// that follows a bullet.
    fn starts_table(&self, i: usize, ctx: EscapeCtx) -> bool {
        ctx.real_lines && self.delimiter_row(i)
    }

    /// Whether the line starting at `i` reads as a GFM delimiter row: pipes,
    /// dashes, colons and spaces, with at least one dash.
    ///
    /// The column count that GFM also insists on is not checked. Getting it
    /// right means re-deriving the header row's cells through whatever inline
    /// markup sits on the line above, and the cost of guessing wrong is one
    /// backslash on a line of dashes and pipes — which is prose nobody writes
    /// except when they mean a table.
    fn delimiter_row(&self, i: usize) -> bool {
        let mut dashes = 0;
        let mut j = i;
        while let Some(ch) = self.ch(j) {
            match ch {
                '\n' => break,
                '-' => dashes += 1,
                '|' | ':' | ' ' | '\t' => {}
                _ => return false,
            }
            j += 1;
        }
        dashes > 0
    }

    /// `1.` / `1)` followed by a space opens an ordered list; escaping the
    /// delimiter is enough to defuse it.
    fn mark_ordered_marker(&self, esc: &mut [bool], i: usize) {
        let mut j = i;
        while self.ch(j).is_some_and(|c| c.is_ascii_digit()) {
            j += 1;
        }
        let digits = j - i;
        if digits > 9 || !matches!(self.ch(j), Some('.' | ')')) {
            return;
        }
        if matches!(self.ch(j + 1), None | Some(' ' | '\t' | '\n')) && self.chars[j].literal {
            esc[j] = true;
        }
    }

    /// In an ATX heading a trailing `#` run preceded by a space (or filling the
    /// whole content) is stripped as a closing sequence.
    fn mark_heading_close(&self, esc: &mut [bool], ctx: EscapeCtx) {
        if !ctx.heading || self.chars.is_empty() {
            return;
        }
        let mut start = self.chars.len();
        while start > 0 && self.chars[start - 1].ch == '#' {
            start -= 1;
        }
        if start == self.chars.len() {
            return;
        }
        let closes = start == 0 || matches!(self.ch(start - 1), Some(' ' | '\t'));
        if closes && self.chars[start].literal {
            esc[start] = true;
        }
    }

    /// Length of the run of identical characters starting at `i`.
    fn run_len(&self, i: usize) -> usize {
        let c = self.chars[i].ch;
        let mut j = i;
        while self.ch(j) == Some(c) {
            j += 1;
        }
        j - i
    }

    /// How many copies of the character at `i` the rest of the line holds, if
    /// the line contains nothing but that character and spaces (0 otherwise).
    /// Three or more make a thematic break; for `-` and `=`, one or more make a
    /// setext underline.
    fn rule_line(&self, i: usize) -> usize {
        let c = self.chars[i].ch;
        let mut count = 0;
        let mut j = i;
        while let Some(ch) = self.ch(j) {
            match ch {
                '\n' => break,
                ' ' | '\t' => {}
                ch if ch == c => count += 1,
                _ => return 0,
            }
            j += 1;
        }
        count
    }
}

fn is_punct(c: char) -> bool {
    !c.is_alphanumeric() && !c.is_whitespace()
}

/// CommonMark left-flanking: not followed by whitespace, and either not
/// followed by punctuation or preceded by whitespace/punctuation. The run's
/// missing neighbours (start and end of the line) count as whitespace.
fn is_left_flanking(prev: Option<char>, next: Option<char>) -> bool {
    let Some(next) = next.filter(|c| !c.is_whitespace()) else {
        return false;
    };
    !is_punct(next) || prev.is_none_or(|p| p.is_whitespace() || is_punct(p))
}

fn is_right_flanking(prev: Option<char>, next: Option<char>) -> bool {
    let Some(prev) = prev.filter(|c| !c.is_whitespace()) else {
        return false;
    };
    !is_punct(prev) || next.is_none_or(|n| n.is_whitespace() || is_punct(n))
}

/// Whether a run of `c` could open or close emphasis in this position. `_` has
/// the extra intraword restriction that keeps `snake_case_names` unescaped.
fn is_delimiter_run(c: char, prev: Option<char>, next: Option<char>) -> bool {
    let left = is_left_flanking(prev, next);
    let right = is_right_flanking(prev, next);
    if c == '*' {
        return left || right;
    }
    let prev_punct = prev.is_some_and(is_punct);
    let next_punct = next.is_some_and(is_punct);
    (left && (!right || prev_punct)) || (right && (!left || next_punct))
}

/// Render a link or image destination so it parses back to exactly `url`.
///
/// Spaces and angle brackets force the `<…>` form; unbalanced parens are
/// backslash-escaped in place, which keeps the common case byte-identical.
pub fn link_destination(url: &str) -> String {
    let url = escape_entities(url);
    if url.is_empty() {
        return url;
    }
    if url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '<' || c == '>')
    {
        let inner: String = url
            .chars()
            .flat_map(|c| {
                let slash = matches!(c, '<' | '>' | '\\').then_some('\\');
                slash.into_iter().chain(std::iter::once(c))
            })
            .collect();
        return format!("<{inner}>");
    }
    if parens_balanced(&url) {
        return url;
    }
    url.chars()
        .flat_map(|c| {
            let slash = matches!(c, '(' | ')' | '\\').then_some('\\');
            slash.into_iter().chain(std::iter::once(c))
        })
        .collect()
}

/// Re-escape `&` where it would otherwise be decoded as an entity. Used for
/// URLs, where backslash escapes are legal but noisier than `&amp;`.
pub fn escape_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '&' && entity_at(&chars, i) {
            out.push_str("&amp;");
        } else {
            out.push(c);
        }
    }
    out
}

fn entity_at(chars: &[char], i: usize) -> bool {
    let at = |j: usize| chars.get(j).copied();
    let mut j = i + 1;
    if at(j) == Some('#') {
        j += 1;
    }
    let start = j;
    while at(j).is_some_and(|c| c.is_ascii_alphanumeric()) {
        j += 1;
    }
    j > start && j - start <= 32 && at(j) == Some(';')
}

fn parens_balanced(url: &str) -> bool {
    let mut depth = 0i32;
    for c in url.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str, ctx: EscapeCtx) -> String {
        let mut buf = InlineBuf::new();
        buf.push_text(s, false);
        buf.finish(ctx)
    }

    fn para(s: &str) -> String {
        text(s, EscapeCtx::paragraph(false))
    }

    #[test]
    fn clean_prose_is_untouched() {
        for s in [
            "just some prose",
            "2 * 3 = 6",
            "snake_case_and_more_names",
            "a < b and c > d",
            "R&D, AT&T",
            "call foo(bar) now",
            "C:\\path\\to",
            "an [aside] in brackets",
            "5 - 3",
            "version 1.2.3",
        ] {
            assert_eq!(para(s), s, "should not escape: {s}");
        }
    }

    #[test]
    fn escapes_emphasis_delimiters() {
        assert_eq!(para("*not em*"), "\\*not em\\*");
        assert_eq!(para("_not em_"), "\\_not em\\_");
    }

    #[test]
    fn escapes_block_openers_only_at_line_start() {
        assert_eq!(para("# not heading"), "\\# not heading");
        assert_eq!(para("- x"), "\\- x");
        assert_eq!(para("> x"), "\\> x");
        assert_eq!(para("1. x"), "1\\. x");
        assert_eq!(para("---"), "\\---");
        assert_eq!(para("a\n# b"), "a\n\\# b");
        assert_eq!(text("# not heading", EscapeCtx::heading()), "# not heading");
    }

    /// Paragraph text whose wrapped lines land as real lines of the document.
    fn para_real(s: &str) -> String {
        text(s, EscapeCtx::paragraph(true))
    }

    #[test]
    fn escapes_a_delimiter_row_that_would_open_a_table() {
        assert_eq!(para_real("head\n| --- |"), "head\n\\| --- |");
        assert_eq!(para_real("head\n--- | ---"), "head\n\\--- | ---");
        assert_eq!(para_real("head\n:--- | ---:"), "head\n\\:--- | ---:");
        // Only the delimiter row: the line above it is a header row only in
        // hindsight, and a lone row of pipes is prose.
        assert_eq!(para_real("| a | b |\nplain"), "| a | b |\nplain");
        // A delimiter row needs a header above it, so the first line is safe.
        assert_eq!(para_real("| --- |\ntail"), "| --- |\ntail");
        // Nothing in a delimiter row but pipes, dashes, colons and spaces.
        assert_eq!(para_real("head\n| a | b |"), "head\n| a | b |");
        assert_eq!(para_real("head\n| : |"), "head\n| : |");
    }

    #[test]
    fn leaves_a_delimiter_row_alone_on_a_lazy_line() {
        assert_eq!(para("head\n| --- |"), "head\n| --- |");
        assert_eq!(para("head\n--- | ---"), "head\n--- | ---");
    }

    #[test]
    fn escapes_setext_underline_after_a_break() {
        assert_eq!(para("a\n==="), "a\n\\===");
        assert_eq!(para("a\n--"), "a\n\\--");
    }

    #[test]
    fn escapes_backticks_and_entity_starts() {
        assert_eq!(para("a ` b"), "a \\` b");
        assert_eq!(para("&#42;x&#42;"), "\\&#42;x\\&#42;");
        assert_eq!(para("<div>"), "\\<div>");
    }

    #[test]
    fn escapes_pipes_in_table_cells_only() {
        assert_eq!(text("a | b", EscapeCtx::table_cell()), "a \\| b");
        assert_eq!(para("a | b"), "a | b");
    }

    #[test]
    fn escapes_brackets_that_would_form_links() {
        assert_eq!(para("[y](/url)"), "\\[y](/url)");
        assert_eq!(para("[y]: /url"), "\\[y]: /url");
    }

    #[test]
    fn markup_is_never_escaped() {
        let mut buf = InlineBuf::new();
        buf.push_markup("**");
        buf.push_text("bold", false);
        buf.push_markup("**");
        assert_eq!(buf.finish(EscapeCtx::paragraph(false)), "**bold**");
    }

    #[test]
    fn heading_closing_sequence() {
        assert_eq!(text("foo #", EscapeCtx::heading()), "foo \\#");
        assert_eq!(text("foo#", EscapeCtx::heading()), "foo#");
    }

    #[test]
    fn destinations() {
        assert_eq!(link_destination("/url"), "/url");
        assert_eq!(link_destination("/url with space"), "</url with space>");
        assert_eq!(link_destination("/a(b)c"), "/a(b)c");
        assert_eq!(link_destination("/a)c"), "/a\\)c");
        assert_eq!(link_destination("/a?x=1&amp;y=2"), "/a?x=1&amp;amp;y=2");
    }
}
