//! GPUI grid rendering, terminal input encoding, selection, scrollback viewport, and find highlighting.

mod blocks;
pub mod buffer;
pub mod command_marks;
mod contrast;
pub mod crossfade;
pub mod cursor_focus;
pub mod cursor_motion;
pub mod element;
pub mod find;
pub mod glyphs;
pub mod identity_hue;
pub mod inline_images;
pub mod keys;
pub mod messages;
pub mod metrics;
pub mod scroll_glide;
pub mod scrollback;
pub mod selection;
mod selection_shape;
pub mod selection_shimmer;
pub mod smooth_scroll;
mod sprites;
pub mod theme;
pub mod theme_file;
pub mod themes;
pub mod tints;
