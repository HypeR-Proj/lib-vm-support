// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn empty_slot_identifies_standard_root_port_without_a_downstream_function() {
    let port = RootPort::default();
    assert_eq!(port.read(0x0a, 2), Some(0x0604));
    assert_eq!(port.read(0x0e, 1), Some(1));
    assert_eq!(port.read(0x42, 2), Some(0x0142));
    assert_eq!(port.read(0x54, 4), Some(0x000c0060));
    assert!(!port.routes_endpoint(1, 0, 0));
    assert_eq!(decode_ecam(ECAM_BASE + (1 << 20)), Some((1, 0, 0, 0)));
    assert_eq!(decode_ecam(ECAM_BASE + ECAM_BYTES), None);
    assert_eq!(absent(4), Some(0xffff_ffff));
}
#[test]
fn masked_presence_event_survives_until_enabled_and_w1c_acknowledged() {
    let mut port = RootPort::default();
    port.set_present(true);
    assert_eq!(port.read(0x5a, 2), Some(0x148));
    assert!(!port.irq_asserted());
    assert_eq!(port.write(0x58, 2, 0x1028), Some(()));
    assert!(port.irq_asserted());
    assert_eq!(port.read(6, 2), Some(0x18));
    assert_eq!(port.write(0x5a, 1, 8), Some(()));
    assert!(port.irq_asserted());
    assert_eq!(port.write(0x5b, 1, 1), Some(()));
    assert!(!port.irq_asserted());
    assert_eq!(port.read(0x5a, 2), Some(0x40));
    port.set_present(false);
    assert!(port.irq_asserted());
    assert_eq!(port.write(4, 2, 0x400), Some(()));
    assert!(!port.irq_asserted());
    assert_eq!(port.read(6, 2), Some(0x18));
    assert_eq!(port.write(4, 2, 0), Some(()));
    assert!(port.irq_asserted());
}
#[test]
fn bridge_memory_enable_bus_numbers_and_link_reset_gate_endpoint_access() {
    let mut port = RootPort::default();
    port.set_present(true);
    assert!(port.routes_endpoint(1, 0, 0));
    assert!(!port.routes_endpoint(1, 1, 0));
    assert!(!port.forwards_memory(MEMORY_BASE, 4));
    assert!(!port.bus_master_enabled());
    assert_eq!(port.write(4, 2, 2), Some(()));
    assert!(port.forwards_memory(MEMORY_BASE, 4));
    assert!(!port.bus_master_enabled());
    assert_eq!(port.write(4, 2, 6), Some(()));
    assert!(port.bus_master_enabled());
    assert!(port.forwards_memory(MEMORY_BASE, 4));
    assert!(!port.forwards_memory(MEMORY_BASE + MEMORY_BYTES, 4));
    assert_eq!(port.write(0x19, 1, 0), Some(()));
    assert!(!port.routes_endpoint(1, 0, 0));
    assert_eq!(port.write(0x19, 1, 1), Some(()));
    assert_eq!(port.write(0x3e, 2, 0x40), Some(()));
    assert!(!port.routes_endpoint(1, 0, 0));
    assert!(!port.forwards_memory(MEMORY_BASE, 4));
    assert_eq!(port.write(0x3e, 2, 0), Some(()));
    assert!(port.routes_endpoint(1, 0, 0));
    assert_eq!(port.write(0x50, 2, 0x10), Some(()));
    assert!(!port.routes_endpoint(1, 0, 0));
}
#[test]
fn transfer_preserves_configuration_and_rejects_invalid_state() -> Result<(), &'static str> {
    let mut port = RootPort::default();
    for (offset, width, value) in [
        (4, 2, 6),
        (0x18, 4, 0x00010100),
        (0x20, 4, 0x0ab00aa0),
        (0x58, 2, 0x1028),
    ] {
        assert_eq!(port.write(offset, width, value), Some(()));
    }
    let restored = RootPort::from_state(&port.state()).ok_or("valid state rejected")?;
    assert_eq!(restored, port);
    for offset in (0..4096).step_by(4) {
        assert_eq!(restored.read(offset, 4), port.read(offset, 4));
    }
    for length in 0..STATE_BYTES {
        assert!(RootPort::from_state(&port.state()[..length]).is_none());
    }
    let mut invalid = port.state();
    invalid[31] = 1;
    assert!(RootPort::from_state(&invalid).is_none());
    assert_eq!(port.read(1, 2), None);
    assert_eq!(port.read(0xfff, 4), None);
    assert_eq!(port.write(0x18, 1, 256), None);
    Ok(())
}
