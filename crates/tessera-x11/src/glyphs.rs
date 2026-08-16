//! Client-side glyph rasterisation for the status bar (Nerd Font support).
//!
//! The X core font protocol (`open_font`/`image_text8`) only reaches bitmap
//! fonts registered in the server's font path (PCF/BDF); Nerd Fonts are TTF,
//! and x11rb ships no Xft/XRender binding. So the bar rasterises glyphs HERE,
//! in the client, with `fontdue` (pure Rust, no C dependency), and blits the
//! result with `PutImage`.
//!
//! This module is deliberately X-free and pure: it never touches a
//! connection, so every metric, blend and byte-packing decision is unit
//! testable. `bar_renderer` owns the X side.
//!
//! Alpha is resolved here too. The bar's tag background is a solid colour the
//! renderer itself just painted, so `blend(fill, glyph, coverage)` is EXACT —
//! no XRender, no server-side alpha, no reading the drawable back.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use fontdue::{Font, FontSettings};
use tessera_core::DErr;

/// One rasterised glyph: 8-bit coverage plus the metrics needed to place it
/// against the pen position and the baseline.
#[derive(Debug)]
pub struct Glyph {
    /// Coverage-bitmap size in pixels; both are `0` for a blank glyph.
    width: usize,
    height: usize,
    /// Whole-pixel pen advance to the next glyph.
    advance: i32,
    /// Left side bearing: bitmap left edge relative to the pen.
    xmin: i32,
    /// Bitmap bottom edge relative to the baseline (y grows UP here, while
    /// the X drawable's y grows down — `render_run` flips it).
    ymin: i32,
    /// 8-bit coverage, row-major, top row first (fontdue's layout).
    coverage: Vec<u8>,
}

/// The server's ZPixmap layout for one depth.
///
/// Read from the connection setup, NEVER assumed: hardcoding BGRX
/// little-endian silently swaps red and blue on an MSB-first server, and
/// guesses `bits_per_pixel` wrong on anything but a 32bpp/24-depth visual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixmapFormat {
    pub depth: u8,
    pub bits_per_pixel: u8,
    pub scanline_pad: u8,
    /// `true` when the setup's `image_byte_order` is `LSB_FIRST`.
    pub lsb_first: bool,
}

impl PixmapFormat {
    /// `None` when the layout is not one this blitter can pack EXACTLY.
    /// Colours arrive as `0x00RRGGBB` (`frames::pixel`), which only lines up
    /// with 24- or 32-bit-per-pixel ZPixmap data; anything else (packed
    /// 16bpp 5-6-5, 8bpp colormapped) would need a visual-mask conversion
    /// the bar does not do, so the caller falls back to the core font
    /// instead of guessing.
    pub fn new(depth: u8, bits_per_pixel: u8, scanline_pad: u8, lsb_first: bool) -> Option<Self> {
        let packable = matches!(bits_per_pixel, 24 | 32) && matches!(scanline_pad, 8 | 16 | 32);
        packable.then_some(PixmapFormat {
            depth,
            bits_per_pixel,
            scanline_pad,
            lsb_first,
        })
    }

    /// Bytes per scanline: `width * bits_per_pixel` bits rounded up to the
    /// server's `scanline_pad` (X requires every ZPixmap row padded).
    fn stride(&self, width: u16) -> usize {
        let pad = self.scanline_pad as usize;
        let bits = width as usize * self.bits_per_pixel as usize;
        bits.div_ceil(pad) * pad / 8
    }

    /// Writes one pixel at `offset` in the server's byte order.
    fn write_pixel(&self, data: &mut [u8], offset: usize, pixel: u32) {
        let bytes = self.bits_per_pixel as usize / 8;
        let le = pixel.to_le_bytes();
        for i in 0..bytes {
            // LSB-first puts the low-order byte first; MSB-first reverses the
            // SIGNIFICANT bytes only, so a 24bpp pixel never sends the unused
            // top byte of the 0x00RRGGBB value.
            let source = if self.lsb_first { i } else { bytes - 1 - i };
            if let (Some(slot), Some(byte)) = (data.get_mut(offset + i), le.get(source)) {
                *slot = *byte;
            }
        }
    }
}

/// A packed ZPixmap rectangle ready for `PutImage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlyphImage {
    pub width: u16,
    pub height: u16,
    pub data: Vec<u8>,
}

