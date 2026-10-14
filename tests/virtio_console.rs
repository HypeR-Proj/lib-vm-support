// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::virtqueue::GuestMemory;
#[path = "virtqueue_memory.rs"]
mod memory;
use memory::{BASE, Memory, SIZE};

fn show<E: std::fmt::Debug>(error: E) -> String {
    format!("{error:?}")
}

fn queues() -> [Queue; QUEUES] {
    let mut queues = [Queue::default(); QUEUES];
    for (index, queue) in queues[..4].iter_mut().enumerate() {
        *queue = Memory::queue(index);
    }
    queues
}

fn console(features: u64) -> Result<Console, String> {
    let mut console = Console::new(AGENT_PORT_NAME, BASE, SIZE).map_err(show)?;
    console.activate(features, queues()).map_err(show)?;
    console.set_open(true).map_err(show)?;
    Ok(console)
}

fn payload(queue: usize, descriptor: u16) -> u64 {
    BASE + 0x10000 + queue as u64 * 0x10000 + u64::from(descriptor) * 1024
}

fn input(memory: &Memory, sequence: u16, id: u32, event: u16, value: u16) -> Result<(), String> {
    let queue = Memory::queue(3);
    let index = sequence % queue.size as u16;
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&id.to_le_bytes());
    bytes[4..6].copy_from_slice(&event.to_le_bytes());
    bytes[6..].copy_from_slice(&value.to_le_bytes());
    memory.write(payload(3, index), &bytes).map_err(show)?;
    memory
        .descriptor(queue, index, payload(3, index), 8, 0, 0)
        .map_err(show)?;
    memory.publish(queue, sequence, index).map_err(show)
}

fn output(memory: &Memory, sequence: u16) -> Result<(), String> {
    let queue = Memory::queue(2);
    let index = sequence % queue.size as u16;
    memory
        .descriptor(queue, index, payload(2, index), 256, 2, 0)
        .map_err(show)?;
    memory.publish(queue, sequence, index).map_err(show)
}

fn handshake(console: &mut Console, memory: &Memory) -> Result<(), String> {
    input(memory, 0, u32::MAX, DEVICE_READY, 1)?;
    output(memory, 0)?;
    assert!(console.service(memory).map_err(show)?.interrupt);
    let reply = memory.bytes(payload(2, 0), 8).map_err(show)?;
    assert_eq!(&reply[4..8], &[1, 0, 1, 0]);
    input(memory, 1, 0, PORT_READY, 1)?;
    output(memory, 1)?;
    output(memory, 2)?;
    console.service(memory).map_err(show)?;
    assert!(console.named_port_ready());
    let reply = memory
        .bytes(payload(2, 1), 8 + AGENT_PORT_NAME.len())
        .map_err(show)?;
    assert_eq!(&reply[4..6], &PORT_NAME.to_le_bytes());
    assert_eq!(&reply[8..], AGENT_PORT_NAME.as_bytes());
    assert!(!console.guest_open());
    input(memory, 2, 0, PORT_OPEN, 1)?;
    console.service(memory).map_err(show)?;
    assert!(console.guest_open());
    Ok(())
}

#[test]
fn named_serial_handshake_and_bidirectional_bytes() -> Result<(), String> {
    let memory = Memory::new();
    let mut console = console(VERSION_1 | MULTIPORT)?;
    assert_eq!(console.send(b"premature"), 0);
    handshake(&mut console, &memory)?;
    assert_eq!(console.send(b"guest-ping"), 10);
    let rx = Memory::queue(0);
    memory
        .descriptor(rx, 0, payload(0, 0), 1024, 2, 0)
        .map_err(show)?;
    memory.publish(rx, 0, 0).map_err(show)?;
    let tx = Memory::queue(1);
    memory
        .descriptor(tx, 0, payload(1, 0), 4, 0, 0)
        .map_err(show)?;
    memory.write(payload(1, 0), b"pong").map_err(show)?;
    memory.publish(tx, 0, 0).map_err(show)?;
    let progress = console.service(&memory).map_err(show)?;
    assert_eq!(progress.completed, 2);
    assert_eq!(
        memory.bytes(payload(0, 0), 10).map_err(show)?,
        b"guest-ping"
    );
    let mut response = [0; 16];
    assert_eq!(console.receive(&mut response), 4);
    assert_eq!(&response[..4], b"pong");
    Ok(())
}

#[test]
fn control_backpressure_requests_repoll_after_receive_frees_capacity() -> Result<(), String> {
    let memory = Memory::new();
    let mut console = console(VERSION_1 | MULTIPORT)?;
    for sequence in 0..8 {
        input(&memory, sequence, u32::MAX, DEVICE_READY, 1)?;
    }
    console.service(&memory).map_err(show)?;
    assert_eq!(memory.used(Memory::queue(3)), Ok(7));
    for sequence in 0..7 {
        output(&memory, sequence)?;
    }
    let progress = console.service(&memory).map_err(show)?;
    assert!(progress.more);
    console.service(&memory).map_err(show)?;
    assert_eq!(memory.used(Memory::queue(3)), Ok(8));
    Ok(())
}

