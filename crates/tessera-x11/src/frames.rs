#[cfg(test)]
mod tests {
    //! RED (T17): frame creation, reparenting, mapping and configure must
    //! record the exact X calls described by SC-x11-07/09 on a scripted seam.

    use std::cell::{Cell, RefCell};

    use tessera_core::{DErr, FrameId, Rect, WindowId};
    use x11rb::protocol::xproto::{CreateWindowAux, EventMask, Visualid, Window, WindowClass};

    use super::*;

    const ROOT: Window = 0x0000_0010;
    const CLIENT: WindowId = 42;
    const DEPTH: u8 = 24;
    const VISUAL: Visualid = 0x20;
    /// First id `generate_id` hands out (sequential from here).
    const FIRST_FRAME: Window = 0x0000_1001;

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
        Focus(Window),
        Destroy(Window),
    }

    /// Scripted `FrameOps`: records every call and hands out sequential window
    /// ids for the frame.
    struct FakeFrameOps {
        calls: RefCell<Vec<FrameCall>>,
        next_id: Cell<Window>,
    }

    impl FakeFrameOps {
        fn new() -> Self {
            FakeFrameOps {
                calls: RefCell::new(Vec::new()),
                next_id: Cell::new(FIRST_FRAME),
            }
        }

        fn calls(&self) -> Vec<FrameCall> {
            self.calls.borrow().clone()
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
        let frame = create_frame(&fake, ROOT, CLIENT, 2, DEPTH, VISUAL).unwrap();
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
                    border_pixel: FRAME_BORDER_PIXEL,
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
        create_frame(&fake, ROOT, CLIENT, 4, DEPTH, VISUAL).unwrap();
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
                border_pixel: FRAME_BORDER_PIXEL,
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
}
