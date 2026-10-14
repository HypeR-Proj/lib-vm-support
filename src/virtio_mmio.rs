// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Modern virtio-MMIO registers. Queue kicks and IRQ status use the kernel route.

use crate::virtio::{self, DeviceKind, Error, QUEUE_MAX, Register, Transaction};

pub struct Device {
    core: virtio::Device,
}

impl Device {
    pub fn new(features: u64, base: u64, size: u64) -> Result<Self, Error> {
        virtio::Device::new(features, base, size).map(|core| Self { core })
    }
    pub fn network(
        features: u64,
        base: u64,
        size: u64,
        config: crate::virtio_net::Configuration,
    ) -> Result<Self, Error> {
        virtio::Device::network(features, base, size, config).map(|core| Self { core })
    }
    pub(crate) fn into_core(self) -> virtio::Device {
        self.core
    }
    pub fn complete(&mut self, id: u64, success: bool) -> Result<(), Error> {
        self.core.complete(id, success)
    }
    pub fn backend_lost(&mut self) {
        self.core.backend_lost();
    }
    pub fn read(&self, offset: u64, width: u8) -> Result<u64, Error> {
        if offset >= 0x100 {
            return self.core.read_config(offset - 0x100, width);
        }
        if width != 4 || !offset.is_multiple_of(4) {
            return Err(Error::InvalidAccess);
        }
        let queue = self.core.queues().get(self.core.queue_selector as usize);
        let value = match offset {
            0x000 => 0x7472_6976,
            0x004 => 2,
            0x008 => match self.core.kind {
                DeviceKind::Scsi => 8,
                DeviceKind::Network => 1,
            },
            0x00c => 0, // No claimed vendor identifier.
            0x010 => {
                if self.core.device_selector < 2 {
                    (self.core.offered >> (32 * self.core.device_selector)) as u32
                } else {
                    0
                }
            }
            0x034 => {
                if queue.is_some() {
                    QUEUE_MAX
                } else {
                    0
                }
            }
            0x044 => queue.is_some_and(|queue| queue.ready) as u32,
            0x070 => self.core.status,
            0x0b0 | 0x0b4 => u32::MAX, // No shared-memory capability region.
            0x0b8 | 0x0bc | 0x0fc => 0,
            // Hot registers must never silently fall back to an inert model.
            0x050 | 0x060 | 0x064 => return Err(Error::InvalidAccess),
            _ => return Err(Error::InvalidAccess),
        };
        Ok(u64::from(value))
    }

    pub fn write(
        &mut self,
        offset: u64,
        width: u8,
        value: u64,
    ) -> Result<Option<Transaction>, Error> {
        self.core.admit_write()?;
        if width != 4 || !offset.is_multiple_of(4) || value > u64::from(u32::MAX) {
            return Err(Error::InvalidAccess);
        }
        if offset >= 0x100 {
            return self
                .core
                .write_config(offset - 0x100, width, value)
                .map(|()| None);
        }
        let register = match offset {
            0x014 => Register::DeviceSelector,
            0x024 => Register::DriverSelector,
            0x020 => Register::Features,
            0x030 => Register::QueueSelector,
            0x044 => Register::QueueReady,
            0x038 => Register::QueueSize,
            0x080 => Register::DescriptorLow,
            0x084 => Register::DescriptorHigh,
            0x090 => Register::AvailableLow,
            0x094 => Register::AvailableHigh,
            0x0a0 => Register::UsedLow,
            0x0a4 => Register::UsedHigh,
            0x070 => Register::Status,
            0x0ac => return Ok(None),
            _ => return Err(Error::InvalidAccess),
        };
        self.core.write(register, value as u32)
    }
}

#[cfg(test)]
#[path = "../tests/virtio_mmio.rs"]
mod tests;
