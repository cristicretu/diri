//! Mermaid diagrams in notes. A code block fenced `mermaid` draws as its
//! diagram: `mermaid-rs-renderer` parses and lays it out to SVG off the main
//! thread, and gpui rasterizes the SVG like any other picture. No browser,
//! Node.js or network is involved.
//!
//! [`Diagrams`] keeps one drawing per block, keyed by its source and the
//! appearance it was drawn for. While an edit's redraw is on its way the
//! block keeps showing its last good drawing, so typing never flickers.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use gpui::{Image, ImageFormat, SharedString};
use mermaid_rs_renderer::{LayoutConfig, Theme, compute_layout, parse_mermaid_strict, render};

/// Sources longer than this are not drawn: layout time grows faster than
/// the text, and a diagram this big is unreadable at a note's width anyway.
const MAX_SOURCE: usize = 32 * 1024;

/// A diagram drawn to a picture, with its natural size in points.
#[derive(Clone)]
pub(super) struct Drawing {
    pub image: Arc<Image>,
    pub width: f32,
    pub height: f32,
}

/// What a diagram block shows this frame.
#[derive(Clone)]
pub(super) enum Look {
    /// Not drawn yet, and nothing earlier to stand in.
    Pending,
    Drawn(Drawing),
    /// The source does not draw; the message says why. The last good
    /// drawing, if any, rides along for an edit in progress.
    Failed(SharedString, Option<Drawing>),
}

struct Entry {
    key: u64,
    /// The drawing for `key`, once it lands.
    result: Option<Result<Drawing, SharedString>>,
    /// The last good drawing, shown while `key`'s is on its way.
    last: Option<Drawing>,
}

/// Each diagram block's drawing, by block id.
#[derive(Default)]
pub(super) struct Diagrams {
    entries: HashMap<u64, Entry>,
    /// Pictures no block shows any more, whose rasters gpui still caches
    /// until the view releases them ([`Self::take_retired`]).
    retired: Vec<Arc<Image>>,
}

impl Diagrams {
    /// What block `id` shows for `source`. The key is returned when the
    /// caller should start drawing it ([`draw`]) and report back through
    /// [`Self::landed`]: the first time a block is seen, and after each
    /// change to its source or the appearance.
    pub fn look(&mut self, id: u64, source: &str, dark: bool) -> (Look, Option<u64>) {
        let key = key(source, dark);
        let mut start = None;
        let entry = self.entries.entry(id).or_insert_with(|| {
            start = Some(key);
            Entry {
                key,
                result: None,
                last: None,
            }
        });
        if entry.key != key {
            let last = match entry.result.take() {
                Some(Ok(drawing)) => {
                    self.retired.extend(entry.last.take().map(|old| old.image));
                    Some(drawing)
                }
                _ => entry.last.take(),
            };
            *entry = Entry {
                key,
                result: None,
                last,
            };
            start = Some(key);
        }
        let look = match &entry.result {
            Some(Ok(drawing)) => Look::Drawn(drawing.clone()),
            Some(Err(message)) => Look::Failed(message.clone(), entry.last.clone()),
            None => match &entry.last {
                Some(drawing) => Look::Drawn(drawing.clone()),
                None => Look::Pending,
            },
        };
        (look, start)
    }

    /// A drawing started for `key` finished. Returns whether it is still
    /// wanted: a later edit may have moved the block on to another key.
    pub fn landed(&mut self, id: u64, key: u64, result: Result<Drawing, SharedString>) -> bool {
        match self.entries.get_mut(&id) {
            Some(entry) if entry.key == key => {
                // A new drawing replaces the stand-in; a failure keeps it
                // to show faded under the error.
                if result.is_ok() {
                    self.retired.extend(entry.last.take().map(|old| old.image));
                }
                entry.result = Some(result);
                true
            }
            _ => false,
        }
    }

    #[cfg(test)]
    pub fn is_drawn(&self, id: u64) -> bool {
        self.entries
            .get(&id)
            .is_some_and(|entry| matches!(entry.result, Some(Ok(_))))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Forgets blocks that are gone or no longer diagrams.
    pub fn retain(&mut self, keep: impl Fn(u64) -> bool) {
        let retired = &mut self.retired;
        self.entries.retain(|id, entry| {
            let kept = keep(*id);
            if !kept {
                retired.extend(entry.images());
            }
            kept
        });
    }

    /// Pictures dropped since the last call, for the view to release from
    /// gpui's image cache and sprite atlas.
    pub fn take_retired(&mut self) -> Vec<Arc<Image>> {
        std::mem::take(&mut self.retired)
    }

    /// Every picture held, emptying the cache: the view is going away.
    pub fn take_all(&mut self) -> Vec<Arc<Image>> {
        let mut all = self.take_retired();
        for (_, mut entry) in self.entries.drain() {
            all.extend(entry.images());
        }
        all
    }
}

impl Entry {
    /// Takes the pictures this entry holds.
    fn images(&mut self) -> impl Iterator<Item = Arc<Image>> + use<> {
        let result = match self.result.take() {
            Some(Ok(drawing)) => Some(drawing.image),
            _ => None,
        };
        result.into_iter().chain(self.last.take().map(|d| d.image))
    }
}

fn key(source: &str, dark: bool) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    dark.hash(&mut hasher);
    hasher.finish()
}

