// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Nonblocking configuration owner for one virtio device backend.
//!
//! The caller multiplexes mailbox, VM lifetime, console and vCPU observations.
//! A control request never blocks that event loop, and a guest MMIO instruction
//! is completed only after the exact backend transaction is acknowledged.

use crate::io_protocol::{self, Command, Reply, Request, Status};
use crate::virtio_mmio::{self, BackendOperation, Device, DeviceKind, Transaction};
use hyper_os::guest_io::{Mailbox, Notification, Operation};
use hyper_os::vm::{MmioCompletion, MmioOperation, MmioRequest};
use std::num::NonZeroU64;

#[derive(Debug)]
pub enum Error {
    Native(hyper_os::Error),
    Protocol(io_protocol::Error),
    Device(virtio_mmio::Error),
    BackendRejected(Status),
    Disconnected,
    InvalidState,
    SequenceExhausted,
}

#[derive(Clone, Copy, Debug)]
pub struct Completion {
    pub vcpu: usize,
    pub id: NonZeroU64,
    pub result: MmioCompletion,
}

struct Pending {
    request: Request,
    phase: Phase,
    guest: Option<(Transaction, Completion)>,
}

#[derive(Clone, Copy)]
enum Phase {
    Disable { started: bool },
    Request { sent: bool },
    Notify { operation: Operation, started: bool },
}

/// Low-frequency backend configuration transport; queue notifications bypass it.
pub trait ControlTransport {
    fn send(&self, record: &[u8]) -> hyper_os::Result<()>;
    fn receive(&self, record: &mut [u8]) -> hyper_os::Result<usize>;
}
impl ControlTransport for Mailbox {
    fn send(&self, record: &[u8]) -> hyper_os::Result<()> {
        Mailbox::send(self, record)
    }
    fn receive(&self, record: &mut [u8]) -> hyper_os::Result<usize> {
        Mailbox::receive(self, record)
    }
}
impl ControlTransport for hyper_os::OwnedHandle<hyper_os::handle::ByteChannelObject> {
    fn send(&self, record: &[u8]) -> hyper_os::Result<()> {
        self.as_byte_channel().try_send(record)
    }
    fn receive(&self, record: &mut [u8]) -> hyper_os::Result<usize> {
        self.as_byte_channel().try_receive(record)
    }
}

/// Starts/polls one notification transaction without waiting for its peer.
/// `None` preserves the transaction; callers must not start a second one.
pub trait NotificationControl {
    fn start(&mut self, operation: Operation) -> hyper_os::Result<Option<u32>>;
    fn poll(&mut self) -> hyper_os::Result<Option<u32>>;
    fn wants_write(&self) -> bool {
        false
    }
    /// A lost remote lane cannot safely start another transaction. Local
    /// notification capabilities can still raise a diagnostic interrupt.
    fn disconnected(&mut self) -> hyper_os::Result<()> {
        Ok(())
    }
}
impl NotificationControl for Notification {
    fn disconnected(&mut self) -> hyper_os::Result<()> {
        Notification::control(self, Operation::RaiseConfigurationInterrupt).map(|_| ())
    }
    fn start(&mut self, operation: Operation) -> hyper_os::Result<Option<u32>> {
        Notification::control(self, operation).map(Some)
    }
    fn poll(&mut self) -> hyper_os::Result<Option<u32>> {
        Err(hyper_os::Error::InvalidResponse)
    }
}

