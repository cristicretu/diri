use std::borrow::Cow;

use gpui::{
    AnyElement, App, AssetSource, IntoElement, RenderOnce, Rgba, Window, prelude::*, px, svg,
};

/// Optical sizes for diri's shared 24×24 line icons.
///
/// The compatibility layer still receives point sizes that were tuned for SF
/// Symbols. Snapping those values to this scale keeps the replacement SVGs
/// legible and consistent across sidebars, toolbars, menus, and empty states.
pub struct IconSize;

impl IconSize {
    /// Supporting marks such as chevrons and inline status actions.
    pub const COMPACT: f32 = 14.0;
    /// Default size for row, navigation, and toolbar icons.
    pub const REGULAR: f32 = 16.0;
    /// Prominent icons in larger controls and cards.
    pub const LARGE: f32 = 20.0;
    /// Empty-state and other display-size icons.
    pub const DISPLAY: f32 = 28.0;

    /// Maps former platform-symbol sizes onto the shared optical scale.
    /// Values above the display range stay explicit so intentionally large
    /// illustrations are not unexpectedly reduced.
    pub const fn from_legacy_points(size: f32) -> f32 {
        if size <= 11.0 {
            Self::COMPACT
        } else if size <= 17.0 {
            Self::REGULAR
        } else if size <= 23.0 {
            Self::LARGE
        } else if size <= 32.0 {
            Self::DISPLAY
        } else {
            size
        }
    }
}

/// The shared, platform-independent icon vocabulary for diri.
///
/// Every glyph is authored as a 24×24 SVG with a 1.75pt rounded stroke. Keeping
/// names semantic prevents views from reaching back into a platform symbol
/// catalog and lets the whole app evolve as one visual system.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IconName {
    Activity,
    Archive,
    ArrowDown,
    Branch,
    Check,
    CheckCircle,
    Checklist,
    ChevronDown,
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    ChevronUpDown,
    Clock,
    Close,
    CloseCircle,
    Code,
    Comment,
    Cube,
    Download,
    ExternalLink,
    Folder,
    Grid,
    LocalAgents,
    Merge,
    Monitor,
    Moon,
    More,
    Network,
    NewAgent,
    Plus,
    Pointer,
    Power,
    PullRequest,
    Refresh,
    ResizeHorizontal,
    Search,
    Server,
    Settings,
    Sidebar,
    SidebarRight,
    Sparkle,
    Stack,
    Terminal,
    Trash,
    Unarchive,
    Warning,
    Worktree,
}

impl IconName {
    pub const ALL: [Self; 47] = [
        Self::Activity,
        Self::Archive,
        Self::ArrowDown,
        Self::Branch,
        Self::Check,
        Self::CheckCircle,
        Self::Checklist,
        Self::ChevronDown,
        Self::ChevronLeft,
        Self::ChevronRight,
        Self::ChevronUp,
        Self::ChevronUpDown,
        Self::Clock,
        Self::Close,
        Self::CloseCircle,
        Self::Code,
        Self::Comment,
        Self::Cube,
        Self::Download,
        Self::ExternalLink,
        Self::Folder,
        Self::Grid,
        Self::LocalAgents,
        Self::Merge,
        Self::Monitor,
        Self::Moon,
        Self::More,
        Self::Network,
        Self::NewAgent,
        Self::Plus,
        Self::Pointer,
        Self::Power,
        Self::PullRequest,
        Self::Refresh,
        Self::ResizeHorizontal,
        Self::Search,
        Self::Server,
        Self::Settings,
        Self::Sidebar,
        Self::SidebarRight,
        Self::Sparkle,
        Self::Stack,
        Self::Terminal,
        Self::Trash,
        Self::Unarchive,
        Self::Warning,
        Self::Worktree,
    ];

