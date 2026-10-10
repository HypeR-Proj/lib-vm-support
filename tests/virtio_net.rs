// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::virtio_mmio::{BackendOperation, Device, Error, VERSION_1};

fn device() -> Result<Device, Error> {
    with_features(u64::MAX)
}

fn with_features(features: u64) -> Result<Device, Error> {
    Device::network(
        features,
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
    assert_eq!(device.read(0x10, 4)?, MAC_FEATURE | TX_OFFLOADS);
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
fn network_offloads_follow_backend_capabilities_and_checksum_dependency() -> Result<(), Error> {
    for (backend, offered) in [
        (0, 0),
        (CSUM, CSUM),
        (HOST_TSO4 | HOST_TSO6, 0),
        (CSUM | HOST_TSO4, CSUM | HOST_TSO4),
        (CSUM | HOST_TSO6, CSUM | HOST_TSO6),
        (u64::MAX, TX_OFFLOADS),
    ] {
        let device = with_features(VERSION_1 | backend)?;
        assert_eq!(device.read(0x10, 4)?, MAC_FEATURE | offered);
    }
    Ok(())
}

fn negotiate(device: &mut Device, features: u64) -> Result<(), Error> {
    device.write(0x70, 4, 1)?;
    device.write(0x70, 4, 3)?;
    device.write(0x24, 4, 0)?;
    device.write(0x20, 4, features as u32 as u64)?;
    device.write(0x24, 4, 1)?;
    device.write(0x20, 4, features >> 32)?;
    device.write(0x70, 4, 11)?;
    Ok(())
}

#[test]
fn network_rejects_unoffered_or_incomplete_offload_negotiation() -> Result<(), Error> {
    for (backend, requested) in [
        (u64::MAX, HOST_TSO4),
        (u64::MAX, HOST_TSO6),
        (u64::MAX, TX_OFFLOADS | (1 << 1)), // RX checksum is not implemented.
        (VERSION_1, CSUM),                  // An older backend has no offloads.
        (VERSION_1 | CSUM, CSUM | HOST_TSO4),
    ] {
        let mut device = with_features(backend)?;
        negotiate(&mut device, VERSION_1 | requested)?;
        assert_eq!(device.read(0x70, 4)?, 3);
        assert!(device.write(0x70, 4, 15).is_err());
    }
    Ok(())
}

#[test]
fn network_activation_requires_rx_and_tx_and_reset_is_acknowledged() -> Result<(), Error> {
    for features in [VERSION_1, VERSION_1 | MAC_FEATURE | TX_OFFLOADS] {
        activation_and_reset(features)?;
    }
    Ok(())
}

fn ready_queues(device: &mut Device) -> Result<(), Error> {
    for queue in 0..2 {
        device.write(0x30, 4, queue)?;
        device.write(0x38, 4, 8)?;
        let base = 0x4000_0000 + queue * 4096;
        device.write(0x80, 4, base)?;
        device.write(0x90, 4, base + 256)?;
        device.write(0xa0, 4, base + 512)?;
        device.write(0x44, 4, 1)?;
    }
    Ok(())
}

fn activation_and_reset(features: u64) -> Result<(), Error> {
    let mut device = device()?;
    negotiate(&mut device, features)?;
    assert!(device.write(0x70, 4, 15).is_err());
    ready_queues(&mut device)?;
    let activation = device.write(0x70, 4, 15)?.ok_or(Error::InvalidStatus)?;
    let BackendOperation::Activate {
        queues,
        features: forwarded,
    } = activation.operation
    else {
        return Err(Error::InvalidStatus);
    };
    assert_eq!(forwarded, features);
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
    // A fresh driver may decline offloads after reset; no negotiated state
    // from the previous activation may leak into its transaction.
    device.write(0x70, 4, 1)?;
    device.write(0x70, 4, 3)?;
    // Leave the low feature bank untouched so this also proves reset cleared
    // the old CSUM/TSO bits, rather than a new driver write clearing them.
    device.write(0x24, 4, 1)?;
    device.write(0x20, 4, 1)?;
    device.write(0x70, 4, 11)?;
    assert_eq!(device.read(0x70, 4)?, 11);
    device.write(0x14, 4, 0)?;
    assert_eq!(device.read(0x10, 4)?, MAC_FEATURE | TX_OFFLOADS);
    ready_queues(&mut device)?;
    let activation = device.write(0x70, 4, 15)?.ok_or(Error::InvalidStatus)?;
    assert!(matches!(
        activation.operation,
        BackendOperation::Activate {
            features: VERSION_1,
            ..
        }
    ));
    device.complete(activation.id, true)?;
    Ok(())
}
