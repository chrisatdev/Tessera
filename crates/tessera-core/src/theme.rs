//! Pure theme model: canonical ayu_dark palette, lenient TOML parse, strict keys.
//!
//! `Color` and `Theme` live in `tessera-core` so the palette is headless-testable
//! and reusable by every X-facing consumer (frames, the future bar). Parsing is
//! validated once at load time: bad hex is a parse error, never a silent default.

/// A single RGB colour, parsed from a `#RRGGBB` hex string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    /// Parses a `#RRGGBB` hex string (case-insensitive digits).
    ///
    /// Anything other than exactly `#` + 6 hex digits is rejected with a
    /// descriptive message, so a bad palette value can never silently corrupt
    /// a frame's border colour.
    pub fn parse_hex(s: &str) -> Result<Color, String> {
        let bytes = s.as_bytes();
        if bytes.len() != 7 || bytes[0] != b'#' {
            return Err(format!("invalid color {s:?}: expected a #RRGGBB string"));
        }
        let nibble = |b: u8| match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            b'A'..=b'F' => Ok(b - b'A' + 10),
            _ => Err(format!("invalid color {s:?}: non-hex digit")),
        };
        let octet = |i: usize| -> Result<u8, String> {
            let hi = nibble(bytes[i])?;
            let lo = nibble(bytes[i + 1])?;
            Ok(hi << 4 | lo)
        };
        Ok(Color {
            r: octet(1)?,
            g: octet(3)?,
            b: octet(5)?,
        })
    }
}

/// Theme palette: the 10 ayu_dark colours plus optional explicit border overrides.
///
/// Every palette field defaults to the embedded ayu_dark palette; a `theme.toml`
/// only overrides the keys it provides (lenient per-field fallback). Unknown keys
/// are rejected, matching `GeneralConfig`'s strict-field policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub background: Color,
    pub foreground: Color,
    pub comment: Color,
    /// Primary accent colour (ayu `orange`); focused-frame border default (D3).
    pub accent: Color,
    pub yellow: Color,
    pub blue: Color,
    pub cyan: Color,
    pub green: Color,
    pub magenta: Color,
    pub red: Color,
    /// Explicit focused-border colour; `None` derives from `accent` (D3).
    pub active_border: Option<Color>,
    /// Explicit unfocused-border colour; `None` derives from `comment` (D3).
    pub inactive_border: Option<Color>,
}

impl Theme {
    /// Border colour for focused frames: explicit override or `accent` (D3).
    pub fn active_border(&self) -> Color {
        self.active_border.unwrap_or(self.accent)
    }

    /// Border colour for unfocused frames: explicit override or `comment` (D3).
    pub fn inactive_border(&self) -> Color {
        self.inactive_border.unwrap_or(self.comment)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            background: default_background(),
            foreground: default_foreground(),
            comment: default_comment(),
            accent: default_accent(),
            yellow: default_yellow(),
            blue: default_blue(),
            cyan: default_cyan(),
            green: default_green(),
            magenta: default_magenta(),
            red: default_red(),
            active_border: None,
            inactive_border: None,
        }
    }
}

/// Errors while reading or parsing a theme file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeError {
    Io(String),
    Parse(String),
}

