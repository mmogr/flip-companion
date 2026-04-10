/// Unique identifier for a display output (e.g. KScreen output ID or connector name).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutputId(pub String);

/// Physical geometry of a display output.
#[derive(Debug, Clone)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Information about a single display output.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub id: OutputId,
    pub name: String,
    pub enabled: bool,
    pub geometry: Geometry,
}
