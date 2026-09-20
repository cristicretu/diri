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
pub mod motion;
pub mod scroller;
mod status;
mod svg;
pub mod title_fade;
mod tokens;

pub use brand::{
    AgentKind, AgentLogo, BrandMark, BrandMarkKind, CLAUDE_PATH, CURSOR_PATH, GEMINI_PATH,
    MarkRasterizer, OPENAI_PATH, set_mark_rasterizer,
};
pub use components::{
    AlertChip, FloatingSurface, GlassMenuRow, GlassPill, HairlineDivider, HoverMarquee,
    LoadingIndicator, RowFill, StateChip,
};
pub use icon::{Icon, IconAssets, IconName, IconSize, icon_from_system_name};
pub use scroller::{
    Overscroll, ScrollArea, ScrollTarget, ScrollerState, ScrollerStyle, ThumbGeometry,
    WheelOutcome, WheelSample, scroll_area, scroller_style, set_scroller_style, thumb_geometry,
};
pub use status::{
    AnimationPhase, AttentionDot, AttentionLevel, StatusGlyph, StatusState, wall_clock_seconds,
};
pub use svg::{PathCommand, SvgPath, SvgPathError};
pub use title_fade::{TitleFade, title_fade};
pub use tokens::{
    Appearance, Chip, Fill, Glass, Ink, Material, MemoryFormat, Metrics, Motion, Palette, Radius,
    SemanticColors, Space, Spring, TextRole, TextTone, TypeStyle, Typo, composite, rgba_f32,
};
