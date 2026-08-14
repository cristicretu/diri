//! Bounded, presentation-neutral Markdown parsing for native artifact views.
//!
//! The inspector consumes a small owned document model. Markdown syntax,
//! hostile-input limits, and link activation policy stay inside this module;
//! no renderer needs to interpret source text or decide whether a URL is safe.

use std::ops::Range;

const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_BLOCKS: usize = 8_192;
const MAX_LIST_ITEMS: usize = 4_096;
const MAX_QUOTE_DEPTH: usize = 24;
const MAX_INLINE_DEPTH: usize = 32;
const TRUNCATION_NOTICE: &str = "[Markdown truncated at 256 KiB]";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarkdownDocument {
    pub blocks: Vec<MarkdownBlock>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MarkdownBlock {
    Heading {
        level: u8,
        content: InlineText,
    },
    Paragraph(InlineText),
    List {
        ordered: bool,
        start: usize,
        items: Vec<MarkdownListItem>,
    },
    Quote(Vec<MarkdownBlock>),
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    ThematicBreak,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarkdownListItem {
    pub checked: Option<bool>,
    pub content: InlineText,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InlineText {
    pub spans: Vec<MarkdownSpan>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarkdownSpan {
    pub text: String,
    pub style: InlineStyle,
    pub link: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strikethrough: bool,
}

/// Natural transcript/Markdown selection: a whole turn by default, with an
/// optional UTF-8 byte range when a richer text surface can provide one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TurnSelection {
    turn: Option<usize>,
    text_range: Option<Range<usize>>,
}

impl TurnSelection {
    pub fn select_turn(&mut self, turn: usize) {
        self.turn = Some(turn);
        self.text_range = None;
    }

    pub fn select_text(&mut self, turn: usize, range: Range<usize>, text: &str) {
        let start = floor_char_boundary(text, range.start.min(text.len()));
        let end = ceil_char_boundary(text, range.end.min(text.len()));
        self.turn = Some(turn);
        self.text_range = (start < end).then_some(start..end);
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    #[must_use]
    pub fn selected_text<'a>(&self, turns: &'a [String]) -> Option<&'a str> {
        let text = turns.get(self.turn?)?;
        self.text_range
            .as_ref()
            .map_or(Some(text.as_str()), |range| text.get(range.clone()))
    }

    #[must_use]
    pub fn turn(&self) -> Option<usize> {
        self.turn
    }
}

fn floor_char_boundary(text: &str, mut offset: usize) -> usize {
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn ceil_char_boundary(text: &str, mut offset: usize) -> usize {
    while offset < text.len() && !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

impl MarkdownDocument {
    #[must_use]
    pub fn parse(source: &str) -> Self {
        let (source, source_truncated) = bounded_source(source);
        let lines = source.lines().collect::<Vec<_>>();
        let parsed = parse_blocks(&lines, 0);
        let truncated = source_truncated || parsed.limited;
        let mut blocks = parsed.blocks;
        if truncated {
            blocks.push(MarkdownBlock::Paragraph(parse_inline(TRUNCATION_NOTICE)));
        }
        Self { blocks, truncated }
    }

    /// A structure-preserving text projection for agent context and copying.
    #[must_use]
    pub fn plain_text(&self) -> String {
        blocks_plain_text(&self.blocks)
    }
}

impl InlineText {
    #[must_use]
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

struct ParsedBlocks {
    blocks: Vec<MarkdownBlock>,
    limited: bool,
}

#[derive(Clone, Copy)]
struct Fence<'a> {
    marker: u8,
    length: usize,
    info: &'a str,
}

#[derive(Clone, Copy)]
struct ListMarker<'a> {
    ordered: bool,
    number: usize,
    content: &'a str,
}

fn bounded_source(source: &str) -> (&str, bool) {
    if source.len() <= MAX_SOURCE_BYTES {
        return (source, false);
    }
    let mut end = MAX_SOURCE_BYTES;
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    (&source[..end], true)
}

fn parse_blocks(lines: &[&str], quote_depth: usize) -> ParsedBlocks {
    let mut blocks = Vec::new();
    let mut index = 0;
    let mut limited = false;

    while index < lines.len() {
        if blocks.len() >= MAX_BLOCKS {
            limited = true;
            break;
        }
        if lines[index].trim().is_empty() {
            index += 1;
            continue;
        }

        if let Some(fence) = fence_start(lines[index]) {
            let mut code_lines = Vec::new();
            index += 1;
            while index < lines.len() && !is_fence_close(lines[index], fence) {
                code_lines.push(lines[index]);
                index += 1;
            }
            if index < lines.len() {
                index += 1;
            }
            blocks.push(MarkdownBlock::CodeBlock {
                language: fence
                    .info
                    .split_whitespace()
                    .next()
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
                code: code_lines.join("\n"),
            });
            continue;
        }

        if let Some((level, content)) = heading(lines[index]) {
            blocks.push(MarkdownBlock::Heading {
                level,
                content: parse_inline(content),
            });
            index += 1;
            continue;
        }

        if is_thematic_break(lines[index]) {
            blocks.push(MarkdownBlock::ThematicBreak);
            index += 1;
            continue;
        }

        if quote_content(lines[index]).is_some() {
            if quote_depth >= MAX_QUOTE_DEPTH {
                limited = true;
                break;
            }
            let mut quote_lines = Vec::new();
            while index < lines.len() {
                let Some(content) = quote_content(lines[index]) else {
                    break;
                };
                quote_lines.push(content.to_owned());
                index += 1;
            }
            let quote_refs = quote_lines.iter().map(String::as_str).collect::<Vec<_>>();
            let nested = parse_blocks(&quote_refs, quote_depth + 1);
            limited |= nested.limited;
            blocks.push(MarkdownBlock::Quote(nested.blocks));
            continue;
        }

        if let Some(first) = list_marker(lines[index]) {
            let mut items = Vec::new();
            let ordered = first.ordered;
            let start = first.number;
            while index < lines.len() {
                let Some(marker) = list_marker(lines[index]) else {
                    break;
                };
                if marker.ordered != ordered {
                    break;
                }
                if items.len() >= MAX_LIST_ITEMS {
                    limited = true;
                    index = lines.len();
                    break;
                }

                let (checked, first_line) = task_marker(marker.content);
                let mut content = first_line.trim().to_owned();
                index += 1;
                while index < lines.len() {
                    if lines[index].trim().is_empty() {
                        break;
                    }
                    if list_marker(lines[index]).is_some() || starts_block(lines[index]) {
                        break;
                    }
                    if leading_indent(lines[index]) < 2 {
                        break;
                    }
                    let continuation = lines[index].trim();
                    if !continuation.is_empty() {
                        if !content.is_empty() {
                            content.push(' ');
                        }
                        content.push_str(continuation);
                    }
                    index += 1;
                }
                items.push(MarkdownListItem {
                    checked,
                    content: parse_inline(&content),
                });
            }
            blocks.push(MarkdownBlock::List {
                ordered,
                start,
                items,
            });
            continue;
        }

        let mut paragraph = lines[index].trim().to_owned();
        index += 1;
        while index < lines.len() && !lines[index].trim().is_empty() && !starts_block(lines[index])
        {
            let continuation = lines[index].trim();
            if !continuation.is_empty() {
                if !paragraph.is_empty() {
                    paragraph.push(' ');
                }
                paragraph.push_str(continuation);
            }
            index += 1;
        }
        blocks.push(MarkdownBlock::Paragraph(parse_inline(&paragraph)));
    }

    ParsedBlocks { blocks, limited }
}

fn starts_block(line: &str) -> bool {
    fence_start(line).is_some()
        || heading(line).is_some()
        || is_thematic_break(line)
        || quote_content(line).is_some()
        || list_marker(line).is_some()
}

fn strip_block_indent(line: &str) -> Option<&str> {
    let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
    (spaces <= 3).then_some(&line[spaces..])
}

fn fence_start(line: &str) -> Option<Fence<'_>> {
    let line = strip_block_indent(line)?;
    let marker = *line.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let length = line.bytes().take_while(|byte| *byte == marker).count();
    if length < 3 {
        return None;
    }
    let info = line[length..].trim();
    if marker == b'`' && info.contains('`') {
        return None;
    }
    Some(Fence {
        marker,
        length,
        info,
    })
}

fn is_fence_close(line: &str, fence: Fence<'_>) -> bool {
    let Some(line) = strip_block_indent(line) else {
        return false;
    };
    let length = line
        .bytes()
        .take_while(|byte| *byte == fence.marker)
        .count();
    length >= fence.length && line[length..].trim().is_empty()
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let line = strip_block_indent(line)?;
    let level = line.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut content = rest.trim();
    let without_hashes = content.trim_end_matches('#');
    if without_hashes.len() != content.len()
        && without_hashes
            .chars()
            .last()
            .is_none_or(char::is_whitespace)
    {
        content = without_hashes.trim_end();
    }
    Some((level as u8, content))
}

fn is_thematic_break(line: &str) -> bool {
    let Some(line) = strip_block_indent(line) else {
        return false;
    };
    let mut marker = None;
    let mut count = 0;
    for character in line.chars() {
        if character.is_whitespace() {
            continue;
        }
        if !matches!(character, '-' | '*' | '_') {
            return false;
        }
        if marker.is_some_and(|current| current != character) {
            return false;
        }
        marker = Some(character);
        count += 1;
    }
    count >= 3
}

fn quote_content(line: &str) -> Option<&str> {
    let line = strip_block_indent(line)?;
    let content = line.strip_prefix('>')?;
    Some(content.strip_prefix(' ').unwrap_or(content))
}

fn list_marker(line: &str) -> Option<ListMarker<'_>> {
    let line = strip_block_indent(line)?;
    let bytes = line.as_bytes();
    match bytes.first().copied()? {
        b'-' | b'+' | b'*' if bytes.get(1).is_some_and(u8::is_ascii_whitespace) => {
            Some(ListMarker {
                ordered: false,
                number: 1,
                content: &line[2..],
            })
        }
        first if first.is_ascii_digit() => {
            let digits = bytes
                .iter()
                .take(9)
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            let delimiter = *bytes.get(digits)?;
            if !matches!(delimiter, b'.' | b')')
                || !bytes.get(digits + 1).is_some_and(u8::is_ascii_whitespace)
            {
                return None;
            }
            let number = line[..digits].parse().ok()?;
            Some(ListMarker {
                ordered: true,
                number,
                content: &line[digits + 2..],
            })
        }
        _ => None,
    }
}

fn task_marker(content: &str) -> (Option<bool>, &str) {
    let bytes = content.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'[' && bytes[2] == b']' {
        let checked = match bytes[1] {
            b' ' => Some(false),
            b'x' | b'X' => Some(true),
            _ => None,
        };
        if checked.is_some()
            && (bytes.len() == 3 || bytes.get(3).is_some_and(u8::is_ascii_whitespace))
        {
            return (checked, content[3..].trim_start());
        }
    }
    (None, content)
}

fn leading_indent(line: &str) -> usize {
    line.chars()
        .take_while(|character| matches!(character, ' ' | '\t'))
        .map(|character| if character == '\t' { 4 } else { 1 })
        .sum()
}

fn parse_inline(source: &str) -> InlineText {
    let mut spans = Vec::new();
    parse_inline_into(source, InlineStyle::default(), None, 0, &mut spans);
    InlineText { spans }
}

fn parse_inline_into(
    source: &str,
    style: InlineStyle,
    inherited_link: Option<&str>,
    depth: usize,
    spans: &mut Vec<MarkdownSpan>,
) {
    if source.is_empty() {
        return;
    }
    if depth >= MAX_INLINE_DEPTH {
        push_span(spans, source, style, inherited_link);
        return;
    }

    let mut index = 0;
    while index < source.len() {
        let rest = &source[index..];

        if let Some(escaped) = escaped_character(rest) {
            push_span(spans, &escaped.to_string(), style, inherited_link);
            index += 1 + escaped.len_utf8();
            continue;
        }

        if rest.starts_with('`') {
            let ticks = rest.bytes().take_while(|byte| *byte == b'`').count();
            let delimiter = "`".repeat(ticks);
            if let Some(close) = find_unescaped(&rest[ticks..], &delimiter) {
                let mut code = &rest[ticks..ticks + close];
                if code.starts_with(' ')
                    && code.ends_with(' ')
                    && code.len() > 2
                    && !code.trim().is_empty()
                {
                    code = &code[1..code.len() - 1];
                }
                let mut code_style = style;
                code_style.code = true;
                push_span(spans, code, code_style, inherited_link);
                index += ticks + close + ticks;
                continue;
            }
        }

        if rest.starts_with("![")
            && let Some((label, _destination, consumed)) = parse_link(&rest[1..])
        {
            // Images are deliberately presentation-neutral and never fetched;
            // retain their useful alt text as ordinary inline content.
            parse_inline_into(label, style, inherited_link, depth + 1, spans);
            index += 1 + consumed;
            continue;
        }

        if rest.starts_with('[')
            && let Some((label, destination, consumed)) = parse_link(rest)
        {
            let link = sanitize_link(destination);
            parse_inline_into(
                label,
                style,
                link.as_deref().or(inherited_link),
                depth + 1,
                spans,
            );
            index += consumed;
            continue;
        }

        if rest.starts_with('<')
            && let Some(close) = rest[1..].find('>')
        {
            let candidate = &rest[1..1 + close];
            if looks_like_autolink(candidate) {
                let link = sanitize_link(candidate);
                push_span(spans, candidate, style, link.as_deref().or(inherited_link));
                index += close + 2;
                continue;
            }
        }

        if let Some((delimiter, update)) = style_delimiter(rest) {
            let after_open = &rest[delimiter.len()..];
            if let Some(close) = find_style_close(after_open, delimiter) {
                let inner = &after_open[..close];
                if !inner.is_empty() {
                    let mut nested = style;
                    update(&mut nested);
                    parse_inline_into(inner, nested, inherited_link, depth + 1, spans);
                    index += delimiter.len() + close + delimiter.len();
                    continue;
                }
            }
        }

        if inherited_link.is_none()
            && (index == 0
                || source[..index]
                    .chars()
                    .next_back()
                    .is_none_or(|character| !character.is_alphanumeric() && character != '_'))
            && let Some((url, consumed)) = bare_url(rest)
        {
            let link = sanitize_link(url);
            push_span(spans, url, style, link.as_deref());
            index += consumed;
            continue;
        }

        let character = rest.chars().next().expect("non-empty remainder");
        push_span(spans, &character.to_string(), style, inherited_link);
        index += character.len_utf8();
    }
}

fn escaped_character(source: &str) -> Option<char> {
    let mut characters = source.chars();
    if characters.next()? != '\\' {
        return None;
    }
    let escaped = characters.next()?;
    escaped.is_ascii_punctuation().then_some(escaped)
}

type StyleUpdate = fn(&mut InlineStyle);

fn style_delimiter(source: &str) -> Option<(&'static str, StyleUpdate)> {
    fn bold(style: &mut InlineStyle) {
        style.bold = true;
    }
    fn italic(style: &mut InlineStyle) {
        style.italic = true;
    }
    fn strike(style: &mut InlineStyle) {
        style.strikethrough = true;
    }

    if source.starts_with("**") {
        Some(("**", bold))
    } else if source.starts_with("__") {
        Some(("__", bold))
    } else if source.starts_with("~~") {
        Some(("~~", strike))
    } else if source.starts_with('*') {
        Some(("*", italic))
    } else if source.starts_with('_')
        && source[1..]
            .chars()
            .next()
            .is_some_and(|character| !character.is_whitespace())
    {
        Some(("_", italic))
    } else {
        None
    }
}

fn find_style_close(source: &str, delimiter: &str) -> Option<usize> {
    let mut offset = 0;
    while let Some(relative) = source[offset..].find(delimiter) {
        let candidate = offset + relative;
        if candidate > 0
            && !is_escaped(source, candidate)
            && source[..candidate]
                .chars()
                .next_back()
                .is_some_and(|character| !character.is_whitespace())
        {
            return Some(candidate);
        }
        offset = candidate + delimiter.len();
        if offset >= source.len() {
            return None;
        }
    }
    None
}

fn find_unescaped(source: &str, needle: &str) -> Option<usize> {
    let mut offset = 0;
    while let Some(relative) = source[offset..].find(needle) {
        let candidate = offset + relative;
        if !is_escaped(source, candidate) {
            return Some(candidate);
        }
        offset = candidate + needle.len();
    }
    None
}

fn is_escaped(source: &str, index: usize) -> bool {
    source.as_bytes()[..index]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count()
        % 2
        == 1
}

fn parse_link(source: &str) -> Option<(&str, &str, usize)> {
    if !source.starts_with('[') {
        return None;
    }
    let label_end = matching_bracket(source, 1)?;
    let destination_open = label_end + 1;
    if source.as_bytes().get(destination_open) != Some(&b'(') {
        return None;
    }
    let destination_end = matching_parenthesis(source, destination_open + 1)?;
    let label = &source[1..label_end];
    let raw_destination = source[destination_open + 1..destination_end].trim();
    let destination = link_destination(raw_destination);
    Some((label, destination, destination_end + 1))
}

fn matching_bracket(source: &str, mut index: usize) -> Option<usize> {
    let mut nested = 0;
    while index < source.len() {
        let character = source[index..].chars().next()?;
        if !is_escaped(source, index) {
            match character {
                '[' => nested += 1,
                ']' if nested == 0 => return Some(index),
                ']' => nested -= 1,
                _ => {}
            }
        }
        index += character.len_utf8();
    }
    None
}

fn matching_parenthesis(source: &str, mut index: usize) -> Option<usize> {
    let mut nested = 0;
    let mut angle_destination = false;
    let mut quoted = None;
    while index < source.len() {
        let character = source[index..].chars().next()?;
        if is_escaped(source, index) {
            index += character.len_utf8();
            continue;
        }
        if let Some(quote) = quoted {
            if character == quote {
                quoted = None;
            }
        } else {
            match character {
                '<' if nested == 0 => angle_destination = true,
                '>' if angle_destination => angle_destination = false,
                '"' | '\'' if !angle_destination => quoted = Some(character),
                '(' if !angle_destination => nested += 1,
                ')' if !angle_destination && nested == 0 => return Some(index),
                ')' if !angle_destination => nested -= 1,
                _ => {}
            }
        }
        index += character.len_utf8();
    }
    None
}

fn link_destination(source: &str) -> &str {
    if let Some(inner) = source
        .strip_prefix('<')
        .and_then(|value| value.split_once('>'))
    {
        return inner.0.trim();
    }
    let end = source
        .char_indices()
        .find(|(index, character)| character.is_whitespace() && !is_escaped(source, *index))
        .map_or(source.len(), |(index, _)| index);
    source[..end].trim()
}

fn looks_like_autolink(candidate: &str) -> bool {
    !candidate.contains(char::is_whitespace)
        && (candidate.contains("://")
            || (candidate.contains('@') && candidate.rsplit_once('.').is_some()))
}

fn sanitize_link(destination: &str) -> Option<String> {
    let destination = destination.trim();
    if destination.is_empty()
        || destination
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return None;
    }
    let (scheme, remainder) = destination.split_once(':')?;
    if !matches_ignore_ascii_case(scheme, &["http", "https", "file"])
        || !remainder.starts_with("//")
    {
        return None;
    }
    Some(destination.to_owned())
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn bare_url(source: &str) -> Option<(&str, usize)> {
    if !["http://", "https://", "file://"].iter().any(|prefix| {
        source
            .get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    }) {
        return None;
    }
    let mut end = source.len();
    for (index, character) in source.char_indices() {
        if character.is_whitespace() || matches!(character, '<' | '>' | '"' | '\'' | '`') {
            end = index;
            break;
        }
    }
    let candidate = &source[..end];
    let usable = trim_url_punctuation(candidate);
    if usable.is_empty() {
        return None;
    }
    Some((usable, usable.len()))
}

fn trim_url_punctuation(mut candidate: &str) -> &str {
    candidate = candidate.trim_end_matches(['.', ',', ';', ':', '!', '?']);
    for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
        while candidate.ends_with(close)
            && candidate
                .chars()
                .filter(|character| *character == close)
                .count()
                > candidate
                    .chars()
                    .filter(|character| *character == open)
                    .count()
        {
            candidate = &candidate[..candidate.len() - close.len_utf8()];
        }
    }
    candidate
}

