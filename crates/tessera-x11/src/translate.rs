//! Pure translation of raw x11rb events into the core's [`Event`] enum (D2).
//!
//! The event loop feeds every raw event through [`translate_event`]; events
//! the core does not act on yet yield `None` and are skipped. Translation is
//! deliberately connection-free (a pure function) so the full mapping is
//! testable headless.

use tessera_core::Event;

/// Translates one raw X event, or `None` when the core has no use for it yet.
pub fn translate_event(raw: &x11rb::protocol::Event) -> Option<Event> {
    let _ = raw;
    todo!("T16: raw x11rb event -> core Event mapping")
}

/// Translates a batch of raw events in arrival order, dropping the ones the
/// core ignores (REQ-x11-004 / SC-x11-06: each X event is translated and
/// published in the order it arrived).
pub fn translate_events<'a>(raw: impl IntoIterator<Item = &'a x11rb::protocol::Event>) -> Vec<Event> {
    let _ = raw;
    todo!("T16: ordered batch translation")
}

#[cfg(test)]
mod tests {
    use tessera_core::{Event, KeyCombo, Rect};
    use x11rb::protocol::xproto::{
        ButtonPressEvent, ClientMessageData, ClientMessageEvent, ConfigWindow,
        ConfigureNotifyEvent, ConfigureRequestEvent, DestroyNotifyEvent, ExposeEvent,
        KeyButMask, KeyPressEvent, MapNotifyEvent, MapRequestEvent, Mapping, MappingNotifyEvent,
        Motion, MotionNotifyEvent, Property, PropertyNotifyEvent, ReparentNotifyEvent,
        SelectionClearEvent, StackMode, UnmapNotifyEvent,
    };
    use x11rb::protocol::Event as RawEvent;

    use super::{translate_event, translate_events};

    fn map_request(window: u32) -> RawEvent {
        RawEvent::MapRequest(MapRequestEvent {
            response_type: 0,
            sequence: 0,
            parent: 0,
            window,
        })
    }

    fn configure_request(window: u32, x: i16, y: i16, width: u16, height: u16) -> RawEvent {
        RawEvent::ConfigureRequest(ConfigureRequestEvent {
            response_type: 0,
            stack_mode: StackMode::ABOVE,
            sequence: 0,
            parent: 0,
            window,
            sibling: 0,
            x,
            y,
            width,
            height,
            border_width: 0,
            value_mask: ConfigWindow::from(0u16),
        })
    }

    fn destroy_notify(window: u32) -> RawEvent {
        RawEvent::DestroyNotify(DestroyNotifyEvent {
            response_type: 0,
            sequence: 0,
            event: 0,
            window,
        })
    }

    fn unmap_notify(window: u32) -> RawEvent {
        RawEvent::UnmapNotify(UnmapNotifyEvent {
            response_type: 0,
            sequence: 0,
            event: 0,
            window,
            from_configure: false,
        })
    }

    fn key_press(detail: u8, state: u16) -> RawEvent {
        RawEvent::KeyPress(KeyPressEvent {
            response_type: 0,
            detail,
            sequence: 0,
            time: 0,
            root: 0,
            event: 0,
            child: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: KeyButMask::from(state),
            same_screen: true,
        })
    }

    #[test]
    fn translate_map_request_to_window_map_requested() {
        assert_eq!(
            translate_event(&map_request(42)),
            Some(Event::WindowMapRequested(42))
        );
    }

    #[test]
    fn translate_configure_request_to_window_configure_requested() {
        // Negative x/y prove the i16 -> i32 widening preserves the sign
        // (configure requests may move a window off-screen).
        assert_eq!(
            translate_event(&configure_request(7, -5, 10, 800, 600)),
            Some(Event::WindowConfigureRequested(
                7,
                Rect { x: -5, y: 10, w: 800, h: 600 }
            ))
        );
    }

    #[test]
    fn translate_destroy_notify_to_window_destroy_notify() {
        assert_eq!(
            translate_event(&destroy_notify(3)),
            Some(Event::WindowDestroyNotify(3))
        );
    }

    #[test]
    fn translate_unmap_notify_to_window_unmap_notify() {
        assert_eq!(
            translate_event(&unmap_notify(4)),
            Some(Event::WindowUnmapNotify(4))
        );
    }

