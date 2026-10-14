// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::virtio::Queue;
use crate::virtqueue::GuestMemory;
use std::cell::RefCell;

pub const BASE: u64 = 0x4000_0000;
pub const SIZE: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fault;

pub struct Memory {
    bytes: RefCell<Vec<u8>>,
    pub fail_write: RefCell<Option<u64>>,
}

impl Memory {
    pub fn new() -> Self {
        Self {
            bytes: RefCell::new(vec![0; SIZE as usize]),
            fail_write: RefCell::new(None),
        }
    }
    pub fn queue(index: usize) -> Queue {
        let descriptor = BASE + index as u64 * 4096;
        Queue {
            size: 8,
            descriptor,
            available: descriptor + 256,
            used: descriptor + 512,
            ready: true,
        }
    }
    pub fn descriptor(
        &self,
        queue: Queue,
        index: u16,
        address: u64,
        length: u32,
        flags: u16,
        next: u16,
    ) -> Result<(), Fault> {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&address.to_le_bytes());
        bytes[8..12].copy_from_slice(&length.to_le_bytes());
        bytes[12..14].copy_from_slice(&flags.to_le_bytes());
        bytes[14..].copy_from_slice(&next.to_le_bytes());
        self.write(queue.descriptor + u64::from(index) * 16, &bytes)
    }
    pub fn publish(&self, queue: Queue, index: u16, head: u16) -> Result<(), Fault> {
        let slot = u64::from(index) % u64::from(queue.size);
        self.store_u16_release(queue.available + 4 + 2 * slot, head)?;
        self.store_u16_release(queue.available + 2, index.wrapping_add(1))
    }
    pub fn used(&self, queue: Queue) -> Result<u16, Fault> {
        self.load_u16_acquire(queue.used + 2)
    }
    pub fn bytes(&self, address: u64, length: usize) -> Result<Vec<u8>, Fault> {
        let mut bytes = vec![0; length];
        for (index, chunk) in bytes.chunks_mut(4096).enumerate() {
            self.read(address + (index * 4096) as u64, chunk)?;
        }
        Ok(bytes)
    }
}

impl GuestMemory for Memory {
    type Error = Fault;

    fn read(&self, address: u64, bytes: &mut [u8]) -> Result<(), Fault> {
        if bytes.len() > 4096 {
            return Err(Fault);
        }
        let offset = usize::try_from(address.checked_sub(BASE).ok_or(Fault)?).map_err(|_| Fault)?;
        let end = offset.checked_add(bytes.len()).ok_or(Fault)?;
        bytes.copy_from_slice(self.bytes.borrow().get(offset..end).ok_or(Fault)?);
        Ok(())
    }
    fn write(&self, address: u64, bytes: &[u8]) -> Result<(), Fault> {
        if bytes.len() > 4096 || *self.fail_write.borrow() == Some(address) {
            return Err(Fault);
        }
        let offset = usize::try_from(address.checked_sub(BASE).ok_or(Fault)?).map_err(|_| Fault)?;
        let end = offset.checked_add(bytes.len()).ok_or(Fault)?;
        self.bytes
            .borrow_mut()
            .get_mut(offset..end)
            .ok_or(Fault)?
            .copy_from_slice(bytes);
        Ok(())
    }
    fn load_u16_acquire(&self, address: u64) -> Result<u16, Fault> {
        if address & 1 != 0 {
            return Err(Fault);
        }
        let mut bytes = [0; 2];
        self.read(address, &mut bytes)?;
        Ok(u16::from_le_bytes(bytes))
    }
    fn store_u16_release(&self, address: u64, value: u16) -> Result<(), Fault> {
        if address & 1 != 0 {
            return Err(Fault);
        }
        self.write(address, &value.to_le_bytes())
    }
}
