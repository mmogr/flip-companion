use super::display::OutputId;

/// Identifies a window managed by KWin.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WindowId(pub String);

/// Information about a single window, as reported by the KWin script.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: WindowId,
    pub caption: String,
    /// Which output the window is currently on, if known.
    pub output: Option<OutputId>,
}

/// Which direction to shuttle a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShuttleDirection {
    /// Move window to the top (primary) screen.
    Up,
    /// Move window to the bottom (companion) screen.
    Down,
}
