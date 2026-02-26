//! Parse Omarchy terminal color configs and generate CSS variable overrides.
//!
//! Omarchy themes include terminal color configs (ghostty.conf, alacritty.toml).
//! This module parses those colors and maps them to Supervillain's CSS custom
//! properties, providing theme support for ALL Omarchy themes — including static
//! themes that lack colors.toml and don't get processed by the template pipeline.

/// All 16 terminal colors + primary bg/fg + optional selection.
pub struct ThemeColors {
    pub bg: String,
    pub fg: String,
    pub normal: [String; 8], // black,red,green,yellow,blue,magenta,cyan,white
    pub bright: [String; 8],
    pub selection_bg: Option<String>,
}

/// Normalize a hex color value from various terminal config formats.
/// Handles `'#fdf6e3'`, `"0x1d2021"`, `=#aabbcc` (ghostty), bare `#hex`.
/// Strips inline comments (e.g., `'#fdf6e3' # solarized light`).
/// Returns `#rrggbb` or None if invalid.
fn normalize_hex(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    // Extract quoted value, or strip inline `# comment` from unquoted value
    let s = if (trimmed.starts_with('\'') || trimmed.starts_with('"'))
        && let Some(end) = trimmed[1..].find(trimmed.as_bytes()[0] as char)
    {
        &trimmed[1..=end]
    } else if let Some(pos) = trimmed.find(" #") {
        trimmed[..pos].trim()
    } else {
        trimmed
    };
    let hex = s
        .strip_prefix('#')
        .or_else(|| s.strip_prefix("0x"))
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(format!("#{}", hex.to_ascii_lowercase()))
    } else {
        None
    }
}

/// Convert `#rrggbb` to `"r,g,b"` decimal string for use in rgba().
fn hex_to_rgb(hex: &str) -> String {
    let h = hex.strip_prefix('#').unwrap_or(hex);
    let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0);
    let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0);
    let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0);
    format!("{r},{g},{b}")
}

// ---------------------------------------------------------------------------
// Ghostty parser
// ---------------------------------------------------------------------------

/// Parse a ghostty.conf color config.
/// Format: `background =#hex`, `foreground =#hex`, `palette = N=#hex`
pub fn parse_ghostty_colors(content: &str) -> Option<ThemeColors> {
    let mut bg = None;
    let mut fg = None;
    let mut palette: [Option<String>; 16] = Default::default();
    let mut selection_bg = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim();
            let value = value.trim();

            match key {
                "background" => bg = normalize_hex(value),
                "foreground" => fg = normalize_hex(value),
                "selection-background" => selection_bg = normalize_hex(value),
                "palette" => {
                    // "N=#hex" format
                    if let Some((idx_str, hex)) = value.split_once('=')
                        && let Ok(idx) = idx_str.trim().parse::<usize>()
                        && idx < 16
                    {
                        palette[idx] = normalize_hex(hex);
                    }
                }
                _ => {}
            }
        }
    }

    // Require bg, fg, and all 16 palette colors
    let mut normal = [(); 8].map(|_| String::new());
    let mut bright = [(); 8].map(|_| String::new());
    for i in 0..8 {
        normal[i] = palette[i].take()?;
        bright[i] = palette[i + 8].take()?;
    }

    Some(ThemeColors {
        bg: bg?,
        fg: fg?,
        normal,
        bright,
        selection_bg,
    })
}

// ---------------------------------------------------------------------------
// Alacritty parser
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum AlacrittySection {
    None,
    Primary,
    Normal,
    Bright,
    Selection,
}

/// Parse an alacritty.toml color config.
/// Handles both `#hex` and `0xhex` formats, single and double quotes.
pub fn parse_alacritty_colors(content: &str) -> Option<ThemeColors> {
    let mut section = AlacrittySection::None;
    let mut bg = None;
    let mut fg = None;
    let mut normal: [Option<String>; 8] = Default::default();
    let mut bright: [Option<String>; 8] = Default::default();
    let mut selection_bg = None;

    let color_names = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if trimmed.starts_with('[') {
            section = if trimmed.contains("colors.primary") {
                AlacrittySection::Primary
            } else if trimmed.contains("colors.normal") {
                AlacrittySection::Normal
            } else if trimmed.contains("colors.bright") {
                AlacrittySection::Bright
            } else if trimmed.contains("colors.selection") {
                AlacrittySection::Selection
            } else {
                AlacrittySection::None
            };
            continue;
        }

        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim();
            let hex = match normalize_hex(value) {
                Some(h) => h,
                None => continue,
            };

            match section {
                AlacrittySection::Primary => match key {
                    "background" => bg = Some(hex),
                    "foreground" => fg = Some(hex),
                    _ => {}
                },
                AlacrittySection::Normal => {
                    if let Some(i) = color_names.iter().position(|&n| n == key) {
                        normal[i] = Some(hex);
                    }
                }
                AlacrittySection::Bright => {
                    if let Some(i) = color_names.iter().position(|&n| n == key) {
                        bright[i] = Some(hex);
                    }
                }
                AlacrittySection::Selection => {
                    if key == "background" {
                        selection_bg = Some(hex);
                    }
                }
                AlacrittySection::None => {}
            }
        }
    }

    let mut normal_out = [(); 8].map(|_| String::new());
    let mut bright_out = [(); 8].map(|_| String::new());
    for i in 0..8 {
        normal_out[i] = normal[i].take()?;
        bright_out[i] = bright[i].take()?;
    }

    Some(ThemeColors {
        bg: bg?,
        fg: fg?,
        normal: normal_out,
        bright: bright_out,
        selection_bg,
    })
}