#[test]
fn failed_registration_discards_pending_positive_control_replies() -> Result<(), String> {
    for (id, event) in [(0, PORT_READY), (u32::MAX, DEVICE_READY)] {
        let memory = Memory::new();
        let mut console = console(VERSION_1 | MULTIPORT)?;
        input(&memory, 0, u32::MAX, DEVICE_READY, 1)?;
        input(&memory, 1, 0, PORT_READY, 1)?;
        input(&memory, 2, id, event, 0)?;
        console.service(&memory).map_err(show)?;
        assert!(!console.named_port_ready());
        assert!(!console.guest_open());
        assert!(console.take_guest_disconnect());
        assert!(!console.take_guest_disconnect());
        output(&memory, 0)?;
        console.service(&memory).map_err(show)?;
        assert_eq!(memory.used(Memory::queue(2)), Ok(0));
        input(&memory, 3, u32::MAX, DEVICE_READY, 1)?;
        console.service(&memory).map_err(show)?;
        assert_eq!(memory.used(Memory::queue(2)), Ok(1));
        let reply = memory.bytes(payload(2, 0), 8).map_err(show)?;
        assert_eq!(&reply[4..8], &[1, 0, 1, 0]);
    }
    Ok(())
}

#[test]
fn guest_close_and_reopen_in_one_turn_latches_session_retirement() -> Result<(), String> {
    let memory = Memory::new();
    let mut console = console(VERSION_1 | MULTIPORT)?;
    assert!(!console.take_guest_disconnect());
    handshake(&mut console, &memory)?;
    assert_eq!(console.send(b"previous session"), 16);
    input(&memory, 3, 0, PORT_OPEN, 0)?;
    input(&memory, 4, 0, PORT_OPEN, 1)?;
    let tx = Memory::queue(1);
    memory
        .descriptor(tx, 0, payload(1, 0), 4, 0, 0)
        .map_err(show)?;
    memory.write(payload(1, 0), b"next").map_err(show)?;
    memory.publish(tx, 0, 0).map_err(show)?;
    console.service(&memory).map_err(show)?;
    assert!(console.guest_open());
    assert!(console.to_guest.is_empty());
    assert_eq!(console.send(b"old host"), 0);
    assert_eq!(console.receive(&mut [0; 16]), 0);
    assert_eq!(memory.used(tx), Ok(0));
    assert!(console.take_guest_disconnect());
    assert!(!console.take_guest_disconnect());
    // The runtime drops the previous host endpoint before allowing byte I/O.
    console.set_open(false).map_err(show)?;
    console.service(&memory).map_err(show)?;
    assert_eq!(memory.used(tx), Ok(0));
    console.set_open(true).map_err(show)?;
    console.service(&memory).map_err(show)?;
    let mut bytes = [0; 16];
    assert_eq!(console.receive(&mut bytes), 4);
    assert_eq!(&bytes[..4], b"next");
    Ok(())
}

#[test]
fn initial_reset_and_activation_do_not_disconnect_attached_host() -> Result<(), String> {
    let mut console = Console::new(AGENT_PORT_NAME, BASE, SIZE).map_err(show)?;
    console.set_open(true).map_err(show)?;
    console.reset();
    assert!(!console.take_guest_disconnect());
    console
        .activate(VERSION_1 | MULTIPORT, queues())
        .map_err(show)?;
    assert!(!console.take_guest_disconnect());
    let memory = Memory::new();
    handshake(&mut console, &memory)?;
    assert_eq!(console.send(b"connected"), 9);
    Ok(())
}

#[test]
fn host_receive_backpressure_retains_guest_chain_until_drained() -> Result<(), String> {
    let memory = Memory::new();
    let mut console = console(VERSION_1)?;
    let queue = Memory::queue(1);
    for sequence in 0..2 {
        memory
            .descriptor(queue, sequence, BASE + 0x80000, BUFFER_BYTES as u32, 0, 0)
            .map_err(show)?;
        memory.publish(queue, sequence, sequence).map_err(show)?;
    }
    console.service(&memory).map_err(show)?;
    assert_eq!(memory.used(queue), Ok(1));
    let mut bytes = vec![0; BUFFER_BYTES];
    assert_eq!(console.receive(&mut bytes), BUFFER_BYTES);
    console.service(&memory).map_err(show)?;
    assert_eq!(memory.used(queue), Ok(2));
    assert_eq!(console.send(&vec![1; BUFFER_BYTES + 1]), BUFFER_BYTES);
    assert_eq!(console.send(b"full"), 0);
    Ok(())
}

