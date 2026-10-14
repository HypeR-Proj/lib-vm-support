// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::virtio::{BackendOperation, VERSION_1};

fn device() -> Result<Device, Error> {
    Ok(Device::new(
        virtio::Device::new(VERSION_1, 0x4000_0000, 0x20_0000)?,
        0x0a00_0000,
    ))
}

#[test]
fn pci_reports_backend_queue_capacity() -> Result<(), Error> {
    let mut device = Device::new(
        virtio::Device::scsi_with_queues(VERSION_1, 0x4000_0000, 0x20_0000, 1)?,
        0x0a00_0000,
    );
    device.function.write(4, 2, 6);
    assert_eq!(device.read(18, 2)?, 3);
    assert_eq!(device.read(DEVICE_OFFSET, 4)?, 1);
    device.write(22, 2, 3)?;
    assert_eq!(device.read(24, 2)?, 0);
    Ok(())
}

#[test]
fn pci_enumeration_bar_probe_and_capability_chain() -> Result<(), Error> {
    let mut device = device()?;
    let function = &mut device.function;
    assert_eq!(function.read(0, 4), Some(0x1048_1af4));
    assert_eq!(function.read(4, 2), Some(0));
    let original = function.bar();
    function.write(0x10, 4, u32::MAX.into());
    assert_eq!(function.read(0x10, 4), Some(0xffff_f000));
    assert_eq!(function.bar(), original); // Probe cannot redirect the doorbell.
    function.write(0x10, 4, 0x0a00_4000);
    assert_eq!(function.bar(), 0x0a00_4000);
    function.write(0x11, 1, 0x6f);
    assert_eq!(function.bar(), 0x0a00_6000);
    function.write(0x10, 2, 0x8fff);
    assert_eq!(function.bar(), 0x0a00_8000);
    assert_eq!(function.read(0x14, 4), Some(0));
    function.write(0x14, 4, u32::MAX.into());
    assert_eq!(function.read(0x14, 4), Some(0)); // Unimplemented BAR.
    let mut cap = function.read(0x34, 1).ok_or(Error::InvalidAccess)?;
    for (kind, offset, size) in [
        (1, 0, 56),
        (2, NOTIFY_OFFSET, 2),
        (3, ISR_OFFSET, 1),
        (4, DEVICE_OFFSET, 36),
        (5, 0, 0),
    ] {
        assert_eq!(function.read(cap, 1), Some(9));
        assert_eq!(function.read(cap + 3, 1), Some(kind));
        assert_eq!(function.read(cap + 4, 1), Some(0));
        assert_eq!(function.read(cap + 8, 4), Some(offset));
        assert_eq!(function.read(cap + 12, 4), Some(size));
        cap = function.read(cap + 1, 1).ok_or(Error::InvalidAccess)?;
    }
    assert_eq!(cap, 0);
    assert_eq!(function.read(0x60, 4), Some(0)); // notify multiplier.
    assert_eq!(function.read(4095, 2), None);
    Ok(())
}

#[test]
fn pci_linux_split_queue_initialization_and_synchronous_reset() -> Result<(), Error> {
    let mut device = device()?;
    device.function.write(4, 2, 6);
    device.write(20, 1, 1)?;
    device.write(20, 1, 3)?;
    device.write(8, 4, 1)?;
    device.write(12, 4, 1)?;
    device.write(20, 1, 11)?;
    assert_eq!(device.read(20, 1), Ok(11));
    assert_eq!(device.read(18, 2), Ok(6));
    assert_eq!(device.read(16, 2), Ok(0xffff));
    for queue in 0..3 {
        let base = 0x4000_0000 + queue * 0x10000;
        device.write(22, 2, queue)?;
        assert_eq!(device.read(24, 2), Ok(128));
        device.write(24, 2, 64)?;
        device.write(32, 8, base)?;
        device.write(40, 4, base + 0x1000)?;
        device.write(44, 4, 0)?;
        device.write(48, 8, base + 0x2000)?;
        device.write(28, 2, 1)?;
    }
    let activation = device.write(20, 1, 15)?.ok_or(Error::InvalidStatus)?;
    assert!(matches!(
        activation.operation,
        BackendOperation::Activate {
            features: VERSION_1,
            ..
        }
    ));
    assert_eq!(device.read(20, 1), Ok(11));
    assert_eq!(device.write(32, 8, 0), Err(Error::Busy));
    device.complete(activation.id, true)?;
    assert_eq!(device.read(20, 1), Ok(15));
    let reset = device.write(20, 1, 0)?.ok_or(Error::InvalidStatus)?;
    assert_eq!(
        device.complete(reset.id, false),
        Err(Error::BackendResetFailed)
    );
    assert_eq!(device.read(28, 2), Ok(1));
    device.complete(reset.id, true)?;
    assert_eq!(device.read(20, 1), Ok(0));
    assert_eq!(device.read(28, 2), Ok(0));
    // Virtio reset does not reset PCI enumeration/resource ownership.
    assert_eq!(device.function.bar(), 0x0a00_0000);
    assert_eq!(device.function.command(), 6);
    Ok(())
}

#[test]
fn pci_rejects_unoffered_features_and_configuration_widths() -> Result<(), Error> {
    let mut device = device()?;
    device.write(20, 1, 3)?;
    device.write(12, 4, 1)?;
    device.write(20, 1, 11)?;
    assert_eq!(device.read(20, 1), Ok(3));
    assert_eq!(device.write(20, 1, 15), Err(Error::InvalidStatus));
    assert_eq!(device.write(20, 4, 3), Err(Error::InvalidAccess));
    assert_eq!(device.write(24, 2, 65536), Err(Error::InvalidAccess));
    assert_eq!(device.read(NOTIFY_OFFSET, 2), Err(Error::InvalidAccess));
    assert_eq!(device.read(ISR_OFFSET, 1), Err(Error::InvalidAccess));
    Ok(())
}