/// Exact compositing of `glyph` over `fill` at `coverage/255`, per channel.
///
/// The base is the slot's OWN fill, which the renderer painted a moment ago
/// with `poly_fill_rectangle`, so the blend is exact rather than an
/// approximation of whatever happens to be on the drawable. Blending against
/// the wrong base is the client-side twin of the old `image_text8` bug that
/// drew every tag number inside a black box.
pub fn blend(fill: u32, glyph: u32, coverage: u8) -> u32 {
    let channel = |shift: u32| -> u32 {
        let base = ((fill >> shift) & 0xFF) as i32;
        let ink = ((glyph >> shift) & 0xFF) as i32;
        // Exact at both ends: 0 -> base, 255 -> ink.
        (base + (ink - base) * coverage as i32 / 255) as u32 & 0xFF
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

/// A loaded font plus its per-`char` rasterisation cache.
///
/// The size is fixed for the renderer's lifetime, so `char` alone is a
/// complete cache key. Entries are `Arc`s: a cache hit hands back the SAME
/// allocation, never a re-rasterisation.
pub struct GlyphCache {
    font: Font,
    px: f32,
    /// Baseline offset from the line box's top edge, whole pixels.
    ascent: i32,
    /// Line box height (ascent + descent), whole pixels, at least 1.
    line_height: u16,
    // Interior mutability: `BarRenderer::draw` takes `&self`. Borrows never
    // overlap — `glyph` releases each one before taking the next.
    cache: RefCell<HashMap<char, Arc<Glyph>>>,
}

impl GlyphCache {
    /// Loads `path` (an absolute TTF/OTF path) at `px` pixels per em.
    ///
    /// Every failure — unreadable file, unparseable font, missing horizontal
    /// line metrics, nonsense size — is an `Err`, never a panic: the bar
    /// falls back to the core font rather than failing to draw (D7).
    pub fn load(path: &str, px: f32) -> Result<Self, DErr> {
        if !px.is_finite() || px <= 0.0 {
            return Err(DErr::X(format!("bar.font_size must be positive, got {px}")));
        }
        let data = std::fs::read(path)
            .map_err(|err| DErr::X(format!("cannot read bar font {path:?}: {err}")))?;
        let settings = FontSettings {
            scale: px,
            ..FontSettings::default()
        };
        let font = Font::from_bytes(data, settings)
            .map_err(|err| DErr::X(format!("cannot parse bar font {path:?}: {err}")))?;
        let metrics = font
            .horizontal_line_metrics(px)
            .ok_or_else(|| DErr::X(format!("bar font {path:?} has no horizontal line metrics")))?;
        // `as` on floats saturates in Rust, so a broken font yields clamped
        // metrics instead of UB or a panic.
        let ascent = metrics.ascent.ceil().max(0.0) as i32;
        let descent = (-metrics.descent).ceil().max(0.0) as i32;
        let line_height = (ascent + descent).clamp(1, u16::MAX as i32) as u16;
        Ok(GlyphCache {
            font,
            px,
            ascent,
            line_height,
            cache: RefCell::new(HashMap::new()),
        })
    }

    /// Height of one line box: the vertical extent `render_run` produces.
    pub fn line_height(&self) -> u16 {
        self.line_height
    }

    /// The cached glyph for `ch`, rasterising it exactly once.
    pub fn glyph(&self, ch: char) -> Arc<Glyph> {
        if let Some(hit) = self.cache.borrow().get(&ch).map(Arc::clone) {
            return hit;
        }
        let (metrics, coverage) = self.font.rasterize(ch, self.px);
        let glyph = Arc::new(Glyph {
            width: metrics.width,
            height: metrics.height,
            advance: metrics.advance_width.round().max(0.0) as i32,
            xmin: metrics.xmin,
            ymin: metrics.ymin,
            coverage,
        });
        self.cache.borrow_mut().insert(ch, Arc::clone(&glyph));
        glyph
    }

    /// Total advance width of `text` in whole pixels — the REAL per-glyph
    /// advances of the loaded font, which is what the bar's tag slot width is
    /// derived from (the old hardcoded 8px `fixed` advance is gone).
    pub fn run_width(&self, text: &str) -> u16 {
        let total: i32 = text.chars().map(|ch| self.glyph(ch).advance).sum();
        total.clamp(0, u16::MAX as i32) as u16
    }

    /// Composites `text` into a ZPixmap buffer covering exactly the glyph
    /// run's bounding box (`run_width` x `line_height`), clipped to
    /// `clip = (width, height)` so the blit never runs off the drawable.
    /// `None` means there is nothing to blit.
    pub fn render_run(
        &self,
        text: &str,
        fill: u32,
        glyph_color: u32,
        clip: (u16, u16),
        format: &PixmapFormat,
    ) -> Option<GlyphImage> {
        let width = self.run_width(text).min(clip.0);
        let height = self.line_height.min(clip.1);
        if width == 0 || height == 0 {
            return None;
        }
        let (w, h) = (width as usize, height as usize);
        // Coverage first, colour second: two glyphs may overlap (kerned or
        // negative-bearing), and keeping the max coverage avoids compositing
        // ink over already-composited ink, which would darken the overlap.
        let mut coverage = vec![0u8; w * h];
        let mut pen: i32 = 0;
        for ch in text.chars() {
            let glyph = self.glyph(ch);
            let left = pen + glyph.xmin;
            // fontdue's ymin grows up from the baseline; the drawable's y
            // grows down from the line box's top edge.
            let top = self.ascent - glyph.ymin - glyph.height as i32;
            for row in 0..glyph.height {
                let y = top + row as i32;
                if y < 0 || y >= h as i32 {
                    continue;
                }
                for col in 0..glyph.width {
                    let x = left + col as i32;
                    if x < 0 || x >= w as i32 {
                        continue;
                    }
                    let Some(&value) = glyph.coverage.get(row * glyph.width + col) else {
                        continue;
                    };
                    if let Some(slot) = coverage.get_mut(y as usize * w + x as usize) {
                        *slot = (*slot).max(value);
                    }
                }
            }
            pen += glyph.advance;
        }
        let stride = format.stride(width);
        let pixel_bytes = format.bits_per_pixel as usize / 8;
        let mut data = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                let value = coverage.get(y * w + x).copied().unwrap_or(0);
                let pixel = blend(fill, glyph_color, value);
                format.write_pixel(&mut data, y * stride + x * pixel_bytes, pixel);
            }
        }
        Some(GlyphImage {
            width,
            height,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// The default `bar.font`. Every test that needs real glyph outlines
    /// SKIPS (returns) when it is absent, so the suite still passes on a
    /// machine without this Nerd Font installed.
    const NERD_FONT: &str = "/usr/share/fonts/TTF/HackNerdFontMono-Regular.ttf";

    fn cache(px: f32) -> Option<GlyphCache> {
        if !Path::new(NERD_FONT).exists() {
            return None;
        }
        GlyphCache::load(NERD_FONT, px).ok()
    }

    fn lsb_32() -> PixmapFormat {
        match PixmapFormat::new(24, 32, 32, true) {
            Some(format) => format,
            None => unreachable!("24/32/32 LSB is a packable layout"),
        }
    }

    #[test]
    fn rasterising_the_same_char_twice_reuses_the_cached_glyph() {
        let Some(cache) = cache(12.0) else { return };
        let first = cache.glyph('1');
        let second = cache.glyph('1');
        assert!(
            Arc::ptr_eq(&first, &second),
            "a cache hit must hand back the SAME rasterisation, not a new one"
        );
        assert!(
            !Arc::ptr_eq(&first, &cache.glyph('2')),
            "a different char must rasterise into its own entry"
        );
    }

    #[test]
    fn advance_widths_come_from_the_font_not_from_a_fixed_8px_cell() {
        // The bar's slot width used to be `8 * name.len()` (the `fixed` core
        // font's cell). Real advances scale with the size instead.
        let (Some(small), Some(large)) = (cache(12.0), cache(24.0)) else {
            return;
        };
        assert!(small.run_width("123") > 0);
        assert!(
            large.run_width("123") > small.run_width("123"),
            "a bigger font size must produce a wider glyph run"
        );
        assert!(
            large.run_width("1") != 8,
            "the run width must be a real advance, not the 8px fixed-font cell"
        );
    }

    #[test]
    fn blend_is_exact_at_both_ends_of_the_coverage_ramp() {
        let (fill, ink) = (0x0022_2222, 0x00FF_8F40);
        assert_eq!(blend(fill, ink, 0), fill, "no coverage keeps the fill");
        assert_eq!(blend(fill, ink, 255), ink, "full coverage is pure ink");
        let half = blend(fill, ink, 128);
        assert!(half != fill && half != ink, "partial coverage mixes");
        // Every channel stays inside the fill..ink interval.
        for shift in [0, 8, 16] {
            let (a, b) = ((fill >> shift) & 0xFF, (ink >> shift) & 0xFF);
            let m = (half >> shift) & 0xFF;
            assert!(m >= a.min(b) && m <= a.max(b));
        }
        assert_eq!(blend(fill, ink, 200) >> 24, 0, "the high byte stays clear");
    }

    #[test]
    fn stride_pads_every_scanline_to_the_servers_scanline_pad() {
        let Some(bpp24_pad32) = PixmapFormat::new(24, 24, 32, true) else {
            unreachable!("24/24/32 is packable")
        };
        // 3 px * 24 bits = 72 bits -> padded to 96 bits = 12 bytes.
        assert_eq!(bpp24_pad32.stride(3), 12);
        assert_eq!(lsb_32().stride(3), 12);
        let Some(bpp24_pad8) = PixmapFormat::new(24, 24, 8, true) else {
            unreachable!("24/24/8 is packable")
        };
        assert_eq!(bpp24_pad8.stride(3), 9);
    }

    #[test]
    fn an_unpackable_server_layout_has_no_pixmap_format() {
        // 16bpp 5-6-5 needs a visual-mask conversion the bar does not do, so
        // the renderer must fall back rather than write swapped colours.
        assert_eq!(PixmapFormat::new(16, 16, 32, true), None);
        assert_eq!(PixmapFormat::new(8, 8, 32, true), None);
        assert!(PixmapFormat::new(32, 32, 32, false).is_some());
    }

    #[test]
    fn pixels_are_packed_in_the_servers_byte_order() {
        let Some(cache) = cache(12.0) else { return };
        let msb = match PixmapFormat::new(24, 32, 32, false) {
            Some(format) => format,
            None => unreachable!("24/32/32 MSB is a packable layout"),
        };
        let fill = 0x0011_2233;
        let lsb_image = cache.render_run("1", fill, fill, (1000, 1000), &lsb_32());
        let msb_image = cache.render_run("1", fill, fill, (1000, 1000), &msb);
        let (Some(lsb_image), Some(msb_image)) = (lsb_image, msb_image) else {
            unreachable!("a rendered '1' must produce an image")
        };
        // Same colour everywhere (fill == ink), so byte order is the only
        // difference: LSB-first writes 33 22 11 00, MSB-first 00 11 22 33.
        assert_eq!(&lsb_image.data[0..4], &[0x33, 0x22, 0x11, 0x00]);
        assert_eq!(&msb_image.data[0..4], &[0x00, 0x11, 0x22, 0x33]);
    }

    #[test]
    fn render_run_sizes_the_image_to_the_run_and_clips_it_to_the_drawable() {
        let Some(cache) = cache(12.0) else { return };
        let format = lsb_32();
        let Some(image) = cache.render_run("12", 0, 0x00FF_FFFF, (1000, 1000), &format) else {
            unreachable!("a rendered run must produce an image")
        };
        assert_eq!(image.width, cache.run_width("12"));
        assert_eq!(image.height, cache.line_height());
        assert_eq!(
            image.data.len(),
            format.stride(image.width) * image.height as usize
        );
        // A drawable smaller than the run clips instead of overflowing.
        let Some(clipped) = cache.render_run("12", 0, 0x00FF_FFFF, (4, 3), &format) else {
            unreachable!("a clipped run still produces an image")
        };
        assert_eq!((clipped.width, clipped.height), (4, 3));
        assert_eq!(cache.render_run("12", 0, 0, (0, 10), &format), None);
        assert_eq!(cache.render_run("", 0, 0, (100, 100), &format), None);
    }

    #[test]
    fn a_missing_or_unparseable_font_is_an_error_never_a_panic() {
        assert!(GlyphCache::load("/nonexistent/tessera-no-such-font.ttf", 12.0).is_err());
        assert!(GlyphCache::load(NERD_FONT, 0.0).is_err());
        assert!(GlyphCache::load(NERD_FONT, f32::NAN).is_err());
        // A real file that is not a font: this source file.
        assert!(GlyphCache::load(file!(), 12.0).is_err());
    }
}
