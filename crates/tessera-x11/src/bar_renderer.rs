//! Status-bar rendering on the X11 display (design D1/D2/D3, Phase 2).
//!
//! [`BarRenderer`] maps a fresh [`WmState`] snapshot into X poly-fill
//! rectangles + core-font text through the [`BarOps`] seam, mirroring the
//! `FrameOps`/`FakeFrameOps` scripted-headless pattern: production uses
//! [`RustConnection`], headless unit tests use a recording fake.
//!
//! The bar window is created once at startup ([`BarRenderer::new`]) with a
//! single reused `Pixmap` + `Gcontext` (D11: no per-draw allocation); every
//! [`BarRenderer::draw`] paints the workspace tags directly on the window.
//! `draw` runs exactly when the binary recomputes (D4) — never on idle event
//! polling — so an idle WM issues no X traffic from the bar.

use std::sync::Arc;
use std::sync::Once;

use tessera_core::{BarConfig, DErr, Rect, WmState};
// Re-exported from the renderer so `tessera_x11` can expose it (design:
// `pub use bar_renderer::{BarRenderer, BarPosition};`).
pub use tessera_core::BarPosition;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeGCAux, CreateGCAux, CreateWindowAux, Drawable, Fontable, Gcontext, Pixmap, Rectangle,
    Visualid, Window, WindowClass, change_gc, create_gc, create_pixmap, create_window, image_text8,
    map_window, open_font, poly_fill_rectangle, query_font,
};
use x11rb::rust_connection::RustConnection;

use crate::display_server::{map_conn_error, map_reply_error};
use crate::frames::pixel;

/// Per-edge default bar thickness (design D6): `thickness = None` resolves to
/// this for `Top`/`Bottom` bars.
pub(crate) const DEFAULT_EDGE_THICKNESS: u16 = 22;
/// Per-edge default bar thickness for `Left`/`Right` bars (design D6).
pub(crate) const DEFAULT_SIDE_THICKNESS: u16 = 6;

/// Vertical extent of one row of the `fixed` core font (a 8x13 bitmap).
const GLYPH_HEIGHT: i16 = 13;
/// Advance width of one `fixed` glyph.
const GLYPH_WIDTH: i16 = 8;
/// Horizontal padding on EACH side of a tag's glyphs (D3 legibility): without
/// it a one-character tag owns an 8px slot and the next tag starts at pixel 9,
/// so the numbers butt against each other with no visible separation. It is
/// deliberately focus-independent — a focused tag that is wider than its
/// unfocused self shifts the whole strip sideways every time focus moves.
const TAG_PADDING_X: i16 = 6;

/// Position + thickness resolved into the rectangle the bar occupies.
pub(crate) type BarGeometry = Rect;

/// The bar's thickness along the configured edge: the explicit `thickness`,
/// or the per-edge default (22 top/bottom, 6 left/right) when unset (D6).
pub(crate) fn resolve_thickness(bar: &BarConfig) -> u16 {
    bar.thickness.unwrap_or(match bar.position {
        BarPosition::Top | BarPosition::Bottom => DEFAULT_EDGE_THICKNESS,
        BarPosition::Left | BarPosition::Right => DEFAULT_SIDE_THICKNESS,
    })
}

/// Resolves the bar rectangle inside `monitor` for `bar`'s position and
/// thickness (D6). The window the renderer maps is placed exactly here.
pub(crate) fn bar_geometry(monitor: Rect, bar: &BarConfig) -> BarGeometry {
    let t = resolve_thickness(bar);
    match bar.position {
        BarPosition::Top => Rect {
            x: monitor.x,
            y: monitor.y,
            w: monitor.w,
            h: t,
        },
        BarPosition::Bottom => Rect {
            x: monitor.x,
            y: monitor.y + monitor.h as i32 - t as i32,
            w: monitor.w,
            h: t,
        },
        BarPosition::Left => Rect {
            x: monitor.x,
            y: monitor.y,
            w: t,
            h: monitor.h,
        },
        BarPosition::Right => Rect {
            x: monitor.x + monitor.w as i32 - t as i32,
            y: monitor.y,
            w: t,
            h: monitor.h,
        },
    }
}

/// The work area left for the WM once the bar is subtracted from `monitor`
/// along the configured edge (task 2.6: the binary passes this as the tiling
/// area to the core instead of the full screen).
pub fn tiling_area(monitor: Rect, bar: &BarConfig) -> Rect {
    let t = resolve_thickness(bar) as i32;
    match bar.position {
        BarPosition::Top => Rect {
            x: monitor.x,
            y: monitor.y + t,
            w: monitor.w,
            h: (monitor.h as i32 - t) as u16,
        },
        BarPosition::Bottom => Rect {
            x: monitor.x,
            y: monitor.y,
            w: monitor.w,
            h: (monitor.h as i32 - t) as u16,
        },
        BarPosition::Left => Rect {
            x: monitor.x + t,
            y: monitor.y,
            w: (monitor.w as i32 - t) as u16,
            h: monitor.h,
        },
        BarPosition::Right => Rect {
            x: monitor.x,
            y: monitor.y,
            w: (monitor.w as i32 - t) as u16,
            h: monitor.h,
        },
    }
}