fn push_span(spans: &mut Vec<MarkdownSpan>, text: &str, style: InlineStyle, link: Option<&str>) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.style == style
        && last.link.as_deref() == link
    {
        last.text.push_str(text);
        return;
    }
    spans.push(MarkdownSpan {
        text: text.to_owned(),
        style,
        link: link.map(str::to_owned),
    });
}

fn blocks_plain_text(blocks: &[MarkdownBlock]) -> String {
    blocks
        .iter()
        .map(block_plain_text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn block_plain_text(block: &MarkdownBlock) -> String {
    match block {
        MarkdownBlock::Heading { content, .. } | MarkdownBlock::Paragraph(content) => {
            content.plain_text()
        }
        MarkdownBlock::List {
            ordered,
            start,
            items,
        } => items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let marker = if *ordered {
                    format!("{}. ", start.saturating_add(index))
                } else {
                    "- ".to_owned()
                };
                let task = match item.checked {
                    Some(true) => "[x] ",
                    Some(false) => "[ ] ",
                    None => "",
                };
                format!("{marker}{task}{}", item.content.plain_text())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        MarkdownBlock::Quote(blocks) => blocks_plain_text(blocks)
            .lines()
            .map(|line| {
                if line.is_empty() {
                    ">".to_owned()
                } else {
                    format!("> {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        MarkdownBlock::CodeBlock { code, .. } => code.clone(),
        MarkdownBlock::ThematicBreak => "---".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paragraph(document: &MarkdownDocument, index: usize) -> &InlineText {
        match &document.blocks[index] {
            MarkdownBlock::Paragraph(content) => content,
            block => panic!("expected paragraph, got {block:?}"),
        }
    }

    #[test]
    fn parses_headings_paragraph_continuations_and_rules_as_blocks() {
        let document = MarkdownDocument::parse(
            "# Review cockpit\n\nFirst line\ncontinues here.\n\n- - -\n\n###### Small ######",
        );

        assert!(!document.truncated);
        assert_eq!(document.blocks.len(), 4);
        assert_eq!(
            document.blocks[0],
            MarkdownBlock::Heading {
                level: 1,
                content: parse_inline("Review cockpit"),
            }
        );
        assert_eq!(
            paragraph(&document, 1).plain_text(),
            "First line continues here."
        );
        assert_eq!(document.blocks[2], MarkdownBlock::ThematicBreak);
        assert_eq!(
            document.blocks[3],
            MarkdownBlock::Heading {
                level: 6,
                content: parse_inline("Small"),
            }
        );
    }

    #[test]
    fn parses_unordered_ordered_and_task_lists_with_continuations() {
        let document = MarkdownDocument::parse(
            "- plain\n- [ ] waiting\n- [X] shipped\n  with details\n\n3. third\n4) fourth",
        );

        let MarkdownBlock::List {
            ordered,
            start,
            items,
        } = &document.blocks[0]
        else {
            panic!("unordered list")
        };
        assert!(!ordered);
        assert_eq!(*start, 1);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].checked, None);
        assert_eq!(items[1].checked, Some(false));
        assert_eq!(items[2].checked, Some(true));
        assert_eq!(items[2].content.plain_text(), "shipped with details");

        let MarkdownBlock::List {
            ordered,
            start,
            items,
        } = &document.blocks[1]
        else {
            panic!("ordered list")
        };
        assert!(ordered);
        assert_eq!(*start, 3);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn parses_nested_quotes_into_nested_documents() {
        let document = MarkdownDocument::parse(
            "> ## Reviewer\n> First paragraph\n> continues.\n>\n> > Nested **answer**",
        );
        let MarkdownBlock::Quote(outer) = &document.blocks[0] else {
            panic!("outer quote")
        };
        assert!(matches!(
            outer.first(),
            Some(MarkdownBlock::Heading { level: 2, .. })
        ));
        assert_eq!(
            match &outer[1] {
                MarkdownBlock::Paragraph(text) => text.plain_text(),
                block => panic!("paragraph, got {block:?}"),
            },
            "First paragraph continues."
        );
        assert!(matches!(outer[2], MarkdownBlock::Quote(_)));
    }

    #[test]
    fn fenced_code_preserves_source_and_language_without_inline_parsing() {
        let document = MarkdownDocument::parse(
            "```rust\nlet marker = \"**not bold**\";\nprintln!(\"{marker}\");\n```",
        );
        assert_eq!(
            document.blocks,
            vec![MarkdownBlock::CodeBlock {
                language: Some("rust".to_owned()),
                code: "let marker = \"**not bold**\";\nprintln!(\"{marker}\");".to_owned(),
            }]
        );
    }

    #[test]
    fn parses_inline_emphasis_code_strike_and_nested_styles() {
        let document = MarkdownDocument::parse(
            "Normal **bold and _italic_** plus *soft*, ~~gone~~, and `a **literal**`.",
        );
        let spans = &paragraph(&document, 0).spans;
        assert_eq!(
            paragraph(&document, 0).plain_text(),
            "Normal bold and italic plus soft, gone, and a **literal**."
        );
        assert!(
            spans
                .iter()
                .any(|span| span.text == "bold and " && span.style.bold)
        );
        assert!(
            spans
                .iter()
                .any(|span| { span.text == "italic" && span.style.bold && span.style.italic })
        );
        assert!(
            spans
                .iter()
                .any(|span| span.text == "soft" && span.style.italic)
        );
        assert!(
            spans
                .iter()
                .any(|span| span.text == "gone" && span.style.strikethrough)
        );
        assert!(
            spans
                .iter()
                .any(|span| span.text == "a **literal**" && span.style.code)
        );
    }

    #[test]
    fn keeps_link_labels_but_sanitizes_activation_data() {
        let document = MarkdownDocument::parse(
            "[Web](https://example.com/a) [Local](file:///tmp/a.rs) [Bad](javascript:alert(1)) [Relative](../README.md)",
        );
        let spans = &paragraph(&document, 0).spans;
        assert_eq!(
            paragraph(&document, 0).plain_text(),
            "Web Local Bad Relative"
        );
        assert_eq!(
            spans.iter().find(|span| span.text == "Web").unwrap().link,
            Some("https://example.com/a".to_owned())
        );
        assert_eq!(
            spans.iter().find(|span| span.text == "Local").unwrap().link,
            Some("file:///tmp/a.rs".to_owned())
        );
        assert_eq!(
            spans
                .iter()
                .filter_map(|span| span.link.as_deref())
                .collect::<Vec<_>>(),
            vec!["https://example.com/a", "file:///tmp/a.rs"]
        );

        let unsafe_document =
            MarkdownDocument::parse("[Bad](javascript:alert(1)) [Relative](../README.md)");
        assert_eq!(paragraph(&unsafe_document, 0).plain_text(), "Bad Relative");
        assert!(
            paragraph(&unsafe_document, 0)
                .spans
                .iter()
                .all(|span| span.link.is_none())
        );
    }

    #[test]
    fn parses_angle_and_bare_autolinks_without_swallowing_punctuation() {
        let document = MarkdownDocument::parse(
            "See <https://example.com/docs> and https://example.com/path?q=1. Email <dev@example.com>.",
        );
        let content = paragraph(&document, 0);
        assert_eq!(
            content.plain_text(),
            "See https://example.com/docs and https://example.com/path?q=1. Email dev@example.com."
        );
        let links = content
            .spans
            .iter()
            .filter_map(|span| span.link.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(
            links,
            vec!["https://example.com/docs", "https://example.com/path?q=1"]
        );
    }

    #[test]
    fn raw_html_is_retained_as_text_and_images_become_alt_text() {
        let document = MarkdownDocument::parse(
            "<section class=\"note\">hello</section> ![diagram](https://example.com/x.png)",
        );
        assert_eq!(
            paragraph(&document, 0).plain_text(),
            "<section class=\"note\">hello</section> diagram"
        );
        assert!(
            paragraph(&document, 0)
                .spans
                .iter()
                .all(|span| span.link.is_none())
        );
    }

    #[test]
    fn source_limit_respects_utf8_and_adds_a_visible_notice() {
        let mut source = "a".repeat(MAX_SOURCE_BYTES - 1);
        source.push('🛠');
        source.push_str("tail");
        let document = MarkdownDocument::parse(&source);

        assert!(document.truncated);
        assert_eq!(
            paragraph(&document, document.blocks.len() - 1).plain_text(),
            TRUNCATION_NOTICE
        );
        assert!(document.plain_text().contains(TRUNCATION_NOTICE));
    }

    #[test]
    fn plain_text_keeps_block_list_quote_and_code_structure() {
        let document = MarkdownDocument::parse(
            "# Summary\n\n- [x] Tests\n- Review\n\n> Important\n\n```text\nline one\nline two\n```",
        );
        assert_eq!(
            document.plain_text(),
            "Summary\n\n- [x] Tests\n- Review\n\n> Important\n\nline one\nline two"
        );
    }

    #[test]
    fn malformed_inline_syntax_remains_readable_literal_text() {
        let document =
            MarkdownDocument::parse("Unclosed **bold and [link](missing plus \\*escaped stars\\*.");
        assert_eq!(
            paragraph(&document, 0).plain_text(),
            "Unclosed **bold and [link](missing plus *escaped stars*."
        );
    }

    #[test]
    fn turn_selection_defaults_to_a_whole_turn_and_supports_utf8_ranges() {
        let turns = vec!["first turn".to_owned(), "修复 this turn".to_owned()];
        let mut selection = TurnSelection::default();
        selection.select_turn(1);
        assert_eq!(selection.selected_text(&turns), Some("修复 this turn"));

        // Deliberately land inside the first scalar; selection expands to
        // valid boundaries rather than producing malformed UTF-8.
        selection.select_text(1, 1..5, &turns[1]);
        assert_eq!(selection.selected_text(&turns), Some("修复"));
        assert_eq!(selection.turn(), Some(1));
        selection.clear();
        assert_eq!(selection.selected_text(&turns), None);
    }
}