// ---------------------------------------------------------------------------
// Theme directory → ThemeColors
// ---------------------------------------------------------------------------

/// Try to parse theme colors from a theme directory.
/// Tries ghostty.conf first (Omarchy default terminal), then alacritty.toml.
pub fn load_from_theme_dir(theme_dir: &std::path::Path) -> Option<ThemeColors> {
    // Prefer ghostty.conf (current Omarchy default terminal)
    if let Ok(content) = std::fs::read_to_string(theme_dir.join("ghostty.conf"))
        && let Some(colors) = parse_ghostty_colors(&content)
    {
        return Some(colors);
    }

    // Fall back to alacritty.toml (widely available in static themes)
    if let Ok(content) = std::fs::read_to_string(theme_dir.join("alacritty.toml"))
        && let Some(colors) = parse_alacritty_colors(&content)
    {
        return Some(colors);
    }

    None
}

/// Check if the theme directory indicates a light theme.
pub fn is_light_theme(theme_dir: &std::path::Path) -> bool {
    theme_dir.join("light.mode").exists()
}

// ---------------------------------------------------------------------------
// CSS generation
// ---------------------------------------------------------------------------

/// Generate CSS that overrides Supervillain's theme variables.
///
/// Color mapping from terminal palette to UI semantics:
///   background  → --bg          (main background)
///   palette[0]  → --bg-secondary (panels, sidebars)
///   palette[8]  → --bg-tertiary, --fg-dim, --border (dim/inactive)
///   foreground  → --fg          (primary text)
///   palette[7]  → --fg-muted    (secondary text)
///   palette[6]  → --accent      (cyan = accent)
///   palette[4]  → --accent-dim  (blue = dimmer accent)
///   palette[2]  → --success     (green)
///   palette[3]  → --warning     (yellow)
///   palette[1]  → --danger      (red)
///   selection   → --selection   (falls back to palette[8])
pub fn generate_theme_css(colors: &ThemeColors, is_light: bool) -> String {
    let selection = colors.selection_bg.as_deref().unwrap_or(&colors.bright[0]); // bright black
    let bg_rgb = hex_to_rgb(&colors.bg);

    let mut css = format!(
        "\
:root {{
    --bg: {bg};
    --bg-secondary: {bg_secondary};
    --bg-tertiary: {bg_tertiary};
    --fg: {fg};
    --fg-muted: {fg_muted};
    --fg-dim: {fg_dim};
    --accent: {accent};
    --accent-dim: {accent_dim};
    --success: {success};
    --warning: {warning};
    --danger: {danger};
    --selection: {selection};
    --border: {border};
}}

#help-overlay {{
    background: rgba({bg_rgb}, 0.9);
}}

#split-modal {{
    background: rgba({bg_rgb}, 0.9);
}}",
        bg = colors.bg,
        bg_secondary = colors.normal[0], // black
        bg_tertiary = colors.bright[0],  // bright black
        fg = colors.fg,
        fg_muted = colors.normal[7],   // white
        fg_dim = colors.bright[0],     // bright black
        accent = colors.normal[6],     // cyan
        accent_dim = colors.normal[4], // blue
        success = colors.normal[2],    // green
        warning = colors.normal[3],    // yellow
        danger = colors.normal[1],     // red
        selection = selection,
        border = colors.bright[0], // bright black
        bg_rgb = bg_rgb,
    );

    if is_light {
        // Frontend detects light themes via css.includes('--light-mode') in app.js
        css.push_str("\n\n/* --light-mode */");
    }

    css.push('\n');
    css
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Alacritty test fixtures --

    const ALACRITTY_SOLARIZED: &str = "\
# Colors (Solarized Light)

