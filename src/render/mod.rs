pub mod colormap;
pub mod hud;
pub mod overlay;
pub mod raster;
pub mod timeline;

/// Codepoints taken from the Nerd Fonts glyphnames manifest and confirmed
/// present in the configured terminal font, whose charset covers e300-e3e3 and
/// f000-f381. An unmapped codepoint would render as a tofu box.
pub mod glyph {
    pub const TORNADO: char = '\u{e36c}';
    pub const THUNDERSTORM: char = '\u{e31d}';
    pub const FLOOD: char = '\u{e375}';
    pub const STRONG_WIND: char = '\u{e34b}';
    pub const STORM_WARNING: char = '\u{e3c6}';
    pub const NO_DATA: char = '\u{e374}';
    pub const REFRESH: char = '\u{e348}';
    pub const HOME: char = '\u{f041}';
    pub const PLAY: char = '\u{f04b}';
    pub const PAUSE: char = '\u{f04c}';
    pub const CLOCK: char = '\u{f017}';
}