/// The X surface [`BarRenderer`] draws through (design D2), mirroring
/// `FrameOps`. Production is backed by [`RustConnection`]; headless unit
/// tests use `FakeBarOps` and assert the exact X call sequence.
///
/// `#[doc(hidden)] pub` rather than `pub(crate)`: the public
/// [`BarRenderer`] type must be able to name its seam, and Rust has no
/// `pub(crate)` bound that keeps a public type generic over an internal
/// trait. It stays an implementation detail (hidden from the docs).
#[doc(hidden)]
pub trait BarOps {
    /// Allocates a fresh X resource id (window, pixmap or GC).
    fn generate_id(&self) -> Result<u32, DErr>;
    /// Creates the background pixmap the bar window's background points at.
    fn create_pixmap(
        &self,
        depth: u8,
        pid: Pixmap,
        drawable: Drawable,
        width: u16,
        height: u16,
    ) -> Result<(), DErr>;
    /// Creates the (override-redirect) bar window with a background pixmap.
    #[allow(clippy::too_many_arguments)]
    fn create_window(
        &self,
        depth: u8,
        wid: Window,
        parent: Window,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        class: WindowClass,
        visual: Visualid,
        aux: &CreateWindowAux,
    ) -> Result<(), DErr>;
    /// Creates the single reused GC (filled with the bar background first).
    fn create_gc(&self, cid: Gcontext, drawable: Drawable, aux: &CreateGCAux) -> Result<(), DErr>;
    /// Updates GC state for the next paint.
    fn change_gc(&self, gc: Gcontext, aux: &ChangeGCAux) -> Result<(), DErr>;
    /// Fills `rectangles` on `drawable` with the current GC foreground.
    fn poly_fill_rectangle(
        &self,
        drawable: Drawable,
        gc: Gcontext,
        rectangles: &[Rectangle],
    ) -> Result<(), DErr>;
    /// Renders a one-line text string with the current GC font.
    #[allow(clippy::too_many_arguments)]
    fn image_text8(
        &self,
        drawable: Drawable,
        gc: Gcontext,
        x: i16,
        y: i16,
        string: &[u8],
    ) -> Result<(), DErr>;
    /// Maps (shows) the bar window.
    fn map_window(&self, window: Window) -> Result<(), DErr>;
    /// Probes the core font `name`; `None` means the font is unavailable and
    /// tags must render as filled rectangles only (design D7).
    fn query_font(&self, name: &str) -> Result<Option<Fontable>, DErr>;
    /// Pushes buffered requests to the server. Required because the event
    /// loop's reads do not flush the shared write buffer.
    fn flush(&self) -> Result<(), DErr>;
}

