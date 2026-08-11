//! Dirijor's shared GPUI design system.
//!
//! Tokens, vector brand marks, and status behavior carry over from the retired
//! SwiftUI client.
//! GPUI currently exposes circular rounded rectangles rather than SwiftUI's
//! continuous-corner squircles, so radii use GPUI rounded corners as the
//! documented approximation. See the gallery example for every supported state.

mod brand;
mod components;
mod icon;
mod status;
mod svg;
mod tokens;

pub use brand::{
    AgentKind, AgentLogo, BrandMark, BrandMarkKind, CLAUDE_PATH, CURSOR_PATH, GEMINI_PATH,
    MarkRasterizer, OPENAI_PATH, set_mark_rasterizer,
};
pub use components::{FloatingSurface, HairlineDivider, HoverMarquee, LoadingIndicator, RowFill};
pub use icon::{Icon, IconAssets, IconName, IconSize, icon_from_system_name};
pub use status::{
    AnimationPhase, AttentionDot, AttentionLevel, StatusGlyph, StatusState, wall_clock_seconds,
};
pub use svg::{PathCommand, SvgPath, SvgPathError};
pub use tokens::{
    Appearance, Fill, Ink, MemoryFormat, Metrics, Motion, Palette, Radius, SemanticColors, Space,
    Spring, TextRole, TextTone, TypeStyle, Typo, rgba_f32,
};