/// Draws `source` for a light or dark note. Runs off the main thread: the
/// first call in the process loads the system fonts the renderer measures
/// labels with (one shared database behind a lock, so draws run one at a
/// time).
pub(super) fn draw(source: &str, dark: bool) -> Result<Drawing, SharedString> {
    if source.trim().is_empty() {
        return Err(crate::i18n::t("notes.diagram.empty").into());
    }
    if source.len() > MAX_SOURCE {
        return Err(crate::i18n::t("notes.diagram.too_big").into());
    }
    // A third-party parser meeting text it never expected must not take
    // the app down with it, nor file a crash report per keystroke.
    let drawn = diri_telemetry::catch_expected(|| svg(source, dark)).map_err(|_| {
        SharedString::from(crate::i18n::t("notes.diagram.failed"))
    })??;
    let (svg, width, height) = drawn;
    Ok(Drawing {
        image: Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes())),
        width,
        height,
    })
}

fn svg(source: &str, dark: bool) -> Result<(String, f32, f32), SharedString> {
    let parsed = parse_mermaid_strict(source).map_err(|e| SharedString::from(first_line(&e)))?;
    let theme = theme(dark);
    let config = LayoutConfig::default();
    let layout = compute_layout(&parsed.graph, &theme, &config);
    let size = render::measure_svg_dimensions(&layout, &config, None);
    let (width, height) = (size.viewbox_width, size.viewbox_height);
    if !(width.is_finite() && height.is_finite() && width >= 1.0 && height >= 1.0) {
        return Err(crate::i18n::t("notes.diagram.failed").into());
    }
    // An explicit size: some diagram kinds otherwise ask for `width="100%"`,
    // which only means something to a browser.
    let svg = render::render_svg_with_dimensions(&layout, &theme, &config, Some((width, height)));
    Ok((svg, width, height))
}

fn first_line(error: &impl std::fmt::Display) -> String {
    let text = error.to_string();
    let line = text.lines().next().unwrap_or_default().trim();
    let mut short: String = line.chars().take(160).collect();
    if short.len() < line.len() {
        short.push('…');
    }
    short
}