# Default colors
[colors.primary]
background = '#fdf6e3'
foreground = '#586e75'

# Normal colors
[colors.normal]
black   = '#073642'
red     = '#dc322f'
green   = '#859900'
yellow  = '#b58900'
blue    = '#268bd2'
magenta = '#d33682'
cyan    = '#2aa198'
white   = '#eee8d5'

# Bright colors
[colors.bright]
black   = '#002b36'
red     = '#cb4b16'
green   = '#586e75'
yellow  = '#657b83'
blue    = '#839496'
magenta = '#6c71c4'
cyan    = '#93a1a1'
white   = '#fdf6e3'
";

    const ALACRITTY_0X: &str = "\
[colors]

# Primary colors
[colors.primary]
background = \"0x1d2021\"
foreground = \"0xd4be98\"

# Selection colors
[colors.selection]
text = \"0x1d2021\"
background = \"0xd4be98\"

# Normal colors
[colors.normal]
black   = \"0x665c54\"
red     = \"0xea6962\"
green   = \"0xa9b665\"
yellow  = \"0xe78a4e\"
blue    = \"0x7daea3\"
magenta = \"0xd3869b\"
cyan    = \"0x89b482\"
white   = \"0xd4be98\"

# Bright colors
[colors.bright]
black   = \"0x928374\"
red     = \"0xea6962\"
green   = \"0xa9b665\"
yellow  = \"0xd8a657\"
blue    = \"0x7daea3\"
magenta = \"0xd3869b\"
cyan    = \"0x89b482\"
white   = \"0xd4be98\"
";

    // -- Ghostty test fixture --

    const GHOSTTY_GRUVU: &str = "\
# Background and Foreground
background =#1d2021
foreground =#d5c4a1

# Cursor
cursor-color=#d5c4a1
cursor-text=#1d2021

# Selection
selection-background =#665c54
selection-foreground =#d5c4a1

