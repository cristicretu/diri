//! Native GPUI presentation for the bounded Markdown document model.
//!
//! Parsing and sanitization live in `markdown`; this module translates the
//! resulting blocks into the inspector's visual language. Keeping the view
//! here gives PR descriptions, discussions, and future agent notes one
//! consistent readable measure and typographic hierarchy.

use std::ops::Range;

use diri_ui::{Ink, Radius, SemanticColors};
use gpui::{
    AnyElement, FontStyle, FontWeight, HighlightStyle, InteractiveText, Rgba, SharedString,
    StrikethroughStyle, StyledText, UnderlineStyle, div, prelude::*, px, rgba,
};

use crate::markdown::{InlineText, MarkdownBlock, MarkdownDocument};

/// Thin spaces that pad inline code inside its tint, so the tint does not sit
/// flush against the glyphs. They stay outside the monospace run.
const CODE_PAD: &str = "\u{2009}";

/// A type scale for one kind of surface.
#[derive(Clone, Copy, Debug)]
pub struct MarkdownLook {
    body_size: f32,
    body_line: f32,
    block_gap: f32,
    measure: f32,
    /// `(size, line height, space above)` for heading levels 1 through 4+.
    headings: [(f32, f32, f32); 4],
    /// Body copy in the primary ink rather than the secondary one.
    primary_body: bool,
    /// Code blocks on a dark card, rather than a tint of the canvas.
    code_card: bool,
}

impl MarkdownLook {
    /// Inspector panes: PR descriptions, discussions, skill files.
    pub const COMPACT: Self = Self {
        body_size: 11.5,
        body_line: 18.0,
        block_gap: 10.0,
        measure: 760.0,
        headings: [
            (18.0, 23.0, 5.0),
            (15.0, 20.0, 4.0),
            (13.0, 18.0, 3.0),
            (12.0, 17.0, 2.0),
        ],
        primary_body: false,
        code_card: true,
    };

    /// A page meant to be read top to bottom, such as release notes.
    pub const READING: Self = Self {
        body_size: 13.5,
        body_line: 22.0,
        block_gap: 12.0,
        measure: crate::notes::editor_view::MEASURE,
        headings: [
            (20.0, 27.0, 14.0),
            (16.5, 23.0, 12.0),
            (14.5, 21.0, 8.0),
            (13.5, 21.0, 6.0),
        ],
        primary_body: true,
        code_card: false,
    };
}

pub fn render_markdown(document: &MarkdownDocument, colors: SemanticColors) -> AnyElement {
    render_markdown_with(document, colors, MarkdownLook::COMPACT)
}

pub fn render_markdown_with(
    document: &MarkdownDocument,
    colors: SemanticColors,
    look: MarkdownLook,
) -> AnyElement {
    let mut content = div()
        .w_full()
        .max_w(px(look.measure))
        .flex()
        .flex_col()
        .gap(px(look.block_gap));

    for (index, block) in document.blocks.iter().enumerate() {
        content = content.child(render_block(block, colors, look, &[index]));
    }

    content.into_any_element()
}

fn body_ink(colors: SemanticColors, look: MarkdownLook) -> Rgba {
    if look.primary_body {
        colors.primary.alpha(0.86)
    } else {
        colors.secondary
    }
}