    #[test]
    fn translate_key_press_to_key_pressed() {
        // detail is the X keyCODE, state the modifier mask; both are carried
        // as raw integers in KeyCombo. The keycode -> keysym mapping is
        // keyboard.rs (T18) — here the raw code must survive unchanged.
        assert_eq!(
            translate_event(&key_press(38, 64)),
            Some(Event::KeyPressed(KeyCombo { mods: 64, key: 38 }))
        );
    }

    #[test]
    fn translate_unrelated_events_to_none() {
        // Pointer/exposure/notify events the core never acts on must be
        // skipped without error, not surfaced as events.
        let raw = [
            RawEvent::MotionNotify(MotionNotifyEvent {
                response_type: 0,
                detail: Motion::NORMAL,
                sequence: 0,
                time: 0,
                root: 0,
                event: 0,
                child: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: KeyButMask::from(0u16),
                same_screen: true,
            }),
            RawEvent::Expose(ExposeEvent {
                response_type: 0,
                sequence: 0,
                window: 0,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                count: 0,
            }),
            RawEvent::MapNotify(MapNotifyEvent {
                response_type: 0,
                sequence: 0,
                event: 0,
                window: 0,
                override_redirect: false,
            }),
            RawEvent::ConfigureNotify(ConfigureNotifyEvent {
                response_type: 0,
                sequence: 0,
                event: 0,
                window: 0,
                above_sibling: 0,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                border_width: 0,
                override_redirect: false,
            }),
            RawEvent::ButtonPress(ButtonPressEvent {
                response_type: 0,
                detail: 1,
                sequence: 0,
                time: 0,
                root: 0,
                event: 0,
                child: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: KeyButMask::from(0u16),
                same_screen: true,
            }),
            RawEvent::ReparentNotify(ReparentNotifyEvent {
                response_type: 0,
                sequence: 0,
                event: 0,
                window: 0,
                parent: 0,
                x: 0,
                y: 0,
                override_redirect: false,
            }),
        ];
        for raw in &raw {
            assert_eq!(translate_event(raw), None, "unrelated event must be skipped");
        }
    }

    #[test]
    fn translate_deferred_events_to_none() {
        // Recognized but deferred until U4-B: property changes need a
        // connection to fetch the value (T18 EWMH titles), ClientMessage
        // covers WM_DELETE/EWMH messages (T18+), SelectionClear is the WM_S0
        // loss signal and MappingNotify the keyboard map change (T18).
        // Deliberately NOT a wildcard drop: these variants must stay visible
        // here so a future change notices them.
        let raw = [
            RawEvent::PropertyNotify(PropertyNotifyEvent {
                response_type: 0,
                sequence: 0,
                window: 5,
                atom: 0,
                time: 0,
                state: Property::NEW_VALUE,
            }),
            RawEvent::ClientMessage(ClientMessageEvent {
                response_type: 0,
                format: 32,
                sequence: 0,
                window: 5,
                type_: 0,
                data: ClientMessageData::from([0u32; 5]),
            }),
            RawEvent::SelectionClear(SelectionClearEvent {
                response_type: 0,
                sequence: 0,
                time: 0,
                owner: 0,
                selection: 0,
            }),
            RawEvent::MappingNotify(MappingNotifyEvent {
                response_type: 0,
                sequence: 0,
                request: Mapping::KEYBOARD,
                first_keycode: 0,
                count: 0,
            }),
        ];
        for raw in &raw {
            assert_eq!(translate_event(raw), None, "deferred event must be skipped in v1");
        }
    }

    #[test]
    fn translate_events_preserves_arrival_order() {
        // SC-x11-06: a mixed drain is translated in arrival order, with the
        // unrelated Expose dropped in place — the WM's per-event publish
        // order matches the X server's delivery order.
        let batch = [
            map_request(1),
            RawEvent::Expose(ExposeEvent {
                response_type: 0,
                sequence: 0,
                window: 99,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                count: 0,
            }),
            configure_request(2, 0, 0, 320, 240),
            destroy_notify(3),
            key_press(38, 64),
            unmap_notify(4),
        ];
        assert_eq!(
            translate_events(&batch),
            vec![
                Event::WindowMapRequested(1),
                Event::WindowConfigureRequested(2, Rect { x: 0, y: 0, w: 320, h: 240 }),
                Event::WindowDestroyNotify(3),
                Event::KeyPressed(KeyCombo { mods: 64, key: 38 }),
                Event::WindowUnmapNotify(4),
            ]
        );
    }
}
