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
    Account,
    Activity,
    Archive,
    ArrowDown,
    ArrowUp,
    ArrowTurnUpLeft,
    ArrowTurnDownRight,
    Pin,
    Bell,
    Branch,
    ChartBar,
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
    File,
    Folder,
    Grid,
    Keyboard,
    Lock,
    LocalAgents,
    Merge,
    Monitor,
    Moon,
    More,
    Network,
    NewAgent,
    Pencil,
    Plus,
    Pointer,
    Power,
    PullRequest,
    Refresh,
    ResizeHorizontal,
    Return,
    Search,
    Server,
    Settings,
    Share,
    Sidebar,
    Toolbar,
    SidebarRight,
    Sparkle,
    Stack,
    Projects,
    Split,
    Expand,
    Collapse,
    Tab,
    Terminal,
    Trash,
    Unarchive,
    Warning,
    Worktree,
    WindowMinimize,
    WindowMaximize,
    WindowRestore,
    WindowClose,
    Text,
    Heading1,
    Heading2,
    Heading3,
    BulletList,
    NumberedList,
    Todo,
    Quote,
    Divider,
    Notion,
    GoogleDoc,
    GoogleSheet,
    GoogleSlides,
    GoogleDrive,
    Linear,
    HubSpot,
    Figma,
    Slack,
    GitHub,
    Image,
    Info,
    Table,
    ArrowLeft,
    ArrowRight,
    AlignLeft,
    AlignCenter,
    AlignRight,
}

impl IconName {
    pub const ALL: [Self; 97] = [
        Self::Account,
        Self::Activity,
        Self::Archive,
        Self::ArrowDown,
        Self::ArrowUp,
        Self::ArrowTurnUpLeft,
        Self::ArrowTurnDownRight,
        Self::Pin,
        Self::Bell,
        Self::Branch,
        Self::ChartBar,
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
        Self::File,
        Self::Folder,
        Self::Grid,
        Self::Keyboard,
        Self::Lock,
        Self::LocalAgents,
        Self::Merge,
        Self::Monitor,
        Self::Moon,
        Self::More,
        Self::Network,
        Self::NewAgent,
        Self::Pencil,
        Self::Plus,
        Self::Pointer,
        Self::Power,
        Self::PullRequest,
        Self::Refresh,
        Self::ResizeHorizontal,
        Self::Return,
        Self::Search,
        Self::Server,
        Self::Settings,
        Self::Share,
        Self::Sidebar,
        Self::Toolbar,
        Self::SidebarRight,
        Self::Sparkle,
        Self::Stack,
        Self::Projects,
        Self::Split,
        Self::Expand,
        Self::Collapse,
        Self::Tab,
        Self::Terminal,
        Self::Trash,
        Self::Unarchive,
        Self::Warning,
        Self::Worktree,
        Self::WindowMinimize,
        Self::WindowMaximize,
        Self::WindowRestore,
        Self::WindowClose,
        Self::Text,
        Self::Heading1,
        Self::Heading2,
        Self::Heading3,
        Self::BulletList,
        Self::NumberedList,
        Self::Todo,
        Self::Quote,
        Self::Divider,
        Self::Notion,
        Self::GoogleDoc,
        Self::GoogleSheet,
        Self::GoogleSlides,
        Self::GoogleDrive,
        Self::Linear,
        Self::HubSpot,
        Self::Figma,
        Self::Slack,
        Self::GitHub,
        Self::Image,
        Self::Info,
        Self::Table,
        Self::ArrowLeft,
        Self::ArrowRight,
        Self::AlignLeft,
        Self::AlignCenter,
        Self::AlignRight,
    ];

