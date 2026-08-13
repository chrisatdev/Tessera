//! Border-only frame windows: reparenting clients into a 2px-bordered frame
//! and driving that frame (REQ-x11-005/006, SC-x11-07/09).
//!
//! v1 frames carry no title bar — the frame is the border, and its only job is
//! to give the WM a stable parent window to map/unmap/configure so a client's
//! own geometry never leaks into the tiling. The frame is also the future
//! seam for title bars (design "frame = seam for title bars").
//!
//! Every X side effect goes through the [`FrameOps`] seam so the mechanics are
//! scriptable headless; [`RustConnection`] implements it directly.

use std::collections::HashMap;

use tessera_core::{Color, DErr, FrameId, Rect, WindowId};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConfigureWindowAux, CreateWindowAux, EventMask, InputFocus,
    Visualid, Window, WindowClass, change_window_attributes, configure_window, create_window,
    destroy_window, map_window, reparent_window, set_input_focus, unmap_window,
};
use x11rb::rust_connection::RustConnection;

use crate::display_server::{map_conn_error, map_reply_error};

/// Background of the frame window behind the client (black; only visible in
/// the gap before the first configure).
pub(crate) const FRAME_BACKGROUND_PIXEL: u32 = 0;

/// The X11 `PointerRoot` pseudo-window id (`#define PointerRoot ((Window)1)`
/// in `X.h`) — not a real window, but the value X expects as the `focus`
/// argument of `SetInputFocus` when `revert_to` is `InputFocus::POINTER_ROOT`
/// (D3 fallback: focus reverts to whichever window the pointer is over,
/// keeping input focus set even when no client can be focused directly).
const POINTER_ROOT: Window = 1;

/// Packs a theme [`Color`] into the 24-bit `border_pixel` X expects
/// (D2): on TrueColor roots the low 24 bits are R/G/B as `0xRRGGBB`, and
/// bits ≥ 24 are ignored. The conversion lives at the X boundary so
/// `tessera-core` never depends on X (pure-core rule).
pub(crate) fn pixel(c: Color) -> u32 {
    (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b)
}

/// The X surface frame management needs, abstracted so frame mechanics are
/// scriptable headless (same seam shape as [`X11Startup`](crate::display_server::X11Startup)).
pub(crate) trait FrameOps {
    /// Allocates a fresh X id for the frame window.
    fn generate_id(&self) -> Result<Window, DErr>;
    /// Creates the frame window.
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
        border_width: u16,
        class: WindowClass,
        visual: Visualid,
        aux: &CreateWindowAux,
    ) -> Result<(), DErr>;
    /// Reparents `window` into `parent` at `x`,`y`.
    fn reparent(&self, window: Window, parent: Window, x: i16, y: i16) -> Result<(), DErr>;
    /// Maps `window`.
    fn map_window(&self, window: Window) -> Result<(), DErr>;
    /// Unmaps `window`.
    fn unmap_window(&self, window: Window) -> Result<(), DErr>;
    /// Resizes/moves `window` to `x`,`y`,`width`,`height` with `border_width`.
    fn configure(
        &self,
        window: Window,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        border_width: u32,
    ) -> Result<(), DErr>;
    /// Repaints `window`'s border with `border_pixel` (theme active/inactive
    /// repaint on focus change, REQ-x11-005 modified / SC-x11-13).
    fn set_border_pixel(&self, window: Window, border_pixel: u32) -> Result<(), DErr>;
    /// Sets input focus to `window`.
    fn focus(&self, window: Window) -> Result<(), DErr>;
    /// Sets X input focus to `PointerRoot` (D3 fallback): a genuinely new X
    /// request, kept as its own verb rather than reusing `focus` with the
    /// `PointerRoot` magic window id, so the fallback is directly assertable
    /// in a test double's call log.
    fn focus_pointer_root(&self) -> Result<(), DErr>;
    /// Destroys `window`.
    fn destroy(&self, window: Window) -> Result<(), DErr>;
}

