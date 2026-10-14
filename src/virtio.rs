// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Transport-independent virtio state and backend activation transactions.
//!
//! Queue ownership changes only after backend acknowledgement. Both MMIO and
//! PCI transports use this owner for feature negotiation and ring validation.

pub const VERSION_1: u64 = 1 << 32;
pub const REQUEST_QUEUES: usize = 4;
pub const QUEUES: usize = 2 + REQUEST_QUEUES;
pub const QUEUE_MAX: u32 = 128;
use crate::virtio_net;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum DeviceKind {
    Scsi = 0,
    Network = 1,
    Console = 2,
}

impl DeviceKind {
    pub const fn queue_count(self) -> usize {
        match self {
            Self::Scsi => QUEUES,
            Self::Network => virtio_net::QUEUES,
            Self::Console => 4,
        }
    }
}
const ACKNOWLEDGE: u32 = 1;
const DRIVER: u32 = 2;
const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;
const NEEDS_RESET: u32 = 64;
const FAILED: u32 = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    UnsupportedBackend,
    InvalidAccess,
    InvalidStatus,
    InvalidQueue,
    Busy,
    StaleCompletion,
    BackendResetFailed,
    GenerationExhausted,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Queue {
    pub size: u32,
    pub descriptor: u64,
    pub available: u64,
    pub used: u64,
    pub ready: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendOperation {
    Activate {
        features: u64,
        queues: [Queue; QUEUES],
    },
    Reset,
    /// Stop the vhost endpoint and drain in-flight I/O before acknowledging a
    /// queue stop. This backend cannot independently retire one SCSI queue;
    /// successful quiescence requests a whole-device reset from the guest.
    StopQueue {
        queue: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Transaction {
    pub id: u64,
    pub operation: BackendOperation,
}

#[derive(Clone, Copy)]
struct Pending {
    transaction: Transaction,
    next_status: u32,
}

/// Semantic configuration writes shared by the two register transports.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Register {
    DeviceSelector,
    DriverSelector,
    Features,
    QueueSelector,
    QueueSize,
    QueueReady,
    DescriptorLow,
    DescriptorHigh,
    AvailableLow,
    AvailableHigh,
    UsedLow,
    UsedHigh,
    Status,
}

pub struct Device {
    pub(crate) kind: DeviceKind,
    network: Option<virtio_net::Configuration>,
    pub(crate) offered: u64,
    pub(crate) features: u64,
    pub(crate) device_selector: u32,
    pub(crate) driver_selector: u32,
    pub(crate) queue_selector: u32,
    queues: [Queue; QUEUES],
    pub(crate) status: u32,
    memory_base: u64,
    memory_end: u64,
    next_transaction: u64,
    pending: Option<Pending>,
    backend_lost: bool,
}

impl Device {
    pub fn new(backend_features: u64, memory_base: u64, memory_size: u64) -> Result<Self, Error> {
        let memory_end = memory_base
            .checked_add(memory_size)
            .filter(|end| *end > memory_base)
            .ok_or(Error::InvalidQueue)?;
        if backend_features & VERSION_1 == 0 {
            return Err(Error::UnsupportedBackend);
        }
        Ok(Self {
            kind: DeviceKind::Scsi,
            network: None,
            // No indirect descriptors, EVENT_IDX, packed rings, or SCSI hotplug
            // until the complete bridge supports their semantics.
            offered: backend_features & VERSION_1,
            features: 0,
            device_selector: 0,
            driver_selector: 0,
            queue_selector: 0,
            queues: [Queue::default(); QUEUES],
            status: 0,
            memory_base,
            memory_end,
            next_transaction: 1,
            pending: None,
            backend_lost: false,
        })
    }

    pub fn network(
        backend_features: u64,
        memory_base: u64,
        memory_size: u64,
        configuration: virtio_net::Configuration,
    ) -> Result<Self, Error> {
        if !configuration.valid() {
            return Err(Error::UnsupportedBackend);
        }
        let mut device = Self::new(backend_features, memory_base, memory_size)?;
        device.kind = DeviceKind::Network;
        device.network = Some(configuration);
        // MAC is implemented by this configuration model, not Linux vhost.
        // TX checksum/TSO metadata is handled by the backend's TAP. RX
        // offloads, mergeable buffers, control queues and multiqueue stay off.
        device.offered |=
            virtio_net::MAC_FEATURE | virtio_net::supported_offloads(backend_features);
        Ok(device)
    }

    /// One serial port, with the standard control queues when MULTIPORT is
    /// negotiated. The port's policy and byte stream belong to its backend.
    pub fn console(memory_base: u64, memory_size: u64) -> Result<Self, Error> {
        let mut device = Self::new(VERSION_1, memory_base, memory_size)?;
        device.kind = DeviceKind::Console;
        device.offered |= crate::virtio_console::MULTIPORT;
        Ok(device)
    }

    pub(crate) fn queues(&self) -> &[Queue] {
        &self.queues[..self.kind.queue_count()]
    }

    pub(crate) fn write(
        &mut self,
        register: Register,
        value: u32,
    ) -> Result<Option<Transaction>, Error> {
        self.admit_write()?;
        match register {
            Register::DeviceSelector => self.device_selector = value,
            Register::DriverSelector => self.driver_selector = value,
            Register::Features => {
                if self.status & DRIVER == 0
                    || self.status & (FEATURES_OK | DRIVER_OK | FAILED) != 0
                {
                    return Err(Error::InvalidStatus);
                }
                if self.driver_selector < 2 {
                    let shift = 32 * self.driver_selector;
                    self.features = (self.features & !(u64::from(u32::MAX) << shift))
                        | (u64::from(value) << shift);
                } else if value != 0 {
                    return Err(Error::InvalidAccess);
                }
            }
            Register::QueueSelector => self.queue_selector = value,
            Register::QueueReady if value == 0 => {
                let queue = self
                    .queues()
                    .get(self.queue_selector as usize)
                    .ok_or(Error::InvalidQueue)?;
                if queue.ready && self.status & (DRIVER_OK | NEEDS_RESET) != 0 {
                    return self
                        .begin(
                            BackendOperation::StopQueue {
                                queue: self.queue_selector,
                            },
                            self.status | NEEDS_RESET,
                        )
                        .map(Some);
                }
                self.queues[self.queue_selector as usize].ready = false;
            }
            Register::QueueSize
            | Register::QueueReady
            | Register::DescriptorLow
            | Register::DescriptorHigh
            | Register::AvailableLow
            | Register::AvailableHigh
            | Register::UsedLow
            | Register::UsedHigh => {
                if self.status & FEATURES_OK == 0
                    || self.status & (DRIVER_OK | FAILED | NEEDS_RESET) != 0
                {
                    return Err(Error::InvalidStatus);
                }
                let index = self.queue_selector as usize;
                let mut queue = *self.queues().get(index).ok_or(Error::InvalidQueue)?;
                // Ready queues cannot change their metadata. QueueReady=0
                // above is a synchronization point even without RING_RESET.
                if queue.ready {
                    return Err(Error::InvalidQueue);
                }
                match register {
                    Register::QueueSize => queue.size = value,
                    Register::QueueReady if value == 1 => {
                        self.validate_queue(queue)?;
                        queue.ready = true;
                    }
                    Register::QueueReady => return Err(Error::InvalidQueue),
                    Register::DescriptorLow => set_half(&mut queue.descriptor, false, value),
                    Register::DescriptorHigh => set_half(&mut queue.descriptor, true, value),
                    Register::AvailableLow => set_half(&mut queue.available, false, value),
                    Register::AvailableHigh => set_half(&mut queue.available, true, value),
                    Register::UsedLow => set_half(&mut queue.used, false, value),
                    Register::UsedHigh => set_half(&mut queue.used, true, value),
                    _ => return Err(Error::InvalidAccess),
                }
                self.queues[index] = queue;
            }
            Register::Status => return self.set_status(value),
        }
        Ok(None)
    }

    pub(crate) fn admit_write(&self) -> Result<(), Error> {
        if self.backend_lost {
            return Err(Error::BackendResetFailed);
        }
        if self.pending.is_some() {
            return Err(Error::Busy);
        }
        Ok(())
    }

    pub(crate) fn stop_bus_master(&mut self) -> Result<Option<Transaction>, Error> {
        self.admit_write()?;
        if self.status & DRIVER_OK == 0 || self.status & NEEDS_RESET != 0 {
            return Ok(None);
        }
        // Quiescing vhost is the proof that clearing BME retired its DMA.
        // A subsequent enable needs a full virtio reset/renegotiation: the
        // backend does not support suspending and resuming live queue indices.
        self.begin(
            BackendOperation::StopQueue { queue: 0 },
            self.status | NEEDS_RESET,
        )
        .map(Some)
    }

    fn set_status(&mut self, value: u32) -> Result<Option<Transaction>, Error> {
        if value == 0 {
            return self.begin(BackendOperation::Reset, 0).map(Some);
        }
        let driver_bits = ACKNOWLEDGE | DRIVER | DRIVER_OK | FEATURES_OK | FAILED;
        if value & !(driver_bits | NEEDS_RESET) != 0
            || value & self.status != self.status
            || (value & NEEDS_RESET != 0 && self.status & NEEDS_RESET == 0)
            || value & ACKNOWLEDGE == 0
            || (value & FEATURES_OK != 0 && value & DRIVER == 0)
            || (value & DRIVER_OK != 0 && self.status & FEATURES_OK == 0)
        {
            return Err(Error::InvalidStatus);
        }
        if value & FEATURES_OK != 0
            && (self.features & !self.offered != 0
                || self.features & VERSION_1 == 0
                || (self.kind == DeviceKind::Network && !virtio_net::valid_features(self.features)))
        {
            self.status = value & !(FEATURES_OK | DRIVER_OK);
            return Ok(None);
        }
        if value & DRIVER_OK != 0 && self.status & DRIVER_OK == 0 {
            if value & FAILED != 0 || self.status & (FAILED | NEEDS_RESET) != 0 {
                return Err(Error::InvalidStatus);
            }
            self.validate_queues()?;
            return self
                .begin(
                    BackendOperation::Activate {
                        features: self.features,
                        queues: self.queues,
                    },
                    value,
                )
                .map(Some);
        }
        self.status = value;
        Ok(None)
    }

    fn begin(
        &mut self,
        operation: BackendOperation,
        next_status: u32,
    ) -> Result<Transaction, Error> {
        let next = self
            .next_transaction
            .checked_add(1)
            .ok_or(Error::GenerationExhausted)?;
        let transaction = Transaction {
            id: self.next_transaction,
            operation,
        };
        self.next_transaction = next;
        self.pending = Some(Pending {
            transaction,
            next_status,
        });
        Ok(transaction)
    }

    /// Reports terminal loss of the backend without releasing queue state or
    /// accepting stale acknowledgements as proof of DMA quiescence. The caller
    /// must raise the transport configuration IRQ after this status publication.
    pub fn backend_lost(&mut self) {
        self.backend_lost = true;
        self.status |= NEEDS_RESET;
    }

    /// `success` must come from the exact backend operation, not merely sending
    /// its request. A failed reset retains the transaction and all queue state;
    /// the caller must retain memory and may retry or quarantine the backend.
    /// When completion sets `NEEDS_RESET`, the transport must also assert the
    /// configuration-change interrupt through its notification binding.
    pub fn complete(&mut self, id: u64, success: bool) -> Result<(), Error> {
        if self.backend_lost {
            return Err(Error::BackendResetFailed);
        }
        let pending = self
            .pending
            .filter(|pending| pending.transaction.id == id)
            .ok_or(Error::StaleCompletion)?;
        match pending.transaction.operation {
            BackendOperation::Reset | BackendOperation::StopQueue { .. } if !success => {
                return Err(Error::BackendResetFailed);
            }
            BackendOperation::StopQueue { queue } => {
                self.queues[queue as usize].ready = false;
                self.status = pending.next_status;
            }
            BackendOperation::Reset => {
                self.features = 0;
                self.device_selector = 0;
                self.driver_selector = 0;
                self.queue_selector = 0;
                self.queues = [Queue::default(); QUEUES];
                self.status = 0;
            }
            BackendOperation::Activate { .. } if success => self.status = pending.next_status,
            BackendOperation::Activate { .. } => self.status |= NEEDS_RESET,
        }
        self.pending = None;
        Ok(())
    }

    fn validate_queue(&self, queue: Queue) -> Result<(), Error> {
        if queue.size == 0 || !queue.size.is_power_of_two() || queue.size > QUEUE_MAX {
            return Err(Error::InvalidQueue);
        }
        for (base, length, align) in queue_regions(queue) {
            if !base.is_multiple_of(align)
                || base < self.memory_base
                || base
                    .checked_add(length)
                    .is_none_or(|end| end > self.memory_end)
            {
                return Err(Error::InvalidQueue);
            }
        }
        Ok(())
    }

    fn validate_queues(&self) -> Result<(), Error> {
        let mut regions = [(0u64, 0u64); QUEUES * 3];
        for (index, queue) in self.queues().iter().copied().enumerate() {
            if self.kind == DeviceKind::Console
                && self.features & crate::virtio_console::MULTIPORT == 0
                && index >= 2
            {
                continue;
            }
            if !queue.ready {
                if self.kind == DeviceKind::Scsi && index >= 3 {
                    continue;
                }
                return Err(Error::InvalidQueue);
            }
            self.validate_queue(queue)?;
            for (part, (base, length, _)) in queue_regions(queue).into_iter().enumerate() {
                let previous = index * 3 + part;
                let end = base + length; // validate_queue proved no overflow.
                if regions[..previous]
                    .iter()
                    .any(|(old_base, old_end)| base < *old_end && *old_base < end)
                {
                    return Err(Error::InvalidQueue);
                }
                regions[previous] = (base, end);
            }
        }
        Ok(())
    }

    pub(crate) fn write_config(&self, offset: u64, width: u8, value: u64) -> Result<(), Error> {
        self.admit_write()?;
        // The backend implements the standard fixed SCSI sense/CDB sizes.
        match (self.kind, offset, width, value) {
            (DeviceKind::Scsi, 20, 4, 96) | (DeviceKind::Scsi, 24, 4, 32) => Ok(()),
            _ => Err(Error::InvalidAccess),
        }
    }

    pub(crate) fn read_config(&self, offset: u64, width: u8) -> Result<u64, Error> {
        let mut bytes = [0u8; 36];
        if self.kind == DeviceKind::Console {
            bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
            return read_config_bytes(&bytes[..12], offset, width);
        }
        if let Some(network) = self.network {
            bytes[..6].copy_from_slice(&network.mac);
            return read_config_bytes(&bytes[..6], offset, width);
        }
        for (index, value) in [REQUEST_QUEUES as u32, 126, 1024, 64, 16, 96, 32]
            .into_iter()
            .enumerate()
        {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[30..32].copy_from_slice(&255u16.to_le_bytes());
        bytes[32..36].copy_from_slice(&16383u32.to_le_bytes());
        read_config_bytes(&bytes, offset, width)
    }
}

fn read_config_bytes(bytes: &[u8], offset: u64, width: u8) -> Result<u64, Error> {
    if !matches!(width, 1 | 2 | 4) || !offset.is_multiple_of(u64::from(width)) {
        return Err(Error::InvalidAccess);
    }
    let start = usize::try_from(offset).map_err(|_| Error::InvalidAccess)?;
    let end = start
        .checked_add(usize::from(width))
        .ok_or(Error::InvalidAccess)?;
    let source = bytes.get(start..end).ok_or(Error::InvalidAccess)?;
    let mut value = [0u8; 8];
    value[..source.len()].copy_from_slice(source);
    Ok(u64::from_le_bytes(value))
}

fn set_half(address: &mut u64, high: bool, value: u32) {
    let shift = if high { 32 } else { 0 };
    *address = (*address & !(u64::from(u32::MAX) << shift)) | (u64::from(value) << shift);
}

fn queue_regions(queue: Queue) -> [(u64, u64, u64); 3] {
    let size = u64::from(queue.size);
    [
        (queue.descriptor, 16 * size, 16),
        (queue.available, 6 + 2 * size, 2),
        (queue.used, 6 + 8 * size, 4),
    ]
}