    pub const fn asset_path(self) -> &'static str {
        match self {
            Self::Activity => "icons/activity.svg",
            Self::Archive => "icons/archive.svg",
            Self::ArrowDown => "icons/arrow-down.svg",
            Self::Branch => "icons/branch.svg",
            Self::Check => "icons/check.svg",
            Self::CheckCircle => "icons/check-circle.svg",
            Self::Checklist => "icons/checklist.svg",
            Self::ChevronDown => "icons/chevron-down.svg",
            Self::ChevronLeft => "icons/chevron-left.svg",
            Self::ChevronRight => "icons/chevron-right.svg",
            Self::ChevronUp => "icons/chevron-up.svg",
            Self::ChevronUpDown => "icons/chevron-up-down.svg",
            Self::Clock => "icons/clock.svg",
            Self::Close => "icons/close.svg",
            Self::CloseCircle => "icons/close-circle.svg",
            Self::Code => "icons/code.svg",
            Self::Comment => "icons/comment.svg",
            Self::Cube => "icons/cube.svg",
            Self::Download => "icons/download.svg",
            Self::ExternalLink => "icons/external-link.svg",
            Self::Folder => "icons/folder.svg",
            Self::Grid => "icons/grid.svg",
            Self::LocalAgents => "icons/local-agents.svg",
            Self::Merge => "icons/merge.svg",
            Self::Monitor => "icons/monitor.svg",
            Self::Moon => "icons/moon.svg",
            Self::More => "icons/more.svg",
            Self::Network => "icons/network.svg",
            Self::NewAgent => "icons/new-agent.svg",
            Self::Plus => "icons/plus.svg",
            Self::Pointer => "icons/pointer.svg",
            Self::Power => "icons/power.svg",
            Self::PullRequest => "icons/pull-request.svg",
            Self::Refresh => "icons/refresh.svg",
            Self::ResizeHorizontal => "icons/resize-horizontal.svg",
            Self::Search => "icons/search.svg",
            Self::Server => "icons/server.svg",
            Self::Settings => "icons/settings.svg",
            Self::Sidebar => "icons/sidebar.svg",
            Self::SidebarRight => "icons/sidebar-right.svg",
            Self::Sparkle => "icons/sparkle.svg",
            Self::Stack => "icons/stack.svg",
            Self::Terminal => "icons/terminal.svg",
            Self::Trash => "icons/trash.svg",
            Self::Unarchive => "icons/unarchive.svg",
            Self::Warning => "icons/warning.svg",
            Self::Worktree => "icons/worktree.svg",
        }
    }

    /// Compatibility bridge while call sites migrate from SF Symbol strings.
    /// All names currently used by diri resolve to the shared SVG vocabulary.
    pub fn from_system_name(name: &str) -> Option<Self> {
        Some(match name {
            "waveform.circle" | "waveform.circle.fill" => Self::Activity,
            "archivebox" | "archivebox.fill" => Self::Archive,
            "arrow.down" => Self::ArrowDown,
            "arrow.branch" => Self::Branch,
            "checkmark" => Self::Check,
            "checkmark.circle" | "checkmark.circle.fill" => Self::CheckCircle,
            "checklist" => Self::Checklist,
            "chevron.down" => Self::ChevronDown,
            "arrow.left" | "chevron.left" => Self::ChevronLeft,
            "chevron.right" => Self::ChevronRight,
            "chevron.up" => Self::ChevronUp,
            "chevron.up.chevron.down" => Self::ChevronUpDown,
            "clock.fill" => Self::Clock,
            "xmark" => Self::Close,
            "xmark.circle" | "xmark.circle.fill" => Self::CloseCircle,
            "chevron.left.forwardslash.chevron.right" => Self::Code,
            "bubble.left" => Self::Comment,
            "cube" => Self::Cube,
            "arrow.down.circle" => Self::Download,
            "link" => Self::ExternalLink,
            "folder" | "folder.fill" => Self::Folder,
            "square.grid.2x2" | "terminal.grid" => Self::Grid,
            "person.crop.circle" => Self::LocalAgents,
            "arrow.triangle.merge" => Self::Merge,
            "desktopcomputer" => Self::Monitor,
            "moon.fill" => Self::Moon,
            "ellipsis" => Self::More,
            "network" => Self::Network,
            "square.and.pencil" => Self::NewAgent,
            "plus" => Self::Plus,
            "cursorarrow.rays" | "cursorarrow.click.2" => Self::Pointer,
            "power" => Self::Power,
            "arrow.triangle.pull" => Self::PullRequest,
            "arrow.triangle.2.circlepath" | "arrow.clockwise.circle" => Self::Refresh,
            "arrow.left.and.right" | "arrow.left.arrow.right" => Self::ResizeHorizontal,
            "magnifyingglass" => Self::Search,
            "server.rack" => Self::Server,
            "gearshape" => Self::Settings,
            "sidebar.left" => Self::Sidebar,
            "sidebar.right" => Self::SidebarRight,
            "sparkle" | "sparkles" => Self::Sparkle,
            "square.stack.3d.up" => Self::Stack,
            "terminal" => Self::Terminal,
            "trash" => Self::Trash,
            "tray.and.arrow.up.fill" => Self::Unarchive,
            "exclamationmark.triangle" => Self::Warning,
            "point.3.filled.connected.trianglepath.dotted" => Self::Worktree,
            _ => return None,
        })
    }
}