/// Broker notification adapter sharing the protocol lane, never a wait thread.
pub struct RemoteNotification<T> {
    channel: T,
    pending: Option<([u8; 16], bool)>,
    kind: DeviceKind,
}
impl<T> RemoteNotification<T> {
    pub fn new(channel: T) -> Self {
        Self {
            channel,
            pending: None,
            kind: DeviceKind::Scsi,
        }
    }
    pub fn network(channel: T) -> Self {
        Self {
            channel,
            pending: None,
            kind: DeviceKind::Network,
        }
    }
}
impl<T: ControlTransport> NotificationControl for RemoteNotification<T> {
    fn start(&mut self, operation: Operation) -> hyper_os::Result<Option<u32>> {
        if self.pending.is_some() {
            return Err(hyper_os::Error::InvalidResponse);
        }
        let mut request = [0; 16];
        request[..8].copy_from_slice(b"HIONOT01");
        request[8..12].copy_from_slice(&(operation as u32).to_le_bytes());
        request[12..16].copy_from_slice(&(self.kind as u32).to_le_bytes());
        self.pending = Some((request, false));
        self.poll()
    }
    fn wants_write(&self) -> bool {
        self.pending.as_ref().is_some_and(|(_, sent)| !sent)
    }
    fn poll(&mut self) -> hyper_os::Result<Option<u32>> {
        let (request, sent) = self
            .pending
            .as_mut()
            .ok_or(hyper_os::Error::InvalidResponse)?;
        if !*sent {
            match self.channel.send(request) {
                Ok(()) => *sent = true,
                Err(hyper_os::Error::Status(hyper_os::Status::WOULD_BLOCK)) => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        let mut reply = [0; 16];
        match self.channel.receive(&mut reply) {
            Ok(16)
                if &reply[..8] == b"HIONOTR1"
                    && reply[12..] == (self.kind as u32).to_le_bytes() =>
            {
                let epoch = u32::from_le_bytes(
                    reply[8..12]
                        .try_into()
                        .map_err(|_| hyper_os::Error::InvalidResponse)?,
                );
                if epoch == 0 {
                    return Err(hyper_os::Error::InvalidResponse);
                }
                self.pending = None;
                Ok(Some(epoch))
            }
            Err(hyper_os::Error::Status(hyper_os::Status::WOULD_BLOCK)) => Ok(None),
            Err(error) => Err(error),
            _ => Err(hyper_os::Error::InvalidResponse),
        }
    }
}

impl ControlTransport
    for std::sync::Arc<hyper_os::OwnedHandle<hyper_os::handle::ByteChannelObject>>
{
    fn send(&self, record: &[u8]) -> hyper_os::Result<()> {
        self.as_byte_channel().try_send(record)
    }
    fn receive(&self, record: &mut [u8]) -> hyper_os::Result<usize> {
        self.as_byte_channel().try_receive(record)
    }
}

/// Budget for a whole Disable/protocol/Enable operation, not each wakeup.
#[derive(Default)]
pub struct OperationDeadline(Option<u64>);
impl OperationDeadline {
    /// Observe completion first: already completed work wins at the boundary.
    pub fn observe(&mut self, pending: bool, now: u64) -> Result<(), Error> {
        if !pending {
            self.0 = None;
            return Ok(());
        }
        let limit = match self.0 {
            Some(limit) => limit,
            None => {
                let limit = now.checked_add(60_000_000_000).ok_or(Error::InvalidState)?;
                self.0 = Some(limit);
                limit
            }
        };
        if now >= limit {
            return Err(Error::Native(hyper_os::Error::Status(
                hyper_os::Status::TIMED_OUT,
            )));
        }
        Ok(())
    }
    pub fn raw(&self) -> u64 {
        self.0.unwrap_or(hyper_os::DEADLINE_INFINITE)
    }
}

pub struct Backend<T = Mailbox, N = Notification> {
    kind: DeviceKind,
    mailbox: T,
    notification: N,
    device: Option<Device>,
    memory_base: u64,
    memory_size: u64,
    mmio_base: u64,
    device_id: NonZeroU64,
    binding: u64,
    next_transaction: u64,
    pending: Option<Pending>,
    terminal: bool,
}

impl<T: ControlTransport, N: NotificationControl> Backend<T, N> {
    pub fn new(
        mailbox: T,
        notification: N,
        memory: (u64, u64),
        mmio_base: u64,
        device_id: NonZeroU64,
    ) -> Result<Self, Error> {
        Ok(Self {
            kind: DeviceKind::Scsi,
            mailbox,
            notification,
            device: None,
            memory_base: memory.0,
            memory_size: memory.1,
            mmio_base,
            device_id,
            binding: device_id.get(),
            next_transaction: 2,
            terminal: false,
            pending: Some(Pending {
                request: Request {
                    binding: device_id.get(),
                    epoch: 0, // Filled only after the Disable reply.
                    transaction: 1,
                    command: Command::Hello,
                },
                phase: Phase::Disable { started: false },
                guest: None,
            }),
        })
    }

    /// A second endpoint shares the memory binding and serialized control lane,
    /// while keeping its own configuration model, notification epoch and MMIO ID.
    pub fn network(
        mailbox: T,
        notification: N,
        memory: (u64, u64),
        mmio_base: u64,
        device_id: NonZeroU64,
        binding: NonZeroU64,
    ) -> Result<Self, Error> {
        let mut backend = Self::new(mailbox, notification, memory, mmio_base, device_id)?;
        backend.kind = DeviceKind::Network;
        backend.binding = binding.get();
        if let Some(pending) = backend.pending.as_mut() {
            pending.request.binding = binding.get();
            pending.request.command = Command::NetworkHello;
        }
        Ok(backend)
    }

    pub fn device_id(&self) -> NonZeroU64 {
        self.device_id
    }

    /// Preserve outstanding queue ownership and expose failure to the guest.
    /// The supervisor still owns stopping/reaping the backend and its DMA grants.
    pub fn disconnected(&mut self) -> Result<(), Error> {
        // Retain `pending`: neither a lost peer nor a late reply proves that
        // its descriptors and DMA buffers have been quiesced. Terminal state
        // independently opens diagnostic reads of DEVICE_NEEDS_RESET.
        self.terminal = true;
        if let Some(device) = self.device.as_mut() {
            device.backend_lost();
        }
        self.notification.disconnected().map_err(Error::Native)?;
        Ok(())
    }

    pub fn mailbox(&self) -> &T {
        &self.mailbox
    }
    pub fn notification(&self) -> &N {
        &self.notification
    }
    pub fn ready(&self) -> bool {
        self.device.is_some() && !self.terminal
    }
    /// Whether backend progress currently blocks ordinary configuration MMIO.
    /// Terminal failure retains ownership, but must not park diagnostic reads.
    pub fn busy(&self) -> bool {
        self.pending.is_some() && !self.terminal
    }
    pub fn wants_write(&self) -> bool {
        !self.terminal
            && self
                .pending
                .as_ref()
                .is_some_and(|pending| match pending.phase {
                    Phase::Request { sent } => !sent,
                    Phase::Disable { started } | Phase::Notify { started, .. } => {
                        !started || self.notification.wants_write()
                    }
                })
    }

    /// Advance a bounded number of immediate phases. Every transport operation
    /// is a try operation: delayed notifications never block console/power.
    pub fn progress(&mut self) -> Result<Option<Completion>, Error> {
        if self.terminal {
            return Err(Error::Disconnected);
        }
        // Disable -> request -> post-notification: at most three transitions.
        for _ in 0..3 {
            let Some(pending) = self.pending.as_mut() else {
                return Ok(None);
            };
            match pending.phase {
                Phase::Disable { started } | Phase::Notify { started, .. } => {
                    let operation = match pending.phase {
                        Phase::Disable { .. } => Operation::Disable,
                        Phase::Notify { operation, .. } => operation,
                        Phase::Request { .. } => return Err(Error::InvalidState),
                    };
                    let epoch = if started {
                        self.notification.poll()
                    } else {
                        pending.phase = match pending.phase {
                            Phase::Disable { .. } => Phase::Disable { started: true },
                            _ => Phase::Notify {
                                operation,
                                started: true,
                            },
                        };
                        self.notification.start(operation)
                    }
                    .map_err(Error::Native)?;
                    let Some(epoch) = epoch else {
                        return Ok(None);
                    };
                    if matches!(pending.phase, Phase::Disable { .. }) {
                        pending.request.epoch = epoch;
                        pending.phase = Phase::Request { sent: false };
                        continue;
                    }
                    if epoch != pending.request.epoch {
                        return Err(Error::InvalidState);
                    }
                    let completion = pending.guest.map(|(_, completion)| completion);
                    self.pending = None;
                    return Ok(completion);
                }
                Phase::Request { sent } => {
                    if !sent {
                        let mut record = [0; io_protocol::MAX_RECORD];
                        let length = pending
                            .request
                            .encode(&mut record)
                            .map_err(Error::Protocol)?;
                        match self.mailbox.send(&record[..length]) {
                            Ok(()) => pending.phase = Phase::Request { sent: true },
                            Err(hyper_os::Error::Status(hyper_os::Status::WOULD_BLOCK)) => {
                                return Ok(None);
                            }
                            Err(error) => return Err(Error::Native(error)),
                        }
                    }
                    let mut record = [0; io_protocol::MAX_RECORD];
                    let length = match self.mailbox.receive(&mut record) {
                        Ok(length) => length,
                        Err(hyper_os::Error::Status(hyper_os::Status::WOULD_BLOCK)) => {
                            return Ok(None);
                        }
                        Err(error) => return Err(Error::Native(error)),
                    };
                    let reply = Reply::decode(&record[..length], pending.request)
                        .map_err(Error::Protocol)?;
                    if let Some((transaction, completion)) = pending.guest {
                        let success = match reply.status {
                            Status::Success => true,
                            Status::BackendFailure
                                if matches!(
                                    transaction.operation,
                                    BackendOperation::Activate { .. }
                                ) =>
                            {
                                false
                            }
                            status => return Err(Error::BackendRejected(status)),
                        };
                        self.device
                            .as_mut()
                            .ok_or(Error::InvalidState)?
                            .complete(transaction.id, success)
                            .map_err(Error::Device)?;
                        let operation =
                            if matches!(transaction.operation, BackendOperation::Activate { .. })
                                && success
                            {
                                Some(Operation::Enable)
                            } else if matches!(
                                transaction.operation,
                                BackendOperation::StopQueue { .. }
                            ) || !success
                            {
                                Some(Operation::RaiseConfigurationInterrupt)
                            } else {
                                None
                            };
                        if let Some(operation) = operation {
                            pending.phase = Phase::Notify {
                                operation,
                                started: false,
                            };
                            continue;
                        }
                        self.pending = None;
                        return Ok(Some(completion));
                    }
                    if reply.status != Status::Success {
                        return Err(Error::BackendRejected(reply.status));
                    }
                    let features = reply.features.ok_or(Error::InvalidState)?;
                    self.device = Some(
                        match self.kind {
                            DeviceKind::Scsi => {
                                Device::new(features, self.memory_base, self.memory_size)
                            }
                            DeviceKind::Network => Device::network(
                                features,
                                self.memory_base,
                                self.memory_size,
                                reply.network.ok_or(Error::InvalidState)?,
                            ),
                        }
                        .map_err(Error::Device)?,
                    );
                    self.pending = None;
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }

    /// `None` means the instruction is deliberately parked. Other vCPUs may
    /// also park configuration requests while this single device transaction
    /// is outstanding; retry them after progress returns its completion.
    pub fn mmio(&mut self, vcpu: usize, request: MmioRequest) -> Result<Option<Completion>, Error> {
        if self.terminal && !matches!(request.operation, MmioOperation::Read) {
            return Err(Error::Disconnected);
        }
        if self.busy() {
            return Ok(None);
        }
        if request.device != self.device_id {
            return Err(Error::InvalidState);
        }
        let offset = request
            .address
            .checked_sub(self.mmio_base)
            .ok_or(Error::InvalidState)?;
        let device = self.device.as_mut().ok_or(Error::InvalidState)?;
        let width = u8::try_from(request.width).map_err(|_| Error::InvalidState)?;
        let mut completion = Completion {
            vcpu,
            id: request.id,
            result: MmioCompletion::Write,
        };
        match request.operation {
            MmioOperation::Read => {
                completion.result =
                    MmioCompletion::Read(device.read(offset, width).map_err(Error::Device)?);
            }
            MmioOperation::Write(value) => {
                if let Some(transaction) =
                    device.write(offset, width, value).map_err(Error::Device)?
                {
                    let next = self
                        .next_transaction
                        .checked_add(1)
                        .ok_or(Error::SequenceExhausted)?;
                    self.pending = Some(Pending {
                        request: Request {
                            binding: self.binding,
                            epoch: 0,
                            transaction: self.next_transaction,
                            command: match self.kind {
                                DeviceKind::Scsi => Command::Device(transaction.operation),
                                DeviceKind::Network => {
                                    Command::NetworkDevice(transaction.operation)
                                }
                            },
                        },
                        phase: Phase::Disable { started: false },
                        guest: Some((transaction, completion)),
                    });
                    self.next_transaction = next;
                    return Ok(None);
                }
            }
        }
        Ok(Some(completion))
    }
}

#[cfg(test)]
#[path = "../tests/io_backend.rs"]
mod tests;
