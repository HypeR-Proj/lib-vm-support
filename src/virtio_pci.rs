// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Modern virtio PCI transport, using `INTx` and split virtqueues.
//!
//! One memory BAR contains common configuration, notification, ISR and device
//! configuration capabilities. The kernel implements only the notification/ISR
//! accesses; feature negotiation and all queue/device policy stay here.

use crate::pci;
use crate::virtio::{self, DeviceKind, Error, QUEUE_MAX, Register, Transaction};

pub const NOTIFY_OFFSET: u64 = 0x50;
pub const ISR_OFFSET: u64 = 0x60;
pub const DEVICE_OFFSET: u64 = 0x100;
const CONFIG_ACCESS: u64 = 0x84;
pub const CONFIG_DATA: u64 = CONFIG_ACCESS + 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigurationWindow {
    pub offset: u64,
    pub width: u8,
}

pub struct Device {
    function: pci::Function,
    core: virtio::Device,
    config_bar: u8,
    config_offset: u32,
    config_length: u32,
    config_data: u32,
}

impl Device {
    pub fn new(core: virtio::Device, bar: u32) -> Self {
        let (id, class, length) = match core.kind {
            DeviceKind::Scsi => (0x1048, 0x010000, 36),
            DeviceKind::Network => (0x1041, 0x020000, 6),
            DeviceKind::Console => (0x1043, 0x078000, 12),
        };
        let mut capabilities = [0u8; 148];
        for (start, next, kind, offset, size) in [
            (0, 0x50, 1, 0, 56u32),
            (16, 0x64, 2, NOTIFY_OFFSET as u32, 2),
            (36, 0x74, 3, ISR_OFFSET as u32, 1),
            (52, 0x84, 4, DEVICE_OFFSET as u32, length),
            (68, 0, 5, 0, 0),
        ] {
            let cap = &mut capabilities[start..start + 16];
            cap[0] = 9; // PCI_CAP_ID_VNDR
            cap[1] = next;
            cap[2] = if kind == 2 || kind == 5 { 20 } else { 16 };
            cap[3] = kind;
            cap[8..12].copy_from_slice(&offset.to_le_bytes());
            cap[12..16].copy_from_slice(&size.to_le_bytes());
        }
        let capability_length = if core.kind == DeviceKind::Console {
            // A hotplugged downstream function identifies itself as a PCIe
            // endpoint. Existing non-PCIe disk/network functions retain their
            // exact capability chain.
            capabilities[69] = 0x98;
            capabilities[88] = 0x10;
            capabilities[90..92].copy_from_slice(&2u16.to_le_bytes());
            capabilities[100..104].copy_from_slice(&0x11u32.to_le_bytes());
            capabilities[106..108].copy_from_slice(&0x11u16.to_le_bytes());
            capabilities.len()
        } else {
            88
        };
        // notify_off_multiplier=0: every queue writes its index to one doorbell.
        let mut function = pci::Function::new(id, class, bar, &capabilities[..capability_length]);
        if core.kind == DeviceKind::Console {
            // Downstream slot zero preserves the pin during bridge swizzling.
            // INTB keeps the endpoint separate from the root port's INTA.
            function.use_interrupt_b();
        }
        Self {
            function,
            core,
            config_bar: 0,
            config_offset: 0,
            config_length: 0,
            config_data: 0,
        }
    }

    pub fn function(&self) -> &pci::Function {
        &self.function
    }

    /// The mandatory `PCI_CFG` window accesses the same registers as BAR0,
    /// including read-to-clear ISR and queue notifications. It works regardless
    /// of PCI memory decoding; the caller forwards hot registers to the kernel.
    pub fn configuration_window(
        &self,
        offset: u64,
        width: u8,
    ) -> Result<Option<ConfigurationWindow>, Error> {
        if !(CONFIG_DATA..CONFIG_DATA + 4).contains(&offset) {
            return Ok(None);
        }
        if !matches!(width, 1 | 2 | 4)
            || !offset.is_multiple_of(u64::from(width))
            || offset + u64::from(width) > CONFIG_DATA + 4
            || !matches!(self.config_length, 1 | 2 | 4)
            || self.config_bar != 0
            || !self.config_offset.is_multiple_of(self.config_length)
            || u64::from(self.config_offset) + u64::from(self.config_length) > pci::BAR_SIZE
        {
            return Err(Error::InvalidAccess);
        }
        Ok(Some(ConfigurationWindow {
            offset: u64::from(self.config_offset),
            width: self.config_length as u8,
        }))
    }

