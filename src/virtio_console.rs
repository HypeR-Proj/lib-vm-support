// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Userspace virtio serial backend with one bounded, named byte-stream port.
//!
//! The standard MULTIPORT handshake creates a serial port, not a Linux hvc
//! console. PCI/MMIO register state and interrupt line ownership stay with the
//! transport. This module does not interpret guest-agent messages.

use crate::virtio::{QUEUES, Queue, VERSION_1};
use crate::virtqueue::{self, Chain, GuestMemory, MemoryRange, Ring};
use std::collections::VecDeque;

pub const MULTIPORT: u64 = 1 << 1;
pub const AGENT_PORT_NAME: &str = "org.qemu.guest_agent.0";
pub const BUFFER_BYTES: usize = 64 * 1024;
const MAX_CONTROL: usize = 8;
const SERVICE_BUDGET: usize = 32;
const DEVICE_READY: u16 = 0;
const PORT_ADD: u16 = 1;
const PORT_READY: u16 = 3;
const PORT_OPEN: u16 = 6;
const PORT_NAME: u16 = 7;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigurationError {
    Name,
    Features,
    Queue(virtqueue::Invalid),
    ControlFull,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Error<E> {
    Queue(virtqueue::Error<E>),
    Control,
    Failed,
}

impl<E> From<virtqueue::Error<E>> for Error<E> {
    fn from(error: virtqueue::Error<E>) -> Self {
        Self::Queue(error)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Progress {
    /// At least one used-ring publication requested a guest interrupt.
    pub interrupt: bool,
    /// A queue exhausted its per-call budget. Service again before waiting.
    pub more: bool,
    pub completed: usize,
}

pub struct Console {
    name: String,
    memory: MemoryRange,
    rings: Option<[Ring; 4]>,
    multiport: bool,
    device_ready: bool,
    port_ready: bool,
    host_open: bool,
    guest_open: bool,
    guest_disconnect: bool,
    failed: bool,
    control: VecDeque<Vec<u8>>,
    to_guest: VecDeque<u8>,
    from_guest: VecDeque<u8>,
    transmit: Option<(Chain, usize)>,
}

impl Console {
    pub fn new(name: &str, memory_base: u64, memory_size: u64) -> Result<Self, ConfigurationError> {
        if name.is_empty() || name.len() > 255 || name.bytes().any(|byte| byte == 0) {
            return Err(ConfigurationError::Name);
        }
        Ok(Self {
            name: name.into(),
            memory: MemoryRange::new(memory_base, memory_size)
                .map_err(ConfigurationError::Queue)?,
            rings: None,
            multiport: false,
            device_ready: false,
            port_ready: false,
            host_open: false,
            guest_open: false,
            guest_disconnect: false,
            failed: false,
            control: VecDeque::new(),
            to_guest: VecDeque::new(),
            from_guest: VecDeque::new(),
            transmit: None,
        })
    }

    /// Called only for the exact `DRIVER_OK` transaction. Ring metadata must be
    /// fully installed before the transport acknowledges that transaction.
    pub fn activate(
        &mut self,
        features: u64,
        queues: [Queue; QUEUES],
    ) -> Result<(), ConfigurationError> {
        if features & VERSION_1 == 0 || features & !(VERSION_1 | MULTIPORT) != 0 {
            return Err(ConfigurationError::Features);
        }
        let multiport = features & MULTIPORT != 0;
        let count = if multiport { 4 } else { 2 };
        let first = Ring::new(queues[0], self.memory).map_err(ConfigurationError::Queue)?;
        let mut rings = [first; 4];
        for index in 0..count {
            let ring = Ring::new(queues[index], self.memory).map_err(ConfigurationError::Queue)?;
            if rings[..index].iter().any(|other| other.overlaps(&ring)) {
                return Err(ConfigurationError::Queue(virtqueue::Invalid::Layout));
            }
            rings[index] = ring;
        }
        self.reset();
        self.multiport = multiport;
        self.guest_open = !multiport;
        self.port_ready = !multiport;
        self.rings = Some(rings);
        Ok(())
    }

    /// Quiescent local reset. The transport serializes this with service, so no
    /// delayed callback can complete an old ring after a guest driver rebinds.
    pub fn reset(&mut self) {
        self.rings = None;
        self.multiport = false;
        self.device_ready = false;
        self.port_ready = false;
        self.guest_open = false;
        self.guest_disconnect = false;
        self.failed = false;
        self.control.clear();
        self.to_guest.clear();
        self.from_guest.clear();
        self.transmit = None;
    }

    pub fn guest_open(&self) -> bool {
        self.port_ready && self.guest_open
    }
    pub fn named_port_ready(&self) -> bool {
        self.multiport && self.port_ready
    }

    /// Reports a guest session boundary even if the guest closed and reopened
    /// within one service turn. The owner must revoke its old host endpoint
    /// before moving more bytes. Initial activation/reset does not emit this
    /// event; the transport itself owns endpoint retirement on device reset.
    pub fn take_guest_disconnect(&mut self) -> bool {
        std::mem::take(&mut self.guest_disconnect)
    }

    /// Reflects the host consumer lifetime, independently of the guest opening
    /// its character device. Closing discards bytes from the previous session.
    pub fn set_open(&mut self, open: bool) -> Result<(), ConfigurationError> {
        if self.host_open == open {
            return Ok(());
        }
        if self.multiport && self.port_ready {
            self.enqueue(PORT_OPEN, u16::from(open), &[])?;
        }
        self.host_open = open;
        if !open {
            self.discard_session();
        }
        Ok(())
    }

    /// Nonblocking host send. A short result leaves the remainder with the
    /// caller; a closed peer consumes nothing.
    pub fn send(&mut self, bytes: &[u8]) -> usize {
        if !self.host_open || !self.guest_open() || self.guest_disconnect || self.failed {
            return 0;
        }
        let length = bytes.len().min(BUFFER_BYTES - self.to_guest.len());
        self.to_guest.extend(&bytes[..length]);
        length
    }

    /// Nonblocking host receive. Call service again after draining to unblock
    /// guest transmit descriptors that were retained for backpressure.
    pub fn receive(&mut self, bytes: &mut [u8]) -> usize {
        let length = bytes.len().min(self.from_guest.len());
        for output in &mut bytes[..length] {
            if let Some(byte) = self.from_guest.pop_front() {
                *output = byte;
            }
        }
        length
    }

    /// Processes a bounded amount of work from every active queue. Notification
    /// suppression is intentionally never enabled, so guest kick publication
    /// and the caller's level-triggered wait cannot lose an arm/recheck race.
    pub fn service<M: GuestMemory>(&mut self, memory: &M) -> Result<Progress, Error<M::Error>> {
        if self.failed {
            return Err(Error::Failed);
        }
        let Some(mut rings) = self.rings.take() else {
            return Ok(Progress::default());
        };
        let result = self.service_rings(memory, &mut rings);
        self.rings = Some(rings);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn service_rings<M: GuestMemory>(
        &mut self,
        memory: &M,
        rings: &mut [Ring; 4],
    ) -> Result<Progress, Error<M::Error>> {
        let mut progress = Progress::default();
        let count = if self.multiport { 4 } else { 2 };
        for ring in &mut rings[..count] {
            ring.initialize(memory)?;
        }
        if self.multiport {
            self.control_transmit(memory, &mut rings[3], &mut progress)?;
            self.control_receive(memory, &mut rings[2], &mut progress)?;
            // Receiving control messages may have removed the only reason the
            // transmit side stopped. There need not be another guest kick.
            if progress.completed != 0
                && self.control.len() <= MAX_CONTROL - 2
                && rings[3].peek(memory, false)?.is_some()
            {
                progress.more = true;
            }
        }
        if self.host_open && self.guest_open() && !self.guest_disconnect {
            self.data_transmit(memory, &mut rings[1], &mut progress)?;
            self.data_receive(memory, &mut rings[0], &mut progress)?;
        } else if let Some((chain, _)) = self.transmit.take() {
            // Closing a stream abandons its partially accepted chain. Return
            // ownership to the driver without replaying its tail on reopen.
            progress.interrupt |= rings[1].complete(memory, chain, 0)?;
            progress.completed += 1;
        }
        Ok(progress)
    }

    fn control_transmit<M: GuestMemory>(
        &mut self,
        memory: &M,
        ring: &mut Ring,
        progress: &mut Progress,
    ) -> Result<(), Error<M::Error>> {
        for step in 0..SERVICE_BUDGET {
            // PORT_READY produces two messages. Reserve capacity before taking
            // any input, so a full control receive queue applies backpressure.
            if self.control.len() > MAX_CONTROL - 2 {
                break;
            }
            let Some(chain) = ring.peek(memory, false)? else {
                break;
            };
            if chain.len() != 8 {
                return Err(Error::Control);
            }
            let mut bytes = [0; 8];
            chain.read(memory, &mut bytes)?;
            let id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            let event = u16::from_le_bytes([bytes[4], bytes[5]]);
            let value = u16::from_le_bytes([bytes[6], bytes[7]]);
            match (id, event, value) {
                (u32::MAX, DEVICE_READY, 1) => {
                    self.device_ready = true;
                    self.enqueue(PORT_ADD, 1, &[]).map_err(|_| Error::Control)?;
                }
                (0, PORT_READY, 1) if self.device_ready => {
                    self.port_ready = true;
                    let name = self.name.clone();
                    self.enqueue(PORT_NAME, 1, name.as_bytes())
                        .map_err(|_| Error::Control)?;
                    self.enqueue(PORT_OPEN, u16::from(self.host_open), &[])
                        .map_err(|_| Error::Control)?;
                }
                (0, PORT_OPEN, 0 | 1) if self.port_ready => {
                    self.guest_disconnect |= self.guest_open && value == 0;
                    self.guest_open = value != 0;
                    if !self.guest_open {
                        self.discard_session();
                    }
                }
                (0, PORT_READY, 0) => {
                    self.guest_disconnect = true;
                    self.port_ready = false;
                    self.guest_open = false;
                    self.control.clear();
                    self.discard_session();
                }
                (u32::MAX, DEVICE_READY, 0) => {
                    self.guest_disconnect = true;
                    self.device_ready = false;
                    self.port_ready = false;
                    self.guest_open = false;
                    self.control.clear();
                    self.discard_session();
                }
                _ => (),
            }
            progress.interrupt |= ring.complete(memory, chain, 0)?;
            progress.completed += 1;
            progress.more |= step + 1 == SERVICE_BUDGET;
        }
        Ok(())
    }

    fn control_receive<M: GuestMemory>(
        &mut self,
        memory: &M,
        ring: &mut Ring,
        progress: &mut Progress,
    ) -> Result<(), Error<M::Error>> {
        for step in 0..SERVICE_BUDGET {
            let Some(message) = self.control.front() else {
                break;
            };
            let Some(chain) = ring.peek(memory, true)? else {
                break;
            };
            if chain.len() < message.len() {
                return Err(Error::Control);
            }
            chain.write(memory, message)?;
            progress.interrupt |= ring.complete(memory, chain, message.len())?;
            self.control.pop_front();
            progress.completed += 1;
            progress.more |= step + 1 == SERVICE_BUDGET;
        }
        Ok(())
    }

    fn data_transmit<M: GuestMemory>(
        &mut self,
        memory: &M,
        ring: &mut Ring,
        progress: &mut Progress,
    ) -> Result<(), Error<M::Error>> {
        for step in 0..SERVICE_BUDGET {
            let (chain, offset) = if let Some(pending) = self.transmit.take() {
                pending
            } else if let Some(chain) = ring.peek(memory, false)? {
                (chain, 0)
            } else {
                break;
            };
            let length = (chain.len() - offset).min(BUFFER_BYTES - self.from_guest.len());
            if length == 0 && offset != chain.len() {
                self.transmit = Some((chain, offset));
                break;
            }
            let mut bytes = vec![0; length];
            chain.read_at(memory, offset, &mut bytes)?;
            let next = offset + length;
            if next == chain.len() {
                progress.interrupt |= ring.complete(memory, chain, 0)?;
                progress.completed += 1;
            } else {
                self.transmit = Some((chain, next));
            }
            self.from_guest.extend(bytes);
            progress.more |= step + 1 == SERVICE_BUDGET;
        }
        Ok(())
    }

    fn data_receive<M: GuestMemory>(
        &mut self,
        memory: &M,
        ring: &mut Ring,
        progress: &mut Progress,
    ) -> Result<(), Error<M::Error>> {
        for step in 0..SERVICE_BUDGET {
            if self.to_guest.is_empty() {
                break;
            }
            let Some(chain) = ring.peek(memory, true)? else {
                break;
            };
            let length = chain.len().min(self.to_guest.len());
            let bytes: Vec<_> = self.to_guest.iter().take(length).copied().collect();
            chain.write(memory, &bytes)?;
            progress.interrupt |= ring.complete(memory, chain, length)?;
            self.to_guest.drain(..length);
            progress.completed += 1;
            progress.more |= step + 1 == SERVICE_BUDGET;
        }
        Ok(())
    }

    fn enqueue(
        &mut self,
        event: u16,
        value: u16,
        payload: &[u8],
    ) -> Result<(), ConfigurationError> {
        if self.control.len() == MAX_CONTROL {
            return Err(ConfigurationError::ControlFull);
        }
        let mut message = Vec::with_capacity(8 + payload.len());
        message.extend(0u32.to_le_bytes());
        message.extend(event.to_le_bytes());
        message.extend(value.to_le_bytes());
        message.extend(payload);
        self.control.push_back(message);
        Ok(())
    }

    fn discard_session(&mut self) {
        self.to_guest.clear();
        self.from_guest.clear();
        if let Some((chain, offset)) = &mut self.transmit {
            *offset = chain.len();
        }
    }
}

#[cfg(test)]
#[path = "../tests/virtio_console.rs"]
mod tests;