/// `path` is the block's position in the document, so every link region has
/// a stable element id across frames.
fn render_block(
    block: &MarkdownBlock,
    colors: SemanticColors,
    look: MarkdownLook,
    path: &[usize],
) -> AnyElement {
    match block {
        MarkdownBlock::Heading { level, content } => {
            let (size, line_height, top) = look.headings[usize::from((*level).clamp(1, 4) - 1)];
            div()
                .mt(px(top))
                .line_height(px(line_height))
                .text_size(px(size))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.primary)
                .child(render_inline(content, colors, path))
                .into_any_element()
        }
        MarkdownBlock::Paragraph(content) => div()
            .line_height(px(look.body_line))
            .text_size(px(look.body_size))
            .text_color(body_ink(colors, look))
            .child(render_inline(content, colors, path))
            .into_any_element(),
        MarkdownBlock::List {
            ordered,
            start,
            items,
        } => {
            let mut list = div().flex().flex_col().gap(px(look.block_gap * 0.5));
            for (index, item) in items.iter().enumerate() {
                let marker = match item.checked {
                    Some(true) => "✓".to_owned(),
                    Some(false) => "○".to_owned(),
                    None if *ordered => format!("{}.", start + index),
                    None => "•".to_owned(),
                };
                let marker_color = match item.checked {
                    Some(true) => Ink::FRESH,
                    Some(false) => colors.tertiary,
                    None => colors.tertiary,
                };
                let item_path = [path, &[index]].concat();
                list = list.child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(8.0))
                        .line_height(px(look.body_line))
                        .text_size(px(look.body_size))
                        .text_color(body_ink(colors, look))
                        .child(
                            div()
                                .w(px(18.0))
                                .flex_none()
                                .text_right()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(marker_color)
                                .child(marker),
                        )
                        .child(div().min_w(px(0.0)).flex_1().child(render_inline(
                            &item.content,
                            colors,
                            &item_path,
                        ))),
                );
            }
            list.into_any_element()
        }
        MarkdownBlock::Quote(blocks) => {
            let mut quote = div()
                .pl(px(11.0))
                .py(px(3.0))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .border_l_2()
                .border_color(rgba(0xd9775788))
                .text_color(colors.secondary);
            for (index, block) in blocks.iter().enumerate() {
                quote = quote.child(render_block(
                    block,
                    colors,
                    look,
                    &[path, &[index]].concat(),
                ));
            }
            quote.into_any_element()
        }
        MarkdownBlock::CodeBlock { language, code } => {
            let mut code_lines = div()
                .w_full()
                .p(px(10.0))
                .flex()
                .flex_col()
                .font_family(crate::fonts::mono_family())
                .line_height(px(17.0))
                .text_size(px(10.5))
                .text_color(if look.code_card {
                    rgba(0xd8dee9ff)
                } else {
                    colors.primary.alpha(0.86)
                });
            for line in code.lines() {
                code_lines = code_lines.child(
                    div()
                        .whitespace_nowrap()
                        .child(SharedString::from(if line.is_empty() { " " } else { line })),
                );
            }
            div()
                .rounded(px(Radius::BADGE))
                .when(look.code_card, |surface| {
                    surface
                        .bg(rgba(0x0d1117aa))
                        .border_1()
                        .border_color(colors.primary.alpha(0.075))
                })
                .when(!look.code_card, |surface| {
                    surface.bg(colors.primary.alpha(0.04))
                })
                .overflow_hidden()
                .when_some(language.clone(), |surface, language| {
                    surface.child(
                        div()
                            .h(px(25.0))
                            .px(px(9.0))
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(colors.primary.alpha(0.06))
                            .text_size(px(9.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.tertiary)
                            .child(language),
                    )
                })
                .child(
                    div()
                        .id(SharedString::from(format!("markdown-code-{code:p}")))
                        .w_full()
                        .overflow_x_scroll()
                        .child(code_lines),
                )
                .into_any_element()
        }
        MarkdownBlock::ThematicBreak => div()
            .my(px(3.0))
            .h(px(1.0))
            .w_full()
            .bg(colors.primary.alpha(0.08))
            .into_any_element(),
    }
}

/// One block's inline content flattened into a single run of text, so the
/// text system wraps it as prose. Laying each span out as its own box made a
/// long span an unbreakable flex item that the block clipped.
#[derive(Debug, Default, PartialEq)]
struct InlineRuns {
    text: String,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    monospace: Vec<Range<usize>>,
    links: Vec<(Range<usize>, String)>,
}

fn inline_runs(content: &InlineText, colors: SemanticColors) -> InlineRuns {
    let code_ink = Ink::on_surface(rgba(0xe7b49fff), colors);
    let link_ink = Ink::on_surface(rgba(0x8bb9e8ff), colors);
    let mut runs = InlineRuns::default();
    for span in &content.spans {
        if span.text.is_empty() {
            continue;
        }
        let style = span.style;
        let mut highlight = HighlightStyle {
            font_weight: style.bold.then_some(FontWeight::SEMIBOLD),
            font_style: style.italic.then_some(FontStyle::Italic),
            strikethrough: style.strikethrough.then(|| StrikethroughStyle {
                thickness: px(1.0),
                color: None,
            }),
            ..HighlightStyle::default()
        };
        if style.code {
            highlight.color = Some(code_ink.into());
            highlight.background_color = Some(colors.primary.alpha(0.065).into());
        }
        if span.link.is_some() {
            highlight.color = Some(link_ink.into());
            highlight.underline = Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(link_ink.alpha(0.5).into()),
                wavy: false,
            });
        }

        let start = runs.text.len();
        if style.code {
            // Three highlights, not one: a font override only lands on a run
            // that sits wholly inside it, so the padding needs runs of its own.
            for (piece, mono) in [
                (CODE_PAD, false),
                (span.text.as_str(), true),
                (CODE_PAD, false),
            ] {
                let from = runs.text.len();
                runs.text.push_str(piece);
                runs.highlights.push((from..runs.text.len(), highlight));
                if mono {
                    runs.monospace.push(from..runs.text.len());
                }
            }
        } else {
            runs.text.push_str(&span.text);
            if highlight != HighlightStyle::default() {
                runs.highlights.push((start..runs.text.len(), highlight));
            }
        }
        if let Some(link) = &span.link {
            runs.links.push((start..runs.text.len(), link.clone()));
        }
    }
    runs
}