/// A tintable SVG icon from diri's shared 24×24 icon family.
#[derive(IntoElement)]
pub struct Icon {
    name: IconName,
    size: f32,
    color: Rgba,
}

impl Icon {
    pub const fn new(name: IconName, size: f32, color: Rgba) -> Self {
        Self { name, size, color }
    }

    pub fn from_system_name(name: &str, size: f32, color: Rgba) -> Option<Self> {
        IconName::from_system_name(name).map(|name| Self::new(name, size, color))
    }
}

impl RenderOnce for Icon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        svg()
            .path(self.name.asset_path())
            .flex_none()
            .size(px(self.size))
            .text_color(self.color)
    }
}

/// Embedded SVG assets used by [`Icon`]. The app installs this source once,
/// keeping the binary self-contained in development and in the packaged app.
#[derive(Clone, Copy, Debug, Default)]
pub struct IconAssets;

impl AssetSource for IconAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(embedded_svg(path).map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        if path == "icons" || path == "icons/" {
            Ok(IconName::ALL
                .into_iter()
                .map(|icon| icon.asset_path().into())
                .collect())
        } else {
            Ok(Vec::new())
        }
    }
}

fn embedded_svg(path: &str) -> Option<&'static [u8]> {
    Some(match path {
        "icons/activity.svg" => include_bytes!("../assets/icons/activity.svg"),
        "icons/archive.svg" => include_bytes!("../assets/icons/archive.svg"),
        "icons/arrow-down.svg" => include_bytes!("../assets/icons/arrow-down.svg"),
        "icons/branch.svg" => include_bytes!("../assets/icons/branch.svg"),
        "icons/check.svg" => include_bytes!("../assets/icons/check.svg"),
        "icons/check-circle.svg" => include_bytes!("../assets/icons/check-circle.svg"),
        "icons/checklist.svg" => include_bytes!("../assets/icons/checklist.svg"),
        "icons/chevron-down.svg" => include_bytes!("../assets/icons/chevron-down.svg"),
        "icons/chevron-left.svg" => include_bytes!("../assets/icons/chevron-left.svg"),
        "icons/chevron-right.svg" => include_bytes!("../assets/icons/chevron-right.svg"),
        "icons/chevron-up.svg" => include_bytes!("../assets/icons/chevron-up.svg"),
        "icons/chevron-up-down.svg" => include_bytes!("../assets/icons/chevron-up-down.svg"),
        "icons/clock.svg" => include_bytes!("../assets/icons/clock.svg"),
        "icons/close.svg" => include_bytes!("../assets/icons/close.svg"),
        "icons/close-circle.svg" => include_bytes!("../assets/icons/close-circle.svg"),
        "icons/code.svg" => include_bytes!("../assets/icons/code.svg"),
        "icons/comment.svg" => include_bytes!("../assets/icons/comment.svg"),
        "icons/cube.svg" => include_bytes!("../assets/icons/cube.svg"),
        "icons/download.svg" => include_bytes!("../assets/icons/download.svg"),
        "icons/external-link.svg" => include_bytes!("../assets/icons/external-link.svg"),
        "icons/folder.svg" => include_bytes!("../assets/icons/folder.svg"),
        "icons/grid.svg" => include_bytes!("../assets/icons/grid.svg"),
        "icons/local-agents.svg" => include_bytes!("../assets/icons/local-agents.svg"),
        "icons/merge.svg" => include_bytes!("../assets/icons/merge.svg"),
        "icons/monitor.svg" => include_bytes!("../assets/icons/monitor.svg"),
        "icons/moon.svg" => include_bytes!("../assets/icons/moon.svg"),
        "icons/more.svg" => include_bytes!("../assets/icons/more.svg"),
        "icons/network.svg" => include_bytes!("../assets/icons/network.svg"),
        "icons/new-agent.svg" => include_bytes!("../assets/icons/new-agent.svg"),
        "icons/plus.svg" => include_bytes!("../assets/icons/plus.svg"),
        "icons/pointer.svg" => include_bytes!("../assets/icons/pointer.svg"),
        "icons/power.svg" => include_bytes!("../assets/icons/power.svg"),
        "icons/pull-request.svg" => include_bytes!("../assets/icons/pull-request.svg"),
        "icons/refresh.svg" => include_bytes!("../assets/icons/refresh.svg"),
        "icons/resize-horizontal.svg" => include_bytes!("../assets/icons/resize-horizontal.svg"),
        "icons/search.svg" => include_bytes!("../assets/icons/search.svg"),
        "icons/server.svg" => include_bytes!("../assets/icons/server.svg"),
        "icons/settings.svg" => include_bytes!("../assets/icons/settings.svg"),
        "icons/sidebar.svg" => include_bytes!("../assets/icons/sidebar.svg"),
        "icons/sidebar-right.svg" => include_bytes!("../assets/icons/sidebar-right.svg"),
        "icons/sparkle.svg" => include_bytes!("../assets/icons/sparkle.svg"),
        "icons/stack.svg" => include_bytes!("../assets/icons/stack.svg"),
        "icons/terminal.svg" => include_bytes!("../assets/icons/terminal.svg"),
        "icons/trash.svg" => include_bytes!("../assets/icons/trash.svg"),
        "icons/unarchive.svg" => include_bytes!("../assets/icons/unarchive.svg"),
        "icons/warning.svg" => include_bytes!("../assets/icons/warning.svg"),
        "icons/worktree.svg" => include_bytes!("../assets/icons/worktree.svg"),
        _ => return None,
    })
}

