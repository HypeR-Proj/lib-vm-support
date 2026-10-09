// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::virtio_mmio::{BackendOperation, Device, Error, VERSION_1};

fn device() -> Result<Device, Error> {
    Device::network(
        u64::MAX,
        0x4000_0000,
        0x20_0000,
        Configuration {
            mac: [2, 0x48, 0x59, 0, 0, 1],
            mtu: MTU,
        },
    )
}

#[test]
fn network_advertises_only_implemented_features_and_two_queues() -> Result<(), Error> {
    let mut device = device()?;
    assert_eq!(device.read(8, 4)?, 1);
    assert_eq!(device.read(0x10, 4)?, MAC_FEATURE);
    device.write(0x14, 4, 1)?;
    assert_eq!(device.read(0x10, 4)?, VERSION_1 >> 32);
    for index in 0..4 {
        device.write(0x30, 4, index)?;
        assert_eq!(device.read(0x34, 4)?, if index < 2 { 128 } else { 0 });
    }
    assert_eq!(device.read(0x100, 4)?, 0x0059_4802);
    assert_eq!(device.read(0x104, 2)?, 0x0100);
    assert!(device.read(0x106, 2).is_err());
    assert!(device.write(0x114, 4, 96).is_err());
    Ok(())
}

#[test]
fn network_activation_requires_rx_and_tx_and_reset_is_acknowledged() -> Result<(), Error> {
    let mut device = device()?;
    device.write(0x70, 4, 1)?;
    device.write(0x70, 4, 3)?;
    device.write(0x24, 4, 1)?;
    device.write(0x20, 4, 1)?;
    device.write(0x70, 4, 11)?;
    assert!(device.write(0x70, 4, 15).is_err());
    for queue in 0..2 {
        device.write(0x30, 4, queue)?;
        device.write(0x38, 4, 8)?;
        let base = 0x4000_0000 + queue * 4096;
        device.write(0x80, 4, base)?;
        device.write(0x90, 4, base + 256)?;
        device.write(0xa0, 4, base + 512)?;
        device.write(0x44, 4, 1)?;
    }
    let activation = device.write(0x70, 4, 15)?.ok_or(Error::InvalidStatus)?;
    let BackendOperation::Activate { queues, .. } = activation.operation else {
        return Err(Error::InvalidStatus);
    };
    assert!(queues[..2].iter().all(|queue| queue.ready));
    assert!(queues[2..].iter().all(|queue| !queue.ready));
    assert_eq!(device.read(0x70, 4)?, 11);
    device.complete(activation.id, true)?;
    let reset = device.write(0x70, 4, 0)?.ok_or(Error::InvalidStatus)?;
    assert_eq!(device.read(0x70, 4)?, 15);
    assert_eq!(
        device.complete(reset.id, false),
        Err(Error::BackendResetFailed)
    );
    assert_eq!(device.read(0x70, 4)?, 15);
    device.complete(reset.id, true)?;
    assert_eq!(device.read(0x70, 4)?, 0);
    Ok(())
}