    /// `PCI_CFG` first latches the configuration-space write, then transfers the
    /// first cap.length bytes to BAR0. The two access widths are independent.
    pub fn configuration_write_value(
        &mut self,
        offset: u64,
        width: u8,
        value: u64,
    ) -> Result<u64, Error> {
        let window = self
            .configuration_window(offset, width)?
            .ok_or(Error::InvalidAccess)?;
        if value & !byte_mask(width) != 0 {
            return Err(Error::InvalidAccess);
        }
        let shift = 8 * (offset - CONFIG_DATA);
        let mask = byte_mask(width) << shift;
        self.config_data = ((u64::from(self.config_data) & !mask) | (value << shift)) as u32;
        Ok(u64::from(self.config_data) & byte_mask(window.width))
    }

    /// A BAR read fills the first cap.length bytes before returning the bytes
    /// addressed by the PCI read. Untouched data bytes keep their previous value.
    pub fn configuration_read_result(
        &mut self,
        offset: u64,
        width: u8,
        value: u64,
    ) -> Result<u64, Error> {
        let window = self
            .configuration_window(offset, width)?
            .ok_or(Error::InvalidAccess)?;
        let mask = byte_mask(window.width);
        self.config_data = ((u64::from(self.config_data) & !mask) | (value & mask)) as u32;
        Ok((u64::from(self.config_data) >> (8 * (offset - CONFIG_DATA))) & byte_mask(width))
    }

    pub fn read_configuration(
        &self,
        offset: u64,
        width: u8,
        interrupt: bool,
    ) -> Result<u64, Error> {
        let mut value = self
            .function
            .read(offset, width)
            .ok_or(Error::InvalidAccess)?;
        for index in 0..width {
            let address = offset + u64::from(index);
            let byte = match address {
                address if address == CONFIG_ACCESS + 4 => Some(self.config_bar),
                0x8c..=0x8f => Some(self.config_offset.to_le_bytes()[(address - 0x8c) as usize]),
                0x90..=0x93 => Some(self.config_length.to_le_bytes()[(address - 0x90) as usize]),
                _ => None,
            };
            if let Some(byte) = byte {
                value = (value & !(0xff << (8 * index))) | (u64::from(byte) << (8 * index));
            }
            if address == 6 && interrupt {
                value |= 8 << (8 * index); // PCI Status.Interrupt Status, even when INTx masked.
            }
        }
        Ok(value)
    }

    pub fn write_configuration(
        &mut self,
        offset: u64,
        width: u8,
        value: u64,
    ) -> Result<Option<Transaction>, Error> {
        self.core.admit_write()?;
        let command = self.function.command();
        self.function
            .write(offset, width, value)
            .ok_or(Error::InvalidAccess)?;
        for index in 0..width {
            let address = offset + u64::from(index);
            let byte = (value >> (8 * index)) as u8;
            match address {
                address if address == CONFIG_ACCESS + 4 => self.config_bar = byte,
                0x8c..=0x8f => replace_byte(&mut self.config_offset, address - 0x8c, byte),
                0x90..=0x93 => replace_byte(&mut self.config_length, address - 0x90, byte),
                _ => {}
            }
        }
        if command & pci::BUS_MASTER != 0 && self.function.command() & pci::BUS_MASTER == 0 {
            self.core.stop_bus_master()
        } else {
            Ok(None)
        }
    }

    pub fn backend_lost(&mut self) {
        self.core.backend_lost();
    }
    /// A local backend stopped after a malformed queue. Unlike remote backend
    /// loss, an ordinary status-zero reset may recover this device.
    pub fn needs_reset(&mut self) {
        self.core.status |= 64;
    }
    pub fn complete(&mut self, id: u64, success: bool) -> Result<(), Error> {
        self.core.complete(id, success)
    }