fn default_background() -> Color {
    Color {
        r: 0x0A,
        g: 0x0E,
        b: 0x14,
    }
}
fn default_foreground() -> Color {
    Color {
        r: 0xB3,
        g: 0xB1,
        b: 0xAD,
    }
}
fn default_comment() -> Color {
    Color {
        r: 0x62,
        g: 0x6A,
        b: 0x73,
    }
}
fn default_accent() -> Color {
    Color {
        r: 0xFF,
        g: 0x8F,
        b: 0x40,
    }
}
fn default_yellow() -> Color {
    Color {
        r: 0xE6,
        g: 0xB4,
        b: 0x50,
    }
}
fn default_blue() -> Color {
    Color {
        r: 0x39,
        g: 0xBA,
        b: 0xE6,
    }
}
fn default_cyan() -> Color {
    Color {
        r: 0x95,
        g: 0xE6,
        b: 0xCB,
    }
}
fn default_green() -> Color {
    Color {
        r: 0xAA,
        g: 0xD9,
        b: 0x4C,
    }
}
fn default_magenta() -> Color {
    Color {
        r: 0xD2,
        g: 0xA6,
        b: 0xFF,
    }
}
fn default_red() -> Color {
    Color {
        r: 0xF2,
        g: 0x53,
        b: 0x58,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(c: Color) -> String {
        format!("#{:02X}{:02X}{:02X}", c.r, c.g, c.b)
    }

    #[test]
    fn default_theme_matches_canonical_ayu_dark_goldens() {
        let t = Theme::default();
        assert_eq!(hex(t.background), "#0A0E14");
        assert_eq!(hex(t.foreground), "#B3B1AD");
        assert_eq!(hex(t.comment), "#626A73");
        assert_eq!(hex(t.accent), "#FF8F40");
        assert_eq!(hex(t.yellow), "#E6B450");
        assert_eq!(hex(t.blue), "#39BAE6");
        assert_eq!(hex(t.cyan), "#95E6CB");
        assert_eq!(hex(t.green), "#AAD94C");
        assert_eq!(hex(t.magenta), "#D2A6FF");
        assert_eq!(hex(t.red), "#F25358");
    }

    #[test]
    fn default_theme_has_no_explicit_border_overrides() {
        let t = Theme::default();
        assert_eq!(t.active_border, None);
        assert_eq!(t.inactive_border, None);
    }

    #[test]
    fn color_parses_upper_and_lowercase_hex() {
        assert_eq!(
            Color::parse_hex("#0A0E14").expect("valid hex"),
            Color {
                r: 0x0A,
                g: 0x0E,
                b: 0x14
            }
        );
        assert_eq!(
            Color::parse_hex("#aad94c").expect("lowercase hex"),
            Color {
                r: 0xAA,
                g: 0xD9,
                b: 0x4C
            }
        );
    }

    #[test]
    fn color_rejects_bad_hex_inputs() {
        assert!(Color::parse_hex("").is_err());
        assert!(Color::parse_hex("#12345").is_err()); // too short
        assert!(Color::parse_hex("#1234567").is_err()); // too long
        assert!(Color::parse_hex("0A0E14").is_err()); // missing '#'
        assert!(Color::parse_hex("#GGHHII").is_err()); // non-hex digits
    }

    #[test]
    fn derived_borders_fall_back_to_accent_and_comment() {
        let t = Theme::default();
        assert_eq!(hex(t.active_border()), "#FF8F40");
        assert_eq!(hex(t.inactive_border()), "#626A73");
        assert_ne!(t.active_border(), t.inactive_border());
    }

    #[test]
    fn explicit_borders_override_derived_defaults() {
        let t = Theme {
            active_border: Some(Color::parse_hex("#112233").expect("valid hex")),
            inactive_border: Some(Color::parse_hex("#445566").expect("valid hex")),
            ..Theme::default()
        };
        assert_eq!(hex(t.active_border()), "#112233");
        assert_eq!(hex(t.inactive_border()), "#445566");
    }

    #[test]
    fn parse_full_theme_overrides_every_field() {
        let raw = "\
background = \"#101010\"
foreground = \"#202020\"
comment = \"#303030\"
accent = \"#404040\"
yellow = \"#505050\"
blue = \"#606060\"
cyan = \"#707070\"
green = \"#808080\"
magenta = \"#909090\"
red = \"#A0A0A0\"
active_border = \"#B0B0B0\"
inactive_border = \"#C0C0C0\"
";
        let t = Theme::parse(raw).expect("valid full theme");
        assert_eq!(hex(t.background), "#101010");
        assert_eq!(hex(t.foreground), "#202020");
        assert_eq!(hex(t.comment), "#303030");
        assert_eq!(hex(t.accent), "#404040");
        assert_eq!(hex(t.yellow), "#505050");
        assert_eq!(hex(t.blue), "#606060");
        assert_eq!(hex(t.cyan), "#707070");
        assert_eq!(hex(t.green), "#808080");
        assert_eq!(hex(t.magenta), "#909090");
        assert_eq!(hex(t.red), "#A0A0A0");
        assert_eq!(hex(t.active_border()), "#B0B0B0");
        assert_eq!(hex(t.inactive_border()), "#C0C0C0");
    }

    #[test]
    fn parse_partial_theme_falls_back_per_field() {
        let t = Theme::parse("red = \"#F07178\"\n").expect("valid partial theme");
        assert_eq!(hex(t.red), "#F07178");
        assert_eq!(hex(t.background), "#0A0E14");
        assert_eq!(hex(t.foreground), "#B3B1AD");
        assert_eq!(hex(t.comment), "#626A73");
        assert_eq!(hex(t.accent), "#FF8F40");
        assert_eq!(hex(t.yellow), "#E6B450");
        assert_eq!(hex(t.blue), "#39BAE6");
        assert_eq!(hex(t.cyan), "#95E6CB");
        assert_eq!(hex(t.green), "#AAD94C");
        assert_eq!(hex(t.magenta), "#D2A6FF");
        // Borders stay unset in the file → derived defaults apply.
        assert_eq!(hex(t.active_border()), "#FF8F40");
        assert_eq!(hex(t.inactive_border()), "#626A73");
    }

    #[test]
    fn parse_unknown_key_is_rejected_and_named() {
        let msg = |raw: &str| match Theme::parse(raw) {
            Ok(t) => panic!("expected rejection for {raw:?}, got {t:?}"),
            Err(ThemeError::Parse(m)) => m,
            Err(other) => panic!("expected Parse error, got {other:?}"),
        };
        let err1 = msg("bogus = \"#123456\"\n");
        assert!(err1.contains("bogus"), "error should name the key: {err1}");
        let err2 = msg("red = \"#FF0000\"\nnope = \"#000000\"\n");
        assert!(err2.contains("nope"), "error should name the key: {err2}");
    }

    #[test]
    fn parse_malformed_toml_fails() {
        let err = Theme::parse("not [valid toml").expect_err("malformed toml must fail");
        assert!(matches!(err, ThemeError::Parse(_)));
    }

    #[test]
    fn parse_bad_hex_value_fails() {
        let err = Theme::parse("red = \"#ZZZZZZ\"\n").expect_err("bad hex must fail");
        assert!(matches!(err, ThemeError::Parse(_)));
    }

    #[test]
    fn load_missing_file_fails_with_io_error() {
        let missing = std::path::Path::new("/nonexistent/tessera-theme-does-not-exist.toml");
        let err = Theme::load(missing).expect_err("missing file must fail");
        assert!(matches!(err, ThemeError::Io(_)));
    }

    #[test]
    fn load_valid_file_returns_parsed_theme() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tessera-theme-load-test-{}.toml", std::process::id()));
        std::fs::write(&path, "red = \"#F07178\"\n").expect("write temp theme");
        let t = Theme::load(&path).expect("valid theme file loads");
        assert_eq!(hex(t.red), "#F07178");
        assert_eq!(hex(t.background), "#0A0E14");
        assert_eq!(hex(t.active_border()), "#FF8F40");
        let _ = std::fs::remove_file(&path);
    }
}
