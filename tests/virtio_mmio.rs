// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::*;

type TestResult = Result<(), Error>;
fn device() -> Result<Device, Error> {
    Device::new(VERSION_1 | (1 << 29), 0x4000_0000, 0x20_0000)
}
fn negotiate(device: &mut Device) -> TestResult {
    device.write(0x70, 4, 1)?;
    device.write(0x70, 4, 3)?;
    device.write(0x24, 4, 1)?;
    device.write(0x20, 4, 1)?;
    device.write(0x70, 4, 11)?;
    Ok(())
}
fn queues(device: &mut Device, alias: bool) -> TestResult {
    for index in 0..QUEUES as u64 {
        let base = 0x4000_0000 + if alias { 0 } else { index * 0x10000 };
        device.write(0x30, 4, index)?;
        device.write(0x38, 4, 128)?;
        device.write(0x80, 4, base)?;
        device.write(0x90, 4, base + 0x1000)?;
        device.write(0xa0, 4, base + 0x2000)?;
        device.write(0x44, 4, 1)?;
    }
    Ok(())
}
fn begin(device: &mut Device, status: u64) -> Result<Transaction, Error> {
    device.write(0x70, 4, status)?.ok_or(Error::InvalidStatus)
}

#[test]
fn linux_initialization_defers_driver_ok_until_backend_activation() -> TestResult {
    let mut device = device()?;
    assert_eq!(device.read(0, 4), Ok(0x7472_6976));
    assert_eq!(device.read(8, 4), Ok(8));
    let reset = begin(&mut device, 0)?;
    device.complete(reset.id, true)?;
    device.write(0x14, 4, 0)?;
    assert_eq!(device.read(0x10, 4), Ok(0)); // EVENT_IDX is not advertised.
    device.write(0x14, 4, 1)?;
    assert_eq!(device.read(0x10, 4), Ok(1));
    negotiate(&mut device)?;
    queues(&mut device, false)?;
    let activate = begin(&mut device, 15)?;
    assert!(matches!(
        activate.operation,
        BackendOperation::Activate {
            features: VERSION_1,
            ..
        }
    ));
    assert_eq!(device.read(0x70, 4), Ok(11));
    assert_eq!(device.write(0x70, 4, 0), Err(Error::Busy));
    assert_eq!(device.complete(reset.id, true), Err(Error::StaleCompletion));
    device.complete(activate.id, true)?;
    assert_eq!(device.read(0x70, 4), Ok(15));
    assert_eq!(
        device.complete(activate.id, true),
        Err(Error::StaleCompletion)
    );
    Ok(())
}

#[test]
fn failed_reset_cannot_release_queue_state_or_reuse_transaction_ids() -> TestResult {
    let mut device = device()?;
    negotiate(&mut device)?;
    queues(&mut device, false)?;
    let activate = begin(&mut device, 15)?;
    device.complete(activate.id, true)?;
    let reset = begin(&mut device, 0)?;
    assert_eq!(
        device.complete(reset.id, false),
        Err(Error::BackendResetFailed)
    );
    assert_eq!(device.read(0x44, 4), Ok(1));
    assert_eq!(device.read(0x70, 4), Ok(15));
    assert_eq!(device.write(0x80, 4, 0), Err(Error::Busy));
    device.complete(reset.id, true)?;
    assert_eq!(device.read(0x44, 4), Ok(0));
    assert_eq!(device.read(0x70, 4), Ok(0));
    let next_reset = begin(&mut device, 0)?;
    assert_ne!(next_reset.id, reset.id);
    assert_eq!(device.complete(reset.id, true), Err(Error::StaleCompletion));
    Ok(())
}

#[test]
fn invalid_features_queues_and_overlaps_do_not_activate_backend() -> TestResult {
    let mut device = device()?;
    device.write(0x70, 4, 3)?;
    device.write(0x20, 4, 1)?; // Unsupported SCSI feature.
    device.write(0x70, 4, 11)?;
    assert_eq!(device.read(0x70, 4), Ok(3));
    assert_eq!(device.write(0x70, 4, 15), Err(Error::InvalidStatus));
    device.write(0x20, 4, 0)?;
    device.write(0x24, 4, 1)?;
    device.write(0x20, 4, 1)?;
    device.write(0x70, 4, 11)?;
    assert_eq!(device.write(0x44, 4, 1), Err(Error::InvalidQueue));
    assert_eq!(device.read(0x44, 4), Ok(0));
    queues(&mut device, true)?;
    assert_eq!(device.write(0x70, 4, 15), Err(Error::InvalidQueue));
    assert_eq!(device.read(0x70, 4), Ok(11));
    Ok(())
}