#[test]
fn bus_master_clear_is_a_backend_quiescence_transaction() -> Result<(), Error> {
    let mut device = device()?;
    device.function.write(4, 2, 6);
    device.write(20, 1, 3)?;
    device.write(8, 4, 1)?;
    device.write(12, 4, 1)?;
    device.write(20, 1, 11)?;
    for index in 0..3 {
        device.write(22, 2, index)?;
        device.write(24, 2, 128)?;
        let base = 0x4000_0000 + index * 0x10000;
        device.write(32, 8, base)?;
        device.write(40, 8, base + 0x1000)?;
        device.write(48, 8, base + 0x2000)?;
        device.write(28, 2, 1)?;
    }
    let activation = device.write(20, 1, 15)?.ok_or(Error::InvalidStatus)?;
    device.complete(activation.id, true)?;
    let stop = device
        .write_configuration(4, 2, 2)?
        .ok_or(Error::InvalidStatus)?;
    assert!(matches!(stop.operation, BackendOperation::StopQueue { .. }));
    assert_eq!(device.read(20, 1), Ok(15));
    assert_eq!(
        device.complete(stop.id, false),
        Err(Error::BackendResetFailed)
    );
    assert_eq!(device.read(20, 1), Ok(15));
    device.complete(stop.id, true)?;
    assert_eq!(device.read(20, 1), Ok(15 | 64));
    Ok(())
}

#[test]
fn configuration_window_is_writable_but_cannot_escape_bar() -> Result<(), Error> {
    let mut device = device()?;
    assert_eq!(
        device.configuration_window(CONFIG_DATA, 4),
        Err(Error::InvalidAccess)
    );
    device.write_configuration(CONFIG_ACCESS + 8, 4, 4)?;
    device.write_configuration(CONFIG_ACCESS + 12, 4, 4)?;
    assert_eq!(
        device.read_configuration(CONFIG_ACCESS + 8, 4, false),
        Ok(4)
    );
    assert_eq!(
        device.configuration_window(CONFIG_DATA, 4),
        Ok(Some(ConfigurationWindow {
            offset: 4,
            width: 4
        }))
    );
    assert_eq!(device.function.command(), 0); // Does not depend on BAR decoding.
    device.write_configuration(CONFIG_ACCESS + 4, 1, 1)?;
    assert_eq!(
        device.configuration_window(CONFIG_DATA, 4),
        Err(Error::InvalidAccess)
    );
    device.write_configuration(CONFIG_ACCESS + 4, 1, 0)?;
    device.write_configuration(CONFIG_ACCESS + 8, 4, pci::BAR_SIZE)?;
    assert_eq!(
        device.configuration_window(CONFIG_DATA, 4),
        Err(Error::InvalidAccess)
    );
    device.write_configuration(CONFIG_ACCESS + 8, 4, ISR_OFFSET)?;
    device.write_configuration(CONFIG_ACCESS + 12, 4, 1)?;
    assert_eq!(
        device.configuration_window(CONFIG_DATA, 1),
        Ok(Some(ConfigurationWindow {
            offset: ISR_OFFSET,
            width: 1
        }))
    );
    assert_eq!(
        device.configuration_window(CONFIG_DATA, 2),
        Ok(Some(ConfigurationWindow {
            offset: ISR_OFFSET,
            width: 1
        }))
    );
    assert_eq!(device.read_configuration(6, 2, true), Ok(0x18));
    device.write_configuration(6, 2, 0xffff)?;
    assert_eq!(device.read_configuration(6, 2, false), Ok(0x10));
    Ok(())
}

#[test]
fn configuration_data_latches_partial_pci_accesses_at_the_selected_bar_width() -> Result<(), Error>
{
    let mut device = device()?;
    device.write_configuration(CONFIG_ACCESS + 8, 4, ISR_OFFSET)?;
    device.write_configuration(CONFIG_ACCESS + 12, 4, 1)?;
    assert_eq!(
        device.configuration_write_value(CONFIG_DATA, 4, 0xaabb_ccdd)?,
        0xdd
    );
    assert_eq!(
        device.configuration_read_result(CONFIG_DATA, 4, 3)?,
        0xaabb_cc03
    );
    assert_eq!(
        device.configuration_read_result(CONFIG_DATA + 2, 2, 0)?,
        0xaabb
    );
    device.write_configuration(CONFIG_ACCESS + 8, 4, 0)?;
    device.write_configuration(CONFIG_ACCESS + 12, 4, 4)?;
    assert_eq!(
        device.configuration_write_value(CONFIG_DATA + 1, 1, 0x55)?,
        0xaabb_5500
    );
    assert_eq!(
        device.configuration_read_result(CONFIG_DATA + 1, 1, 0x1234_5678)?,
        0x56
    );
    assert_eq!(device.configuration_read_result(CONFIG_DATA, 4, 0)?, 0);
    assert_eq!(
        device.configuration_window(CONFIG_DATA + 1, 2),
        Err(Error::InvalidAccess)
    );
    assert_eq!(
        device.configuration_write_value(CONFIG_DATA, 1, 256),
        Err(Error::InvalidAccess)
    );
    Ok(())
}
