// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device, slot and endpoint contexts (xHCI 1.1 §6.2).
//!
//! A context is a block of `context_size` bytes (32 or 64, `HCCPARAMS1.CSZ`)
//! holding 8 or 16 dwords. Rather than mirror the whole structure as a `repr(C)`
//! type — which would have to be duplicated for both sizes — the builders below
//! write the few dwords enumeration actually uses, named after the fields they
//! carry.

/// Context index of the slot context inside a device/input context.
pub const SLOT_CONTEXT: usize = 0;
/// Context index of endpoint 0 (the control endpoint) — xHCI numbers endpoint
/// contexts from 1, so the device context entry for EP0 is index 1.
pub const EP0_CONTEXT: usize = 1;

/// Slot context dword 0: route string, speed and the number of contexts.
pub fn slot_dword0(speed_id: u8, context_entries: u32) -> u32 {
    slot_dword0_routed(0, speed_id, context_entries, false, false)
}

/// Slot context dword 0 with a route string and the multi-TT flag.
///
/// A device behind a hub is addressed by the **port path**, not by a root port:
/// bits 19:0 carry the route string (one nibble per tier, the port number of
/// each hub along the way). `multi_tt` (bit 25) tells the controller the parent
/// hub has a transaction translator per port; it is only meaningful for a
/// full/low-speed device behind a high-speed hub.
pub fn slot_dword0_routed(
    route: u32,
    speed_id: u8,
    context_entries: u32,
    multi_tt: bool,
    hub: bool,
) -> u32 {
    (route & 0x000f_ffff)
        | ((speed_id as u32) << 20)
        | (u32::from(multi_tt) << 25)
        // "Hub" (bit 26): the controller routes transactions down this slot's
        // port paths only for a slot it knows is a hub, which is why devices
        // behind a hub fail Address Device with a Transaction Error when the
        // parent's slot context never carried this bit.
        | (u32::from(hub) << 26)
        | (context_entries << 27)
}

/// Slot context dword 1: root hub port number is 1-based in xHCI.
pub fn slot_dword1(root_port_1based: u8) -> u32 {
    (root_port_1based as u32) << 16
}

/// Slot context dword 2: the transaction translator of the hub the device is
/// behind (xHCI 1.1 Table 6-4).
///
/// `ttt` is the hub's TT think time, `tt_port` the hub port the device sits on
/// and `hub_slot` the device slot id of that hub. The controller uses these to
/// emit SPLIT transactions for full/low-speed devices behind a high-speed hub;
/// without them such a device is addressed but never answers.
pub fn slot_dword2_tt(ttt: u8, tt_port: u8, hub_slot: u8) -> u32 {
    // Layout per the reference (`xhci_setup_addressable_virt_dev`):
    //   tt_info = hub_slot_id | (udev->ttport << 8)
    // i.e. the hub's slot id in bits 7:0, the hub port in bits 15:8, and the
    // think time in bits 17:16. Getting the two swapped addresses a translator
    // that does not exist, and every transaction to the device times out.
    (u32::from(hub_slot) & 0xff)
        | ((u32::from(tt_port) & 0xff) << 8)
        | ((u32::from(ttt) & 0x3) << 16)
}

/// Endpoint context dword 1: CErr, endpoint type and max packet size.
///
/// `ep_type` is 4 (control) for EP0, 2 (bulk OUT), 6 (bulk IN), 3 (interrupt
/// OUT) or 7 (interrupt IN) — the values xHCI assigns, not the USB `bmAttributes`
/// encoding.
pub fn endpoint_dword1(ep_type: u32, max_packet_size: u16) -> u32 {
    let cerr = 3 << 1;
    (cerr) | (ep_type << 3) | ((max_packet_size as u32) << 16)
}

/// Endpoint context dword 2: transfer ring dequeue pointer, low half.
///
/// The low 4 bits are reserved and the DCS (dequeue cycle state) bit tells the
/// controller which cycle bit the next TRB on that ring carries.
pub fn endpoint_dword2(ring_addr: u64, dcs: bool) -> u32 {
    (ring_addr as u32 & !0xf) | u32::from(dcs)
}

/// Endpoint context dword 4: average TRB length, in bytes.
pub fn endpoint_dword4(average_trb_len: u32) -> u32 {
    average_trb_len & 0xffff
}

/// Input control context: which contexts this command adds (`add`) and which it
/// drops (`drop`), one bit per context index.
pub fn input_control_dword1(add_mask: u32) -> u32 {
    add_mask
}