#[test]
fn split_ring_extent_checks_include_end_fields_and_alignment() -> TestResult {
    let device = device()?;
    let mut queue = Queue {
        size: 128,
        descriptor: 0x4000_0000,
        available: 0x4000_1000,
        used: 0x4000_2000,
        ready: false,
    };
    device.validate_queue(queue)?;
    queue.used = device.memory_end - (4 + 8 * 128);
    assert_eq!(device.validate_queue(queue), Err(Error::InvalidQueue));
    queue.used = 0x4000_2001;
    assert_eq!(device.validate_queue(queue), Err(Error::InvalidQueue));
    queue.used = 0x4000_2000;
    queue.descriptor = u64::MAX - 15;
    assert_eq!(device.validate_queue(queue), Err(Error::InvalidQueue));
    Ok(())
}

#[test]
fn failure_and_hot_registers_cannot_fake_a_working_backend() -> TestResult {
    let mut device = device()?;
    negotiate(&mut device)?;
    queues(&mut device, false)?;
    let activate = begin(&mut device, 15)?;
    device.complete(activate.id, false)?;
    assert_eq!(device.read(0x70, 4), Ok(11 | 64));
    for offset in [0x50, 0x60, 0x64] {
        assert_eq!(device.read(offset, 4), Err(Error::InvalidAccess));
        assert_eq!(device.write(offset, 4, 1), Err(Error::InvalidAccess));
    }
    assert_eq!(device.read(0x11e, 2), Ok(255));
    assert_eq!(device.read(0x120, 4), Ok(16383));
    assert_eq!(device.write(0x114, 4, 128), Err(Error::InvalidAccess));
    assert_eq!(device.read(u64::MAX, 4), Err(Error::InvalidAccess));
    Ok(())
}

#[test]
fn queue_stop_waits_for_inflight_io_and_linux_cleanup_after_reset_is_allowed() -> TestResult {
    let mut device = device()?;
    negotiate(&mut device)?;
    queues(&mut device, false)?;
    let activate = begin(&mut device, 15)?;
    device.complete(activate.id, true)?;
    let stop = device.write(0x44, 4, 0)?.ok_or(Error::InvalidQueue)?;
    assert!(matches!(
        stop.operation,
        BackendOperation::StopQueue { queue } if queue == (QUEUES - 1) as u32
    ));
    assert_eq!(device.read(0x44, 4), Ok(1));
    assert_eq!(
        device.complete(stop.id, false),
        Err(Error::BackendResetFailed)
    );
    assert_eq!(device.read(0x44, 4), Ok(1));
    device.complete(stop.id, true)?;
    assert_eq!(device.read(0x44, 4), Ok(0));
    assert_eq!(device.read(0x70, 4), Ok(15 | 64));
    let reset = begin(&mut device, 0)?;
    device.complete(reset.id, true)?;
    // Linux resets the device, then writes QueueReady=0 for each old queue.
    for index in 0..QUEUES as u64 {
        device.write(0x30, 4, index)?;
        assert_eq!(device.write(0x44, 4, 0), Ok(None));
        assert_eq!(device.read(0x44, 4), Ok(0));
    }
    Ok(())
}

#[test]
fn backend_loss_preserves_pending_reset_and_owned_queue_state() -> TestResult {
    let mut device = device()?;
    negotiate(&mut device)?;
    queues(&mut device, false)?;
    let activate = begin(&mut device, 15)?;
    device.complete(activate.id, true)?;
    let reset = begin(&mut device, 0)?;
    device.backend_lost();
    assert_eq!(device.read(0x70, 4), Ok(15 | 64));
    assert_eq!(device.read(0x44, 4), Ok(1));
    assert_eq!(
        device.complete(reset.id, true),
        Err(Error::BackendResetFailed)
    );
    assert_eq!(device.write(0x70, 4, 0), Err(Error::BackendResetFailed));
    assert_eq!(device.pending.map(|value| value.transaction), Some(reset));
    device.backend_lost();
    assert_eq!(device.read(0x70, 4), Ok(15 | 64));
    Ok(())
}

#[test]
fn driver_can_use_one_request_queue_or_all_four() -> TestResult {
    let mut device = device()?;
    assert_eq!(device.read(0x100, 4), Ok(REQUEST_QUEUES as u64));
    negotiate(&mut device)?;
    queues(&mut device, false)?;
    for index in 3..QUEUES as u64 {
        device.write(0x30, 4, index)?;
        device.write(0x44, 4, 0)?;
    }
    let activation = begin(&mut device, 15)?;
    match activation.operation {
        BackendOperation::Activate { queues, .. } => {
            assert!(queues[..3].iter().all(|queue| queue.ready));
            assert!(queues[3..].iter().all(|queue| !queue.ready));
        }
        _ => panic!("activation required"),
    }
    device.complete(activation.id, true)?;
    Ok(())
}
