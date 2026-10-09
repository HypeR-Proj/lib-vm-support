// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

#[derive(Default)]
struct State {
    sent: Vec<Vec<u8>>,
    replies: VecDeque<Vec<u8>>,
    blocked: bool,
    closed: bool,
    receives: usize,
}
#[derive(Clone, Default)]
struct Transport(Rc<RefCell<State>>);
impl ControlTransport for Transport {
    fn send(&self, bytes: &[u8]) -> hyper_os::Result<()> {
        let mut state = self.0.borrow_mut();
        if state.closed {
            return Err(hyper_os::Error::Status(hyper_os::Status::PEER_CLOSED));
        }
        if state.blocked {
            return Err(hyper_os::Error::Status(hyper_os::Status::WOULD_BLOCK));
        }
        state.sent.push(bytes.to_vec());
        Ok(())
    }
    fn receive(&self, bytes: &mut [u8]) -> hyper_os::Result<usize> {
        let mut state = self.0.borrow_mut();
        state.receives += 1;
        let Some(reply) = state.replies.pop_front() else {
            return Err(hyper_os::Error::Status(if state.closed {
                hyper_os::Status::PEER_CLOSED
            } else {
                hyper_os::Status::WOULD_BLOCK
            }));
        };
        let output = bytes
            .get_mut(..reply.len())
            .ok_or(hyper_os::Error::InvalidResponse)?;
        output.copy_from_slice(&reply);
        Ok(reply.len())
    }
}
impl Transport {
    fn notification_reply(&self, epoch: u32) {
        let mut reply = b"HIONOTR1".to_vec();
        reply.extend_from_slice(&epoch.to_le_bytes());
        reply.extend_from_slice(&[0; 4]);
        self.0.borrow_mut().replies.push_back(reply);
    }
    fn protocol_reply(&self, status: u32) -> Result<(), Error> {
        let mut state = self.0.borrow_mut();
        let request = Request::decode(state.sent.last().ok_or(Error::InvalidState)?)
            .map_err(Error::Protocol)?;
        let mut bytes = [0; io_protocol::MAX_RECORD];
        request.encode(&mut bytes).map_err(Error::Protocol)?;
        let length: u32 = if request.command == Command::Hello {
            64
        } else {
            48
        };
        bytes[8..12].copy_from_slice(&length.to_le_bytes());
        bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
        bytes[40..48].fill(0);
        bytes[40..44].copy_from_slice(&status.to_le_bytes());
        if request.command == Command::Hello {
            bytes[48..56].copy_from_slice(&virtio_mmio::VERSION_1.to_le_bytes());
            bytes[56..60].copy_from_slice(&(virtio_mmio::QUEUES as u32).to_le_bytes());
            bytes[60..64].copy_from_slice(&virtio_mmio::QUEUE_MAX.to_le_bytes());
        }
        state.replies.push_back(bytes[..length as usize].to_vec());
        Ok(())
    }
}
type TestBackend = Backend<Transport, RemoteNotification<Transport>>;
fn backend(transport: &Transport) -> Result<TestBackend, Error> {
    Backend::new(
        transport.clone(),
        RemoteNotification::new(transport.clone()),
        (0x4000_0000, 0x20_0000),
        0x0a00_0000,
        NonZeroU64::MIN,
    )
}
fn initialize(backend: &mut TestBackend, transport: &Transport) -> Result<(), Error> {
    assert!(backend.progress()?.is_none());
    transport.notification_reply(7);
    assert!(backend.progress()?.is_none());
    transport.protocol_reply(0)?;
    assert!(backend.progress()?.is_none());
    assert!(backend.ready());
    Ok(())
}

#[test]
fn delayed_notification_preserves_lane_and_returns_to_supervisor() -> Result<(), Error> {
    let transport = Transport::default();
    let mut backend = backend(&transport)?;
    // Construction performs no remote operation and cannot wait for the broker.
    assert!(transport.0.borrow().sent.is_empty());
    transport.0.borrow_mut().blocked = true;
    assert!(backend.progress()?.is_none());
    assert!(backend.wants_write());
    assert!(transport.0.borrow().sent.is_empty());
    transport.0.borrow_mut().blocked = false;
    assert!(backend.progress()?.is_none());
    assert!(!backend.wants_write());
    for _ in 0..100 {
        // The supervisor can service console/power between each call.
        assert!(backend.progress()?.is_none());
    }
    assert_eq!(transport.0.borrow().sent.len(), 1);
    assert_eq!(transport.0.borrow().receives, 101);
    transport.notification_reply(7);
    assert!(backend.progress()?.is_none());
    assert_eq!(transport.0.borrow().sent.len(), 2);
    transport.protocol_reply(0)?;
    assert!(backend.progress()?.is_none());
    assert!(backend.ready());
    Ok(())
}

