// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One `PCIe` root port with a standard hotplug slot and a downstream bus.
//!
//! Configuration is owned by one MMIO consumer. The 32-byte state is transferred
//! only after that consumer joins; pending exits remain in the vCPU endpoints.

pub const ECAM_BASE: u64 = 0x0a60_0000;
pub const ECAM_BYTES: u64 = 2 << 20;
pub const MEMORY_BASE: u64 = 0x0aa0_0000;
pub const MEMORY_BYTES: u64 = 2 << 20;
pub const HOTPLUG_IRQ: u32 = 120;
pub const ENDPOINT_IRQ: u32 = 121;
pub const ECAM_COOKIE: u64 = 0x5043_4945;
pub const MEMORY_COOKIE: u64 = 0x5043_494d;
pub const STATE_BYTES: usize = 32;
const CAP: usize = 0x40;
const COMMAND_MASK: u16 = 0x0406;
const SLOT_CONTROL_MASK: u16 = 0x1028;
const SLOT_EVENTS: u16 = 0x0108;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootPort {
    state: [u8; STATE_BYTES],
}
impl Default for RootPort {
    fn default() -> Self {
        let mut state = [0; STATE_BYTES];
        state[3] = 1; // Secondary bus.
        state[4] = 1; // Subordinate bus.
        state[25] = HOTPLUG_IRQ as u8;
        state[8..10].copy_from_slice(&((MEMORY_BASE >> 16) as u16).to_le_bytes());
        state[10..12].copy_from_slice(
            &(((MEMORY_BASE + MEMORY_BYTES - 1) >> 16) as u16 & 0xfff0).to_le_bytes(),
        );
        Self { state }
    }
}
impl RootPort {
    /// Fixed wire representation, independent of host layout or pointers.
    pub const fn state(&self) -> [u8; STATE_BYTES] {
        self.state
    }
    pub fn from_state(bytes: &[u8]) -> Option<Self> {
        let state: [u8; STATE_BYTES] = bytes.try_into().ok()?;
        let port = Self { state };
        if port.word(0) & !COMMAND_MASK != 0
            || port.word(8) & 15 != 0
            || port.word(10) & 15 != 0
            || port.word(12) & !0x0040 != 0
            || port.word(14) & !0x70e0 != 0
            || port.word(16) & !0x0010 != 0
            || port.word(18) & !SLOT_CONTROL_MASK != 0
            || port.word(20) & !SLOT_EVENTS != 0
            || port.word(22) != 0
            || port.state[24] > 1
            || port.state[26..].iter().any(|b| *b != 0)
        {
            return None;
        }
        Some(port)
    }
    fn word(&self, at: usize) -> u16 {
        u16::from_le_bytes([self.state[at], self.state[at + 1]])
    }
    fn set_word(&mut self, at: usize, value: u16) {
        self.state[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }
    pub fn present(&self) -> bool {
        self.state[24] != 0
    }
    pub fn link_active(&self) -> bool {
        self.present() && self.word(16) & 0x10 == 0 && !self.secondary_reset()
    }
    /// Upstream DMA is permitted only while the bridge bus-master bit is set.
    pub fn bus_master_enabled(&self) -> bool {
        self.word(0) & 4 != 0
    }
    pub fn secondary_reset(&self) -> bool {
        self.word(12) & 0x40 != 0
    }
    /// Publish only after the endpoint, memory consumer and interrupt exist.
    pub fn set_present(&mut self, present: bool) {
        let old_link = self.link_active();
        if self.present() != present {
            self.state[24] = u8::from(present);
            self.set_word(20, self.word(20) | 8);
        }
        if old_link != self.link_active() {
            self.set_word(20, self.word(20) | 0x100);
        }
    }
    fn interrupt_pending(&self) -> bool {
        let enabled = self.word(18);
        enabled & 0x20 != 0
            && ((enabled & 8 != 0 && self.word(20) & 8 != 0)
                || (enabled & 0x1000 != 0 && self.word(20) & 0x100 != 0))
    }
    pub fn irq_asserted(&self) -> bool {
        self.word(0) & 0x400 == 0 && self.interrupt_pending()
    }
    pub fn routes_endpoint(&self, bus: u8, device: u8, function: u8) -> bool {
        self.link_active()
            && bus != 0
            && bus == self.state[3]
            && bus <= self.state[4]
            && device == 0
            && function == 0
    }
    pub fn forwards_memory(&self, address: u64, width: u8) -> bool {
        let base = u64::from(self.word(8) & 0xfff0) << 16;
        let end = (u64::from(self.word(10) & 0xfff0) << 16) + (1 << 20);
        self.link_active()
            && self.word(0) & 2 != 0
            && address >= base
            && address
                .checked_add(u64::from(width))
                .is_some_and(|limit| limit <= end)
    }
    pub fn read(&self, offset: u64, width: u8) -> Option<u64> {
        if !valid_access(offset, width) {
            return None;
        }
        let mut bytes = [0u8; 256];
        bytes[0..4].copy_from_slice(&[0x36, 0x1b, 0x0c, 0]);
        bytes[4..6].copy_from_slice(&self.state[0..2]);
        bytes[6] = 0x10 | if self.interrupt_pending() { 8 } else { 0 };
        bytes[8..12].copy_from_slice(&[1, 0, 4, 6]);
        bytes[12..14].copy_from_slice(&self.state[6..8]);
        bytes[14] = 1; // Type-1 bridge.
        bytes[0x18..0x1c].copy_from_slice(&self.state[2..6]);
        bytes[0x20..0x24].copy_from_slice(&self.state[8..12]);
        // I/O and prefetchable windows are not implemented.
        bytes[0x34] = CAP as u8;
        bytes[0x3c] = self.state[25];
        bytes[0x3d] = 1;
        bytes[0x3e..0x40].copy_from_slice(&self.state[12..14]);
        bytes[CAP..CAP + 4].copy_from_slice(&[0x10, 0, 0x42, 1]); // v2 root port, slot.
        bytes[CAP + 8..CAP + 10].copy_from_slice(&self.state[14..16]);
        bytes[CAP + 12..CAP + 16].copy_from_slice(&0x0110_0011u32.to_le_bytes()); // x1, 2.5GT/s, DLLLA reporting.
        bytes[CAP + 16..CAP + 18].copy_from_slice(&self.state[16..18]);
        let link: u16 = if self.link_active() { 0x2011 } else { 0 };
        bytes[CAP + 18..CAP + 20].copy_from_slice(&link.to_le_bytes());
        bytes[CAP + 20..CAP + 24].copy_from_slice(&0x000c_0060u32.to_le_bytes()); // Slot 1, HPC, surprise, NCCS.
        bytes[CAP + 24..CAP + 26].copy_from_slice(&self.state[18..20]);
        let status = self.word(20) | if self.present() { 0x40 } else { 0 };
        bytes[CAP + 26..CAP + 28].copy_from_slice(&status.to_le_bytes());
        let mut value = 0;
        for index in 0..width {
            value |= u64::from(
                bytes
                    .get(offset as usize + usize::from(index))
                    .copied()
                    .unwrap_or(0),
            ) << (8 * index);
        }
        Some(value)
    }
    pub fn write(&mut self, offset: u64, width: u8, value: u64) -> Option<()> {
        if !valid_access(offset, width) || value >> (8 * width) != 0 {
            return None;
        }
        let old_link = self.link_active();
        for index in 0..width {
            let address = offset as usize + usize::from(index);
            let byte = (value >> (8 * index)) as u8;
            let target = match address {
                4..=5 => Some(address - 4),
                12..=13 => Some(address - 6),
                0x18..=0x1b => Some(address - 0x16),
                0x20..=0x23 => Some(address - 0x18),
                0x3c => Some(25),
                0x3e..=0x3f => Some(address - 0x32),
                0x48..=0x49 => Some(address - 0x3a),
                0x50..=0x51 => Some(address - 0x40),
                0x58..=0x59 => Some(address - 0x46),
                0x5a..=0x5b => {
                    self.state[address - 0x46] &= !byte;
                    None
                }
                _ => None,
            };
            if let Some(target) = target {
                self.state[target] = byte;
            }
        }
        for (offset, mask) in [
            (0, COMMAND_MASK),
            (8, 0xfff0),
            (10, 0xfff0),
            (12, 0x40),
            (14, 0x70e0),
            (16, 0x10),
            (18, SLOT_CONTROL_MASK),
        ] {
            self.set_word(offset, self.word(offset) & mask);
        }
        if old_link != self.link_active() {
            self.set_word(20, self.word(20) | 0x100);
        }
        Some(())
    }
}
/// Decode all ECAM functions, including absent ones; absence is all-ones data.
pub fn decode_ecam(address: u64) -> Option<(u8, u8, u8, u64)> {
    let relative = address.checked_sub(ECAM_BASE)?;
    if relative >= ECAM_BYTES {
        return None;
    }
    Some((
        (relative >> 20) as u8,
        ((relative >> 15) & 31) as u8,
        ((relative >> 12) & 7) as u8,
        relative & 4095,
    ))
}
pub fn absent(width: u8) -> Option<u64> {
    match width {
        1 | 2 | 4 => Some((1u64 << (width * 8)) - 1),
        8 => Some(u64::MAX),
        _ => None,
    }
}
fn valid_access(offset: u64, width: u8) -> bool {
    matches!(width, 1 | 2 | 4)
        && offset.is_multiple_of(u64::from(width))
        && offset
            .checked_add(u64::from(width))
            .is_some_and(|end| end <= 4096)
}

#[cfg(test)]
#[path = "../tests/pcie.rs"]
mod tests;