#[test]
fn large_legal_buffers_stream_without_unbounded_host_allocation() -> Result<(), String> {
    let memory = Memory::new();
    let mut console = console(VERSION_1)?;
    let tx = Memory::queue(1);
    memory
        .descriptor(tx, 0, BASE + 0x80000, (2 * BUFFER_BYTES) as u32, 0, 0)
        .map_err(show)?;
    memory.publish(tx, 0, 0).map_err(show)?;
    console.service(&memory).map_err(show)?;
    assert_eq!(memory.used(tx), Ok(0));
    let mut bytes = vec![0; BUFFER_BYTES];
    assert_eq!(console.receive(&mut bytes), BUFFER_BYTES);
    console.service(&memory).map_err(show)?;
    assert_eq!(memory.used(tx), Ok(1));
    assert_eq!(console.receive(&mut bytes), BUFFER_BYTES);
    let rx = Memory::queue(0);
    memory
        .descriptor(rx, 0, BASE + 0x80000, (2 * BUFFER_BYTES) as u32, 2, 0)
        .map_err(show)?;
    memory.publish(rx, 0, 0).map_err(show)?;
    assert_eq!(console.send(b"small reply"), 11);
    console.service(&memory).map_err(show)?;
    assert_eq!(memory.used(rx), Ok(1));
    assert_eq!(
        memory.bytes(BASE + 0x80000, 11).map_err(show)?,
        b"small reply"
    );
    Ok(())
}

#[test]
fn closing_partial_transmit_never_replays_tail_to_reopened_consumer() -> Result<(), String> {
    let memory = Memory::new();
    let mut console = console(VERSION_1)?;
    let tx = Memory::queue(1);
    memory
        .descriptor(tx, 0, BASE + 0x80000, (2 * BUFFER_BYTES) as u32, 0, 0)
        .map_err(show)?;
    memory.publish(tx, 0, 0).map_err(show)?;
    console.service(&memory).map_err(show)?;
    assert_eq!(memory.used(tx), Ok(0));
    console.set_open(false).map_err(show)?;
    console.set_open(true).map_err(show)?;
    console.service(&memory).map_err(show)?;
    assert_eq!(memory.used(tx), Ok(1));
    assert_eq!(console.receive(&mut [0; 16]), 0);
    Ok(())
}

#[test]
fn work_budget_reports_pending_ring_work_before_waiting() -> Result<(), String> {
    let memory = Memory::new();
    let mut layout = queues();
    layout[1].size = 128;
    layout[1].available = layout[1].descriptor + 0x800;
    layout[1].used = layout[1].descriptor + 0xa00;
    let mut console = Console::new(AGENT_PORT_NAME, BASE, SIZE).map_err(show)?;
    console.activate(VERSION_1, layout).map_err(show)?;
    console.set_open(true).map_err(show)?;
    for index in 0..64 {
        memory
            .descriptor(layout[1], index, BASE + 0x80000, 0, 0, 0)
            .map_err(show)?;
        memory.publish(layout[1], index, index).map_err(show)?;
    }
    let first = console.service(&memory).map_err(show)?;
    assert_eq!(first.completed, 32);
    assert!(first.more);
    let second = console.service(&memory).map_err(show)?;
    assert_eq!(second.completed, 32);
    assert!(second.more);
    assert_eq!(console.service(&memory), Ok(Progress::default()));
    assert_eq!(memory.used(layout[1]), Ok(64));
    Ok(())
}

#[test]
fn reset_discards_session_and_replays_full_handshake() -> Result<(), String> {
    let memory = Memory::new();
    let mut console = console(VERSION_1 | MULTIPORT)?;
    handshake(&mut console, &memory)?;
    assert_eq!(console.send(b"old"), 3);
    console.reset();
    assert!(!console.guest_open());
    assert!(!console.named_port_ready());
    assert_eq!(console.send(b"no peer"), 0);
    console
        .activate(VERSION_1 | MULTIPORT, queues())
        .map_err(show)?;
    let fresh = Memory::new();
    handshake(&mut console, &fresh)?;
    assert!(console.to_guest.is_empty());
    console.set_open(false).map_err(show)?;
    assert_eq!(console.send(b"closed"), 0);
    Ok(())
}

#[test]
fn memory_failure_latches_failed_until_explicit_reset() -> Result<(), String> {
    let memory = Memory::new();
    let mut console = console(VERSION_1)?;
    console.service(&memory).map_err(show)?;
    let queue = Memory::queue(1);
    memory
        .descriptor(queue, 0, payload(1, 0), 1, 0, 0)
        .map_err(show)?;
    memory.publish(queue, 0, 0).map_err(show)?;
    *memory.fail_write.borrow_mut() = Some(queue.used + 2);
    assert!(console.service(&memory).is_err());
    *memory.fail_write.borrow_mut() = None;
    assert_eq!(console.service(&memory), Err(Error::Failed));
    assert_eq!(console.receive(&mut [0; 1]), 0);
    console.reset();
    assert_eq!(console.service(&memory), Ok(Progress::default()));
    Ok(())
}

