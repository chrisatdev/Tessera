//! EWMH root-property sync (T18, REQ-ws-003/005, SC-ws-05).
//!
//! `_NET_NUMBER_OF_DESKTOPS` / `_NET_CURRENT_DESKTOP` are 32-bit CARDINAL
//! properties on the root window; `_NET_DESKTOP_NAMES` is a UTF8_STRING
//! property holding the workspace names concatenated, each NUL-terminated.
//! Every X side effect goes through the [`EwmhOps`] seam so the sync is
//! scriptable headless; [`RustConnection`] implements it directly.

use tessera_core::DErr;
use x11rb::protocol::xproto::{Atom, AtomEnum, PropMode, Window, intern_atom};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt;

use crate::display_server::{map_conn_error, map_reply_error};

/// The EWMH surface [`set_desktops`] needs, abstracted so the property sync
/// is scriptable headless (same seam shape as
/// [`X11Startup`](crate::display_server::X11Startup)).
pub(crate) trait EwmhOps {
    /// Interns a named atom and returns its id.
    fn intern(&self, name: &str) -> Result<Atom, DErr>;
    /// Writes a format-32 property (CARDINAL desktop counts).
    fn change_property32(
        &self,
        window: Window,
        property: Atom,
        type_: Atom,
        data: &[u32],
    ) -> Result<(), DErr>;
    /// Writes a format-8 property (UTF8_STRING desktop names).
    fn change_property8(
        &self,
        window: Window,
        property: Atom,
        type_: Atom,
        data: &[u8],
    ) -> Result<(), DErr>;
}

impl EwmhOps for RustConnection {
    fn intern(&self, name: &str) -> Result<Atom, DErr> {
        let cookie = intern_atom(self, false, name.as_bytes()).map_err(map_conn_error)?;
        cookie
            .reply()
            .map(|reply| reply.atom)
            .map_err(map_reply_error)
    }
    fn change_property32(
        &self,
        window: Window,
        property: Atom,
        type_: Atom,
        data: &[u32],
    ) -> Result<(), DErr> {
        let cookie = <Self as ConnectionExt>::change_property32(
            self,
            PropMode::REPLACE,
            window,
            property,
            type_,
            data,
        )
        .map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn change_property8(
        &self,
        window: Window,
        property: Atom,
        type_: Atom,
        data: &[u8],
    ) -> Result<(), DErr> {
        let cookie = <Self as ConnectionExt>::change_property8(
            self,
            PropMode::REPLACE,
            window,
            property,
            type_,
            data,
        )
        .map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
}

/// Syncs the three EWMH desktop root properties (REQ-ws-003 / SC-ws-05):
/// the desktop count and the current desktop as CARDINAL u32s, and the names
/// as one UTF8_STRING of NUL-terminated names. `CARDINAL` is a standard atom
/// (no intern round trip); the `_NET_*` names and `UTF8_STRING` are interned
/// first in a fixed order.
pub(crate) fn set_desktops(
    ops: &impl EwmhOps,
    root: Window,
    n: u32,
    cur: u32,
    names: &[String],
) -> Result<(), DErr> {
    let net_number = ops.intern("_NET_NUMBER_OF_DESKTOPS")?;
    let net_current = ops.intern("_NET_CURRENT_DESKTOP")?;
    let net_names = ops.intern("_NET_DESKTOP_NAMES")?;
    let utf8_string = ops.intern("UTF8_STRING")?;
    let cardinal = u32::from(AtomEnum::CARDINAL);

    ops.change_property32(root, net_number, cardinal, &[n])?;
    ops.change_property32(root, net_current, cardinal, &[cur])?;
    let mut names_bytes = Vec::with_capacity(names.iter().map(|s| s.len() + 1).sum());
    for name in names {
        names_bytes.extend_from_slice(name.as_bytes());
        names_bytes.push(0);
    }
    ops.change_property8(root, net_names, utf8_string, &names_bytes)?;
    Ok(())
}

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
