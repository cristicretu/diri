//! Markdown ⇄ [`Document`] conversion at the file boundary.
//!
//! The writer emits conventional CommonMark-flavoured Markdown (plus GFM task
//! lists and strikethrough) so notes read well in any other editor. The reader
//! accepts that output exactly and the common subset people type by hand. The
//! round-trip `parse(write(doc)) == doc` is the contract the tests hold.

use crate::doc::{Block, BlockKind, Document, MAX_INDENT, Mark, Style};

/// Front matter kept alongside a note. Unknown keys are preserved verbatim.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrontMatter {
    pub fields: Vec<(String, String)>,
}

impl FrontMatter {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn set(&mut self, key: &str, value: Option<String>) {
        match value {
            Some(value) => {
                if let Some(slot) = self.fields.iter_mut().find(|(k, _)| k == key) {
                    slot.1 = value;
                } else {
                    self.fields.push((key.to_owned(), value));
                }
            }
            None => self.fields.retain(|(k, _)| k != key),
        }
    }

    pub fn flag(&self, key: &str) -> bool {
        self.get(key) == Some("true")
    }

    pub fn set_flag(&mut self, key: &str, on: bool) {
        self.set(key, on.then(|| "true".to_owned()));
    }
}

pub fn parse(source: &str) -> (FrontMatter, Document) {
    let source = source.replace("\r\n", "\n");
    let (front, body) = split_front_matter(&source);
    let mut lines: Vec<&str> = body.split('\n').collect();
    // Title: a leading `# ` line.
    let mut title = String::new();
    let mut start = 0;
    while start < lines.len() && lines[start].trim().is_empty() {
        start += 1;
    }
    if let Some(line) = lines.get(start)
        && let Some(rest) = line.strip_prefix("# ")
    {
        let (text, _) = parse_inline(rest.trim());
        title = text;
        start += 1;
    }
    lines.drain(..start);
    let blocks = parse_blocks(&lines);
    (front, Document::new(title, blocks))
}

fn split_front_matter(source: &str) -> (FrontMatter, &str) {
    let mut front = FrontMatter::default();
    let Some(rest) = source.strip_prefix("---\n") else {
        return (front, source);
    };
    let Some(end) = rest.find("\n---\n").map(|i| (i, i + 5)).or_else(|| {
        rest.strip_suffix("\n---")
            .map(|inner| (inner.len(), rest.len()))
    }) else {
        return (front, source);
    };
    for line in rest[..end.0].lines() {
        if let Some((key, value)) = line.split_once(':') {
            front
                .fields
                .push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    (front, &rest[end.1..])
}

fn parse_blocks(lines: &[&str]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        if let Some(fence) = fence_of(trimmed) {
            let info = trimmed
                .trim_start_matches(fence.as_bytes()[0] as char)
                .trim();
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with(fence) {
                code.push(lines[i]);
                i += 1;
            }
            i += 1; // closing fence
            blocks.push(Block::code(0, info, code.join("\n")));
            continue;
        }
        if is_divider(trimmed) {
            blocks.push(Block::new(0, BlockKind::Divider, ""));
            i += 1;
            continue;
        }
        if let Some((hashes, rest)) = heading(trimmed) {
            // `#` is the note title, so body headings start at `##`.
            let level = hashes.saturating_sub(1).clamp(1, 3);
            blocks.push(inline_block(BlockKind::Heading(level), rest));
            i += 1;
            continue;
        }
        if let Some((kind, rest)) = list_item(trimmed) {
            let mut block = inline_block(kind, rest);
            block.indent = indent_of(line);
            blocks.push(block);
            i += 1;
            continue;
        }
        if let Some((cells, next)) = table_at(lines, i) {
            blocks.extend(cells);
            i = next;
            continue;
        }
        if let Some((alt, src)) = image_line(trimmed) {
            blocks.push(Block::image(0, src, alt));
            i += 1;
            continue;
        }
        if let Some(rest) = quote_line(trimmed) {
            let mut text = vec![rest];
            i += 1;
            while i < lines.len()
                && let Some(rest) = quote_line(lines[i].trim_start())
            {
                text.push(rest);
                i += 1;
            }
            // `> [!NOTE]` opens a callout; Obsidian allows text after the tag.
            if let Some((tone, first)) = callout_tag(text[0]) {
                if first.is_empty() {
                    text.remove(0);
                } else {
                    text[0] = first;
                }
                blocks.push(inline_block(BlockKind::Callout(tone), &text.join("\n")));
            } else {
                blocks.push(inline_block(BlockKind::Quote, &text.join("\n")));
            }
            continue;
        }
        let mut text = vec![line.trim()];
        i += 1;
        while i < lines.len() {
            let next = lines[i].trim_start();
            if next.is_empty()
                || fence_of(next).is_some()
                || is_divider(next)
                || heading(next).is_some()
                || list_item(next).is_some()
                || quote_line(next).is_some()
                || image_line(next).is_some()
                || table_at(lines, i).is_some()
            {
                break;
            }
            text.push(lines[i].trim());
            i += 1;
        }
        blocks.push(inline_block(BlockKind::Paragraph, &text.join("\n")));
    }
    blocks
}

/// How a deliberately empty paragraph is written, so spacing survives.
const EMPTY_PARAGRAPH: &str = "&nbsp;";

fn inline_block(kind: BlockKind, source: &str) -> Block {
    if kind == BlockKind::Paragraph && source == EMPTY_PARAGRAPH {
        return Block::new(0, kind, "");
    }
    let (text, marks) = parse_inline(source);
    let mut block = Block::new(0, kind, text);
    block.marks = marks;
    block.normalize();
    block
}

fn fence_of(line: &str) -> Option<&'static str> {
    if line.starts_with("```") {
        // A backtick fence's info string cannot contain backticks, which is
        // what keeps a line-leading code span from reading as a fence.
        let info = line.trim_start_matches('`');
        (!info.contains('`')).then_some("```")
    } else if line.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn is_divider(line: &str) -> bool {
    let line = line.trim_end();
    line.len() >= 3
        && ["-", "*", "_"]
            .iter()
            .any(|c| line.chars().all(|ch| ch.to_string() == *c))
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..];
    if rest.is_empty() {
        return Some((hashes as u8, ""));
    }
    rest.strip_prefix(' ')
        .map(|rest| (hashes as u8, rest.trim()))
}

fn list_item(line: &str) -> Option<(BlockKind, &str)> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            for (task, checked) in [("[ ] ", false), ("[x] ", true), ("[X] ", true)] {
                if let Some(rest) = rest.strip_prefix(task) {
                    return Some((BlockKind::Todo { checked }, rest));
                }
            }
            for (task, checked) in [("[ ]", false), ("[x]", true), ("[X]", true)] {
                if rest == task {
                    return Some((BlockKind::Todo { checked }, ""));
                }
            }
            return Some((BlockKind::Bullet, rest));
        }
        if line == marker.trim_end() {
            return Some((BlockKind::Bullet, ""));
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=9).contains(&digits) {
        let rest = &line[digits..];
        for marker in [". ", ") "] {
            if let Some(rest) = rest.strip_prefix(marker) {
                return Some((BlockKind::Numbered, rest));
            }
        }
        if rest == "." || rest == ")" {
            return Some((BlockKind::Numbered, ""));
        }
    }
    None
}

