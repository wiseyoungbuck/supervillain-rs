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
// Readability repair
// ---------------------------------------------------------------------------
//
// Terminal palettes routinely reuse slots: Everforest sets color0 == color8
// and selection-background == foreground. Any static slot→variable mapping
// (ours in generate_theme_css, or the Omarchy supervillain.css.tpl template)
// then produces text painted in the exact color of the surface under it —
// e.g. --fg-dim (From:/To: labels) == --bg-secondary (email header), or
// --selection == --fg (selected rows). Since no mapping can be correct for
// every palette, repair happens at serve time: check the variable pairs that
// style.css actually layers, and re-derive any failing text color by blending
// the theme's own fg toward its bg (keeping the repaired color on-theme).

/// WCAG relative luminance of an sRGB color.
fn luminance(rgb: [u8; 3]) -> f64 {
    let lin = |c: u8| {
        let c = c as f64 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(rgb[0]) + 0.7152 * lin(rgb[1]) + 0.0722 * lin(rgb[2])
}

/// WCAG contrast ratio between two colors (>= 1.0).
fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Linear blend: `t = 0` returns `a`, `t = 1` returns `b`.
fn mix(a: [u8; 3], b: [u8; 3], t: f64) -> [u8; 3] {
    let ch = |x: u8, y: u8| (x as f64 * (1.0 - t) + y as f64 * t).round() as u8;
    [ch(a[0], b[0]), ch(a[1], b[1]), ch(a[2], b[2])]
}

/// Parse `#rrggbb` into RGB components.
fn parse_hex_rgb(s: &str) -> Option<[u8; 3]> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some([
        u8::from_str_radix(&h[0..2], 16).ok()?,
        u8::from_str_radix(&h[2..4], 16).ok()?,
        u8::from_str_radix(&h[4..6], 16).ok()?,
    ])
}

