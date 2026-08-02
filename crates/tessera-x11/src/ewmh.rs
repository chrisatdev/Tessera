//! EWMH desktop state sync on the root window (T18, REQ-ws-003, SC-ws-05).
//!
//! RED: tests only — the production `EwmhOps` seam, `RustConnection` impl and
//! `set_desktops` arrive with the green commit (REQ-ws-003/005).

#[cfg(test)]
mod tests {
    //! RED (T18): set_desktops must write the three _NET_* root properties
    //! with the exact atom types and payloads of SC-ws-05.

    use std::cell::RefCell;

    use tessera_core::DErr;
    use x11rb::protocol::xproto::{Atom, AtomEnum, Window};

    use super::*;

    const ROOT: Window = 0x0000_0010;
    /// Atom ids the fake assigns to the interned names (server ids are
    /// arbitrary; only the name -> id mapping must stay consistent).
    const NET_NUMBER: Atom = 0x0101;
    const NET_CURRENT: Atom = 0x0102;
    const NET_NAMES: Atom = 0x0103;
    const UTF8_STRING_ATOM: Atom = 0x0201;

    /// `AtomEnum::CARDINAL` as `Atom` (the `From` impl is not const).
    fn cardinal() -> Atom {
        u32::from(AtomEnum::CARDINAL)
    }

    /// One recorded EWMH call, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum EwmhCall {
        Intern(String),
        Prop32 {
            window: Window,
            property: Atom,
            type_: Atom,
            data: Vec<u32>,
        },
        Prop8 {
            window: Window,
            property: Atom,
            type_: Atom,
            data: Vec<u8>,
        },
    }

    /// Scripted `EwmhOps`: assigns a fixed atom id per name and records every
    /// call.
    struct FakeEwmhOps {
        calls: RefCell<Vec<EwmhCall>>,
    }

    impl FakeEwmhOps {
        fn new() -> Self {
            FakeEwmhOps {
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<EwmhCall> {
            self.calls.borrow().clone()
        }
    }

    impl EwmhOps for FakeEwmhOps {
        fn intern(&self, name: &str) -> Result<Atom, DErr> {
            self.calls
                .borrow_mut()
                .push(EwmhCall::Intern(name.to_string()));
            let atom = match name {
                "_NET_NUMBER_OF_DESKTOPS" => NET_NUMBER,
                "_NET_CURRENT_DESKTOP" => NET_CURRENT,
                "_NET_DESKTOP_NAMES" => NET_NAMES,
                "UTF8_STRING" => UTF8_STRING_ATOM,
                other => panic!("unexpected interned name: {other}"),
            };
            Ok(atom)
        }
        fn change_property32(
            &self,
            window: Window,
            property: Atom,
            type_: Atom,
            data: &[u32],
        ) -> Result<(), DErr> {
            self.calls.borrow_mut().push(EwmhCall::Prop32 {
                window,
                property,
                type_,
                data: data.to_vec(),
            });
            Ok(())
        }
        fn change_property8(
            &self,
            window: Window,
            property: Atom,
            type_: Atom,
            data: &[u8],
        ) -> Result<(), DErr> {
            self.calls.borrow_mut().push(EwmhCall::Prop8 {
                window,
                property,
                type_,
                data: data.to_vec(),
            });
            Ok(())
        }
    }

    #[test]
    fn set_desktops_writes_the_three_net_properties() {
        // SC-ws-05: n=2 / cur=1 / ["1","2"] -> the count and current-desktop
        // props are CARDINAL u32s, the names prop is the UTF8_STRING
        // concatenation of the NUL-terminated names. The four atoms are
        // interned first (fixed order), then the three props are written.
        let fake = FakeEwmhOps::new();
        set_desktops(&fake, ROOT, 2, 1, &["1".to_string(), "2".to_string()]).unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                EwmhCall::Intern("_NET_NUMBER_OF_DESKTOPS".to_string()),
                EwmhCall::Intern("_NET_CURRENT_DESKTOP".to_string()),
                EwmhCall::Intern("_NET_DESKTOP_NAMES".to_string()),
                EwmhCall::Intern("UTF8_STRING".to_string()),
                EwmhCall::Prop32 {
                    window: ROOT,
                    property: NET_NUMBER,
                    type_: cardinal(),
                    data: vec![2],
                },
                EwmhCall::Prop32 {
                    window: ROOT,
                    property: NET_CURRENT,
                    type_: cardinal(),
                    data: vec![1],
                },
                EwmhCall::Prop8 {
                    window: ROOT,
                    property: NET_NAMES,
                    type_: UTF8_STRING_ATOM,
                    data: b"1\x002\x00".to_vec(),
                },
            ]
        );
    }

    #[test]
    fn set_desktops_encodes_names_null_terminated_and_skips_padding() {
        // Triangulation: three desktops, current=0 (first), and names with
        // mixed lengths — every name keeps its own trailing NUL.
        let fake = FakeEwmhOps::new();
        set_desktops(
            &fake,
            ROOT,
            3,
            0,
            &["alpha".to_string(), "b".to_string(), "gamma".to_string()],
        )
        .unwrap();
        let calls = fake.calls();
        assert_eq!(
            calls[4],
            EwmhCall::Prop32 {
                window: ROOT,
                property: NET_NUMBER,
                type_: cardinal(),
                data: vec![3],
            }
        );
        assert_eq!(
            calls[5],
            EwmhCall::Prop32 {
                window: ROOT,
                property: NET_CURRENT,
                type_: cardinal(),
                data: vec![0],
            }
        );
        assert_eq!(
            calls[6],
            EwmhCall::Prop8 {
                window: ROOT,
                property: NET_NAMES,
                type_: UTF8_STRING_ATOM,
                data: b"alpha\0b\0gamma\0".to_vec(),
            }
        );
    }
}