impl BarOps for RustConnection {
    fn generate_id(&self) -> Result<u32, DErr> {
        Connection::generate_id(self)
            .map_err(|err| DErr::X(format!("x11 resource id allocation failed: {err}")))
    }
    fn create_pixmap(
        &self,
        depth: u8,
        pid: Pixmap,
        drawable: Drawable,
        width: u16,
        height: u16,
    ) -> Result<(), DErr> {
        let cookie =
            create_pixmap(self, depth, pid, drawable, width, height).map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn create_window(
        &self,
        depth: u8,
        wid: Window,
        parent: Window,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        class: WindowClass,
        visual: Visualid,
        aux: &CreateWindowAux,
    ) -> Result<(), DErr> {
        let cookie = create_window(
            self, depth, wid, parent, x, y, width, height, 0, class, visual, aux,
        )
        .map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn create_gc(&self, cid: Gcontext, drawable: Drawable, aux: &CreateGCAux) -> Result<(), DErr> {
        let cookie = create_gc(self, cid, drawable, aux).map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn change_gc(&self, gc: Gcontext, aux: &ChangeGCAux) -> Result<(), DErr> {
        let cookie = change_gc(self, gc, aux).map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn poly_fill_rectangle(
        &self,
        drawable: Drawable,
        gc: Gcontext,
        rectangles: &[Rectangle],
    ) -> Result<(), DErr> {
        let cookie = poly_fill_rectangle(self, drawable, gc, rectangles).map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn image_text8(
        &self,
        drawable: Drawable,
        gc: Gcontext,
        x: i16,
        y: i16,
        string: &[u8],
    ) -> Result<(), DErr> {
        let cookie = image_text8(self, drawable, gc, x, y, string).map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn map_window(&self, window: Window) -> Result<(), DErr> {
        let cookie = map_window(self, window).map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn query_font(&self, name: &str) -> Result<Option<Fontable>, DErr> {
        let fid = Connection::generate_id(self)
            .map_err(|err| DErr::X(format!("x11 resource id allocation failed: {err}")))?;
        let cookie = open_font(self, fid, name.as_bytes()).map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)?;
        let cookie = query_font(self, fid).map_err(map_conn_error)?;
        cookie.reply().map_err(map_reply_error)?;
        Ok(Some(fid))
    }
    fn flush(&self) -> Result<(), DErr> {
        Connection::flush(self).map_err(map_conn_error)
    }
}

impl BarOps for Arc<RustConnection> {
    // The bar runs on its own thread while the event loop shares the same
    // connection (task 2.7); every op delegates to the inner connection.
    fn generate_id(&self) -> Result<u32, DErr> {
        BarOps::generate_id(self.as_ref())
    }
    fn create_pixmap(
        &self,
        depth: u8,
        pid: Pixmap,
        drawable: Drawable,
        width: u16,
        height: u16,
    ) -> Result<(), DErr> {
        BarOps::create_pixmap(self.as_ref(), depth, pid, drawable, width, height)
    }
    fn create_window(
        &self,
        depth: u8,
        wid: Window,
        parent: Window,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        class: WindowClass,
        visual: Visualid,
        aux: &CreateWindowAux,
    ) -> Result<(), DErr> {
        BarOps::create_window(
            self.as_ref(),
            depth,
            wid,
            parent,
            x,
            y,
            width,
            height,
            class,
            visual,
            aux,
        )
    }
    fn create_gc(&self, cid: Gcontext, drawable: Drawable, aux: &CreateGCAux) -> Result<(), DErr> {
        BarOps::create_gc(self.as_ref(), cid, drawable, aux)
    }
    fn change_gc(&self, gc: Gcontext, aux: &ChangeGCAux) -> Result<(), DErr> {
        BarOps::change_gc(self.as_ref(), gc, aux)
    }
    fn poly_fill_rectangle(
        &self,
        drawable: Drawable,
        gc: Gcontext,
        rectangles: &[Rectangle],
    ) -> Result<(), DErr> {
        BarOps::poly_fill_rectangle(self.as_ref(), drawable, gc, rectangles)
    }
    fn image_text8(
        &self,
        drawable: Drawable,
        gc: Gcontext,
        x: i16,
        y: i16,
        string: &[u8],
    ) -> Result<(), DErr> {
        BarOps::image_text8(self.as_ref(), drawable, gc, x, y, string)
    }
    fn map_window(&self, window: Window) -> Result<(), DErr> {
        BarOps::map_window(self.as_ref(), window)
    }
    fn query_font(&self, name: &str) -> Result<Option<Fontable>, DErr> {
        BarOps::query_font(self.as_ref(), name)
    }
    fn flush(&self) -> Result<(), DErr> {
        BarOps::flush(self.as_ref())
    }
}

/// Warns once when the `fixed` core font cannot be loaded (design D7): tags
/// then render as filled rectangles only, and the WM never aborts.
static FONT_MISS_WARNED: Once = Once::new();
fn warn_font_miss_once() {
    FONT_MISS_WARNED.call_once(|| {
        eprintln!(
            "tessera: warning: the 'fixed' X core font is unavailable; \
             bar tags render as filled rectangles"
        );
    });
}

/// Draws one workspace tag per `WmState` workspace onto the bar window.
///
/// Design D3: [`BarRenderer::draw`] consumes a snapshot; the binary owns the
/// [`StateReceiver`](tessera_core::bus::StateReceiver) and calls `draw` once
/// per recompute (D4) — never on idle event polling.
pub struct BarRenderer<B> {
    ops: B,
    win: Window,
    gc: Gcontext,
    font: Option<Fontable>,
    geom: BarGeometry,
    config: BarConfig,
}

impl<B: BarOps> BarRenderer<B> {
    /// Allocates the bar window (override-redirect, background pixmap), the
    /// single reused pixmap+GC, probes the `fixed` core font and maps the bar
    /// (design D7/D11). The window occupies `bar_geometry(monitor, bar)`.
    pub fn new(
        ops: B,
        root: Window,
        depth: u8,
        visual: Visualid,
        monitor: Rect,
        bar: &BarConfig,
    ) -> Result<Self, DErr> {
        let geom = bar_geometry(monitor, bar);
        // D7: probe the core font up front; a miss (or a failed probe) only
        // drops the text, never aborts startup.
        let font = match ops.query_font("fixed") {
            Ok(font) => font,
            Err(_) => {
                warn_font_miss_once();
                None
            }
        };
        // D11: one window, one reused pixmap + GC, allocated once at startup.
        let win = ops.generate_id()?;
        let pixmap = ops.generate_id()?;
        ops.create_pixmap(depth, pixmap, root, geom.w, geom.h)?;
        let window_aux = CreateWindowAux::default()
            .background_pixmap(pixmap)
            .border_pixel(0)
            .override_redirect(1);
        ops.create_window(
            depth,
            win,
            root,
            geom.x as i16,
            geom.y as i16,
            geom.w,
            geom.h,
            WindowClass::INPUT_OUTPUT,
            visual,
            &window_aux,
        )?;
        let gc = ops.generate_id()?;
        let bg_pixel = pixel(bar.bg_color);
        let gc_aux = CreateGCAux::default().foreground(bg_pixel).font(font);
        ops.create_gc(gc, pixmap, &gc_aux)?;
        ops.poly_fill_rectangle(
            pixmap,
            gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: geom.w,
                height: geom.h,
            }],
        )?;
        ops.map_window(win)?;
        Ok(BarRenderer {
            ops,
            win,
            gc,
            font,
            geom,
            config: bar.clone(),
        })
    }

    /// Paints the current tag strip: one slot per workspace, each glyph run
    /// padded by [`TAG_PADDING_X`] per side so adjacent tags stay visibly
    /// separated (D3). The focused tag is the highlighted one — filled with
    /// the theme accent (the same colour the focused window border uses) and
    /// its glyph drawn in the bar background; every other tag is plain bar
    /// background with foreground text. A font miss (D7) renders slots as
    /// filled rectangles without text; an invisible bar or an empty snapshot
    /// issues no X calls at all.
    pub fn draw(&self, state: &WmState) -> Result<(), DErr> {
        if !self.config.visible || state.workspaces.is_empty() {
            return Ok(());
        }
        let focused = focused_tag_index(state);
        let fg = pixel(self.config.fg_color);
        let bg = pixel(self.config.bg_color);
        // D3: the accent already marks the focused frame's border; reusing it
        // here makes "where am I" one colour across the whole WM.
        let accent = pixel(state.theme.active_border());
        let mut cursor: i16 = 0;
        for (i, ws) in state.workspaces.iter().enumerate() {
            let is_focused = focused == Some(i);
            let len = ws.name.len() as i16;
            let glyph_w = GLYPH_WIDTH * len;
            // Slot width depends ONLY on the tag name's length, so focus
            // moving never shifts any slot's width or x position.
            let slot = Rectangle {
                x: cursor,
                y: 0,
                width: (glyph_w + 2 * TAG_PADDING_X) as u16,
                height: self.geom.h,
            };
            cursor += slot.width as i16;
            // The focused tag is highlighted; every other tag is plain bar
            // background so only ONE block ever stands out.
            let (fill, glyph) = if is_focused { (accent, bg) } else { (bg, fg) };
            self.ops
                .change_gc(self.gc, &ChangeGCAux::default().foreground(fill))?;
            self.ops.poly_fill_rectangle(self.win, self.gc, &[slot])?;
            if let Some(font) = self.font {
                let text_x = slot.x + (slot.width as i16 - glyph_w) / 2;
                let text_y = (self.geom.h as i32 + GLYPH_HEIGHT as i32) / 2;
                // `image_text8` paints the glyph's bounding box with the GC's
                // BACKGROUND (unlike `poly_text8`, which touches only the
                // glyph). Leaving it unset drew every number inside a black
                // box that did not match the tag it sat on, so the background
                // must track the slot's own fill.
                self.ops.change_gc(
                    self.gc,
                    &ChangeGCAux::default()
                        .foreground(glyph)
                        .background(fill)
                        .font(font),
                )?;
                self.ops.image_text8(
                    self.win,
                    self.gc,
                    text_x,
                    text_y as i16,
                    ws.name.as_bytes(),
                )?;
            }
        }
        // The event loop's reads do not flush the shared write buffer; push
        // this recompute's requests explicitly so the server paints now.
        self.ops.flush()
    }

    /// The mapped bar window id.
    pub fn win(&self) -> Window {
        self.win
    }
}

/// The workspace index rendered as the focused tag: the workspace whose id
/// matches `state.current`, or the first workspace as a fallback (an empty
/// workspace list means no focused tag at all).
fn focused_tag_index(state: &WmState) -> Option<usize> {
    state
        .workspaces
        .iter()
        .position(|ws| ws.id == state.current)
        .or((!state.workspaces.is_empty()).then_some(0))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::sync::Arc;

    use tessera_core::{Config, LayoutKind, Theme, WmState, WorkspaceId, WorkspaceState};

    use super::*;

    const ROOT: Window = 0x0000_0010;
    const DEPTH: u8 = 24;
    const VISUAL: Visualid = 0x0000_0020;
    const FIRST_ID: u32 = 0x0000_1001;
    const WIN: Window = FIRST_ID;
    const PIXMAP: Pixmap = FIRST_ID + 1;
    const GC: Gcontext = FIRST_ID + 2;
    const FONT_ID: u32 = 0x0000_2001;
    /// `#222222` (default `bar.bg_color`) packed into the low 24 bits.
    const BG_PIXEL: u32 = 0x0022_2222;
    /// `#eeeeee` (default `bar.fg_color`) packed into the low 24 bits.
    const FG_PIXEL: u32 = 0x00EE_EEEE;
    /// `#ff8f40` (ayu_dark accent = `Theme::default().active_border()`, the
    /// focused frame border colour) packed into the low 24 bits.
    const ACCENT_PIXEL: u32 = 0x00FF_8F40;
    /// Slot width of a 3-glyph tag: `3 * GLYPH_WIDTH + 2 * TAG_PADDING_X`.
    const SLOT_3: u16 = 36;
    /// Slot width of a 5-glyph tag: `5 * GLYPH_WIDTH + 2 * TAG_PADDING_X`.
    const SLOT_5: u16 = 52;

    fn monitor() -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        }
    }

    fn config(position: BarPosition, thickness: Option<u16>) -> BarConfig {
        BarConfig {
            position,
            thickness,
            ..BarConfig::default()
        }
    }

    /// A `WmState` with `current = 1` and one workspace per name (id = i + 1),
    /// so the first tag is the focused one.
    fn state(names: &[&str]) -> WmState {
        let workspaces = names
            .iter()
            .enumerate()
            .map(|(i, name)| WorkspaceState {
                id: i as WorkspaceId + 1,
                name: name.to_string(),
                layout: LayoutKind::MasterStack,
                windows: Vec::new(),
                focus: None,
            })
            .collect();
        WmState {
            current: 1,
            focused: None,
            workspaces,
            config: Arc::new(Config::default()),
            theme: Arc::new(Theme::default()),
        }
    }

    /// Recording `BarOps` double (design D2): returns fresh sequential ids
    /// starting at [`FIRST_ID`] and records every call in order.
    #[derive(Debug, Default)]
    struct FakeBarOps {
        font: Option<u32>,
        font_err: bool,
        next_id: Cell<u32>,
        calls: RefCell<Vec<BarCall>>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum BarCall {
        GenerateId,
        QueryFont(String),
        CreatePixmap {
            depth: u8,
            pid: Pixmap,
            drawable: Drawable,
            width: u16,
            height: u16,
        },
        CreateWindow {
            wid: Window,
            parent: Window,
            x: i16,
            y: i16,
            width: u16,
            height: u16,
            class: WindowClass,
            visual: Visualid,
            background_pixmap: Option<Pixmap>,
            override_redirect: Option<u32>,
        },
        CreateGc {
            cid: Gcontext,
            drawable: Drawable,
            foreground: Option<u32>,
            font: Option<Fontable>,
        },
        ChangeGc {
            foreground: Option<u32>,
            /// Recorded because `image_text8` paints the glyph box with the
            /// GC's BACKGROUND: leaving this unrecorded is what let a black
            /// box behind every tag glyph go unnoticed by the whole suite.
            background: Option<u32>,
            font: Option<Fontable>,
        },
        PolyFillRectangle {
            drawable: Drawable,
            gc: Gcontext,
            rectangles: Vec<TestRect>,
        },
        ImageText8 {
            drawable: Drawable,
            gc: Gcontext,
            x: i16,
            y: i16,
            string: Vec<u8>,
        },
        MapWindow(Window),
        Flush,
    }

    /// Test-only copy of [`Rectangle`] (the x11rb struct derives neither
    /// `PartialEq` nor `Eq`), so call sequences stay assertable.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct TestRect {
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    }

    impl From<Rectangle> for TestRect {
        fn from(r: Rectangle) -> Self {
            TestRect {
                x: r.x,
                y: r.y,
                width: r.width,
                height: r.height,
            }
        }
    }

    impl FakeBarOps {
        fn new(font: Option<u32>) -> Self {
            FakeBarOps {
                font,
                font_err: false,
                next_id: Cell::new(FIRST_ID),
                calls: RefCell::new(Vec::new()),
            }
        }

        /// Simulates a failed font probe (the server has no `fixed` font).
        fn font_error(mut self) -> Self {
            self.font_err = true;
            self
        }

        fn calls(&self) -> Vec<BarCall> {
            self.calls.borrow().clone()
        }
    }

    impl BarOps for FakeBarOps {
        fn generate_id(&self) -> Result<u32, DErr> {
            self.calls.borrow_mut().push(BarCall::GenerateId);
            let id = self.next_id.get();
            self.next_id.set(id + 1);
            Ok(id)
        }
        fn create_pixmap(
            &self,
            depth: u8,
            pid: Pixmap,
            drawable: Drawable,
            width: u16,
            height: u16,
        ) -> Result<(), DErr> {
            self.calls.borrow_mut().push(BarCall::CreatePixmap {
                depth,
                pid,
                drawable,
                width,
                height,
            });
            Ok(())
        }
        fn create_window(
            &self,
            _depth: u8,
            wid: Window,
            parent: Window,
            x: i16,
            y: i16,
            width: u16,
            height: u16,
            class: WindowClass,
            visual: Visualid,
            aux: &CreateWindowAux,
        ) -> Result<(), DErr> {
            self.calls.borrow_mut().push(BarCall::CreateWindow {
                wid,
                parent,
                x,
                y,
                width,
                height,
                class,
                visual,
                background_pixmap: aux.background_pixmap,
                override_redirect: aux.override_redirect,
            });
            Ok(())
        }
        fn create_gc(
            &self,
            cid: Gcontext,
            drawable: Drawable,
            aux: &CreateGCAux,
        ) -> Result<(), DErr> {
            self.calls.borrow_mut().push(BarCall::CreateGc {
                cid,
                drawable,
                foreground: aux.foreground,
                font: aux.font,
            });
            Ok(())
        }
        fn change_gc(&self, gc: Gcontext, aux: &ChangeGCAux) -> Result<(), DErr> {
            self.calls.borrow_mut().push(BarCall::ChangeGc {
                foreground: aux.foreground,
                background: aux.background,
                font: aux.font,
            });
            let _ = gc;
            Ok(())
        }
        fn poly_fill_rectangle(
            &self,
            drawable: Drawable,
            gc: Gcontext,
            rectangles: &[Rectangle],
        ) -> Result<(), DErr> {
            self.calls.borrow_mut().push(BarCall::PolyFillRectangle {
                drawable,
                gc,
                rectangles: rectangles.iter().map(|r| TestRect::from(*r)).collect(),
            });
            Ok(())
        }
        fn image_text8(
            &self,
            drawable: Drawable,
            gc: Gcontext,
            x: i16,
            y: i16,
            string: &[u8],
        ) -> Result<(), DErr> {
            self.calls.borrow_mut().push(BarCall::ImageText8 {
                drawable,
                gc,
                x,
                y,
                string: string.to_vec(),
            });
            Ok(())
        }
        fn map_window(&self, window: Window) -> Result<(), DErr> {
            self.calls.borrow_mut().push(BarCall::MapWindow(window));
            Ok(())
        }
        fn query_font(&self, name: &str) -> Result<Option<Fontable>, DErr> {
            self.calls
                .borrow_mut()
                .push(BarCall::QueryFont(name.to_string()));
            if self.font_err {
                Err(DErr::X("test font error".to_string()))
            } else {
                Ok(self.font)
            }
        }
        fn flush(&self) -> Result<(), DErr> {
            self.calls.borrow_mut().push(BarCall::Flush);
            Ok(())
        }
    }

    #[test]
    fn geometry_top_uses_default_22px_thickness() {
        assert_eq!(
            bar_geometry(monitor(), &config(BarPosition::Top, None)),
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 22
            }
        );
    }

    #[test]
    fn geometry_bottom_anchors_to_the_monitor_bottom() {
        assert_eq!(
            bar_geometry(monitor(), &config(BarPosition::Bottom, Some(30))),
            Rect {
                x: 0,
                y: 1050,
                w: 1920,
                h: 30
            }
        );
    }

    #[test]
    fn geometry_left_uses_default_6px_thickness() {
        assert_eq!(
            bar_geometry(monitor(), &config(BarPosition::Left, None)),
            Rect {
                x: 0,
                y: 0,
                w: 6,
                h: 1080
            }
        );
    }

    #[test]
    fn geometry_right_anchors_to_the_monitor_right() {
        assert_eq!(
            bar_geometry(monitor(), &config(BarPosition::Right, Some(12))),
            Rect {
                x: 1908,
                y: 0,
                w: 12,
                h: 1080
            }
        );
    }

    #[test]
    fn geometry_honors_a_non_zero_monitor_origin() {
        let mon = Rect {
            x: 100,
            y: 50,
            w: 800,
            h: 600,
        };
        assert_eq!(
            bar_geometry(mon, &config(BarPosition::Top, None)),
            Rect {
                x: 100,
                y: 50,
                w: 800,
                h: 22
            }
        );
    }

    #[test]
    fn tiling_area_top_removes_the_bar_strip() {
        assert_eq!(
            tiling_area(monitor(), &config(BarPosition::Top, None)),
            Rect {
                x: 0,
                y: 22,
                w: 1920,
                h: 1058
            }
        );
    }

    #[test]
    fn tiling_area_right_removes_the_right_bar() {
        assert_eq!(
            tiling_area(monitor(), &config(BarPosition::Right, Some(12))),
            Rect {
                x: 0,
                y: 0,
                w: 1908,
                h: 1080
            }
        );
    }

    #[test]
    fn new_allocates_window_pixmap_gc_and_maps_a_visible_bar() {
        let fake = FakeBarOps::new(Some(FONT_ID));
        let renderer = BarRenderer::new(
            fake,
            ROOT,
            DEPTH,
            VISUAL,
            monitor(),
            &config(BarPosition::Top, None),
        )
        .unwrap();
        assert_eq!(renderer.win(), WIN);
        assert_eq!(
            renderer.ops.calls(),
            vec![
                BarCall::QueryFont("fixed".to_string()),
                BarCall::GenerateId,
                BarCall::GenerateId,
                BarCall::CreatePixmap {
                    depth: DEPTH,
                    pid: PIXMAP,
                    drawable: ROOT,
                    width: 1920,
                    height: 22,
                },
                BarCall::CreateWindow {
                    wid: WIN,
                    parent: ROOT,
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 22,
                    class: WindowClass::INPUT_OUTPUT,
                    visual: VISUAL,
                    background_pixmap: Some(PIXMAP),
                    override_redirect: Some(1),
                },
                BarCall::GenerateId,
                BarCall::CreateGc {
                    cid: GC,
                    drawable: PIXMAP,
                    foreground: Some(BG_PIXEL),
                    font: Some(FONT_ID),
                },
                BarCall::PolyFillRectangle {
                    drawable: PIXMAP,
                    gc: GC,
                    rectangles: vec![TestRect {
                        x: 0,
                        y: 0,
                        width: 1920,
                        height: 22,
                    }],
                },
                BarCall::MapWindow(WIN),
            ]
        );
    }

    #[test]
    fn new_without_font_keeps_a_fontless_gc() {
        let renderer = BarRenderer::new(
            FakeBarOps::new(None),
            ROOT,
            DEPTH,
            VISUAL,
            monitor(),
            &config(BarPosition::Top, None),
        )
        .unwrap();
        assert!(
            renderer
                .ops
                .calls()
                .iter()
                .any(|c| matches!(c, BarCall::CreateGc { font: None, .. })),
            "a missing font must leave the GC without a font"
        );
    }

    #[test]
    fn new_survives_a_font_probe_error() {
        // Design D7: a failed font probe never aborts startup.
        let renderer = BarRenderer::new(
            FakeBarOps::new(None).font_error(),
            ROOT,
            DEPTH,
            VISUAL,
            monitor(),
            &config(BarPosition::Top, None),
        )
        .unwrap();
        assert!(
            renderer
                .ops
                .calls()
                .iter()
                .any(|c| matches!(c, BarCall::CreateGc { font: None, .. })),
            "a font probe error must still build a fontless GC"
        );
    }

    #[test]
    fn draw_highlights_the_focused_tag_and_leaves_the_others_on_the_bar_background() {
        // D3 legibility: exactly ONE tag stands out — the focused one, filled
        // with the theme accent and lettered in the bar background. Every
        // other tag is plain bar background with foreground text (before this
        // change the fill/glyph pair was inverted, so every workspace the
        // user was NOT on rendered as a glaring solid block).
        let renderer = BarRenderer::new(
            FakeBarOps::new(Some(FONT_ID)),
            ROOT,
            DEPTH,
            VISUAL,
            monitor(),
            &config(BarPosition::Top, None),
        )
        .unwrap();
        let before = renderer.ops.calls().len();
        let state = state(&["one", "two", "three"]);
        renderer.draw(&state).unwrap();
        assert_eq!(
            &renderer.ops.calls()[before..],
            &vec![
                // focused tag "one": accent fill, bar-background glyphs
                BarCall::ChangeGc {
                    foreground: Some(ACCENT_PIXEL),
                    background: None,
                    font: None
                },
                BarCall::PolyFillRectangle {
                    drawable: WIN,
                    gc: GC,
                    rectangles: vec![TestRect {
                        x: 0,
                        y: 0,
                        width: SLOT_3,
                        height: 22
                    }],
                },
                BarCall::ChangeGc {
                    foreground: Some(BG_PIXEL),
                    background: Some(ACCENT_PIXEL),
                    font: Some(FONT_ID)
                },
                BarCall::ImageText8 {
                    drawable: WIN,
                    gc: GC,
                    x: 6,
                    y: 17,
                    string: b"one".to_vec()
                },
                // unfocused tag "two": bar background, foreground glyphs
                BarCall::ChangeGc {
                    foreground: Some(BG_PIXEL),
                    background: None,
                    font: None
                },
                BarCall::PolyFillRectangle {
                    drawable: WIN,
                    gc: GC,
                    rectangles: vec![TestRect {
                        x: 36,
                        y: 0,
                        width: SLOT_3,
                        height: 22
                    }],
                },
                BarCall::ChangeGc {
                    foreground: Some(FG_PIXEL),
                    background: Some(BG_PIXEL),
                    font: Some(FONT_ID)
                },
                BarCall::ImageText8 {
                    drawable: WIN,
                    gc: GC,
                    x: 42,
                    y: 17,
                    string: b"two".to_vec()
                },
                // unfocused tag "three": bar background, foreground glyphs
                BarCall::ChangeGc {
                    foreground: Some(BG_PIXEL),
                    background: None,
                    font: None
                },
                BarCall::PolyFillRectangle {
                    drawable: WIN,
                    gc: GC,
                    rectangles: vec![TestRect {
                        x: 72,
                        y: 0,
                        width: SLOT_5,
                        height: 22
                    }],
                },
                BarCall::ChangeGc {
                    foreground: Some(FG_PIXEL),
                    background: Some(BG_PIXEL),
                    font: Some(FONT_ID)
                },
                BarCall::ImageText8 {
                    drawable: WIN,
                    gc: GC,
                    x: 78,
                    y: 17,
                    string: b"three".to_vec(),
                },
                BarCall::Flush,
            ]
        );
    }

    #[test]
    fn draw_without_font_renders_rects_only() {
        let renderer = BarRenderer::new(
            FakeBarOps::new(None),
            ROOT,
            DEPTH,
            VISUAL,
            monitor(),
            &config(BarPosition::Top, None),
        )
        .unwrap();
        let before = renderer.ops.calls().len();
        let state = state(&["one", "two"]);
        renderer.draw(&state).unwrap();
        assert_eq!(
            &renderer.ops.calls()[before..],
            &vec![
                BarCall::ChangeGc {
                    foreground: Some(ACCENT_PIXEL),
                    background: None,
                    font: None
                },
                BarCall::PolyFillRectangle {
                    drawable: WIN,
                    gc: GC,
                    rectangles: vec![TestRect {
                        x: 0,
                        y: 0,
                        width: SLOT_3,
                        height: 22
                    }],
                },
                BarCall::ChangeGc {
                    foreground: Some(BG_PIXEL),
                    background: None,
                    font: None
                },
                BarCall::PolyFillRectangle {
                    drawable: WIN,
                    gc: GC,
                    rectangles: vec![TestRect {
                        x: 36,
                        y: 0,
                        width: SLOT_3,
                        height: 22
                    }],
                },
                BarCall::Flush,
            ]
        );
        assert!(
            !renderer
                .ops
                .calls()
                .iter()
                .any(|c| matches!(c, BarCall::ImageText8 { .. })),
            "a font miss must never emit text calls"
        );
    }

    #[test]
    fn draw_skips_all_draw_calls_when_the_bar_is_invisible() {
        let invisible = BarConfig {
            visible: false,
            ..config(BarPosition::Top, None)
        };
        let renderer = BarRenderer::new(
            FakeBarOps::new(Some(FONT_ID)),
            ROOT,
            DEPTH,
            VISUAL,
            monitor(),
            &invisible,
        )
        .unwrap();
        let before = renderer.ops.calls().len();
        let state = state(&["one", "two"]);
        renderer.draw(&state).unwrap();
        assert_eq!(
            renderer.ops.calls().len(),
            before,
            "an invisible bar must issue no draw calls"
        );
    }

    #[test]
    fn draw_with_no_workspaces_issues_no_draw_calls() {
        let renderer = BarRenderer::new(
            FakeBarOps::new(Some(FONT_ID)),
            ROOT,
            DEPTH,
            VISUAL,
            monitor(),
            &config(BarPosition::Top, None),
        )
        .unwrap();
        let before = renderer.ops.calls().len();
        let state = state(&[]);
        renderer.draw(&state).unwrap();
        assert_eq!(
            renderer.ops.calls().len(),
            before,
            "an empty snapshot must issue no draw calls"
        );
    }

    #[test]
    fn draw_focuses_the_first_tag_when_current_matches_no_workspace() {
        let renderer = BarRenderer::new(
            FakeBarOps::new(Some(FONT_ID)),
            ROOT,
            DEPTH,
            VISUAL,
            monitor(),
            &config(BarPosition::Top, None),
        )
        .unwrap();
        let before = renderer.ops.calls().len();
        let mut state = state(&["one", "two"]);
        state.current = 42;
        renderer.draw(&state).unwrap();
        assert_eq!(
            &renderer.ops.calls()[before..],
            &vec![
                BarCall::ChangeGc {
                    foreground: Some(ACCENT_PIXEL),
                    background: None,
                    font: None
                },
                BarCall::PolyFillRectangle {
                    drawable: WIN,
                    gc: GC,
                    rectangles: vec![TestRect {
                        x: 0,
                        y: 0,
                        width: SLOT_3,
                        height: 22
                    }],
                },
                BarCall::ChangeGc {
                    foreground: Some(BG_PIXEL),
                    background: Some(ACCENT_PIXEL),
                    font: Some(FONT_ID)
                },
                BarCall::ImageText8 {
                    drawable: WIN,
                    gc: GC,
                    x: 6,
                    y: 17,
                    string: b"one".to_vec()
                },
                BarCall::ChangeGc {
                    foreground: Some(BG_PIXEL),
                    background: None,
                    font: None
                },
                BarCall::PolyFillRectangle {
                    drawable: WIN,
                    gc: GC,
                    rectangles: vec![TestRect {
                        x: 36,
                        y: 0,
                        width: SLOT_3,
                        height: 22
                    }],
                },
                BarCall::ChangeGc {
                    foreground: Some(FG_PIXEL),
                    background: Some(BG_PIXEL),
                    font: Some(FONT_ID)
                },
                BarCall::ImageText8 {
                    drawable: WIN,
                    gc: GC,
                    x: 42,
                    y: 17,
                    string: b"two".to_vec()
                },
                BarCall::Flush,
            ]
        );
    }

    /// One tag's painted geometry: its slot rectangle and the `x` of the text
    /// call that followed it. Lets the padding/centring tests assert on
    /// geometry alone, without restating the whole call sequence.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct TagBox {
        slot: TestRect,
        text_x: Option<i16>,
    }