fn to_hex(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

// Minimum contrast ratios per role. --fg-dim is deliberately subtle, so its
// floor is well below the WCAG 4.5 body-text bar — just high enough to stay
// legible on every surface it's painted over.
const MIN_FG: f64 = 4.5;
const MIN_MUTED: f64 = 3.0;
const MIN_DIM: f64 = 2.4;
const MIN_ON_SELECTION: f64 = 2.5;
const MIN_ACCENT: f64 = 2.5;

/// Repair unreadable variable combinations in place. Returns the names of the
/// variables that were changed (empty when the theme was already readable).
///
/// The pairs checked mirror how static/style.css layers the variables:
/// - --fg on --bg (body text)
/// - --selection is a background under --fg / --fg-muted / --fg-dim / --accent
///   text (selected email rows, active mailbox)
/// - --fg-muted on --bg / --bg-secondary / --bg-tertiary (subjects, meta)
/// - --fg-dim on --bg / --bg-secondary / --bg-tertiary (labels, previews, dates)
/// - --accent on --bg / --bg-secondary (active items, links, counts)
fn repair_readability(colors: &mut std::collections::HashMap<String, [u8; 3]>) -> Vec<String> {
    let mut changed = Vec::new();
    let (Some(&bg), Some(&fg0)) = (colors.get("bg"), colors.get("fg")) else {
        return changed;
    };

    // --fg is the anchor every repair blends from; fix it first. A theme whose
    // fg fails against its own bg is fundamentally broken — snap to the pole.
    let mut fg = fg0;
    if contrast(fg, bg) < MIN_FG {
        fg = if luminance(bg) < 0.5 {
            [0xff, 0xff, 0xff]
        } else {
            [0x00, 0x00, 0x00]
        };
        colors.insert("fg".into(), fg);
        changed.push("fg".into());
    }

    let bg2 = colors.get("bg-secondary").copied().unwrap_or(bg);
    let bg3 = colors.get("bg-tertiary").copied().unwrap_or(bg);
    let accent = colors.get("accent").copied();

    // --selection: a background tint. When it collides with the text drawn
    // over it (Everforest ships selection-background == foreground), rebuild
    // it as a subtle fg-over-bg tint instead.
    let mut selection = colors.get("selection").copied();
    if let Some(sel) = selection {
        let readable = |s: [u8; 3]| {
            contrast(s, fg) >= MIN_ON_SELECTION && accent.is_none_or(|a| contrast(s, a) >= 2.0)
        };
        if !readable(sel) {
            let repaired = [0.18, 0.26, 0.34, 0.12]
                .iter()
                .map(|&t| mix(bg, fg, t))
                .find(|&c| readable(c))
                .unwrap_or_else(|| mix(bg, fg, 0.18));
            colors.insert("selection".into(), repaired);
            changed.push("selection".into());
            selection = Some(repaired);
        }
    }

    // Dimmed text roles: if any surface fails, re-derive from fg blended
    // toward bg — most-dimmed candidate first so the repaired color keeps its
    // intended visual weight. Falls back to plain fg (readable by rule 1).
    let mut repair_text = |name: &str, dim_steps: &[f64], surfaces: &[([u8; 3], f64)]| {
        let Some(&cur) = colors.get(name) else { return };
        let ok = |c: [u8; 3]| surfaces.iter().all(|&(s, min)| contrast(c, s) >= min);
        if ok(cur) {
            return;
        }
        let repaired = dim_steps
            .iter()
            .map(|&t| mix(fg, bg, t))
            .find(|&c| ok(c))
            .unwrap_or(fg);
        colors.insert(name.into(), repaired);
        changed.push(name.into());
    };

    let sel_surface = selection.map(|s| (s, 2.0));
    let mut muted_surfaces = vec![(bg, MIN_MUTED), (bg2, MIN_MUTED), (bg3, MIN_MUTED)];
    if let Some(s) = selection {
        muted_surfaces.push((s, MIN_ON_SELECTION));
    }
    repair_text("fg-muted", &[0.30, 0.20, 0.10, 0.0], &muted_surfaces);

    let mut dim_surfaces = vec![(bg, MIN_DIM), (bg2, MIN_DIM), (bg3, MIN_DIM)];
    if let Some(s) = sel_surface {
        dim_surfaces.push(s);
    }
    repair_text("fg-dim", &[0.55, 0.45, 0.30, 0.15, 0.0], &dim_surfaces);

    // --accent: text on bg/bg-secondary and on --selection. Blend toward fg
    // (not bg) so a repaired accent stays a highlight, not a dim.
    if let Some(acc) = accent {
        let mut surfaces = vec![(bg, MIN_ACCENT), (bg2, MIN_ACCENT)];
        if let Some(s) = selection {
            surfaces.push((s, 2.0));
        }
        let ok = |c: [u8; 3]| surfaces.iter().all(|&(s, min)| contrast(c, s) >= min);
        if !ok(acc) {
            let repaired = [0.25, 0.5, 0.75, 1.0]
                .iter()
                .map(|&t| mix(acc, fg, t))
                .find(|&c| ok(c))
                .unwrap_or(fg);
            colors.insert("accent".into(), repaired);
            changed.push("accent".into());
        }
    }

    changed
}

/// Rewrite the `:root` variable block of a theme stylesheet so every text
/// color stays readable on the surfaces style.css paints it over. CSS outside
/// the `:root` block, unknown variables, and non-hex values pass through
/// untouched; a stylesheet with no parseable `:root` (or one that is already
/// readable) is returned unchanged.
///
/// Applied to BOTH theme sources (a theme-shipped supervillain.css and our
/// generated fallback) at serve time, because both inherit the same
/// palette-slot collisions from the terminal color scheme they mirror.
pub fn sanitize_theme_css(css: &str) -> String {
    let Some(root_pos) = css.find(":root") else {
        return css.to_string();
    };
    let Some(open) = css[root_pos..].find('{').map(|i| root_pos + i) else {
        return css.to_string();
    };
    let Some(close) = css[open..].find('}').map(|i| open + i) else {
        return css.to_string();
    };
    let block = &css[open + 1..close];

    let mut colors = std::collections::HashMap::new();
    for line in block.lines() {
        if let Some(rest) = line.trim().strip_prefix("--")
            && let Some((name, value)) = rest.split_once(':')
            && let Some(rgb) = parse_hex_rgb(value.trim().trim_end_matches(';').trim())
        {
            colors.insert(name.trim().to_string(), rgb);
        }
    }

    let changed = repair_readability(&mut colors);
    if changed.is_empty() {
        return css.to_string();
    }

    let new_block: Vec<String> = block
        .lines()
        .map(|line| {
            if let Some(rest) = line.trim().strip_prefix("--")
                && let Some((name, _)) = rest.split_once(':')
            {
                let name = name.trim();
                if changed.iter().any(|c| c == name) {
                    let indent = &line[..line.len() - line.trim_start().len()];
                    return format!("{indent}--{name}: {};", to_hex(colors[name]));
                }
            }
            line.to_string()
        })
        .collect();
    let trailing = if block.ends_with('\n') { "\n" } else { "" };
    format!(
        "{}{}{}{}",
        &css[..open + 1],
        new_block.join("\n"),
        trailing,
        &css[close..]
    )
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

    // -----------------------------------------------------------------------
    // sanitize_theme_css / repair_readability
    // -----------------------------------------------------------------------

    // Everforest: color0 == color8 == #475258 (so --fg-dim lands on surfaces
    // of the exact same color) and selection-background == foreground (so
    // selected rows paint fg-colored text on an fg-colored background). The
    // real-world black-on-black report.
    const GHOSTTY_EVERFOREST: &str = "\
background = #2d353b
foreground = #d3c6aa
selection-background = #d3c6aa
selection-foreground = #2d353b

palette = 0=#475258
palette = 1=#e67e80
palette = 2=#a7c080
palette = 3=#dbbc7f
palette = 4=#7fbbb3
palette = 5=#d699b6
palette = 6=#83c092
palette = 7=#d3c6aa
palette = 8=#475258
palette = 9=#e67e80
palette = 10=#a7c080
palette = 11=#dbbc7f
palette = 12=#7fbbb3
palette = 13=#d699b6
palette = 14=#83c092
palette = 15=#d3c6aa
";

    // The supervillain.css Omarchy's template pipeline generates for
    // Everforest (verbatim from ~/.config/omarchy/current/theme/).
    const EVERFOREST_TEMPLATE_CSS: &str = "\
/* Omarchy Theme for Supervillain */

:root {
    --bg: #2d353b;
    --bg-secondary: #475258;
    --bg-tertiary: #475258;
    --fg: #d3c6aa;
    --fg-muted: #d3c6aa;
    --fg-dim: #475258;
    --accent: #7fbbb3;
    --accent-dim: #7fbbb3;
    --success: #a7c080;
    --warning: #dbbc7f;
    --danger: #e67e80;
    --selection: #d3c6aa;
    --border: #475258;
}

/* Help and modal overlays */
#help-overlay {
    background: rgba(45,53,59, 0.9);
}

#split-modal {
    background: rgba(45,53,59, 0.9);
}
";

    /// Extract `--name: #hex;` from a css string.
    fn css_var(css: &str, name: &str) -> [u8; 3] {
        let needle = format!("--{name}:");
        let start = css.find(&needle).unwrap() + needle.len();
        let end = css[start..].find(';').unwrap() + start;
        parse_hex_rgb(css[start..end].trim()).unwrap()
    }

    #[test]
    fn sanitize_everforest_template_css_repairs_collisions() {
        let out = sanitize_theme_css(EVERFOREST_TEMPLATE_CSS);
        let bg = css_var(&out, "bg");
        let bg2 = css_var(&out, "bg-secondary");
        let bg3 = css_var(&out, "bg-tertiary");
        let fg = css_var(&out, "fg");
        let dim = css_var(&out, "fg-dim");
        let sel = css_var(&out, "selection");

        // Anchors and surfaces stay on-theme
        assert_eq!(bg, [0x2d, 0x35, 0x3b]);
        assert_eq!(fg, [0xd3, 0xc6, 0xaa]);
        assert_eq!(bg2, [0x47, 0x52, 0x58]);

        // From:/To: labels: --fg-dim was the identical hex as the email
        // header background it sits on. Must now clear the dim floor on
        // every surface.
        for surface in [bg, bg2, bg3] {
            assert!(
                contrast(dim, surface) >= MIN_DIM,
                "fg-dim {} unreadable on {}",
                to_hex(dim),
                to_hex(surface)
            );
        }

        // Selected rows: --selection was == --fg. Text over it must read.
        assert!(contrast(sel, fg) >= MIN_ON_SELECTION);
        assert!(contrast(sel, css_var(&out, "accent")) >= 2.0);
        assert!(contrast(sel, css_var(&out, "fg-muted")) >= MIN_ON_SELECTION);
    }

    #[test]
    fn sanitize_preserves_untouched_rules_and_vars() {
        let out = sanitize_theme_css(EVERFOREST_TEMPLATE_CSS);
        // Non-:root rules pass through verbatim
        assert!(out.contains("background: rgba(45,53,59, 0.9);"));
        // Vars outside the readability contract are untouched
        assert!(out.contains("--danger: #e67e80;"));
        assert!(out.contains("--warning: #dbbc7f;"));
        assert!(out.contains("--border: #475258;"));
    }

    #[test]
    fn sanitize_generated_css_repairs_everforest() {
        // Path 2 (no supervillain.css): generate_theme_css maps --fg-dim and
        // --bg-tertiary from the same palette slot, so hover surfaces always
        // matched dim text exactly. Sanitize must repair its output too.
        let colors = parse_ghostty_colors(GHOSTTY_EVERFOREST).unwrap();
        let out = sanitize_theme_css(&generate_theme_css(&colors, false));
        let dim = css_var(&out, "fg-dim");
        assert!(contrast(dim, css_var(&out, "bg-tertiary")) >= MIN_DIM);
        assert!(contrast(dim, css_var(&out, "bg-secondary")) >= MIN_DIM);
        assert!(contrast(css_var(&out, "selection"), css_var(&out, "fg")) >= MIN_ON_SELECTION);
    }

    #[test]
    fn sanitize_leaves_readable_theme_unchanged() {
        let css = "\
:root {
    --bg: #000000;
    --bg-secondary: #101010;
    --bg-tertiary: #202020;
    --fg: #e0e0e0;
    --fg-muted: #b0b0b0;
    --fg-dim: #808080;
    --accent: #00c0c0;
    --selection: #303030;
}
";
        assert_eq!(sanitize_theme_css(css), css);
    }

    #[test]
    fn sanitize_light_theme_with_dark_secondary_surface() {
        // Solarized-light shape: light bg, dark bg-secondary, and a
        // near-background --fg-muted. Repaired muted text must read on both.
        let css = "\
:root {
    --bg: #fdf6e3;
    --bg-secondary: #073642;
    --bg-tertiary: #002b36;
    --fg: #586e75;
    --fg-muted: #eee8d5;
    --fg-dim: #002b36;
    --accent: #2aa198;
    --selection: #002b36;
}
";
        let out = sanitize_theme_css(css);
        let muted = css_var(&out, "fg-muted");
        let dim = css_var(&out, "fg-dim");
        for (surface, min) in [
            (css_var(&out, "bg"), MIN_MUTED),
            (css_var(&out, "bg-secondary"), MIN_MUTED),
        ] {
            assert!(contrast(muted, surface) >= min);
        }
        for surface in [
            css_var(&out, "bg"),
            css_var(&out, "bg-secondary"),
            css_var(&out, "bg-tertiary"),
        ] {
            assert!(contrast(dim, surface) >= MIN_DIM);
        }
        assert!(contrast(css_var(&out, "selection"), css_var(&out, "fg")) >= MIN_ON_SELECTION);
    }

    #[test]
    fn sanitize_passes_through_css_without_root_block() {
        assert_eq!(sanitize_theme_css(""), "");
        assert_eq!(
            sanitize_theme_css("body { color: red; }"),
            "body { color: red; }"
        );
        // :root present but no parseable bg/fg → untouched
        let partial = ":root { --accent: #123456; }";
        assert_eq!(sanitize_theme_css(partial), partial);
    }

    #[test]
    fn sanitize_preserves_light_mode_marker() {
        let colors = parse_ghostty_colors(GHOSTTY_EVERFOREST).unwrap();
        let out = sanitize_theme_css(&generate_theme_css(&colors, true));
        assert!(out.contains("--light-mode"));
    }

    #[test]
    fn sanitize_snaps_fg_to_pole_when_fg_matches_bg() {
        let css = "\
:root {
    --bg: #2d353b;
    --fg: #2d353b;
}
";
        let out = sanitize_theme_css(css);
        assert_eq!(css_var(&out, "fg"), [0xff, 0xff, 0xff]);
    }
}
