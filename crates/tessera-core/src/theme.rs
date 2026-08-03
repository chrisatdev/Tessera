//! Pure theme model: canonical ayu_dark palette, lenient TOML parse, strict keys.
//!
//! TDD RED: tests written first; production types (`Color`, `Theme`, `ThemeError`)
//! do not exist yet, so this module intentionally fails to compile.

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
        let mut t = Theme::default();
        t.active_border = Some(Color::parse_hex("#112233").expect("valid hex"));
        t.inactive_border = Some(Color::parse_hex("#445566").expect("valid hex"));
        assert_eq!(hex(t.active_border()), "#112233");
        assert_eq!(hex(t.inactive_border()), "#445566");
    }
}