    /// Every tag `draw` painted in `calls`, in strip order.
    fn tag_boxes(calls: &[BarCall]) -> Vec<TagBox> {
        let mut out: Vec<TagBox> = Vec::new();
        for call in calls {
            match call {
                BarCall::PolyFillRectangle { rectangles, .. } if rectangles.len() == 1 => {
                    out.push(TagBox {
                        slot: rectangles[0],
                        text_x: None,
                    });
                }
                BarCall::ImageText8 { x, .. } => {
                    if let Some(last) = out.last_mut() {
                        last.text_x = Some(*x);
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn drawn_tag_boxes(renderer: &BarRenderer<FakeBarOps>, state: &WmState) -> Vec<TagBox> {
        let before = renderer.ops.calls().len();
        renderer.draw(state).unwrap();
        tag_boxes(&renderer.ops.calls()[before..])
    }

    fn top_bar_renderer() -> BarRenderer<FakeBarOps> {
        BarRenderer::new(
            FakeBarOps::new(Some(FONT_ID)),
            ROOT,
            DEPTH,
            VISUAL,
            monitor(),
            &config(BarPosition::Top, None),
        )
        .unwrap()
    }

    #[test]
    fn draw_pads_every_tag_so_adjacent_tags_stay_separated() {
        // D3 legibility: a slot is `name.len() * GLYPH_WIDTH + 2 *
        // TAG_PADDING_X` wide and the next one starts exactly one slot width
        // later, so consecutive tags never butt against each other. Before
        // this change a one-character tag owned an 8px slot and its
        // neighbour started at pixel 9. The glyphs sit centred inside their
        // slot (one padding in from each edge).
        let renderer = top_bar_renderer();
        let boxes = drawn_tag_boxes(&renderer, &state(&["1", "22", "333"]));

        let widths: Vec<u16> = boxes.iter().map(|b| b.slot.width).collect();
        let expected_widths: Vec<u16> = ["1", "22", "333"]
            .iter()
            .map(|name| name.len() as u16 * GLYPH_WIDTH as u16 + 2 * TAG_PADDING_X as u16)
            .collect();
        assert_eq!(
            widths, expected_widths,
            "slot width must be the glyph run plus one padding per side"
        );
        // Consecutive slots start exactly one slot width apart: no overlap,
        // no gap, and every tag carries its own visible breathing room.
        let mut expected_x: i16 = 0;
        for b in &boxes {
            assert_eq!(b.slot.x, expected_x, "slots must tile the strip end to end");
            // The centring expression is real work now: the glyph run is
            // inset by exactly one padding from the slot's left edge.
            let glyph_w = b.slot.width as i16 - 2 * TAG_PADDING_X;
            assert_eq!(
                b.text_x,
                Some(b.slot.x + (b.slot.width as i16 - glyph_w) / 2),
                "glyphs must be horizontally centred inside their slot"
            );
            assert_eq!(b.text_x, Some(b.slot.x + TAG_PADDING_X));
            expected_x += b.slot.width as i16;
        }
    }

    #[test]
    fn moving_focus_never_shifts_a_slot_width_or_position() {
        // The removed `FOCUSED_EXTRA_PADDING` made the focused tag one pixel
        // per glyph wider, so the whole strip slid sideways every time focus
        // moved. Slot geometry now depends ONLY on the tag names: the two
        // draws below differ in `current` alone and must agree exactly.
        let renderer = top_bar_renderer();
        let mut first = state(&["one", "two", "three"]);
        first.current = 1;
        let mut second = state(&["one", "two", "three"]);
        second.current = 3;

        let with_first_focused: Vec<TestRect> = drawn_tag_boxes(&renderer, &first)
            .into_iter()
            .map(|b| b.slot)
            .collect();
        let with_third_focused: Vec<TestRect> = drawn_tag_boxes(&renderer, &second)
            .into_iter()
            .map(|b| b.slot)
            .collect();

        assert_eq!(
            with_first_focused, with_third_focused,
            "moving focus must not change any slot's width or x position"
        );
    }

    #[test]
    fn draw_paints_the_single_startup_workspace_as_the_focused_tag() {
        // Startup legibility: `App::new` now opens workspace 1, so the very
        // first snapshot the bar receives carries exactly one workspace and
        // `current` matching it — it must render as the highlighted tag
        // (accent fill, bar-background glyph), not as an unfocused block.
        let renderer = top_bar_renderer();
        let before = renderer.ops.calls().len();
        renderer.draw(&state(&["1"])).unwrap();
        assert_eq!(
            &renderer.ops.calls()[before..],
            &vec![
                BarCall::ChangeGc {
                    foreground: Some(ACCENT_PIXEL),
                    background: None,
                    font: None
                },
                BarCall::PolyFillRectangle {
                    drawable: WIN,
                    gc: GC,
                    rectangles: vec![TestRect {
                        x: 0,
                        y: 0,
                        width: 20, // 1 glyph + 6px padding per side
                        height: 22
                    }],
                },
                BarCall::ChangeGc {
                    foreground: Some(BG_PIXEL),
                    background: Some(ACCENT_PIXEL),
                    font: Some(FONT_ID)
                },
                BarCall::ImageText8 {
                    drawable: WIN,
                    gc: GC,
                    x: 6,
                    y: 17,
                    string: b"1".to_vec()
                },
                BarCall::Flush,
            ]
        );
    }
}