/// Render a legacy platform-symbol name through diri's SVG icon family.
pub fn icon_from_system_name(name: &str, size: f32, color: Rgba) -> AnyElement {
    let size = IconSize::from_legacy_points(size);
    Icon::from_system_name(name, size, color)
        .unwrap_or_else(|| Icon::new(IconName::Code, size, color))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_has_an_embedded_asset() {
        for icon in IconName::ALL {
            assert!(embedded_svg(icon.asset_path()).is_some(), "{icon:?}");
        }
    }

    #[test]
    fn important_sidebar_symbols_have_semantic_svg_icons() {
        assert_eq!(
            IconName::from_system_name("square.and.pencil"),
            Some(IconName::NewAgent)
        );
        assert_eq!(
            IconName::from_system_name("folder.fill"),
            Some(IconName::Folder)
        );
        assert_eq!(
            IconName::from_system_name("person.crop.circle"),
            Some(IconName::LocalAgents)
        );
    }

    #[test]
    fn legacy_symbol_sizes_snap_to_the_shared_optical_scale() {
        assert_eq!(IconSize::from_legacy_points(8.0), IconSize::COMPACT);
        assert_eq!(IconSize::from_legacy_points(11.0), IconSize::COMPACT);
        assert_eq!(IconSize::from_legacy_points(12.5), IconSize::REGULAR);
        assert_eq!(IconSize::from_legacy_points(17.0), IconSize::REGULAR);
        assert_eq!(IconSize::from_legacy_points(18.0), IconSize::LARGE);
        assert_eq!(IconSize::from_legacy_points(26.0), IconSize::DISPLAY);
        assert_eq!(IconSize::from_legacy_points(40.0), 40.0);
    }

    #[test]
    fn every_legacy_symbol_used_by_the_app_resolves() {
        for name in [
            "archivebox",
            "archivebox.fill",
            "arrow.branch",
            "arrow.clockwise.circle",
            "arrow.down",
            "arrow.down.circle",
            "arrow.left.and.right",
            "arrow.left.arrow.right",
            "arrow.triangle.2.circlepath",
            "arrow.triangle.merge",
            "arrow.triangle.pull",
            "bubble.left",
            "checklist",
            "checkmark",
            "checkmark.circle",
            "checkmark.circle.fill",
            "chevron.down",
            "chevron.left",
            "chevron.left.forwardslash.chevron.right",
            "chevron.right",
            "chevron.up",
            "chevron.up.chevron.down",
            "clock.fill",
            "cube",
            "cursorarrow.click.2",
            "cursorarrow.rays",
            "desktopcomputer",
            "ellipsis",
            "exclamationmark.triangle",
            "folder",
            "folder.fill",
            "gearshape",
            "link",
            "magnifyingglass",
            "network",
            "person.crop.circle",
            "plus",
            "point.3.filled.connected.trianglepath.dotted",
            "power",
            "server.rack",
            "sidebar.left",
            "sidebar.right",
            "sparkle",
            "sparkles",
            "square.and.pencil",
            "square.grid.2x2",
            "square.stack.3d.up",
            "terminal",
            "terminal.grid",
            "trash",
            "tray.and.arrow.up.fill",
            "waveform.circle",
            "waveform.circle.fill",
            "xmark",
            "xmark.circle",
            "xmark.circle.fill",
        ] {
            assert!(IconName::from_system_name(name).is_some(), "{name}");
        }
    }
}