/// The renderer's modern theme, set on a clear ground so the note's own
/// surface shows through, with a neutral dark variant that sits on diri's
/// grays. Labels use
/// families every platform's font database knows, so what the layout
/// measures is what gpui paints.
fn theme(dark: bool) -> Theme {
    let mut theme = Theme::modern();
    theme.font_family = "\"Helvetica Neue\", Helvetica, Arial, \"Segoe UI\", \"DejaVu Sans\", \
                         \"Liberation Sans\", sans-serif"
        .to_owned();
    theme.background = "none".to_owned();
    if dark {
        let set = |slot: &mut String, value: &str| *slot = value.to_owned();
        set(&mut theme.primary_color, "#27272A");
        set(&mut theme.primary_text_color, "#E4E4E7");
        set(&mut theme.primary_border_color, "#71717A");
        set(&mut theme.line_color, "#A1A1AA");
        set(&mut theme.secondary_color, "#3F3F46");
        set(&mut theme.tertiary_color, "#18181B");
        set(&mut theme.edge_label_background, "#27272A");
        set(&mut theme.cluster_background, "#1C1C1F");
        set(&mut theme.cluster_border, "#3F3F46");
        set(&mut theme.sequence_actor_fill, "#27272A");
        set(&mut theme.sequence_actor_border, "#71717A");
        set(&mut theme.sequence_actor_line, "#A1A1AA");
        set(&mut theme.sequence_note_fill, "#3A2A16");
        set(&mut theme.sequence_note_border, "#B45309");
        set(&mut theme.sequence_activation_fill, "#3F3F46");
        set(&mut theme.sequence_activation_border, "#71717A");
        set(&mut theme.text_color, "#E4E4E7");
        set(&mut theme.pie_title_text_color, "#E4E4E7");
        set(&mut theme.pie_legend_text_color, "#E4E4E7");
        set(&mut theme.pie_stroke_color, "#18181B");
        set(&mut theme.pie_outer_stroke_color, "#3F3F46");
        set(&mut theme.git_commit_label_color, "#E4E4E7");
        set(&mut theme.git_commit_label_background, "#3F3F46");
    }
    theme
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svg_text(drawing: &Drawing) -> String {
        String::from_utf8(drawing.image.bytes().to_vec()).unwrap()
    }

    #[test]
    fn a_flowchart_draws_at_an_explicit_size() {
        let drawing = draw("flowchart LR\n  A[Idea] --> B[Plan] --> C[Ship]", false).unwrap();
        assert!(drawing.width > drawing.height, "a left-right chart is wide");
        let svg = svg_text(&drawing);
        assert!(svg.starts_with("<svg"), "{svg}");
        assert!(!svg.contains("width=\"100%\""), "{svg}");
        assert!(svg.contains("Idea") && svg.contains("Ship"), "{svg}");
    }

    #[test]
    fn every_kind_the_slash_menu_mentions_draws() {
        for source in [
            "sequenceDiagram\n  Alice->>Bob: Hi\n  Bob-->>Alice: Hello",
            "pie title Pets\n  \"Dogs\" : 386\n  \"Cats\" : 85",
            "stateDiagram-v2\n  [*] --> Still\n  Still --> Moving",
            "classDiagram\n  Animal <|-- Duck",
            "gitGraph\n  commit\n  branch dev\n  commit",
            "mindmap\n  root((Plan))\n    A\n    B",
        ] {
            for dark in [false, true] {
                let drawing = draw(source, dark).unwrap_or_else(|e| panic!("{source}: {e}"));
                let svg = svg_text(&drawing);
                assert!(!svg.contains("width=\"100%\""), "{source}: {svg}");
                assert!(drawing.width >= 1.0 && drawing.height >= 1.0, "{source}");
            }
        }
    }

    #[test]
    fn broken_or_empty_sources_fail_with_a_message() {
        assert!(draw("", false).is_err());
        assert!(draw("   \n ", false).is_err());
        for source in ["this is not mermaid", "flowchart LR\n  A --> B -->"] {
            match draw(source, false) {
                Err(message) => assert!(!message.is_empty(), "{source}"),
                Ok(_) => panic!("{source:?} drew"),
            }
        }
        assert!(draw(&"a".repeat(MAX_SOURCE + 1), false).is_err());
    }

    #[test]
    fn an_edit_keeps_the_last_drawing_until_the_new_one_lands() {
        let mut diagrams = Diagrams::default();
        let (look, start) = diagrams.look(7, "graph TD; A-->B", false);
        assert!(matches!(look, Look::Pending));
        let first = start.expect("a new block is drawn");
        let drawing = draw("graph TD; A-->B", false).unwrap();
        assert!(diagrams.landed(7, first, Ok(drawing)));
        // Same source: nothing to start, the drawing shows.
        let (look, start) = diagrams.look(7, "graph TD; A-->B", false);
        assert!(matches!(look, Look::Drawn(_)) && start.is_none());
        // An edit starts a redraw and keeps showing the old picture.
        let (look, start) = diagrams.look(7, "graph TD; A-->C", false);
        let second = start.expect("an edit redraws");
        assert!(matches!(look, Look::Drawn(_)));
        // A stale result for the first source is dropped.
        assert!(!diagrams.landed(7, first, Err("old".into())));
        assert!(diagrams.landed(7, second, Err("bad".into())));
        let (look, _) = diagrams.look(7, "graph TD; A-->C", false);
        assert!(matches!(look, Look::Failed(m, Some(_)) if m == "bad"));
        // The appearance is part of the key.
        let (_, start) = diagrams.look(7, "graph TD; A-->C", true);
        assert!(start.is_some());
        diagrams.retain(|id| id != 7);
        assert!(diagrams.look(7, "graph TD; A-->C", true).1.is_some());
    }

    #[test]
    fn replaced_and_removed_pictures_are_retired_once() {
        let picture = |source| draw(source, false).unwrap();
        let mut diagrams = Diagrams::default();
        let first = diagrams.look(1, "graph TD; A-->B", false).1.unwrap();
        diagrams.landed(1, first, Ok(picture("graph TD; A-->B")));
        assert!(diagrams.take_retired().is_empty());
        // The edit's redraw lands: the stand-in goes.
        let second = diagrams.look(1, "graph TD; A-->C", false).1.unwrap();
        diagrams.landed(1, second, Ok(picture("graph TD; A-->C")));
        assert_eq!(diagrams.take_retired().len(), 1);
        // A failure keeps the last good picture to show faded.
        let third = diagrams.look(1, "graph TD; A-->", false).1.unwrap();
        diagrams.landed(1, third, Err("bad".into()));
        assert!(diagrams.take_retired().is_empty());
        // The block goes: its picture with it.
        diagrams.retain(|_| false);
        assert_eq!(diagrams.take_retired().len(), 1);
        assert!(diagrams.take_all().is_empty());
    }
}