    pub fn read(&self, offset: u64, width: u8) -> Result<u64, Error> {
        if offset >= DEVICE_OFFSET {
            return self.core.read_config(offset - DEVICE_OFFSET, width);
        }
        let queue = self.core.queues().get(self.core.queue_selector as usize);
        let value = match (offset, width) {
            (0, 4) => u64::from(self.core.device_selector),
            (4, 4) => {
                if self.core.device_selector < 2 {
                    (self.core.offered >> (32 * self.core.device_selector)) & 0xffff_ffff
                } else {
                    0
                }
            }
            (8, 4) => u64::from(self.core.driver_selector),
            (12, 4) => {
                if self.core.driver_selector < 2 {
                    (self.core.features >> (32 * self.core.driver_selector)) & 0xffff_ffff
                } else {
                    0
                }
            }
            (16, 2) | (26, 2) => 0xffff, // No MSI-X capability.
            (18, 2) => self.core.queues().len() as u64,
            (20, 1) => u64::from(self.core.status),
            (21, 1) | (30, 2) => 0, // Stable configuration, shared doorbell.
            (22, 2) => u64::from(self.core.queue_selector),
            (24, 2) => queue.map_or(0, |queue| {
                u64::from(if queue.size == 0 {
                    QUEUE_MAX
                } else {
                    queue.size
                })
            }),
            (28, 2) => u64::from(queue.is_some_and(|queue| queue.ready)),
            (32..=52, 4) if offset.is_multiple_of(4) => {
                let queue = queue.ok_or(Error::InvalidQueue)?;
                let address = match offset / 8 {
                    4 => queue.descriptor,
                    5 => queue.available,
                    _ => queue.used,
                };
                (address >> (32 * ((offset / 4) & 1))) & 0xffff_ffff
            }
            (32 | 40 | 48, 8) => {
                let queue = queue.ok_or(Error::InvalidQueue)?;
                match offset {
                    32 => queue.descriptor,
                    40 => queue.available,
                    _ => queue.used,
                }
            }
            _ => return Err(Error::InvalidAccess),
        };
        Ok(value)
    }

    pub fn write(
        &mut self,
        offset: u64,
        width: u8,
        value: u64,
    ) -> Result<Option<Transaction>, Error> {
        if offset >= DEVICE_OFFSET {
            return self
                .core
                .write_config(offset - DEVICE_OFFSET, width, value)
                .map(|()| None);
        }
        if width < 8 && value >> (8 * width) != 0 {
            return Err(Error::InvalidAccess);
        }
        let register = match (offset, width) {
            (0, 4) => Register::DeviceSelector,
            (8, 4) => Register::DriverSelector,
            (12, 4) => Register::Features,
            (16 | 26, 2) => return Ok(None), // MSI-X absent: readback is NO_VECTOR.
            (20, 1) => {
                if value & 4 != 0 && self.function.command() & pci::BUS_MASTER == 0 {
                    return Err(Error::InvalidStatus);
                }
                Register::Status
            }
            (22, 2) => Register::QueueSelector,
            (24, 2) => Register::QueueSize,
            (28, 2) => Register::QueueReady,
            (32, 4) => Register::DescriptorLow,
            (36, 4) => Register::DescriptorHigh,
            (40, 4) => Register::AvailableLow,
            (44, 4) => Register::AvailableHigh,
            (48, 4) => Register::UsedLow,
            (52, 4) => Register::UsedHigh,
            (32 | 40 | 48, 8) => {
                let (low, high) = match offset {
                    32 => (Register::DescriptorLow, Register::DescriptorHigh),
                    40 => (Register::AvailableLow, Register::AvailableHigh),
                    _ => (Register::UsedLow, Register::UsedHigh),
                };
                // Both writes have identical admission: no transaction can be
                // started between them, and no guest sees partially set state.
                self.core.write(low, value as u32)?;
                return self.core.write(high, (value >> 32) as u32);
            }
            _ => return Err(Error::InvalidAccess),
        };
        self.core.write(register, value as u32)
    }
}

fn replace_byte(word: &mut u32, index: u64, byte: u8) {
    let shift = index * 8;
    *word = (*word & !(0xff << shift)) | (u32::from(byte) << shift);
}

fn byte_mask(width: u8) -> u64 {
    (1u64 << (8 * width)) - 1
}

#[cfg(test)]
#[path = "../tests/virtio_pci.rs"]
mod tests;