# Color Palette (based on Gruvbox Dark Hard)
palette = 0=#1d2021
palette = 1=#cc241d
palette = 2=#b8bb26
palette = 3=#d79921
palette = 4=#83a598
palette = 5=#d3869b
palette = 6=#8ec07c
palette = 7=#d5c4a1
palette = 8=#665c54
palette = 9=#cc241d
palette = 10=#b8bb26
palette = 11=#d79921
palette = 12=#83a598
palette = 13=#d3869b
palette = 14=#b8bb26
palette = 15=#ebdbb2
";

    // -----------------------------------------------------------------------
    // normalize_hex
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_hex_hash_prefix() {
        assert_eq!(normalize_hex("'#fdf6e3'"), Some("#fdf6e3".into()));
        assert_eq!(normalize_hex("\"#AABBCC\""), Some("#aabbcc".into()));
    }

    #[test]
    fn normalize_hex_0x_prefix() {
        assert_eq!(normalize_hex("\"0x1d2021\""), Some("#1d2021".into()));
        assert_eq!(normalize_hex("'0xAABBCC'"), Some("#aabbcc".into()));
    }

    #[test]
    fn normalize_hex_bare_hash() {
        // ghostty format: =#hex (no quotes)
        assert_eq!(normalize_hex("#1d2021"), Some("#1d2021".into()));
    }

    #[test]
    fn normalize_hex_strips_inline_comments() {
        assert_eq!(
            normalize_hex("'#fdf6e3' # solarized light"),
            Some("#fdf6e3".into())
        );
        assert_eq!(normalize_hex("#1d2021 # dark bg"), Some("#1d2021".into()));
        assert_eq!(
            normalize_hex("\"0x1d2021\" # gruvbox"),
            Some("#1d2021".into())
        );
    }

    #[test]
    fn normalize_hex_rejects_invalid() {
        assert_eq!(normalize_hex("'#xyz'"), None);
        assert_eq!(normalize_hex("'not-a-color'"), None);
        assert_eq!(normalize_hex("''"), None);
        assert_eq!(normalize_hex("'#12345'"), None);
        assert_eq!(normalize_hex("'#1234567'"), None);
    }

    // -----------------------------------------------------------------------
    // hex_to_rgb
    // -----------------------------------------------------------------------

    #[test]
    fn hex_to_rgb_converts_correctly() {
        assert_eq!(hex_to_rgb("#fdf6e3"), "253,246,227");
        assert_eq!(hex_to_rgb("#000000"), "0,0,0");
        assert_eq!(hex_to_rgb("#ffffff"), "255,255,255");
        assert_eq!(hex_to_rgb("#1d2021"), "29,32,33");
    }

    // -----------------------------------------------------------------------
    // Ghostty parser
    // -----------------------------------------------------------------------

    #[test]
    fn ghostty_parse_full_palette() {
        let colors = parse_ghostty_colors(GHOSTTY_GRUVU).unwrap();
        assert_eq!(colors.bg, "#1d2021");
        assert_eq!(colors.fg, "#d5c4a1");
        assert_eq!(colors.normal[0], "#1d2021"); // palette 0 = black
        assert_eq!(colors.normal[1], "#cc241d"); // palette 1 = red
        assert_eq!(colors.normal[6], "#8ec07c"); // palette 6 = cyan
        assert_eq!(colors.bright[0], "#665c54"); // palette 8
        assert_eq!(colors.bright[7], "#ebdbb2"); // palette 15
        assert_eq!(colors.selection_bg.as_deref(), Some("#665c54"));
    }

    #[test]
    fn ghostty_returns_none_for_empty() {
        assert!(parse_ghostty_colors("").is_none());
    }

    #[test]
    fn ghostty_returns_none_for_missing_palette() {
        let partial = "\
background =#1d2021
foreground =#d5c4a1
palette = 0=#1d2021
";
        assert!(parse_ghostty_colors(partial).is_none());
    }

    #[test]
    fn ghostty_ignores_comments() {
        let with_comments = format!(
            "# Full theme\n\
             # with lots of comments\n\
             {GHOSTTY_GRUVU}"
        );
        assert!(parse_ghostty_colors(&with_comments).is_some());
    }

    // -----------------------------------------------------------------------
    // Alacritty parser
    // -----------------------------------------------------------------------

    #[test]
    fn alacritty_parse_hash_prefix() {
        let colors = parse_alacritty_colors(ALACRITTY_SOLARIZED).unwrap();
        assert_eq!(colors.bg, "#fdf6e3");
        assert_eq!(colors.fg, "#586e75");
        assert_eq!(colors.normal[0], "#073642"); // black
        assert_eq!(colors.normal[1], "#dc322f"); // red
        assert_eq!(colors.normal[6], "#2aa198"); // cyan
        assert_eq!(colors.normal[7], "#eee8d5"); // white
        assert_eq!(colors.bright[0], "#002b36"); // bright black
        assert_eq!(colors.bright[7], "#fdf6e3"); // bright white
        assert!(colors.selection_bg.is_none());
    }

    #[test]
    fn alacritty_parse_0x_prefix() {
        let colors = parse_alacritty_colors(ALACRITTY_0X).unwrap();
        assert_eq!(colors.bg, "#1d2021");
        assert_eq!(colors.fg, "#d4be98");
        assert_eq!(colors.normal[0], "#665c54");
        assert_eq!(colors.normal[6], "#89b482");
        assert_eq!(colors.bright[0], "#928374");
        assert_eq!(colors.selection_bg.as_deref(), Some("#d4be98"));
    }

    #[test]
    fn alacritty_returns_none_for_empty() {
        assert!(parse_alacritty_colors("").is_none());
    }

    #[test]
    fn alacritty_returns_none_for_garbage() {
        assert!(parse_alacritty_colors("not a toml file at all").is_none());
    }

    #[test]
    fn alacritty_returns_none_when_bright_missing() {
        let partial = "\
[colors.primary]
background = '#fdf6e3'
foreground = '#586e75'

[colors.normal]
black   = '#073642'
red     = '#dc322f'
green   = '#859900'
yellow  = '#b58900'
blue    = '#268bd2'
magenta = '#d33682'
cyan    = '#2aa198'
white   = '#eee8d5'
";
        assert!(parse_alacritty_colors(partial).is_none());
    }

    #[test]
    fn alacritty_returns_none_when_color_missing() {
        // Missing normal.magenta
        let missing = "\
[colors.primary]
background = '#fdf6e3'
foreground = '#586e75'

[colors.normal]
black   = '#073642'
red     = '#dc322f'
green   = '#859900'
yellow  = '#b58900'
blue    = '#268bd2'
cyan    = '#2aa198'
white   = '#eee8d5'

[colors.bright]
black   = '#002b36'
red     = '#cb4b16'
green   = '#586e75'
yellow  = '#657b83'
blue    = '#839496'
magenta = '#6c71c4'
cyan    = '#93a1a1'
white   = '#fdf6e3'
";
        assert!(parse_alacritty_colors(missing).is_none());
    }

    #[test]
    fn alacritty_handles_extra_whitespace() {
        let spaced = "\
[colors.primary]
background  =  '#aabbcc'
foreground  =  '#112233'

[colors.normal]
black   =   '#000000'
red     =   '#110000'
green   =   '#001100'
yellow  =   '#111100'
blue    =   '#000011'
magenta =   '#110011'
cyan    =   '#001111'
white   =   '#ffffff'

[colors.bright]
black   =   '#333333'
red     =   '#440000'
green   =   '#004400'
yellow  =   '#444400'
blue    =   '#000044'
magenta =   '#440044'
cyan    =   '#004444'
white   =   '#cccccc'
";
        let colors = parse_alacritty_colors(spaced).unwrap();
        assert_eq!(colors.bg, "#aabbcc");
        assert_eq!(colors.fg, "#112233");
        assert_eq!(colors.normal[0], "#000000");
    }

    // -----------------------------------------------------------------------
    // CSS generation
    // -----------------------------------------------------------------------

    #[test]
    fn generate_css_contains_all_variables() {
        let colors = parse_alacritty_colors(ALACRITTY_SOLARIZED).unwrap();
        let css = generate_theme_css(&colors, false);
        assert!(css.contains("--bg: #fdf6e3;"));
        assert!(css.contains("--bg-secondary: #073642;"));
        assert!(css.contains("--bg-tertiary: #002b36;"));
        assert!(css.contains("--fg: #586e75;"));
        assert!(css.contains("--fg-muted: #eee8d5;"));
        assert!(css.contains("--fg-dim: #002b36;"));
        assert!(css.contains("--accent: #2aa198;"));
        assert!(css.contains("--accent-dim: #268bd2;"));
        assert!(css.contains("--success: #859900;"));
        assert!(css.contains("--warning: #b58900;"));
        assert!(css.contains("--danger: #dc322f;"));
        assert!(css.contains("--selection: #002b36;"));
        assert!(css.contains("--border: #002b36;"));
    }

    #[test]
    fn generate_css_overlay_uses_bg_rgba() {
        let colors = parse_alacritty_colors(ALACRITTY_SOLARIZED).unwrap();
        let css = generate_theme_css(&colors, false);
        assert!(css.contains("#help-overlay"));
        assert!(css.contains("rgba(253,246,227, 0.9)"));
        assert!(css.contains("#split-modal"));
    }

    #[test]
    fn generate_css_light_mode_has_marker() {
        let colors = parse_alacritty_colors(ALACRITTY_SOLARIZED).unwrap();
        let css = generate_theme_css(&colors, true);
        assert!(css.contains("--light-mode"));
    }

    #[test]
    fn generate_css_dark_mode_no_marker() {
        let colors = parse_alacritty_colors(ALACRITTY_SOLARIZED).unwrap();
        let css = generate_theme_css(&colors, false);
        assert!(!css.contains("--light-mode"));
    }

    #[test]
    fn generate_css_uses_selection_bg_when_present() {
        let colors = parse_ghostty_colors(GHOSTTY_GRUVU).unwrap();
        let css = generate_theme_css(&colors, false);
        assert!(css.contains("--selection: #665c54;"));
    }

    #[test]
    fn generate_css_falls_back_to_bright_black_for_selection() {
        let colors = parse_alacritty_colors(ALACRITTY_SOLARIZED).unwrap();
        let css = generate_theme_css(&colors, false);
        // No selection section → uses bright[0] (bright black) = #002b36
        assert!(css.contains("--selection: #002b36;"));
    }

    // -----------------------------------------------------------------------
    // load_from_theme_dir (filesystem integration)
    // -----------------------------------------------------------------------

    #[test]
    fn load_from_theme_dir_prefers_ghostty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ghostty.conf"), GHOSTTY_GRUVU).unwrap();
        std::fs::write(dir.path().join("alacritty.toml"), ALACRITTY_SOLARIZED).unwrap();

        let colors = load_from_theme_dir(dir.path()).unwrap();
        // Should pick ghostty (gruvu bg) not alacritty (solarized bg)
        assert_eq!(colors.bg, "#1d2021");
    }

    #[test]
    fn load_from_theme_dir_falls_back_to_alacritty() {
        let dir = tempfile::tempdir().unwrap();
        // No ghostty.conf
        std::fs::write(dir.path().join("alacritty.toml"), ALACRITTY_SOLARIZED).unwrap();

        let colors = load_from_theme_dir(dir.path()).unwrap();
        assert_eq!(colors.bg, "#fdf6e3");
    }

    #[test]
    fn load_from_theme_dir_returns_none_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_from_theme_dir(dir.path()).is_none());
    }

    #[test]
    fn is_light_theme_detects_light_mode_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_light_theme(dir.path()));

        std::fs::write(dir.path().join("light.mode"), "# light theme").unwrap();
        assert!(is_light_theme(dir.path()));
    }
}