fn render_inline(content: &InlineText, colors: SemanticColors, path: &[usize]) -> AnyElement {
    let InlineRuns {
        text,
        highlights,
        monospace,
        links,
    } = inline_runs(content, colors);
    let mono = SharedString::from(crate::fonts::mono_family());
    let styled = StyledText::new(text)
        .with_highlights(highlights)
        .with_font_family_overrides(monospace.into_iter().map(|range| (range, mono.clone())));
    if links.is_empty() {
        return styled.into_any_element();
    }
    let id = path
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join("-");
    let (ranges, urls): (Vec<_>, Vec<_>) = links.into_iter().unzip();
    InteractiveText::new(SharedString::from(format!("markdown-inline-{id}")), styled)
        .on_click(ranges, move |index, _, cx| {
            if let Some(url) = urls.get(index) {
                cx.open_url(url);
            }
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::MarkdownDocument;
    use gpui::{TestAppContext, VisualTestContext};

    #[test]
    fn renderer_source_keeps_a_readable_measure_and_native_block_treatment() {
        let source = include_str!("markdown_view.rs");
        assert!(source.contains("measure: 760.0"));
        assert!(source.contains("MarkdownBlock::CodeBlock"));
        assert!(source.contains("MarkdownBlock::List"));
        assert!(source.contains("MarkdownBlock::Quote"));
    }

    fn paragraph(source: &str) -> InlineText {
        match MarkdownDocument::parse(source).blocks.remove(0) {
            MarkdownBlock::Paragraph(content) => content,
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn inline_styles_become_runs_over_one_continuous_string() {
        let runs = inline_runs(
            &paragraph("Run `diri doctor` or read [the docs](https://diri.sh/docs/) **now**."),
            SemanticColors::dark(),
        );

        assert_eq!(
            runs.text,
            "Run \u{2009}diri doctor\u{2009} or read the docs now."
        );
        let code = runs.text.find("diri doctor").unwrap();
        assert_eq!(runs.monospace, vec![code..code + "diri doctor".len()]);
        let link = runs.text.find("the docs").unwrap();
        assert_eq!(
            runs.links,
            [(
                link..link + "the docs".len(),
                "https://diri.sh/docs/".to_owned()
            )]
        );
        let bold = runs.text.find("now").unwrap();
        assert!(runs.highlights.iter().any(|(range, style)| {
            *range == (bold..bold + 3) && style.font_weight == Some(FontWeight::SEMIBOLD)
        }));
        let mut end = 0;
        for (range, _) in &runs.highlights {
            assert!(range.start >= end, "highlights are ordered and disjoint");
            end = range.end;
        }
    }

    #[gpui::test]
    fn a_long_paragraph_wraps_inside_its_column_instead_of_clipping(cx: &mut TestAppContext) {
        const WIDTH: f32 = 240.0;
        // A bold lead and then one long plain span: the shape of the 0.9.0
        // notes, whose spans each stayed on one line and ran past the card.
        let sentence = "Sessions reconnect quietly after sleep, the Holder keeps its PTY \
            alive, and release notes read like prose. ";
        let document = MarkdownDocument::parse(&format!(
            "**Notes** {} [#597](https://diri.sh/)",
            sentence.repeat(6)
        ));

        struct Page(MarkdownDocument);
        impl gpui::Render for Page {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                div()
                    .w(px(WIDTH))
                    .child(div().debug_selector(|| "markdown-column".into()).child(
                        render_markdown_with(
                            &self.0,
                            SemanticColors::light(),
                            MarkdownLook::READING,
                        ),
                    ))
            }
        }

        let window = cx.add_window(|_, _| Page(document));
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();

        let column = cx
            .debug_bounds("markdown-column")
            .expect("the markdown column is laid out");
        assert!(
            column.size.width <= px(WIDTH),
            "the text stays inside its column: {column:?}"
        );
        assert!(
            column.size.height >= px(MarkdownLook::READING.body_line * 8.0),
            "six long sentences wrap onto many lines rather than one clipped line: {column:?}"
        );
    }
}