/// `![alt](src)` alone on a line, with an optional `<…>` destination and
/// `"title"` (dropped). Anything else, including an image inside text, is
/// read as text.
fn image_line(line: &str) -> Option<(String, String)> {
    let rest = line.trim_end().strip_prefix("![")?.strip_suffix(')')?;
    // The alt text ends at the `](` whose `]` is not escaped.
    let bytes = rest.as_bytes();
    let mut split = None;
    let mut i = 0;
    while i + 1 < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b']' if bytes[i + 1] == b'(' => {
                split = Some(i);
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let split = split?;
    let alt = parse_inline(&rest[..split]).0;
    let mut dest = rest[split + 2..].trim();
    if let Some(inner) = dest.strip_prefix('<') {
        dest = inner.split_once('>').map(|(src, _)| src)?;
    } else if let Some((src, title)) = dest.split_once(char::is_whitespace)
        && title.trim_start().starts_with('"')
    {
        dest = src;
    }
    if dest.is_empty()
        || dest.contains(char::is_whitespace) && !rest[split + 2..].trim().starts_with('<')
    {
        return None;
    }
    Some((alt, dest.to_owned()))
}

/// `[!TIP] rest` → (Tip, "rest").
fn callout_tag(line: &str) -> Option<(crate::doc::Tone, &str)> {
    let rest = line.trim_start().strip_prefix("[!")?;
    let (tag, after) = rest.split_once(']')?;
    let tone = crate::doc::Tone::from_tag(tag)?;
    Some((tone, after.trim()))
}

fn quote_line(line: &str) -> Option<&str> {
    line.strip_prefix("> ").or_else(|| line.strip_prefix('>'))
}

fn indent_of(line: &str) -> u8 {
    let mut width = 0usize;
    for ch in line.chars() {
        match ch {
            ' ' => width += 1,
            '\t' => width += 4,
            _ => break,
        }
    }
    // Two spaces per level, as the writer emits; tabs count as two levels.
    ((width / 2).min(usize::from(MAX_INDENT))) as u8
}

// ---------------------------------------------------------------------------
// Inline parsing

const ESCAPABLE: &str = "\\`*_~[]()#>+-.!|";

/// Parses inline Markdown into plain text plus marks.
pub fn parse_inline(source: &str) -> (String, Vec<Mark>) {
    let mut out = String::new();
    let mut marks = Vec::new();
    parse_into(source, &mut out, &mut marks, 0);
    (out, marks)
}

/// One inline token before emphasis resolution.
enum Item {
    /// Literal text with marks already resolved (escapes, code spans, links).
    Text(String, Vec<Mark>),
    /// A run of `*`, `_`, or `~`; which characters survive as literal text is
    /// decided by [`resolve_emphasis`].
    Run(Run),
}

struct Run {
    ch: char,
    len: usize,
    remaining: usize,
    can_open: bool,
    can_close: bool,
    /// Characters consumed as a closer (taken from the run's left side).
    closed: usize,
}

fn parse_into(src: &str, out: &mut String, marks: &mut Vec<Mark>, depth: usize) {
    let mut items: Vec<Item> = Vec::new();
    let mut literal = String::new();
    let flush = |literal: &mut String, items: &mut Vec<Item>| {
        if !literal.is_empty() {
            items.push(Item::Text(std::mem::take(literal), Vec::new()));
        }
    };
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < src.len() {
        let rest = &src[i..];
        let c = bytes[i];
        if c == b'\\'
            && let Some(next) = rest[1..].chars().next()
            && ESCAPABLE.contains(next)
        {
            literal.push(next);
            i += 1 + next.len_utf8();
            continue;
        }
        if c == b'`' {
            let ticks = rest.bytes().take_while(|b| *b == b'`').count();
            if let Some(close) = find_code_close(&rest[ticks..], ticks) {
                let mut inner = &rest[ticks..ticks + close];
                if inner.len() > 1
                    && inner.starts_with(' ')
                    && inner.ends_with(' ')
                    && !inner.trim().is_empty()
                {
                    inner = &inner[1..inner.len() - 1];
                }
                flush(&mut literal, &mut items);
                items.push(Item::Text(
                    inner.to_owned(),
                    vec![Mark {
                        range: 0..inner.len(),
                        style: Style::Code,
                    }],
                ));
                i += ticks + close + ticks;
                continue;
            }
            literal.push_str(&rest[..ticks]);
            i += ticks;
            continue;
        }
        if c == b'['
            && depth < 16
            && let Some((label_end, url, consumed)) = link_at(rest)
        {
            let mut text = String::new();
            let mut inner = Vec::new();
            parse_into(&rest[1..label_end], &mut text, &mut inner, depth + 1);
            if !text.is_empty() {
                inner.push(Mark {
                    range: 0..text.len(),
                    style: Style::Link(url),
                });
            }
            flush(&mut literal, &mut items);
            items.push(Item::Text(text, inner));
            i += consumed;
            continue;
        }
        if matches!(c, b'*' | b'_' | b'~') {
            let run = rest.bytes().take_while(|b| *b == c).count();
            let before = src[..i].chars().next_back();
            let after = src[i + run..].chars().next();
            let left = flanks(before, after);
            let right = flanks(after, before);
            let (can_open, can_close) = match c {
                b'_' => (
                    left && (!right || is_punct(before)),
                    right && (!left || is_punct(after)),
                ),
                _ => (left, right),
            };
            flush(&mut literal, &mut items);
            items.push(Item::Run(Run {
                ch: c as char,
                len: run,
                remaining: run,
                can_open,
                can_close,
                closed: 0,
            }));
            i += run;
            continue;
        }
        let ch = rest.chars().next().expect("non-empty");
        literal.push(ch);
        i += ch.len_utf8();
    }
    flush(&mut literal, &mut items);
    let pairs = resolve_emphasis(&mut items);
    emit(&items, &pairs, out, marks);
}

/// CommonMark flanking: a run is left-flanking when `next` starts content
/// (`prev` is the character on the other side).
fn flanks(prev: Option<char>, next: Option<char>) -> bool {
    let Some(next) = next else { return false };
    if next.is_whitespace() {
        return false;
    }
    !is_punct(Some(next)) || prev.is_none_or(|p| p.is_whitespace() || is_punct(Some(p)))
}

fn is_punct(ch: Option<char>) -> bool {
    ch.is_some_and(|ch| ch.is_ascii_punctuation() || (!ch.is_alphanumeric() && !ch.is_whitespace()))
}

/// (opener item, closer item, style); the process-emphasis algorithm of the
/// CommonMark spec, including the rule of three, plus GFM `~~`.
fn resolve_emphasis(items: &mut [Item]) -> Vec<(usize, usize, Style)> {
    let mut pairs = Vec::new();
    let runs: Vec<usize> = (0..items.len())
        .filter(|i| matches!(items[*i], Item::Run(_)))
        .collect();
    let mut active: Vec<bool> = vec![true; items.len()];
    for &closer in &runs {
        while let Item::Run(c) = &items[closer] {
            if !c.can_close || c.remaining == 0 {
                break;
            }
            let (cch, clen, cremaining, cboth) =
                (c.ch, c.len, c.remaining, c.can_open && c.can_close);
            let mut found = None;
            for &opener in runs.iter().rev().filter(|o| **o < closer) {
                if !active[opener] {
                    continue;
                }
                let Item::Run(o) = &items[opener] else {
                    continue;
                };
                if o.ch != cch || !o.can_open || o.remaining == 0 {
                    continue;
                }
                if cch == '~' {
                    if o.remaining >= 2 && cremaining >= 2 {
                        found = Some(opener);
                        break;
                    }
                    continue;
                }
                let both = cboth || (o.can_open && o.can_close);
                if both && (o.len + clen) % 3 == 0 && !(o.len % 3 == 0 && clen % 3 == 0) {
                    continue;
                }
                found = Some(opener);
                break;
            }
            let Some(opener) = found else { break };
            let Item::Run(o) = &items[opener] else { break };
            let used = if cch == '~' || (o.remaining >= 2 && cremaining >= 2) {
                2
            } else {
                1
            };
            let style = match (cch, used) {
                ('~', _) => Style::Strike,
                (_, 2) => Style::Bold,
                _ => Style::Italic,
            };
            if let Item::Run(o) = &mut items[opener] {
                o.remaining -= used;
            }
            if let Item::Run(c) = &mut items[closer] {
                c.remaining -= used;
                c.closed += used;
            }
            active[opener + 1..closer].fill(false);
            pairs.push((opener, closer, style));
        }
    }
    pairs
}

fn emit(items: &[Item], pairs: &[(usize, usize, Style)], out: &mut String, marks: &mut Vec<Mark>) {
    // Offsets where each run's closing and opening content boundaries land.
    let mut close_at = vec![0; items.len()];
    let mut open_at = vec![0; items.len()];
    for (index, item) in items.iter().enumerate() {
        match item {
            Item::Text(text, inner) => {
                let base = out.len();
                out.push_str(text);
                for mark in inner {
                    marks.push(Mark {
                        range: mark.range.start + base..mark.range.end + base,
                        style: mark.style.clone(),
                    });
                }
            }
            Item::Run(run) => {
                close_at[index] = out.len();
                // Consumed characters vanish; the rest stay literal between
                // the closing (left) and opening (right) boundaries.
                for _ in 0..run.remaining {
                    out.push(run.ch);
                }
                open_at[index] = out.len();
            }
        }
    }
    for (opener, closer, style) in pairs {
        push_mark(marks, open_at[*opener]..close_at[*closer], style.clone());
    }
}

fn push_mark(marks: &mut Vec<Mark>, range: std::ops::Range<usize>, style: Style) {
    if !range.is_empty() {
        marks.push(Mark { range, style });
    }
}

fn find_code_close(src: &str, ticks: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run = bytes[i..].iter().take_while(|b| **b == b'`').count();
            if run == ticks && i > 0 {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

/// `[label](url)` at the start of `src`: (label end, url, bytes consumed).
fn link_at(src: &str) -> Option<(usize, String, usize)> {
    let bytes = src.as_bytes();
    let mut depth = 0;
    let mut i = 1;
    let mut label_end = None;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'[' => depth += 1,
            b']' if depth == 0 => {
                label_end = Some(i);
                break;
            }
            b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    let label_end = label_end?;
    if label_end == 1 {
        return None;
    }
    let after = &src[label_end + 1..];
    let inner = after.strip_prefix('(')?;
    // Balanced parentheses and backslash escapes, as CommonMark allows.
    let mut url = String::new();
    let mut depth = 0usize;
    let mut chars = inner.char_indices();
    let close = loop {
        let (i, ch) = chars.next()?;
        match ch {
            '\\' => {
                let (_, escaped) = chars.next()?;
                url.push(escaped);
            }
            '(' => {
                depth += 1;
                url.push(ch);
            }
            ')' if depth == 0 => break i,
            ')' => {
                depth -= 1;
                url.push(ch);
            }
            ch if ch.is_whitespace() => return None,
            ch => url.push(ch),
        }
    };
    if url.is_empty() {
        return None;
    }
    Some((label_end, url, label_end + 1 + 1 + close + 1))
}

fn write_url(url: &str) -> String {
    let mut depth = 0i32;
    let balanced = url.chars().all(|ch| {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        depth >= 0
    }) && depth == 0;
    let mut out = String::with_capacity(url.len());
    for ch in url.chars() {
        match ch {
            '(' | ')' if !balanced => {
                out.push('\\');
                out.push(ch);
            }
            '\\' => out.push_str("\\\\"),
            ' ' => out.push_str("%20"),
            ch => out.push(ch),
        }
    }
    out
}

fn is_word(ch: Option<char>) -> bool {
    ch.is_some_and(char::is_alphanumeric)
}

// ---------------------------------------------------------------------------
// Writing

pub fn write(front: &FrontMatter, doc: &Document) -> String {
    let mut out = String::new();
    if !front.fields.is_empty() {
        out.push_str("---\n");
        for (key, value) in &front.fields {
            out.push_str(key);
            out.push_str(": ");
            out.push_str(value);
            out.push('\n');
        }
        out.push_str("---\n");
    }
    if !doc.title.is_empty() {
        out.push_str("# ");
        out.push_str(&escape_text(&doc.title, true));
        out.push_str("\n\n");
    }
    // Trailing empty paragraphs are where the caret rests; they have no
    // Markdown form.
    let end = doc
        .blocks
        .iter()
        .rposition(|b| !(b.kind == BlockKind::Paragraph && b.text.is_empty()))
        .map_or(0, |i| i + 1);
    let mut previous: Option<&Block> = None;
    let mut index = 0;
    while index < end {
        let block = &doc.blocks[index];
        if let Some(previous) = previous {
            let tight = previous.kind.is_list() && block.kind.is_list();
            out.push_str(if tight { "\n" } else { "\n\n" });
        }
        if block.kind.is_cell() {
            let range = crate::doc::table_range(&doc.blocks, index).unwrap_or(index..index + 1);
            write_table(&mut out, &doc.blocks[range.clone()]);
            previous = doc.blocks.get(range.end - 1);
            index = range.end;
            continue;
        }
        write_block(&mut out, doc, index, block);
        previous = Some(block);
        index += 1;
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn write_block(out: &mut String, doc: &Document, index: usize, block: &Block) {
    let pad = "  ".repeat(usize::from(block.indent));
    match block.kind {
        // Tables are written whole by `write_table`; a stray cell reads as text.
        BlockKind::Cell(_) => out.push_str(&write_inline(block, true)),
        // The editor lifts the title into `Document::title` before writing;
        // a stray one reads as ordinary text.
        BlockKind::Title | BlockKind::Paragraph => {
            if block.text.is_empty() {
                out.push_str(EMPTY_PARAGRAPH);
                return;
            }
            out.push_str(&write_inline(block, true));
        }
        BlockKind::Heading(level) => {
            out.push_str(&"#".repeat(usize::from(level.clamp(1, 3)) + 1));
            out.push(' ');
            out.push_str(&write_inline(block, false));
        }
        BlockKind::Bullet => {
            out.push_str(&pad);
            out.push_str("- ");
            out.push_str(&write_inline(block, false));
        }
        BlockKind::Numbered => {
            out.push_str(&pad);
            out.push_str(&format!("{}. ", doc.ordinal(index)));
            out.push_str(&write_inline(block, false));
        }
        BlockKind::Todo { checked } => {
            out.push_str(&pad);
            out.push_str(if checked { "- [x] " } else { "- [ ] " });
            out.push_str(&write_inline(block, false));
        }
        BlockKind::Quote => {
            let text = write_inline(block, false);
            let quoted: Vec<String> = text.split('\n').map(|line| format!("> {line}")).collect();
            out.push_str(&quoted.join("\n"));
        }
        BlockKind::Code => {
            // An info string is one line, and a backtick fence's cannot hold
            // a backtick.
            let info = block.info.split_whitespace().collect::<Vec<_>>().join(" ");
            let fence = if block.text.contains("```") || info.contains('`') {
                "~~~"
            } else {
                "```"
            };
            out.push_str(fence);
            out.push_str(&info);
            out.push('\n');
            out.push_str(&block.text);
            out.push('\n');
            out.push_str(fence);
        }
        BlockKind::Divider => out.push_str("---"),
        BlockKind::Image => {
            out.push_str("![");
            out.push_str(&escape_text(&block.text.replace('\n', " "), false));
            out.push_str("](");
            if block.src.contains([' ', '(', ')', '<', '>']) {
                out.push('<');
                out.push_str(&block.src.replace(['<', '>'], ""));
                out.push('>');
            } else {
                out.push_str(&block.src);
            }
            out.push(')');
        }
        BlockKind::Callout(tone) => {
            out.push_str("> [!");
            out.push_str(tone.tag());
            out.push(']');
            let text = write_inline(block, false);
            if !block.text.is_empty() {
                for line in text.split('\n') {
                    out.push_str("\n> ");
                    out.push_str(line);
                }
            }
        }
    }
}

/// Serializes a block's text with its marks. Crossing marks close and reopen
/// so the output always nests. Emphasis cannot begin or end on whitespace in
/// Markdown, so whitespace at an emphasis edge is written outside it.
pub fn write_inline(block: &Block, paragraph: bool) -> String {
    let text = &block.text;
    let mut cuts: Vec<usize> = vec![0, text.len()];
    for mark in &block.marks {
        cuts.push(mark.range.start);
        cuts.push(mark.range.end);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = String::new();
    // (style, closing delimiter)
    let mut stack: Vec<(Style, String)> = Vec::new();
    // Trailing whitespace of the previous segment, held back until we know
    // whether an emphasis closes after it.
    let mut pending = String::new();
    for window in cuts.windows(2) {
        let (a, b) = (window[0], window[1]);
        let mut active: Vec<Style> = block
            .marks
            .iter()
            .filter(|mark| mark.range.start <= a && b <= mark.range.end)
            .map(|mark| mark.style.clone())
            .collect();
        active.sort_by_key(Style::rank);
        active.dedup();
        let in_code = |stack: &[(Style, String)]| stack.iter().any(|(s, _)| *s == Style::Code);
        // Close everything from the first stacked style no longer active.
        if let Some(first_gone) = stack.iter().position(|(s, _)| !active.contains(s)) {
            let closes_emphasis = stack[first_gone..].iter().any(|(s, _)| is_emphasis(s));
            if !closes_emphasis {
                out.push_str(&pending);
                pending.clear();
            }
            while stack.len() > first_gone {
                let (_, close) = stack.pop().expect("non-empty");
                out.push_str(&close);
            }
        }
        out.push_str(&pending);
        pending.clear();
        let segment = &text[a..b];
        let end_of = |style: &Style| {
            block
                .marks
                .iter()
                .filter(|m| &m.style == style && m.range.start <= a && a < m.range.end)
                .map(|m| m.range.end)
                .max()
                .unwrap_or(b)
        };
        let mut opening: Vec<Style> = active
            .iter()
            .filter(|style| !stack.iter().any(|(s, _)| s == *style))
            .cloned()
            .collect();
        // Links outermost, code innermost; between them the style that runs
        // longest opens first so fewer marks have to close and reopen.
        opening.sort_by(|x, y| {
            let group = |s: &Style| match s {
                Style::Link(_) => 0,
                Style::Code => 2,
                _ => 1,
            };
            group(x)
                .cmp(&group(y))
                .then(end_of(y).cmp(&end_of(x)))
                .then(x.rank().cmp(&y.rank()))
        });
        let mut body = segment;
        if opening.iter().any(is_emphasis) && !in_code(&stack) {
            let trimmed = body.trim_start();
            let lead = &body[..body.len() - trimmed.len()];
            out.push_str(lead);
            body = trimmed;
        }
        for style in opening {
            let (mut open, mut close) = delimiters(&style, block, a);
            if matches!(style, Style::Italic | Style::Bold) {
                // Underscores keep adjacent delimiters from fusing into
                // ambiguous `***` runs; they are only valid outside words.
                let run_end = stack
                    .iter()
                    .map(|(s, _)| end_of(s))
                    .chain(std::iter::once(end_of(&style)))
                    .min()
                    .unwrap_or(b);
                let before = out.chars().next_back();
                let after = text[run_end..].chars().next();
                // Never continue the previous delimiter's character.
                let prefer = match before {
                    Some('*') => true,
                    Some('_') => false,
                    _ => style == Style::Italic,
                };
                if prefer && !is_word(before) && !is_word(after) {
                    let delim = if style == Style::Italic { "_" } else { "__" };
                    open = delim.into();
                    close = delim.into();
                }
            }
            out.push_str(&open);
            stack.push((style, close));
        }
        if in_code(&stack) {
            out.push_str(body);
        } else {
            let trimmed = if stack.iter().any(|(s, _)| is_emphasis(s)) {
                body.trim_end()
            } else {
                body
            };
            let at_start = a == 0 && paragraph;
            out.push_str(&escape_text(trimmed, at_start));
            pending.push_str(&body[trimmed.len()..]);
        }
    }
    while let Some((_, close)) = stack.pop() {
        out.push_str(&close);
    }
    out.push_str(&pending);
    out
}

fn is_emphasis(style: &Style) -> bool {
    matches!(style, Style::Bold | Style::Italic | Style::Strike)
}

fn delimiters(style: &Style, block: &Block, at: usize) -> (String, String) {
    match style {
        Style::Bold => ("**".into(), "**".into()),
        Style::Italic => ("*".into(), "*".into()),
        Style::Strike => ("~~".into(), "~~".into()),
        Style::Code => {
            let span = block
                .marks
                .iter()
                .find(|mark| {
                    mark.style == Style::Code && mark.range.start <= at && at < mark.range.end
                })
                .map_or("", |mark| &block.text[mark.range.clone()]);
            let longest = span.split(|c| c != '`').map(str::len).max().unwrap_or(0);
            let fence = "`".repeat(longest + 1);
            let pad = span.starts_with('`')
                || span.ends_with('`')
                || (span.starts_with(' ') && span.ends_with(' ') && !span.trim().is_empty());
            if pad {
                (format!("{fence} "), format!(" {fence}"))
            } else {
                (fence.clone(), fence)
            }
        }
        Style::Link(url) => ("[".into(), format!("]({})", write_url(url))),
    }
}

fn escape_text(text: &str, line_start: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        let prev = if i == 0 { None } else { Some(chars[i - 1]) };
        let next = chars.get(i + 1).copied();
        let needs = match ch {
            '\\' | '`' | '*' | '[' | ']' | '~' => true,
            '_' => !is_word(prev) || !is_word(next),
            _ => false,
        };
        if needs {
            out.push('\\');
        }
        out.push(ch);
    }
    if line_start {
        escape_block_start(&mut out);
    }
    // Every line of a multi-line paragraph starts a line in the file.
    if out.contains('\n') {
        let lines: Vec<String> = out
            .split('\n')
            .enumerate()
            .map(|(i, line)| {
                let mut line = line.to_owned();
                if i > 0 {
                    escape_block_start(&mut line);
                }
                line
            })
            .collect();
        out = lines.join("\n");
    }
    out
}

fn escape_block_start(line: &mut String) {
    let trimmed = line.trim_start();
    // A text line shaped like a table's delimiter row would turn the line
    // above it into a table header.
    let starts_block = heading(trimmed).is_some()
        || list_item(trimmed).is_some()
        || quote_line(trimmed).is_some()
        || fence_of(trimmed).is_some()
        || is_divider(trimmed)
        || delimiter_row(trimmed).is_some();
    if starts_block && !trimmed.starts_with('\\') {
        let lead = line.len() - trimmed.len();
        let first = trimmed.chars().next().expect("block start is non-empty");
        if first.is_ascii_digit() {
            // `1. x` → `1\. x`
            let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
            line.insert(lead + digits, '\\');
        } else {
            line.insert(lead, '\\');
        }
    }
}

// ---------------------------------------------------------------------------
// Tables (GFM)

/// Splits a table row into raw cells on unescaped pipes, dropping one
/// optional leading and trailing pipe. `None` when the line has no
/// unescaped pipe at all. As in cmark-gfm, a pipe right after a backslash
/// is escaped whatever precedes that backslash (even inside a code span),
/// and unescaping drops just that one backslash.
fn split_row(line: &str) -> Option<Vec<&str>> {
    let line = line.trim();
    let bytes = line.as_bytes();
    let cuts: Vec<usize> = bytes
        .iter()
        .enumerate()
        .filter(|(i, b)| **b == b'|' && (*i == 0 || bytes[i - 1] != b'\\'))
        .map(|(i, _)| i)
        .collect();
    if cuts.is_empty() {
        return None;
    }
    let mut cells = Vec::new();
    let mut start = 0;
    for &cut in &cuts {
        cells.push(&line[start..cut]);
        start = cut + 1;
    }
    cells.push(&line[start..]);
    if cuts.first() == Some(&0) {
        cells.remove(0);
    }
    if cuts.last() == Some(&(line.len() - 1)) {
        cells.pop();
    }
    Some(cells)
}

/// The alignments of a delimiter row such as `| :-- | :-: | --: |`.
fn delimiter_row(line: &str) -> Option<Vec<crate::doc::Align>> {
    use crate::doc::Align;
    if !line.contains('|') || !line.contains('-') {
        return None;
    }
    let cells = split_row(line)?;
    let mut aligns = Vec::with_capacity(cells.len());
    for cell in cells {
        let cell = cell.trim();
        let left = cell.starts_with(':');
        let right = cell.ends_with(':') && cell.len() > 1;
        let dashes = cell.trim_start_matches(':').trim_end_matches(':');
        if dashes.is_empty() || !dashes.bytes().all(|b| b == b'-') {
            return None;
        }
        aligns.push(match (left, right) {
            (true, true) => Align::Center,
            (true, false) => Align::Left,
            (false, true) => Align::Right,
            (false, false) => Align::None,
        });
    }
    (!aligns.is_empty()).then_some(aligns)
}

fn starts_other_block(line: &str) -> bool {
    heading(line).is_some()
        || list_item(line).is_some()
        || quote_line(line).is_some()
        || fence_of(line).is_some()
        || is_divider(line)
        || image_line(line).is_some()
}

/// A GFM table starting at `lines[i]`: a header row, a delimiter row with
/// as many cells, then body rows until a blank line, a line without a pipe,
/// or another block. Short rows are padded and long ones cut, as GFM does.
/// Returns the cells and the index of the first line after the table.
fn table_at(lines: &[&str], i: usize) -> Option<(Vec<Block>, usize)> {
    use crate::doc::{Cell, MAX_TABLE_COLS};
    let header_line = lines.get(i)?.trim();
    if starts_other_block(header_line) {
        return None;
    }
    let header = split_row(header_line)?;
    let aligns = delimiter_row(lines.get(i + 1)?.trim())?;
    if header.len() != aligns.len() {
        return None;
    }
    let cols = aligns.len().min(MAX_TABLE_COLS);
    let mut rows = vec![header];
    let mut next = i + 2;
    while let Some(line) = lines.get(next) {
        let line = line.trim();
        if line.is_empty() || starts_other_block(line) {
            break;
        }
        let Some(cells) = split_row(line) else {
            break;
        };
        rows.push(cells);
        next += 1;
    }
    let mut blocks = Vec::with_capacity(rows.len() * cols);
    for (r, row) in rows.iter().enumerate() {
        for (col, &align) in aligns.iter().enumerate().take(cols) {
            let raw = row
                .get(col)
                .copied()
                .unwrap_or_default()
                .trim()
                .replace("\\|", "|");
            let (text, marks) = parse_inline(&raw);
            let mut block = Block::cell(
                0,
                Cell {
                    col: col as u16,
                    cols: cols as u16,
                    align,
                    header: r == 0,
                },
                text,
            );
            block.marks = marks;
            blocks.push(block);
        }
    }
    Some((blocks, next))
}

/// Writes a table with its columns padded to line up, so the file reads as
/// a table in any text editor and renders as one everywhere.
fn write_table(out: &mut String, cells: &[Block]) {
    use crate::doc::Align;
    let cols = cells
        .first()
        .and_then(|b| b.kind.cell())
        .map_or(1, |c| usize::from(c.cols.max(1)));
    let aligns: Vec<Align> = (0..cols)
        .map(|col| {
            cells
                .get(col)
                .and_then(|b| b.kind.cell())
                .map_or(Align::None, |c| c.align)
        })
        .collect();
    let written: Vec<String> = cells
        .iter()
        .map(|cell| {
            let mut flat = cell.clone();
            flat.text = flat.text.replace('\n', " ");
            write_inline(&flat, false).replace('|', "\\|")
        })
        .collect();
    let width = |s: &str| s.chars().count();
    let mut widths = vec![3usize; cols];
    for (i, text) in written.iter().enumerate() {
        widths[i % cols] = widths[i % cols].max(width(text));
    }
    let pad = |text: &str, col: usize| {
        let gap = widths[col].saturating_sub(width(text));
        match aligns[col] {
            Align::Right => format!("{}{text}", " ".repeat(gap)),
            Align::Center => format!("{}{text}{}", " ".repeat(gap / 2), " ".repeat(gap - gap / 2)),
            _ => format!("{text}{}", " ".repeat(gap)),
        }
    };
    let row = |out: &mut String, cells: &[String]| {
        out.push('|');
        for (col, text) in cells.iter().enumerate() {
            out.push(' ');
            out.push_str(&pad(text, col));
            out.push_str(" |");
        }
    };
    for (r, chunk) in written.chunks(cols).enumerate() {
        if r > 0 {
            out.push('\n');
        }
        let mut chunk = chunk.to_vec();
        chunk.resize(cols, String::new());
        row(out, &chunk);
        if r == 0 {
            out.push_str("\n|");
            for (col, align) in aligns.iter().enumerate() {
                let dashes = widths[col];
                let rule = match align {
                    Align::Left => format!(":{}", "-".repeat(dashes - 1)),
                    Align::Center => format!(":{}:", "-".repeat(dashes - 2)),
                    Align::Right => format!("{}:", "-".repeat(dashes - 1)),
                    Align::None => "-".repeat(dashes),
                };
                out.push(' ');
                out.push_str(&rule);
                out.push_str(" |");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Styles of every non-whitespace character: Markdown cannot style an
    /// edge space, so whitespace styling is not part of the contract.
    fn visible(block: &Block) -> Vec<(usize, Vec<Style>)> {
        block
            .text
            .char_indices()
            .filter(|(_, ch)| !ch.is_whitespace())
            .map(|(i, _)| {
                let mut styles: Vec<Style> = block
                    .marks
                    .iter()
                    .filter(|m| m.range.start <= i && i < m.range.end)
                    .map(|m| m.style.clone())
                    .collect();
                styles.sort();
                (i, styles)
            })
            .collect()
    }

    /// CommonMark can only open emphasis before content that is not
    /// punctuation glued to a preceding letter, and likewise at the end.
    fn expressible(text: &str, range: std::ops::Range<usize>) -> bool {
        let before = text[..range.start].chars().next_back();
        let first = text[range.clone()].chars().next();
        let last = text[range.clone()].chars().next_back();
        let after = text[range.end..].chars().next();
        // Stacked delimiters count as punctuation for each other, so the
        // fuzz styles whole words the way selections usually do.
        let edge = |ch: Option<char>| ch.is_none_or(char::is_whitespace);
        edge(before) && edge(after) && flanks(before, first) && flanks(after, last)
    }

    fn equivalent(a: &Document, b: &Document) -> bool {
        a.title == b.title
            && a.blocks.len() == b.blocks.len()
            && a.blocks.iter().zip(&b.blocks).all(|(x, y)| {
                x.kind == y.kind
                    && x.indent == y.indent
                    && x.text == y.text
                    && x.info == y.info
                    && visible(x) == visible(y)
            })
    }

    fn round_trip(doc: &Document) -> Document {
        let text = write(&FrontMatter::default(), doc);
        let (_, parsed) = parse(&text);
        assert!(
            equivalent(&parsed, doc),
            "round trip changed the note.\n--- markdown ---\n{text}\n--- got ---\n{parsed:#?}\n--- want ---\n{doc:#?}"
        );
        parsed
    }

    fn b(kind: BlockKind, text: &str) -> Block {
        Block::new(0, kind, text)
    }

    #[test]
    fn a_code_fence_keeps_its_info_string() {
        let src = "# D\n\n```mermaid\nflowchart LR\n  A --> B\n```\n\n~~~rust title=main.rs\nfn main() {}\n~~~\n\n```\nplain\n```\n";
        let (_, doc) = parse(src);
        assert_eq!(doc.blocks[0].info, "mermaid");
        assert!(doc.blocks[0].is_diagram());
        assert_eq!(doc.blocks[0].text, "flowchart LR\n  A --> B");
        assert_eq!(doc.blocks[1].info, "rust title=main.rs");
        assert_eq!(doc.blocks[1].lang(), "rust");
        assert!(!doc.blocks[1].is_diagram());
        assert_eq!(doc.blocks[2].info, "");
        let text = write(&FrontMatter::default(), &doc);
        assert!(text.contains("```mermaid\nflowchart LR"), "{text}");
        assert!(
            text.contains("```rust title=main.rs\nfn main() {}"),
            "{text}"
        );
        round_trip(&doc);
    }

    #[test]
    fn an_info_string_with_a_backtick_uses_a_tilde_fence() {
        let doc = Document::new("D", vec![Block::code(0, "odd`lang", "x")]);
        let text = write(&FrontMatter::default(), &doc);
        assert!(text.contains("~~~odd`lang\nx\n~~~"), "{text}");
        round_trip(&doc);
    }

    #[test]
    fn reads_a_typical_note() {
        let src = "---\nid: abc\npinned: true\n---\n# Plan\n\nSome **bold** and _it_ and `code`.\n\n## Todos\n\n- [ ] one\n- [x] two\n  - nested\n1. first\n2. second\n\n> quoted\n> more\n\n```\nfn main() {}\n```\n\n---\n";
        let (front, doc) = parse(src);
        assert_eq!(front.get("id"), Some("abc"));
        assert!(front.flag("pinned"));
        assert_eq!(doc.title, "Plan");
        let kinds: Vec<BlockKind> = doc.blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::Paragraph,
                BlockKind::Heading(1),
                BlockKind::Todo { checked: false },
                BlockKind::Todo { checked: true },
                BlockKind::Bullet,
                BlockKind::Numbered,
                BlockKind::Numbered,
                BlockKind::Quote,
                BlockKind::Code,
                BlockKind::Divider,
            ]
        );
        assert_eq!(doc.blocks[0].text, "Some bold and it and code.");
        assert_eq!(doc.blocks[4].indent, 1);
        assert_eq!(doc.blocks[7].text, "quoted\nmore");
        assert_eq!(doc.blocks[8].text, "fn main() {}");
        round_trip(&doc);
    }

    #[test]
    fn inline_marks_round_trip_including_crossings() {
        let mut block = b(BlockKind::Paragraph, "alpha beta gamma delta");
        block.add_mark(0..10, Style::Bold);
        block.add_mark(6..16, Style::Italic);
        block.add_mark(17..22, Style::Code);
        block.add_mark(11..16, Style::Link("https://x.dev/a_(b)".into()));
        round_trip(&Document::new("T", vec![block]));
    }

    #[test]
    fn plain_text_with_markdown_characters_is_escaped() {
        let texts = [
            "snake_case and *stars* and `ticks` and [brackets]",
            "# not a heading",
            "- not a list",
            "1. not numbered",
            "> not a quote",
            "---",
            "a\\b ~~not struck~~",
            "_leading underscore_",
        ];
        for text in texts {
            round_trip(&Document::new("", vec![b(BlockKind::Paragraph, text)]));
        }
    }

    #[test]
    fn intraword_italic_uses_stars() {
        let mut block = b(BlockKind::Paragraph, "unbelievable");
        block.add_mark(2..6, Style::Italic);
        let text = write_inline(&block, true);
        assert_eq!(text, "un*beli*evable");
        round_trip(&Document::new("", vec![block]));
    }

    #[test]
    fn every_block_kind_round_trips() {
        let mut nested = b(BlockKind::Todo { checked: true }, "nested done");
        nested.indent = 2;
        let doc = Document::new(
            "All kinds",
            vec![
                b(BlockKind::Heading(1), "H1"),
                b(BlockKind::Heading(2), "H2"),
                b(BlockKind::Heading(3), "H3"),
                b(BlockKind::Paragraph, "line one\nline two"),
                b(BlockKind::Bullet, "bullet"),
                nested,
                b(BlockKind::Numbered, "n1"),
                b(BlockKind::Numbered, "n2"),
                b(BlockKind::Quote, "q1\nq2"),
                b(BlockKind::Code, "let x = `y`;\n\n  indented"),
                b(BlockKind::Divider, ""),
                b(BlockKind::Todo { checked: false }, ""),
                b(BlockKind::Paragraph, ""),
                b(BlockKind::Paragraph, "after blank"),
            ],
        );
        round_trip(&doc);
    }

    #[test]
    fn images_and_callouts_round_trip() {
        use crate::doc::Tone;
        let mut callout = b(
            BlockKind::Callout(Tone::Warning),
            "Budget is capped\nat $5k",
        );
        callout.add_mark(10..16, Style::Bold);
        let doc = Document::new(
            "Launch",
            vec![
                Block::image(0, "assets/n-1/3fa9c0.png", "Funnel, week 3"),
                Block::image(0, "assets/My Shots/a (1).png", ""),
                Block::image(0, "https://example.com/chart.png", "chart [v2]"),
                callout,
                b(BlockKind::Callout(Tone::Note), ""),
                b(BlockKind::Paragraph, "![not an image](inline) in text"),
                b(BlockKind::Quote, "[!NOTE] is just text in a quote"),
            ],
        );
        let text = write(&FrontMatter::default(), &doc);
        assert!(
            text.contains("![Funnel, week 3](assets/n-1/3fa9c0.png)"),
            "{text}"
        );
        assert!(text.contains("![](<assets/My Shots/a (1).png>)"), "{text}");
        assert!(
            text.contains("> [!WARNING]\n> Budget is **capped**\n> at $5k"),
            "{text}"
        );
        round_trip(&doc);
    }

    #[test]
    fn reads_hand_written_images_and_alerts() {
        use crate::doc::Tone;
        let (_, doc) = parse(
            "# T\n\n![Screenshot](img/a.png \"title\")\n\n> [!tip] Use the sheet\n> for numbers\n\n> [!CAUTION]\n> Irreversible\n\n> [!unknown] stays a quote\n",
        );
        let kinds: Vec<BlockKind> = doc.blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::Image,
                BlockKind::Callout(Tone::Tip),
                BlockKind::Callout(Tone::Caution),
                BlockKind::Quote,
            ]
        );
        assert_eq!(doc.blocks[0].src, "img/a.png");
        assert_eq!(doc.blocks[0].text, "Screenshot");
        assert_eq!(doc.blocks[1].text, "Use the sheet\nfor numbers");
        assert_eq!(doc.blocks[2].text, "Irreversible");
    }

    fn cells(doc: &Document) -> Vec<(u16, u16, bool, String)> {
        doc.blocks
            .iter()
            .filter_map(|b| {
                b.kind
                    .cell()
                    .map(|c| (c.col, c.cols, c.header, b.text.clone()))
            })
            .collect()
    }

    /// The table from the user's screenshot, as an agent wrote it.
    const AGENT_TABLE: &str = "# Gaps\n\n| What's missing | Type | Requirement for 5/5 |\n|---|---|---|\n| Onboarding email sequence | Content | 3 emails, tested |\n| Pricing page **A/B test** | Experiment | Significant at 95% |\n| [Q4 brief](https://www.notion.so/acme/Q4-brief-1f2e3d4c5b6a79881f2e3d4c5b6a7988) | Doc | Signed off |\n";

    #[test]
    fn reads_the_agent_table_and_writes_it_aligned() {
        let (_, doc) = parse(AGENT_TABLE);
        let cells = cells(&doc);
        assert_eq!(cells.len(), 12);
        assert_eq!(cells[0], (0, 3, true, "What's missing".into()));
        assert_eq!(cells[5], (2, 3, false, "3 emails, tested".into()));
        // Inline marks and links survive inside cells.
        assert!(doc.blocks[3].marks.is_empty());
        assert_eq!(doc.blocks[6].text, "Pricing page A/B test");
        assert_eq!(doc.blocks[6].marks[0].style, Style::Bold);
        assert!(matches!(doc.blocks[9].marks[0].style, Style::Link(_)));
        let text = write(&FrontMatter::default(), &doc);
        let table: Vec<&str> = text.lines().filter(|l| l.starts_with('|')).collect();
        assert_eq!(table.len(), 5);
        let widths: Vec<usize> = table.iter().map(|l| l.chars().count()).collect();
        assert!(
            widths.iter().all(|w| *w == widths[0]),
            "columns line up:\n{text}"
        );
        assert!(table[1].starts_with("| ---"), "{text}");
        round_trip(&doc);
    }

    #[test]
    fn alignment_escapes_and_ragged_rows() {
        use crate::doc::Align;
        let (_, doc) = parse(
            "T | Qty | Note\n:-- | --: | :-:\nshirt | 2\nhat \\| cap | 1 | `a\\|b` | extra\n",
        );
        let aligns: Vec<Align> = doc.blocks[..3]
            .iter()
            .map(|b| b.kind.cell().unwrap().align)
            .collect();
        assert_eq!(aligns, [Align::Left, Align::Right, Align::Center]);
        let cells = cells(&doc);
        assert_eq!(cells.len(), 9, "short row padded, long row cut");
        assert_eq!(cells[5].3, "");
        assert_eq!(cells[6].3, "hat | cap");
        assert_eq!(doc.blocks[8].text, "a|b");
        assert_eq!(doc.blocks[8].marks[0].style, Style::Code);
        let text = write(&FrontMatter::default(), &doc);
        assert!(text.contains("| :-"), "{text}");
        assert!(text.contains("hat \\| cap"), "{text}");
        round_trip(&doc);
    }

    #[test]
    fn tables_never_swallow_other_lines() {
        let (_, doc) = parse(
            "# T\n\nintro | with a pipe\n\n| a | b |\n| - | - |\n| 1 | 2 |\n- a list item | with pipe\n\nafter\n\n| x |\n| --- |\n\n| y |\n| --- |\n| 3 |\n\nno delimiter | here\nstill text\n",
        );
        let kinds: Vec<String> = doc
            .blocks
            .iter()
            .map(|b| match b.kind {
                BlockKind::Cell(c) if c.col == 0 && c.header => "table".into(),
                BlockKind::Cell(_) => "cell".into(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "Paragraph",
                "table",
                "cell",
                "cell",
                "cell",
                "Bullet",
                "Paragraph",
                "table",
                "table",
                "cell",
                "Paragraph"
            ]
        );
        assert_eq!(
            doc.blocks.last().unwrap().text,
            "no delimiter | here\nstill text"
        );
        round_trip(&doc);
    }

    #[test]
    fn text_that_looks_like_a_delimiter_row_stays_text() {
        let doc = Document::new(
            "",
            vec![
                b(BlockKind::Paragraph, "a | b\n--- | ---"),
                b(BlockKind::Paragraph, "|x|"),
            ],
        );
        let text = write(&FrontMatter::default(), &doc);
        let (_, parsed) = parse(&text);
        assert!(parsed.blocks.iter().all(|b| !b.kind.is_cell()), "{text}");
        round_trip(&doc);
    }

    #[test]
    fn random_notes_round_trip() {
        // A tiny deterministic generator keeps the fuzz reproducible without
        // a proptest dependency.
        let mut seed: u64 = 0x5eed;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let alphabet: Vec<char> = "ab c_*`[]~#->1.\\é漢 |:".chars().collect();
        let kinds = [
            BlockKind::Paragraph,
            BlockKind::Heading(2),
            BlockKind::Bullet,
            BlockKind::Numbered,
            BlockKind::Todo { checked: true },
            BlockKind::Quote,
            BlockKind::Callout(crate::doc::Tone::Tip),
            BlockKind::Callout(crate::doc::Tone::Warning),
        ];
        // Mentions are plain links with `diri:` targets; the fuzz covers them
        // beside an ordinary URL so both survive every nesting.
        let styles = [
            Style::Bold,
            Style::Italic,
            Style::Strike,
            Style::Code,
            Style::Link("https://x.dev/a_(b)".into()),
            Style::Link("diri://session/s_4e97a43bd495".into()),
            Style::Link("diri://note/20260930-142501-3fa9".into()),
        ];
        let mut mentions = 0;
        let mut tables = 0;
        for _ in 0..4000 {
            let mut blocks = Vec::new();
            for _ in 0..(1 + next(4)) {
                let len = 1 + next(14) as usize;
                let text: String = (0..len)
                    .map(|_| alphabet[next(alphabet.len() as u64) as usize])
                    .collect();
                let text = text.trim().to_owned();
                if text.is_empty() {
                    continue;
                }
                let mut block = b(kinds[next(kinds.len() as u64) as usize], &text);
                let boundaries: Vec<usize> = (0..=text.len())
                    .filter(|i| text.is_char_boundary(*i))
                    .collect();
                for _ in 0..next(3) {
                    let x = boundaries[next(boundaries.len() as u64) as usize];
                    let y = boundaries[next(boundaries.len() as u64) as usize];
                    let range = x.min(y)..x.max(y);
                    let slice = &text[range.clone()];
                    // Markdown cannot express emphasis that begins or ends in
                    // whitespace; the editor trims such ranges before marking.
                    if slice.trim() != slice || slice.is_empty() {
                        continue;
                    }
                    let style = styles[next(styles.len() as u64) as usize].clone();
                    let link = matches!(style, Style::Link(_));
                    if !link && style != Style::Code && !expressible(&text, range.clone()) {
                        continue;
                    }
                    if style == Style::Code
                        && block
                            .marks
                            .iter()
                            .any(|m| m.range.start < range.end && range.start < m.range.end)
                    {
                        continue;
                    }
                    if block.marks.iter().any(|m| {
                        m.style == Style::Code
                            && m.range.start < range.end
                            && range.start < m.range.end
                    }) {
                        continue;
                    }
                    // Intraword crossings (`a*b**c*d**`) have no faithful
                    // Markdown form; nested and disjoint marks must be exact.
                    let crosses = block.marks.iter().any(|m| {
                        let overlap = m.range.start < range.end && range.start < m.range.end;
                        let nested = (m.range.start <= range.start && range.end <= m.range.end)
                            || (range.start <= m.range.start && m.range.end <= range.end);
                        overlap && !nested
                    });
                    if crosses {
                        continue;
                    }
                    block.add_mark(range, style);
                }
                blocks.push(block);
            }
            // A table now and then, between the other blocks: random cells
            // (pipes and colons included), alignments, and whole-cell marks.
            if next(3) == 0 {
                use crate::doc::{Align, Cell};
                let cols = 1 + next(4) as usize;
                let rows = 1 + next(4) as usize;
                let aligns: Vec<Align> = (0..cols)
                    .map(|_| {
                        [Align::None, Align::Left, Align::Center, Align::Right][next(4) as usize]
                    })
                    .collect();
                let at = next(blocks.len() as u64 + 1) as usize;
                let mut table = Vec::new();
                for r in 0..rows {
                    for (col, &align) in aligns.iter().enumerate() {
                        let len = next(9) as usize;
                        let text: String = (0..len)
                            .map(|_| alphabet[next(alphabet.len() as u64) as usize])
                            .collect();
                        let text = text.trim().to_owned();
                        let mut cell = Block::cell(
                            0,
                            Cell {
                                col: col as u16,
                                cols: cols as u16,
                                align,
                                header: r == 0,
                            },
                            text.clone(),
                        );
                        if !text.is_empty() && next(3) == 0 {
                            let style = styles[next(styles.len() as u64) as usize].clone();
                            if style == Style::Code
                                || matches!(style, Style::Link(_))
                                || expressible(&text, 0..text.len())
                            {
                                cell.add_mark(0..text.len(), style);
                            }
                        }
                        table.push(cell);
                    }
                }
                // Two tables must not touch, or they would read back as one.
                let touches = |i: usize| blocks.get(i).is_some_and(|b: &Block| b.kind.is_cell());
                if !(touches(at) || at > 0 && touches(at - 1)) {
                    blocks.splice(at..at, table);
                }
            }
            if blocks.is_empty() {
                continue;
            }
            let doc = Document::new("", blocks);
            mentions += doc.mentions().len();
            tables += doc.blocks.iter().filter(|b| b.kind.starts_table()).count();
            let text = write(&FrontMatter::default(), &doc);
            let (_, parsed) = parse(&text);
            if !equivalent(&parsed, &doc) {
                let bad = parsed.blocks.iter().zip(&doc.blocks).find(|(x, y)| {
                    !equivalent(
                        &Document::new("", vec![(*x).clone()]),
                        &Document::new("", vec![(*y).clone()]),
                    )
                });
                panic!(
                    "fuzz round trip failed.\n{text}\n{bad:?}\n{} vs {}",
                    parsed.blocks.len(),
                    doc.blocks.len()
                );
            }
        }
        assert!(
            mentions > 500,
            "the fuzz barely exercised mentions: {mentions}"
        );
        assert!(tables > 800, "the fuzz barely exercised tables: {tables}");
    }
}