impl FrameOps for RustConnection {
    fn generate_id(&self) -> Result<Window, DErr> {
        Connection::generate_id(self)
            .map_err(|err| DErr::X(format!("cannot allocate an X window id: {err}")))
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
        border_width: u16,
        class: WindowClass,
        visual: Visualid,
        aux: &CreateWindowAux,
    ) -> Result<(), DErr> {
        let cookie = create_window(
            self,
            depth,
            wid,
            parent,
            x,
            y,
            width,
            height,
            border_width,
            class,
            visual,
            aux,
        )
        .map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn reparent(&self, window: Window, parent: Window, x: i16, y: i16) -> Result<(), DErr> {
        let cookie = reparent_window(self, window, parent, x, y).map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn map_window(&self, window: Window) -> Result<(), DErr> {
        let cookie = map_window(self, window).map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn unmap_window(&self, window: Window) -> Result<(), DErr> {
        let cookie = unmap_window(self, window).map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn configure(
        &self,
        window: Window,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        border_width: u32,
    ) -> Result<(), DErr> {
        let aux = ConfigureWindowAux::default()
            .x(x)
            .y(y)
            .width(width)
            .height(height)
            .border_width(border_width);
        let cookie = configure_window(self, window, &aux).map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn set_border_pixel(&self, window: Window, border_pixel: u32) -> Result<(), DErr> {
        let aux = ChangeWindowAttributesAux::default().border_pixel(border_pixel);
        let cookie = change_window_attributes(self, window, &aux).map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn focus(&self, window: Window) -> Result<(), DErr> {
        let cookie = set_input_focus(self, InputFocus::PARENT, window, x11rb::CURRENT_TIME)
            .map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn focus_pointer_root(&self) -> Result<(), DErr> {
        let cookie = set_input_focus(
            self,
            InputFocus::POINTER_ROOT,
            POINTER_ROOT,
            x11rb::CURRENT_TIME,
        )
        .map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn destroy(&self, window: Window) -> Result<(), DErr> {
        let cookie = destroy_window(self, window).map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
}

/// Creates the border-only frame for `client` and reparents the client into it
/// (REQ-x11-005, SC-x11-07), returning the frame id.
///
/// The frame is created at the origin with a minimal size — the very next
/// layout pass configures it to its placement, and v1 has no use for the
/// client's requested geometry before then. The frame selects
/// SubstructureNotify (the client is no longer a child of root once
/// reparented, so unmap/destroy tracking must move to the frame) and is
/// override-redirect so another WM's SubstructureRedirect never manages it.
pub(crate) fn create_frame(
    ops: &impl FrameOps,
    root: Window,
    client: WindowId,
    border: u16,
    depth: u8,
    visual: Visualid,
    border_pixel: u32,
) -> Result<FrameId, DErr> {
    let frame = ops.generate_id()?;
    let aux = CreateWindowAux::default()
        .background_pixel(FRAME_BACKGROUND_PIXEL)
        .border_pixel(border_pixel)
        .event_mask(EventMask::SUBSTRUCTURE_NOTIFY)
        .override_redirect(1u32);
    ops.create_window(
        depth,
        frame,
        root,
        0,
        0,
        1,
        1,
        border,
        WindowClass::INPUT_OUTPUT,
        visual,
        &aux,
    )?;
    let offset = i16::try_from(border).unwrap_or(i16::MAX);
    ops.reparent(client, frame, offset, offset)?;
    Ok(FrameId(frame))
}

/// Repaints `frame`'s border with `border_pixel` (the X half of a theme
/// focus repaint, SC-x11-13). The frame is found by the display layer; this
/// only issues the `ChangeWindowAttributes` that repaints it.
pub(crate) fn set_border_pixel(
    ops: &impl FrameOps,
    frame: Window,
    border_pixel: u32,
) -> Result<(), DErr> {
    ops.set_border_pixel(frame, border_pixel)
}

/// Repaints frame borders on a focus change (REQ-x11-005 modified,
/// SC-x11-13): the previously focused client's frame (when it differs from
/// the newly focused one and still has a managed frame) is repainted with the
/// inactive pixel, and the newly focused client's frame with the active
/// pixel. Clients without a managed frame are skipped — a focus target can
/// arrive before `manage()` in a race, and repainting nothing is safe.
pub(crate) fn repaint_focus(
    ops: &impl FrameOps,
    frames: &HashMap<WindowId, FrameId>,
    previous: Option<WindowId>,
    next: WindowId,
    active_pixel: u32,
    inactive_pixel: u32,
) -> Result<(), DErr> {
    if let Some(frame) = previous
        .filter(|&prev| prev != next)
        .and_then(|prev| frames.get(&prev))
    {
        set_border_pixel(ops, frame.0, inactive_pixel)?;
    }
    if let Some(frame) = frames.get(&next) {
        set_border_pixel(ops, frame.0, active_pixel)?;
    }
    Ok(())
}

/// Maps the frame and then the client (REQ-x11-005 "frame mapped"): the
/// client may only become visible through its frame.
pub(crate) fn map_frame(ops: &impl FrameOps, frame: Window, client: WindowId) -> Result<(), DErr> {
    ops.map_window(frame)?;
    ops.map_window(client)
}

/// Unmaps the frame (and with it the client) — one call, SC-ws-06.
pub(crate) fn unmap_frame(ops: &impl FrameOps, frame: Window) -> Result<(), DErr> {
    ops.unmap_window(frame)
}

/// Places `frame` at the layout placement `r` and `client` at the frame
/// interior (REQ-x11-006, SC-x11-09): the client is inset by `border` on
/// every side and keeps no border of its own. A frame smaller than twice the
/// border clamps the client to zero size instead of underflowing.
pub(crate) fn configure_frame(
    ops: &impl FrameOps,
    frame: Window,
    client: WindowId,
    r: Rect,
    border: u16,
) -> Result<(), DErr> {
    let b = i32::from(border);
    let inner_w = (i32::from(r.w) - 2 * b).max(0) as u32;
    let inner_h = (i32::from(r.h) - 2 * b).max(0) as u32;
    ops.configure(
        frame,
        r.x,
        r.y,
        i32::from(r.w) as u32,
        i32::from(r.h) as u32,
        i32::from(border) as u32,
    )?;
    ops.configure(client, b, b, inner_w, inner_h, 0)
}

/// Sets input focus to `client` (the core calls `focus_window` with the
/// client id). Focus reverts to the client's parent (the frame) when the
/// client is destroyed or unmapped.
pub(crate) fn focus_client(ops: &impl FrameOps, client: WindowId) -> Result<(), DErr> {
    ops.focus(client)
}

/// Destroys the frame window, whose client already died (the display-layer
/// reaction to `WindowUnmapped`, REQ-x11-007).
pub(crate) fn destroy_frame(ops: &impl FrameOps, frame: Window) -> Result<(), DErr> {
    ops.destroy(frame)
}

/// Owns the whole focus-change order (design D2): `X11Display::focus_window`
/// shrinks to a connection precondition plus a delegate to this pure
/// function. `focused` is committed to `next` *before* anything fallible
/// runs, so no later failure can ever strand it at a destroyed client again
/// (REQ x11-focus-lifecycle). Only a total focus failure — the client focus
/// call AND the `PointerRoot` fallback both failing — returns `Err`; a
/// repaint failure is logged and never blocks.
pub(crate) fn apply_focus(
    ops: &impl FrameOps,
    frames: &HashMap<WindowId, FrameId>,
    focused: &mut Option<WindowId>,
    next: WindowId,
    active_pixel: u32,
    inactive_pixel: u32,
) -> Result<(), DErr> {
    let previous = *focused;
    // Commit intent before anything fallible (D2 step 2): `focused` records
    // what the WM meant to focus, not confirmed X state. If the focus call
    // below fails, the next pass repaints `next` inactive — harmless —
    // whereas keeping the old value would re-create the exact stale-pointer
    // bug this change fixes.
    *focused = Some(next);
    let focus_result = focus_client(ops, next).or_else(|err| {
        eprintln!("tessera: {err}");
        ops.focus_pointer_root()
    });
    if let Err(err) = repaint_focus(ops, frames, previous, next, active_pixel, inactive_pixel) {
        eprintln!("tessera: {err}");
    }
    focus_result
}

#[cfg(test)]
mod tests {
    //! RED (T17): frame creation, reparenting, mapping and configure must
    //! record the exact X calls described by SC-x11-07/09 on a scripted seam.

    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    use tessera_core::{Color, DErr, FrameId, Rect, WindowId};
    use x11rb::protocol::xproto::{CreateWindowAux, EventMask, Visualid, Window, WindowClass};

    use super::*;

    const ROOT: Window = 0x0000_0010;
    const CLIENT: WindowId = 42;
    const DEPTH: u8 = 24;
    const VISUAL: Visualid = 0x20;
    /// First id `generate_id` hands out (sequential from here).
    const FIRST_FRAME: Window = 0x0000_1001;
    /// ayu_dark `accent` (#FF8F40) as a 24-bit X pixel (SC-thm-09).
    const ACTIVE_PIXEL: u32 = 0x00FF_8F40;
    /// ayu_dark `comment` (#626A73) as a 24-bit X pixel (SC-thm-09).
    const INACTIVE_PIXEL: u32 = 0x0062_6A73;

    /// `EventMask::SUBSTRUCTURE_NOTIFY` as `u32` (the `From` impl is not
    /// const, so this is a helper rather than a constant).
    fn substructure_notify() -> u32 {
        u32::from(EventMask::SUBSTRUCTURE_NOTIFY)
    }

    /// One recorded X call, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum FrameCall {
        GenerateId,
        Create {
            depth: u8,
            wid: Window,
            parent: Window,
            border_width: u16,
            class: u16,
            visual: Visualid,
            event_mask: u32,
            override_redirect: bool,
            border_pixel: u32,
        },
        Reparent {
            window: Window,
            parent: Window,
            x: i16,
            y: i16,
        },
        Map(Window),
        Unmap(Window),
        Configure {
            window: Window,
            x: i32,
            y: i32,
            width: u32,
            height: u32,
            border_width: u32,
        },
        SetBorderPixel {
            window: Window,
            border_pixel: u32,
        },
        Focus(Window),
        FocusPointerRoot,
        Destroy(Window),
    }

    /// Scripted `FrameOps`: records every call and hands out sequential window
    /// ids for the frame. `fail_focus`/`fail_border` (D6) script a failure for
    /// `focus`/`set_border_pixel` respectively — the call is still recorded
    /// first, matching the record-then-check ordering the display-layer test
    /// double (`MockDisplay`, D5) already uses.
    struct FakeFrameOps {
        calls: RefCell<Vec<FrameCall>>,
        next_id: Cell<Window>,
        fail_focus: Cell<bool>,
        fail_border: Cell<bool>,
    }

    impl FakeFrameOps {
        fn new() -> Self {
            FakeFrameOps {
                calls: RefCell::new(Vec::new()),
                next_id: Cell::new(FIRST_FRAME),
                fail_focus: Cell::new(false),
                fail_border: Cell::new(false),
            }
        }

        fn calls(&self) -> Vec<FrameCall> {
            self.calls.borrow().clone()
        }

        /// Scripts `focus` to fail (D6): the call is still recorded.
        fn fail_focus(&self) {
            self.fail_focus.set(true);
        }

        /// Scripts `set_border_pixel` to fail (D6): the call is still
        /// recorded.
        fn fail_border(&self) {
            self.fail_border.set(true);
        }
    }

    impl FrameOps for FakeFrameOps {
        fn generate_id(&self) -> Result<Window, DErr> {
            self.calls.borrow_mut().push(FrameCall::GenerateId);
            let id = self.next_id.get();
            self.next_id.set(id + 1);
            Ok(id)
        }
        fn create_window(
            &self,
            depth: u8,
            wid: Window,
            parent: Window,
            _x: i16,
            _y: i16,
            _width: u16,
            _height: u16,
            border_width: u16,
            class: WindowClass,
            visual: Visualid,
            aux: &CreateWindowAux,
        ) -> Result<(), DErr> {
            self.calls.borrow_mut().push(FrameCall::Create {
                depth,
                wid,
                parent,
                border_width,
                class: u16::from(class),
                visual,
                event_mask: u32::from(aux.event_mask.unwrap()),
                override_redirect: aux.override_redirect.unwrap() != 0,
                border_pixel: aux.border_pixel.unwrap(),
            });
            Ok(())
        }
        fn reparent(&self, window: Window, parent: Window, x: i16, y: i16) -> Result<(), DErr> {
            self.calls.borrow_mut().push(FrameCall::Reparent {
                window,
                parent,
                x,
                y,
            });
            Ok(())
        }
        fn map_window(&self, window: Window) -> Result<(), DErr> {
            self.calls.borrow_mut().push(FrameCall::Map(window));
            Ok(())
        }
        fn unmap_window(&self, window: Window) -> Result<(), DErr> {
            self.calls.borrow_mut().push(FrameCall::Unmap(window));
            Ok(())
        }
        fn configure(
            &self,
            window: Window,
            x: i32,
            y: i32,
            width: u32,
            height: u32,
            border_width: u32,
        ) -> Result<(), DErr> {
            self.calls.borrow_mut().push(FrameCall::Configure {
                window,
                x,
                y,
                width,
                height,
                border_width,
            });
            Ok(())
        }
        fn focus(&self, window: Window) -> Result<(), DErr> {
            self.calls.borrow_mut().push(FrameCall::Focus(window));
            if self.fail_focus.get() {
                return Err(DErr::X("scripted focus failure".to_string()));
            }
            Ok(())
        }
        fn focus_pointer_root(&self) -> Result<(), DErr> {
            self.calls.borrow_mut().push(FrameCall::FocusPointerRoot);
            Ok(())
        }
        fn set_border_pixel(&self, window: Window, border_pixel: u32) -> Result<(), DErr> {
            self.calls.borrow_mut().push(FrameCall::SetBorderPixel {
                window,
                border_pixel,
            });
            if self.fail_border.get() {
                return Err(DErr::X("scripted border repaint failure".to_string()));
            }
            Ok(())
        }
        fn destroy(&self, window: Window) -> Result<(), DErr> {
            self.calls.borrow_mut().push(FrameCall::Destroy(window));
            Ok(())
        }
    }

    #[test]
    fn create_frame_reparents_client_into_2px_bordered_frame() {
        // SC-x11-07: a new frame window is created with the configured 2px
        // border, the client is reparented into it at the border offset, and
        // the frame's id is returned. The frame also selects SubstructureNotify
        // (the client is no longer a child of root once reparented — destroy
        // tracking must move to the frame) and is override-redirect so another
        // WM's SubstructureRedirect never sees it.
        let fake = FakeFrameOps::new();
        let frame = create_frame(&fake, ROOT, CLIENT, 2, DEPTH, VISUAL, ACTIVE_PIXEL).unwrap();
        assert_eq!(frame, FrameId(FIRST_FRAME));
        assert_eq!(
            fake.calls(),
            vec![
                FrameCall::GenerateId,
                FrameCall::Create {
                    depth: DEPTH,
                    wid: FIRST_FRAME,
                    parent: ROOT,
                    border_width: 2,
                    class: u16::from(WindowClass::INPUT_OUTPUT),
                    visual: VISUAL,
                    event_mask: substructure_notify(),
                    override_redirect: true,
                    border_pixel: ACTIVE_PIXEL,
                },
                FrameCall::Reparent {
                    window: CLIENT,
                    parent: FIRST_FRAME,
                    x: 2,
                    y: 2,
                },
            ]
        );
    }

    #[test]
    fn create_frame_honors_a_configured_border_width() {
        // A non-default border (config.general.border_width) must be reflected
        // in the frame's border and in the client's offset inside it.
        let fake = FakeFrameOps::new();
        create_frame(&fake, ROOT, CLIENT, 4, DEPTH, VISUAL, ACTIVE_PIXEL).unwrap();
        let calls = fake.calls();
        assert_eq!(
            calls[1],
            FrameCall::Create {
                depth: DEPTH,
                wid: FIRST_FRAME,
                parent: ROOT,
                border_width: 4,
                class: u16::from(WindowClass::INPUT_OUTPUT),
                visual: VISUAL,
                event_mask: substructure_notify(),
                override_redirect: true,
                border_pixel: ACTIVE_PIXEL,
            }
        );
        assert_eq!(
            calls[2],
            FrameCall::Reparent {
                window: CLIENT,
                parent: FIRST_FRAME,
                x: 4,
                y: 4,
            }
        );
    }

    #[test]
    fn pixel_packs_rgb_into_low_24_bits() {
        // D2: on 24-bit TrueColor roots the border_pixel's low 24 bits are
        // R/G/B (0xRRGGBB). #FF8F40 (ayu_dark accent) must pack to
        // 0x00FF_8F40, with the high bits zero.
        assert_eq!(
            pixel(Color {
                r: 0xFF,
                g: 0x8F,
                b: 0x40
            }),
            ACTIVE_PIXEL
        );
        assert_eq!(
            pixel(Color {
                r: 0x62,
                g: 0x6A,
                b: 0x73
            }),
            INACTIVE_PIXEL
        );
    }

    #[test]
    fn active_and_inactive_border_pixels_differ() {
        // SC-thm-09: the derived ayu_dark active (#FF8F40) and inactive
        // (#626A73) pixels must be distinct so a focused frame is visibly
        // different from an unfocused one.
        assert_ne!(ACTIVE_PIXEL, INACTIVE_PIXEL);
    }

    #[test]
    fn create_frame_records_the_themed_border_pixel() {
        // REQ-x11-005 (modified): the frame's border pixel comes from the
        // theme, not a hardcoded constant — the pixel passed in must be what
        // lands in CreateWindowAux.border_pixel. A non-default theme (inactive
        // comment pixel here) proves the value flows through, not a fixed one.
        let fake = FakeFrameOps::new();
        create_frame(&fake, ROOT, CLIENT, 2, DEPTH, VISUAL, INACTIVE_PIXEL).unwrap();
        assert_eq!(
            fake.calls()[1],
            FrameCall::Create {
                depth: DEPTH,
                wid: FIRST_FRAME,
                parent: ROOT,
                border_width: 2,
                class: u16::from(WindowClass::INPUT_OUTPUT),
                visual: VISUAL,
                event_mask: substructure_notify(),
                override_redirect: true,
                border_pixel: INACTIVE_PIXEL,
            }
        );
    }

    #[test]
    fn set_border_pixel_issues_change_window_attributes() {
        // SC-x11-13: repainting a frame border is a ChangeWindowAttributes
        // carrying the new border_pixel — the exact X call the display layer
        // needs on a focus change.
        let fake = FakeFrameOps::new();
        set_border_pixel(&fake, FIRST_FRAME, INACTIVE_PIXEL).unwrap();
        assert_eq!(
            fake.calls(),
            vec![FrameCall::SetBorderPixel {
                window: FIRST_FRAME,
                border_pixel: INACTIVE_PIXEL,
            }]
        );
    }

    #[test]
    fn repaint_focus_repaints_old_frame_inactive_and_new_frame_active() {
        // SC-x11-13: focus moving from client A to client B repaints A's frame
        // with the inactive pixel and B's frame with the active pixel — both
        // exactly once.
        let fake = FakeFrameOps::new();
        let frames = HashMap::from([
            (CLIENT, FrameId(FIRST_FRAME)),
            (43u32, FrameId(0x0000_1002)),
        ]);
        repaint_focus(
            &fake,
            &frames,
            Some(CLIENT),
            43u32,
            ACTIVE_PIXEL,
            INACTIVE_PIXEL,
        )
        .unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                FrameCall::SetBorderPixel {
                    window: FIRST_FRAME,
                    border_pixel: INACTIVE_PIXEL,
                },
                FrameCall::SetBorderPixel {
                    window: 0x0000_1002,
                    border_pixel: ACTIVE_PIXEL,
                },
            ]
        );
    }

    #[test]
    fn repaint_focus_without_previous_only_repaints_the_new_frame_active() {
        // First focus (no previously focused client): only the newly focused
        // frame is repainted, active — nothing is repainted inactive.
        let fake = FakeFrameOps::new();
        let frames = HashMap::from([(CLIENT, FrameId(FIRST_FRAME))]);
        repaint_focus(&fake, &frames, None, CLIENT, ACTIVE_PIXEL, INACTIVE_PIXEL).unwrap();
        assert_eq!(
            fake.calls(),
            vec![FrameCall::SetBorderPixel {
                window: FIRST_FRAME,
                border_pixel: ACTIVE_PIXEL,
            }]
        );
    }

    #[test]
    fn repaint_focus_ignores_frames_missing_from_the_map() {
        // A focus target with no managed frame must not crash or repaint
        // anything (defensive: the map lookup is what proves membership).
        let fake = FakeFrameOps::new();
        let frames = HashMap::new();
        repaint_focus(
            &fake,
            &frames,
            Some(CLIENT),
            99u32,
            ACTIVE_PIXEL,
            INACTIVE_PIXEL,
        )
        .unwrap();
        assert_eq!(fake.calls(), Vec::new());
    }

    #[test]
    fn map_frame_maps_frame_then_client() {
        // The frame must be mapped before the client so the client is only
        // ever visible through its frame (SC-x11-07 "frame mapped").
        let fake = FakeFrameOps::new();
        map_frame(&fake, FIRST_FRAME, CLIENT).unwrap();
        assert_eq!(
            fake.calls(),
            vec![FrameCall::Map(FIRST_FRAME), FrameCall::Map(CLIENT)]
        );
    }

    #[test]
    fn unmap_frame_unmaps_only_the_frame() {
        // Hiding the frame hides the client too — one unmap is enough
        // (workspace switch, SC-ws-06).
        let fake = FakeFrameOps::new();
        unmap_frame(&fake, FIRST_FRAME).unwrap();
        assert_eq!(fake.calls(), vec![FrameCall::Unmap(FIRST_FRAME)]);
    }

    #[test]
    fn configure_frame_places_frame_and_client_interior() {
        // SC-x11-09: the frame gets the layout placement; the client is sized
        // to the frame interior, offset by the border on every side.
        let fake = FakeFrameOps::new();
        let r = Rect {
            x: 10,
            y: 20,
            w: 100,
            h: 80,
        };
        configure_frame(&fake, FIRST_FRAME, CLIENT, r, 2).unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                FrameCall::Configure {
                    window: FIRST_FRAME,
                    x: 10,
                    y: 20,
                    width: 100,
                    height: 80,
                    border_width: 2,
                },
                FrameCall::Configure {
                    window: CLIENT,
                    x: 2,
                    y: 2,
                    width: 96,
                    height: 76,
                    border_width: 0,
                },
            ]
        );
    }

    #[test]
    fn configure_frame_clamps_client_size_on_tiny_frames() {
        // A frame smaller than 2x the border must not underflow the client
        // size (u16): it clamps to zero instead.
        let fake = FakeFrameOps::new();
        let r = Rect {
            x: 0,
            y: 0,
            w: 3,
            h: 3,
        };
        configure_frame(&fake, FIRST_FRAME, CLIENT, r, 2).unwrap();
        assert_eq!(
            fake.calls()[1],
            FrameCall::Configure {
                window: CLIENT,
                x: 2,
                y: 2,
                width: 0,
                height: 0,
                border_width: 0,
            }
        );
    }

    #[test]
    fn focus_client_sets_input_focus_on_the_client() {
        // The client receives input focus, not the frame (focus_window is
        // called with the client id from the core).
        let fake = FakeFrameOps::new();
        focus_client(&fake, CLIENT).unwrap();
        assert_eq!(fake.calls(), vec![FrameCall::Focus(CLIENT)]);
    }

    #[test]
    fn destroy_frame_destroys_the_frame_window() {
        // The display-layer reaction to WindowUnmapped: the orphaned frame
        // window (its client already died) is destroyed (REQ-x11-007).
        let fake = FakeFrameOps::new();
        destroy_frame(&fake, FIRST_FRAME).unwrap();
        assert_eq!(fake.calls(), vec![FrameCall::Destroy(FIRST_FRAME)]);
    }

    #[test]
    fn apply_focus_focuses_the_client_then_repaints_borders_in_order() {
        // Happy path (D2): `focused` moves to `next`, the client is focused
        // (no PointerRoot fallback needed), and only then are the borders
        // repainted — old frame inactive, new frame active (SC-x11-13).
        let fake = FakeFrameOps::new();
        let frames = HashMap::from([
            (CLIENT, FrameId(FIRST_FRAME)),
            (43u32, FrameId(0x0000_1002)),
        ]);
        let mut focused = Some(CLIENT);
        apply_focus(
            &fake,
            &frames,
            &mut focused,
            43u32,
            ACTIVE_PIXEL,
            INACTIVE_PIXEL,
        )
        .unwrap();
        assert_eq!(focused, Some(43u32));
        assert_eq!(
            fake.calls(),
            vec![
                FrameCall::Focus(43u32),
                FrameCall::SetBorderPixel {
                    window: FIRST_FRAME,
                    border_pixel: INACTIVE_PIXEL,
                },
                FrameCall::SetBorderPixel {
                    window: 0x0000_1002,
                    border_pixel: ACTIVE_PIXEL,
                },
            ]
        );
    }

    #[test]
    fn apply_focus_commits_focused_and_focuses_the_client_when_the_repaint_fails() {
        // REQ x11-focus-lifecycle: a border repaint failure must never gate
        // the recorded focused client or the input-focus attempt — it is
        // logged and swallowed (D2 step 4).
        let fake = FakeFrameOps::new();
        fake.fail_border();
        let frames = HashMap::from([(CLIENT, FrameId(FIRST_FRAME))]);
        let mut focused = None;
        let result = apply_focus(
            &fake,
            &frames,
            &mut focused,
            CLIENT,
            ACTIVE_PIXEL,
            INACTIVE_PIXEL,
        );
        assert!(result.is_ok());
        assert_eq!(focused, Some(CLIENT));
        assert_eq!(
            fake.calls(),
            vec![
                FrameCall::Focus(CLIENT),
                FrameCall::SetBorderPixel {
                    window: FIRST_FRAME,
                    border_pixel: ACTIVE_PIXEL,
                },
            ]
        );
    }

    #[test]
    fn apply_focus_falls_back_to_pointer_root_when_the_client_focus_fails() {
        // REQ x11-focus-lifecycle: when focusing the client itself fails, X
        // input focus must never be left at `None` — the PointerRoot
        // fallback keeps it set (D3), and `focused` still records intent.
        let fake = FakeFrameOps::new();
        fake.fail_focus();
        let frames = HashMap::from([(CLIENT, FrameId(FIRST_FRAME))]);
        let mut focused = None;
        let result = apply_focus(
            &fake,
            &frames,
            &mut focused,
            CLIENT,
            ACTIVE_PIXEL,
            INACTIVE_PIXEL,
        );
        assert!(result.is_ok());
        assert_eq!(focused, Some(CLIENT));
        assert_eq!(
            fake.calls(),
            vec![
                FrameCall::Focus(CLIENT),
                FrameCall::FocusPointerRoot,
                FrameCall::SetBorderPixel {
                    window: FIRST_FRAME,
                    border_pixel: ACTIVE_PIXEL,
                },
            ]
        );
    }
}