    pub const fn asset_path(self) -> &'static str {
        match self {
            Self::Account => "icons/account.svg",
            Self::Activity => "icons/activity.svg",
            Self::Archive => "icons/archive.svg",
            Self::Pin => "icons/pin.svg",
            Self::ArrowDown => "icons/arrow-down.svg",
            Self::ArrowUp => "icons/arrow-up.svg",
            Self::ArrowTurnUpLeft => "icons/arrow-turn-up-left.svg",
            Self::ArrowTurnDownRight => "icons/arrow-turn-down-right.svg",
            Self::Bell => "icons/bell.svg",
            Self::Branch => "icons/branch.svg",
            Self::ChartBar => "icons/chart-bar.svg",
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
            Self::File => "icons/file.svg",
            Self::Folder => "icons/folder.svg",
            Self::Grid => "icons/grid.svg",
            Self::Keyboard => "icons/keyboard.svg",
            Self::Lock => "icons/lock.svg",
            Self::LocalAgents => "icons/local-agents.svg",
            Self::Merge => "icons/merge.svg",
            Self::Monitor => "icons/monitor.svg",
            Self::Moon => "icons/moon.svg",
            Self::More => "icons/more.svg",
            Self::Network => "icons/network.svg",
            Self::NewAgent => "icons/new-agent.svg",
            Self::Pencil => "icons/pencil.svg",
            Self::Plus => "icons/plus.svg",
            Self::Pointer => "icons/pointer.svg",
            Self::Power => "icons/power.svg",
            Self::PullRequest => "icons/pull-request.svg",
            Self::Refresh => "icons/refresh.svg",
            Self::ResizeHorizontal => "icons/resize-horizontal.svg",
            Self::Return => "icons/return.svg",
            Self::Search => "icons/search.svg",
            Self::Server => "icons/server.svg",
            Self::Settings => "icons/settings.svg",
            Self::Share => "icons/share.svg",
            Self::Sidebar => "icons/sidebar.svg",
            Self::Toolbar => "icons/toolbar.svg",
            Self::SidebarRight => "icons/sidebar-right.svg",
            Self::Sparkle => "icons/sparkle.svg",
            Self::Split => "icons/split.svg",
            Self::Expand => "icons/expand.svg",
            Self::Collapse => "icons/collapse.svg",
            Self::Tab => "icons/tab.svg",
            Self::Stack => "icons/stack.svg",
            Self::Projects => "icons/projects.svg",
            Self::Terminal => "icons/terminal.svg",
            Self::Trash => "icons/trash.svg",
            Self::Unarchive => "icons/unarchive.svg",
            Self::Warning => "icons/warning.svg",
            Self::Worktree => "icons/worktree.svg",
            Self::WindowMinimize => "icons/window-minimize.svg",
            Self::WindowMaximize => "icons/window-maximize.svg",
            Self::WindowRestore => "icons/window-restore.svg",
            Self::WindowClose => "icons/window-close.svg",
            Self::Text => "icons/text.svg",
            Self::Heading1 => "icons/heading-1.svg",
            Self::Heading2 => "icons/heading-2.svg",
            Self::Heading3 => "icons/heading-3.svg",
            Self::BulletList => "icons/bullet-list.svg",
            Self::NumberedList => "icons/numbered-list.svg",
            Self::Todo => "icons/todo.svg",
            Self::Quote => "icons/quote.svg",
            Self::Divider => "icons/divider.svg",
            Self::Notion => "icons/notion.svg",
            Self::GoogleDoc => "icons/google-doc.svg",
            Self::GoogleSheet => "icons/google-sheet.svg",
            Self::GoogleSlides => "icons/google-slides.svg",
            Self::GoogleDrive => "icons/google-drive.svg",
            Self::Linear => "icons/linear.svg",
            Self::HubSpot => "icons/hubspot.svg",
            Self::Figma => "icons/figma.svg",
            Self::Slack => "icons/slack.svg",
            Self::GitHub => "icons/github.svg",
            Self::Image => "icons/image.svg",
            Self::Info => "icons/info.svg",
            Self::Table => "icons/table.svg",
            Self::ArrowLeft => "icons/arrow-left.svg",
            Self::ArrowRight => "icons/arrow-right.svg",
            Self::AlignLeft => "icons/align-left.svg",
            Self::AlignCenter => "icons/align-center.svg",
            Self::AlignRight => "icons/align-right.svg",
        }
    }

    /// Compatibility bridge while call sites migrate from SF Symbol strings.
    /// All names currently used by diri resolve to the shared SVG vocabulary.
    pub fn from_system_name(name: &str) -> Option<Self> {
        Some(match name {
            "waveform.circle" | "waveform.circle.fill" => Self::Activity,
            "archivebox" | "archivebox.fill" => Self::Archive,
            "pin" | "pin.fill" => Self::Pin,
            "arrow.down" => Self::ArrowDown,
            "arrow.turn.down.right" => Self::ArrowTurnDownRight,
            "arrow.turn.up.left" => Self::ArrowTurnUpLeft,
            "arrow.up.arrow.down" => Self::ResizeHorizontal,
            "arrow.up" => Self::ArrowUp,
            "bell" | "bell.fill" => Self::Bell,
            "arrow.branch" => Self::Branch,
            "chart.bar" | "chart.bar.xaxis" => Self::ChartBar,
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
            "doc" | "doc.fill" | "doc.text" => Self::File,
            "folder" | "folder.fill" => Self::Folder,
            "square.grid.2x2" | "terminal.grid" => Self::Grid,
            "keyboard" => Self::Keyboard,
            "lock" | "lock.fill" => Self::Lock,
            "person.crop.circle" => Self::LocalAgents,
            "account.circle" => Self::Account,
            "arrow.triangle.merge" => Self::Merge,
            "desktopcomputer" => Self::Monitor,
            "moon.fill" => Self::Moon,
            "ellipsis" => Self::More,
            "network" => Self::Network,
            "square.and.pencil" => Self::NewAgent,
            "pencil" => Self::Pencil,
            "plus" => Self::Plus,
            "cursorarrow.rays" | "cursorarrow.click.2" => Self::Pointer,
            "power" => Self::Power,
            "arrow.triangle.pull" => Self::PullRequest,
            "arrow.triangle.2.circlepath" | "arrow.clockwise.circle" | "arrow.counterclockwise" => {
                Self::Refresh
            }
            "arrow.left.and.right" | "arrow.left.arrow.right" => Self::ResizeHorizontal,
            "return" => Self::Return,
            "magnifyingglass" => Self::Search,
            "server.rack" => Self::Server,
            "gearshape" => Self::Settings,
            "square.and.arrow.up" => Self::Share,
            "sidebar.left" => Self::Sidebar,
            "rectangle.topthird.inset.filled" => Self::Toolbar,
            "sidebar.right" => Self::SidebarRight,
            "sparkle" | "sparkles" => Self::Sparkle,
            "square.stack.3d.up" => Self::Stack,
            "rectangle.stack" => Self::Projects,
            "rectangle.split.2x1" => Self::Split,
            "arrow.up.left.and.arrow.down.right" => Self::Expand,
            "arrow.down.right.and.arrow.up.left" => Self::Collapse,
            "rectangle" => Self::Tab,
            "terminal" => Self::Terminal,
            "trash" => Self::Trash,
            "tray.and.arrow.up.fill" => Self::Unarchive,
            "exclamationmark.triangle" => Self::Warning,
            "point.3.filled.connected.trianglepath.dotted" => Self::Worktree,
            "textformat" => Self::Text,
            "textformat.h1" => Self::Heading1,
            "textformat.h2" => Self::Heading2,
            "textformat.h3" => Self::Heading3,
            "list.bullet" => Self::BulletList,
            "list.number" => Self::NumberedList,
            "checkmark.square" => Self::Todo,
            "text.quote" => Self::Quote,
            "divider" => Self::Divider,
            "notion" => Self::Notion,
            "google.doc" => Self::GoogleDoc,
            "google.sheet" => Self::GoogleSheet,
            "google.slides" => Self::GoogleSlides,
            "google.drive" => Self::GoogleDrive,
            "linear" => Self::Linear,
            "hubspot" => Self::HubSpot,
            "figma" => Self::Figma,
            "slack" => Self::Slack,
            "github" => Self::GitHub,
            "photo" => Self::Image,
            "info.circle" => Self::Info,
            "tablecells" => Self::Table,
            "arrow.left.solid" => Self::ArrowLeft,
            "arrow.right" => Self::ArrowRight,
            "text.alignleft" => Self::AlignLeft,
            "text.aligncenter" => Self::AlignCenter,
            "text.alignright" => Self::AlignRight,
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
        "icons/working-0.svg" => include_bytes!("../assets/icons/working-0.svg"),
        "icons/working-1.svg" => include_bytes!("../assets/icons/working-1.svg"),
        "icons/working-2.svg" => include_bytes!("../assets/icons/working-2.svg"),
        "icons/working-3.svg" => include_bytes!("../assets/icons/working-3.svg"),
        "icons/working-4.svg" => include_bytes!("../assets/icons/working-4.svg"),
        "icons/working-5.svg" => include_bytes!("../assets/icons/working-5.svg"),
        "icons/working-6.svg" => include_bytes!("../assets/icons/working-6.svg"),
        "icons/working-7.svg" => include_bytes!("../assets/icons/working-7.svg"),

        "icons/text.svg" => include_bytes!("../assets/icons/text.svg"),
        "icons/heading-1.svg" => include_bytes!("../assets/icons/heading-1.svg"),
        "icons/heading-2.svg" => include_bytes!("../assets/icons/heading-2.svg"),
        "icons/heading-3.svg" => include_bytes!("../assets/icons/heading-3.svg"),
        "icons/bullet-list.svg" => include_bytes!("../assets/icons/bullet-list.svg"),
        "icons/numbered-list.svg" => include_bytes!("../assets/icons/numbered-list.svg"),
        "icons/todo.svg" => include_bytes!("../assets/icons/todo.svg"),
        "icons/quote.svg" => include_bytes!("../assets/icons/quote.svg"),
        "icons/divider.svg" => include_bytes!("../assets/icons/divider.svg"),
        "icons/notion.svg" => include_bytes!("../assets/icons/notion.svg"),
        "icons/google-doc.svg" => include_bytes!("../assets/icons/google-doc.svg"),
        "icons/google-sheet.svg" => include_bytes!("../assets/icons/google-sheet.svg"),
        "icons/google-slides.svg" => include_bytes!("../assets/icons/google-slides.svg"),
        "icons/google-drive.svg" => include_bytes!("../assets/icons/google-drive.svg"),
        "icons/linear.svg" => include_bytes!("../assets/icons/linear.svg"),
        "icons/hubspot.svg" => include_bytes!("../assets/icons/hubspot.svg"),
        "icons/figma.svg" => include_bytes!("../assets/icons/figma.svg"),
        "icons/slack.svg" => include_bytes!("../assets/icons/slack.svg"),
        "icons/github.svg" => include_bytes!("../assets/icons/github.svg"),
        "icons/image.svg" => include_bytes!("../assets/icons/image.svg"),
        "icons/info.svg" => include_bytes!("../assets/icons/info.svg"),
        "icons/table.svg" => include_bytes!("../assets/icons/table.svg"),
        "icons/arrow-left.svg" => include_bytes!("../assets/icons/arrow-left.svg"),
        "icons/arrow-right.svg" => include_bytes!("../assets/icons/arrow-right.svg"),
        "icons/align-left.svg" => include_bytes!("../assets/icons/align-left.svg"),
        "icons/align-center.svg" => include_bytes!("../assets/icons/align-center.svg"),
        "icons/align-right.svg" => include_bytes!("../assets/icons/align-right.svg"),
        "icons/account.svg" => include_bytes!("../assets/icons/account.svg"),
        "icons/activity.svg" => include_bytes!("../assets/icons/activity.svg"),
        "icons/archive.svg" => include_bytes!("../assets/icons/archive.svg"),
        "icons/pin.svg" => include_bytes!("../assets/icons/pin.svg"),
        "icons/arrow-down.svg" => include_bytes!("../assets/icons/arrow-down.svg"),
        "icons/arrow-up.svg" => include_bytes!("../assets/icons/arrow-up.svg"),
        "icons/arrow-turn-down-right.svg" => {
            include_bytes!("../assets/icons/arrow-turn-down-right.svg")
        }
        "icons/arrow-turn-up-left.svg" => include_bytes!("../assets/icons/arrow-turn-up-left.svg"),
        "icons/bell.svg" => include_bytes!("../assets/icons/bell.svg"),
        "icons/branch.svg" => include_bytes!("../assets/icons/branch.svg"),
        "icons/chart-bar.svg" => include_bytes!("../assets/icons/chart-bar.svg"),
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
        "icons/file.svg" => include_bytes!("../assets/icons/file.svg"),
        "icons/folder.svg" => include_bytes!("../assets/icons/folder.svg"),
        "icons/grid.svg" => include_bytes!("../assets/icons/grid.svg"),
        "icons/keyboard.svg" => include_bytes!("../assets/icons/keyboard.svg"),
        "icons/lock.svg" => include_bytes!("../assets/icons/lock.svg"),
        "icons/local-agents.svg" => include_bytes!("../assets/icons/local-agents.svg"),
        "icons/merge.svg" => include_bytes!("../assets/icons/merge.svg"),
        "icons/monitor.svg" => include_bytes!("../assets/icons/monitor.svg"),
        "icons/moon.svg" => include_bytes!("../assets/icons/moon.svg"),
        "icons/more.svg" => include_bytes!("../assets/icons/more.svg"),
        "icons/network.svg" => include_bytes!("../assets/icons/network.svg"),
        "icons/new-agent.svg" => include_bytes!("../assets/icons/new-agent.svg"),
        "icons/pencil.svg" => include_bytes!("../assets/icons/pencil.svg"),
        "icons/plus.svg" => include_bytes!("../assets/icons/plus.svg"),
        "icons/pointer.svg" => include_bytes!("../assets/icons/pointer.svg"),
        "icons/power.svg" => include_bytes!("../assets/icons/power.svg"),
        "icons/pull-request.svg" => include_bytes!("../assets/icons/pull-request.svg"),
        "icons/refresh.svg" => include_bytes!("../assets/icons/refresh.svg"),
        "icons/return.svg" => include_bytes!("../assets/icons/return.svg"),
        "icons/resize-horizontal.svg" => include_bytes!("../assets/icons/resize-horizontal.svg"),
        "icons/search.svg" => include_bytes!("../assets/icons/search.svg"),
        "icons/server.svg" => include_bytes!("../assets/icons/server.svg"),
        "icons/settings.svg" => include_bytes!("../assets/icons/settings.svg"),
        "icons/share.svg" => include_bytes!("../assets/icons/share.svg"),
        "icons/sidebar.svg" => include_bytes!("../assets/icons/sidebar.svg"),
        "icons/toolbar.svg" => include_bytes!("../assets/icons/toolbar.svg"),
        "icons/sidebar-right.svg" => include_bytes!("../assets/icons/sidebar-right.svg"),
        "icons/sparkle.svg" => include_bytes!("../assets/icons/sparkle.svg"),
        "icons/split.svg" => include_bytes!("../assets/icons/split.svg"),
        "icons/expand.svg" => include_bytes!("../assets/icons/expand.svg"),
        "icons/collapse.svg" => include_bytes!("../assets/icons/collapse.svg"),
        "icons/tab.svg" => include_bytes!("../assets/icons/tab.svg"),
        "icons/stack.svg" => include_bytes!("../assets/icons/stack.svg"),
        "icons/projects.svg" => include_bytes!("../assets/icons/projects.svg"),
        "icons/terminal.svg" => include_bytes!("../assets/icons/terminal.svg"),
        "icons/trash.svg" => include_bytes!("../assets/icons/trash.svg"),
        "icons/unarchive.svg" => include_bytes!("../assets/icons/unarchive.svg"),
        "icons/warning.svg" => include_bytes!("../assets/icons/warning.svg"),
        "icons/worktree.svg" => include_bytes!("../assets/icons/worktree.svg"),
        "icons/window-minimize.svg" => include_bytes!("../assets/icons/window-minimize.svg"),
        "icons/window-maximize.svg" => include_bytes!("../assets/icons/window-maximize.svg"),
        "icons/window-restore.svg" => include_bytes!("../assets/icons/window-restore.svg"),
        "icons/window-close.svg" => include_bytes!("../assets/icons/window-close.svg"),
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
            "pin.fill",
            "archivebox",
            "archivebox.fill",
            "arrow.branch",
            "arrow.clockwise.circle",
            "arrow.down",
            "arrow.turn.down.right",
            "arrow.turn.up.left",
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
            "lock.fill",
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
            "square.and.arrow.up",
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