#[test]
fn activation_rejects_bad_features_and_cross_queue_metadata_aliases() -> Result<(), String> {
    let mut console = Console::new(AGENT_PORT_NAME, BASE, SIZE).map_err(show)?;
    assert_eq!(
        console.activate(MULTIPORT, queues()),
        Err(ConfigurationError::Features)
    );
    assert_eq!(
        console.activate(VERSION_1 | (1 << 29), queues()),
        Err(ConfigurationError::Features)
    );
    let mut overlapping = queues();
    overlapping[1] = overlapping[0];
    assert_eq!(
        console.activate(VERSION_1 | MULTIPORT, overlapping),
        Err(ConfigurationError::Queue(virtqueue::Invalid::Layout))
    );
    Ok(())
}

#[test]
fn pci_console_exposes_only_supported_features_and_recovers_local_fault() -> Result<(), String> {
    use crate::{pci, virtio, virtio_pci};
    let mut device = virtio_pci::Device::new(
        virtio::Device::console(BASE, SIZE).map_err(show)?,
        0x0aa0_0000,
    );
    assert_eq!(device.read_configuration(0, 4, false), Ok(0x1043_1af4));
    assert_eq!(device.read_configuration(0x3d, 1, false), Ok(2));
    assert_eq!(device.read_configuration(0x85, 1, false), Ok(0x98));
    assert_eq!(device.read_configuration(0x98, 1, false), Ok(0x10));
    assert_eq!(device.read_configuration(0x9a, 2, false), Ok(2));
    assert_eq!(device.read(4, 4), Ok(MULTIPORT));
    assert_eq!(device.read(18, 2), Ok(4));
    assert_eq!(device.read(virtio_pci::DEVICE_OFFSET + 4, 4), Ok(1));
    device
        .write_configuration(4, 2, u64::from(pci::MEMORY | pci::BUS_MASTER))
        .map_err(show)?;
    device.write(20, 1, 3).map_err(show)?;
    device.needs_reset();
    assert_eq!(device.read(20, 1), Ok(67));
    let reset = device
        .write(20, 1, 0)
        .map_err(show)?
        .ok_or("reset missing")?;
    device.complete(reset.id, true).map_err(show)?;
    assert_eq!(device.read(20, 1), Ok(0));
    Ok(())
}

#[test]
fn pci_console_negotiates_optional_multiport_before_control_handshake() -> Result<(), String> {
    use crate::{pci, virtio, virtio_pci};
    for features in [VERSION_1, VERSION_1 | MULTIPORT] {
        let memory = Memory::new();
        let mut backend = Console::new(AGENT_PORT_NAME, BASE, SIZE).map_err(show)?;
        backend.set_open(true).map_err(show)?;
        let mut device = virtio_pci::Device::new(
            virtio::Device::console(BASE, SIZE).map_err(show)?,
            0x0aa0_0000,
        );
        device
            .write_configuration(4, 2, u64::from(pci::MEMORY | pci::BUS_MASTER))
            .map_err(show)?;
        device.write(20, 1, 3).map_err(show)?;
        device.write(8, 4, 0).map_err(show)?;
        device.write(12, 4, features as u32 as u64).map_err(show)?;
        device.write(8, 4, 1).map_err(show)?;
        device.write(12, 4, features >> 32).map_err(show)?;
        device.write(20, 1, 11).map_err(show)?;
        let count = if features & MULTIPORT != 0 { 4 } else { 2 };
        for index in 0..count {
            let queue = Memory::queue(index);
            device.write(22, 2, index as u64).map_err(show)?;
            device.write(24, 2, u64::from(queue.size)).map_err(show)?;
            device.write(32, 8, queue.descriptor).map_err(show)?;
            device.write(40, 8, queue.available).map_err(show)?;
            device.write(48, 8, queue.used).map_err(show)?;
            device.write(28, 2, 1).map_err(show)?;
        }
        let transaction = device
            .write(20, 1, 15)
            .map_err(show)?
            .ok_or("activation missing")?;
        let virtio::BackendOperation::Activate { features, queues } = transaction.operation else {
            return Err("unexpected transaction".into());
        };
        backend.activate(features, queues).map_err(show)?;
        device.complete(transaction.id, true).map_err(show)?;
        assert_eq!(device.read(20, 1), Ok(15));
        assert_eq!(backend.service(&memory), Ok(Progress::default()));
        if features & MULTIPORT != 0 {
            handshake(&mut backend, &memory)?;
        } else {
            assert!(backend.guest_open());
        }
    }
    Ok(())
}