#[test]
fn activation_mmio_waits_for_enable_ack_without_resending_protocol() -> Result<(), Error> {
    let transport = Transport::default();
    let mut backend = backend(&transport)?;
    initialize(&mut backend, &transport)?;
    let device = backend.device.as_mut().ok_or(Error::InvalidState)?;
    for (offset, value) in [(0x70, 1), (0x70, 3), (0x24, 1), (0x20, 1), (0x70, 11)] {
        device.write(offset, 4, value).map_err(Error::Device)?;
    }
    for index in 0..3 {
        let base = 0x4000_0000 + index * 0x10000;
        for (offset, value) in [
            (0x30, index),
            (0x38, 128),
            (0x80, base),
            (0x90, base + 0x1000),
            (0xa0, base + 0x2000),
            (0x44, 1),
        ] {
            device.write(offset, 4, value).map_err(Error::Device)?;
        }
    }
    let request = MmioRequest {
        id: NonZeroU64::new(17).ok_or(Error::InvalidState)?,
        device: NonZeroU64::MIN,
        address: 0x0a00_0070,
        width: 4,
        operation: MmioOperation::Write(15),
    };
    assert!(backend.mmio(2, request)?.is_none());
    assert!(backend.progress()?.is_none()); // Disable request.
    transport.notification_reply(8);
    assert!(backend.progress()?.is_none()); // ACTIVATE request.
    transport.protocol_reply(0)?;
    assert!(backend.progress()?.is_none()); // Enable request, not MMIO completion.
    assert!(backend.busy());
    let sends = transport.0.borrow().sent.len();
    for _ in 0..10 {
        assert!(backend.progress()?.is_none());
    }
    assert_eq!(transport.0.borrow().sent.len(), sends);
    transport.notification_reply(8);
    let completion = backend.progress()?.ok_or(Error::InvalidState)?;
    assert_eq!(completion.vcpu, 2);
    assert_eq!(completion.id, request.id);
    assert_eq!(completion.result, MmioCompletion::Write);
    assert!(!backend.busy());
    assert!(backend.progress()?.is_none()); // Exactly once.
    Ok(())
}

#[test]
fn wrong_reply_type_does_not_complete_or_release_pending_operation() -> Result<(), Error> {
    let transport = Transport::default();
    let mut backend = backend(&transport)?;
    assert!(backend.progress()?.is_none());
    transport
        .0
        .borrow_mut()
        .replies
        .push_back(b"wrong-reply-type".to_vec());
    assert!(matches!(
        backend.progress(),
        Err(Error::Native(hyper_os::Error::InvalidResponse))
    ));
    assert!(backend.busy());
    assert!(!backend.ready());
    assert_eq!(transport.0.borrow().sent.len(), 1);
    Ok(())
}

#[test]
fn spurious_ready_events_do_not_extend_operation_or_delay_completed_work() -> Result<(), Error> {
    let mut deadline = OperationDeadline::default();
    deadline.observe(true, 10)?;
    let limit = deadline.raw();
    for now in [20, 30, 40, limit - 1] {
        deadline.observe(true, now)?;
        assert_eq!(deadline.raw(), limit);
    }
    assert!(matches!(
        deadline.observe(true, limit),
        Err(Error::Native(hyper_os::Error::Status(
            hyper_os::Status::TIMED_OUT
        )))
    ));
    deadline.observe(false, limit)?;
    assert_eq!(deadline.raw(), hyper_os::DEADLINE_INFINITE);
    deadline.observe(true, limit)?;
    assert!(deadline.raw() > limit);
    Ok(())
}

#[test]
fn stalled_client_does_not_block_an_independent_runtime() -> Result<(), Error> {
    let slow = Transport::default();
    let fast = Transport::default();
    let mut stalled = backend(&slow)?;
    let mut healthy = backend(&fast)?;
    // Both supervisors return immediately after sending Disable. Only the
    // healthy broker route answers; the stalled one remains owned and pending.
    assert!(stalled.progress()?.is_none());
    assert!(healthy.progress()?.is_none());
    fast.notification_reply(7);
    assert!(stalled.progress()?.is_none());
    assert!(healthy.progress()?.is_none());
    fast.protocol_reply(0)?;
    assert!(stalled.progress()?.is_none());
    assert!(healthy.progress()?.is_none());
    assert!(healthy.ready());
    assert!(stalled.busy());
    assert!(!stalled.ready());
    assert_eq!(slow.0.borrow().sent.len(), 1);
    Ok(())
}

#[test]
fn peer_closure_during_send_backpressure_or_reply_wait_retains_ownership() -> Result<(), Error> {
    for blocked in [false, true] {
        let transport = Transport::default();
        transport.0.borrow_mut().blocked = blocked;
        let mut backend = backend(&transport)?;
        assert!(backend.progress()?.is_none());
        transport.0.borrow_mut().closed = true;
        assert!(matches!(
            backend.progress(),
            Err(Error::Native(hyper_os::Error::Status(
                hyper_os::Status::PEER_CLOSED
            )))
        ));
        assert!(backend.pending.is_some());
        assert!(!backend.ready());
        let sent = transport.0.borrow().sent.len();
        backend.disconnected()?;
        assert!(backend.pending.is_some());
        assert!(matches!(backend.progress(), Err(Error::Disconnected)));
        assert_eq!(transport.0.borrow().sent.len(), sent);
    }
    Ok(())
}
