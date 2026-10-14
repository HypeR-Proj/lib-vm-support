// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Type-0 PCI configuration header with one relocatable 32-bit memory BAR.
//!
//! The bus owns address decoding. A BAR write only changes configuration;
//! the caller must publish the corresponding notification route before retiring
//! the guest instruction. BAR sizing probes never move the live resource.

pub const BAR_SIZE: u64 = 4096;
pub const MEMORY: u16 = 2;
pub const BUS_MASTER: u16 = 4;
pub const INTX_DISABLE: u16 = 1 << 10;

pub struct Function {
    bytes: Box<[u8; 256]>,
    bar: u32,
    probe: bool,
    command: u16,
}

impl Function {
    pub fn new(device: u16, class: u32, bar: u32, capabilities: &[u8]) -> Self {
        let mut bytes = Box::new([0; 256]);
        bytes[..2].copy_from_slice(&0x1af4u16.to_le_bytes());
        bytes[2..4].copy_from_slice(&device.to_le_bytes());
        bytes[6] = 0x10; // Capability list.
        bytes[8] = 1; // Modern-only virtio revision.
        bytes[9..12].copy_from_slice(&class.to_le_bytes()[..3]);
        bytes[0x2c..0x2e].copy_from_slice(&0x1af4u16.to_le_bytes());
        bytes[0x2e..0x30].copy_from_slice(&device.to_le_bytes());
        bytes[0x34] = 0x40;
        bytes[0x3d] = 1; // INTA, routed by the host's interrupt-map.
        let length = capabilities.len().min(192);
        bytes[0x40..0x40 + length].copy_from_slice(&capabilities[..length]);
        Self {
            bytes,
            bar: bar & !0xfff,
            probe: false,
            command: 0,
        }
    }

    pub const fn bar(&self) -> u64 {
        self.bar as u64
    }
    pub const fn command(&self) -> u16 {
        self.command
    }

    pub fn read(&self, offset: u64, width: u8) -> Option<u64> {
        if !valid_access(offset, width) {
            return None;
        }
        let mut value = 0u64;
        for index in 0..width {
            let address = offset + u64::from(index);
            let byte = match address {
                4..=5 => self.command.to_le_bytes()[(address - 4) as usize],
                0x10..=0x13 => {
                    let bar = if self.probe { 0xffff_f000u32 } else { self.bar };
                    bar.to_le_bytes()[(address - 0x10) as usize]
                }
                _ => self.bytes.get(address as usize).copied().unwrap_or(0),
            };
            value |= u64::from(byte) << (8 * index);
        }
        Some(value)
    }

    pub fn write(&mut self, offset: u64, width: u8, value: u64) -> Option<()> {
        if !valid_access(offset, width) || value >> (8 * width) != 0 {
            return None;
        }
        if offset == 0x10 && width == 4 {
            self.probe = value == u64::from(u32::MAX);
            if !self.probe {
                self.bar = value as u32 & !0xfff;
            }
            return Some(());
        }
        // Header/capabilities are immutable except command and the conventional
        // software-owned cache-line/latency/interrupt-line bytes. No I/O BARs,
        // ROM, bus mastering by default, MSI-X or legacy virtio registers.
        for index in 0..width {
            let address = offset + u64::from(index);
            let byte = (value >> (8 * index)) as u8;
            match address {
                4..=5 => {
                    let shift = 8 * (address - 4);
                    self.command = (self.command & !(0xff << shift)) | (u16::from(byte) << shift);
                    self.command &= MEMORY | BUS_MASTER | INTX_DISABLE;
                }
                0x0c | 0x0d | 0x3c => self.bytes[address as usize] = byte,
                0x10..=0x13 => {
                    self.probe = false;
                    let shift = 8 * (address - 0x10);
                    self.bar = (self.bar & !(0xff << shift)) | (u32::from(byte) << shift);
                }
                _ => {}
            }
        }
        self.bar &= !0xfff;
        Some(())
    }
}

fn valid_access(offset: u64, width: u8) -> bool {
    matches!(width, 1 | 2 | 4)
        && offset.is_multiple_of(u64::from(width))
        && offset
            .checked_add(u64::from(width))
            .is_some_and(|end| end <= 4096)
}
