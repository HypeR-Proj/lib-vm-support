// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded split-ring access for userspace device backends.
//!
//! Guest bytes are copied, never borrowed as Rust references. Indices use
//! aligned atomic accesses with the virtio publication ordering; descriptor
//! snapshots are validated independently because the guest can mutate RAM.

use crate::virtio::{QUEUE_MAX, Queue};

const NEXT: u16 = 1;
const WRITE: u16 = 2;
const COPY_BYTES: usize = 4096;

/// Access to one retained guest-memory capability. Addresses are guest physical
/// addresses; implementations must reject anything outside that capability.
pub trait GuestMemory {
    type Error;

    fn read(&self, address: u64, bytes: &mut [u8]) -> Result<(), Self::Error>;
    fn write(&self, address: u64, bytes: &[u8]) -> Result<(), Self::Error>;
    fn load_u16_acquire(&self, address: u64) -> Result<u16, Self::Error>;
    /// Publishes the used index after earlier payload/element writes. The
    /// implementation must also order this store before the subsequent load of
    /// available.flags, including on weakly ordered architectures.
    fn store_u16_release(&self, address: u64, value: u16) -> Result<(), Self::Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Invalid {
    Layout,
    AvailableIndex,
    Descriptor,
    Direction,
    TooLarge,
    Length,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Error<E> {
    Memory(E),
    Invalid(Invalid),
}

#[derive(Clone, Copy, Debug)]
pub struct MemoryRange {
    base: u64,
    end: u64,
}

impl MemoryRange {
    pub fn new(base: u64, size: u64) -> Result<Self, Invalid> {
        let end = base
            .checked_add(size)
            .filter(|end| *end > base)
            .ok_or(Invalid::Layout)?;
        Ok(Self { base, end })
    }

    fn contains(self, base: u64, size: u64) -> bool {
        base >= self.base && base.checked_add(size).is_some_and(|end| end <= self.end)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Ring {
    queue: Queue,
    memory: MemoryRange,
    next_available: u16,
    next_used: u16,
    initialized: bool,
}

#[derive(Clone, Copy, Debug)]
struct Segment {
    address: u64,
    length: usize,
}

/// One copied descriptor chain, tied to the next unconsumed ring position.
pub struct Chain {
    queue: Queue,
    head: u16,
    available: u16,
    writable: bool,
    segments: Vec<Segment>,
    length: usize,
}

impl Ring {
    pub fn new(queue: Queue, memory: MemoryRange) -> Result<Self, Invalid> {
        if !queue.ready
            || queue.size == 0
            || !queue.size.is_power_of_two()
            || queue.size > QUEUE_MAX
        {
            return Err(Invalid::Layout);
        }
        for (address, length, alignment) in regions(queue) {
            if !address.is_multiple_of(alignment) || !memory.contains(address, length) {
                return Err(Invalid::Layout);
            }
        }
        let layout = regions(queue);
        for (index, (base, size, _)) in layout.iter().enumerate() {
            if layout[..index]
                .iter()
                .any(|(other, length, _)| *base < *other + *length && *other < *base + *size)
            {
                return Err(Invalid::Layout);
            }
        }
        Ok(Self {
            queue,
            memory,
            next_available: 0,
            next_used: 0,
            initialized: false,
        })
    }

    /// Initialize the device-owned fields once per activation. The final
    /// ordered store precedes the first available-index check.
    pub fn initialize<M: GuestMemory>(&mut self, memory: &M) -> Result<(), Error<M::Error>> {
        if !self.initialized {
            memory
                .store_u16_release(self.queue.used + 2, 0)
                .map_err(Error::Memory)?;
            memory
                .store_u16_release(self.queue.used, 0)
                .map_err(Error::Memory)?;
            self.initialized = true;
        }
        Ok(())
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        regions(self.queue).into_iter().any(|(base, size, _)| {
            regions(other.queue)
                .into_iter()
                .any(|(other_base, other_size, _)| {
                    base < other_base + other_size && other_base < base + size
                })
        })
    }

    /// Returns the next chain without consuming it, allowing bounded output
    /// buffers to apply backpressure without losing guest descriptors.
    pub fn peek<M: GuestMemory>(
        &self,
        memory: &M,
        writable: bool,
    ) -> Result<Option<Chain>, Error<M::Error>> {
        let available = memory
            .load_u16_acquire(self.queue.available + 2)
            .map_err(Error::Memory)?;
        let pending = available.wrapping_sub(self.next_available);
        if u32::from(pending) > self.queue.size {
            return Err(Error::Invalid(Invalid::AvailableIndex));
        }
        if pending == 0 {
            return Ok(None);
        }
        let slot = u64::from(self.next_available) % u64::from(self.queue.size);
        let head = memory
            .load_u16_acquire(self.queue.available + 4 + 2 * slot)
            .map_err(Error::Memory)?;
        let mut index = head;
        let mut segments = Vec::new();
        let mut length = 0usize;
        for _ in 0..self.queue.size {
            if u32::from(index) >= self.queue.size {
                return Err(Error::Invalid(Invalid::Descriptor));
            }
            let mut bytes = [0; 16];
            memory
                .read(self.queue.descriptor + u64::from(index) * 16, &mut bytes)
                .map_err(Error::Memory)?;
            let address = u64::from_le_bytes(
                bytes[..8]
                    .try_into()
                    .map_err(|_| Error::Invalid(Invalid::Descriptor))?,
            );
            let size = u32::from_le_bytes(
                bytes[8..12]
                    .try_into()
                    .map_err(|_| Error::Invalid(Invalid::Descriptor))?,
            ) as usize;
            let flags = u16::from_le_bytes([bytes[12], bytes[13]]);
            if flags & !(NEXT | WRITE) != 0 || !self.memory.contains(address, size as u64) {
                return Err(Error::Invalid(Invalid::Descriptor));
            }
            if (flags & WRITE != 0) != writable {
                return Err(Error::Invalid(Invalid::Direction));
            }
            length = length
                .checked_add(size)
                .ok_or(Error::Invalid(Invalid::TooLarge))?;
            segments.push(Segment {
                address,
                length: size,
            });
            if flags & NEXT == 0 {
                return Ok(Some(Chain {
                    queue: self.queue,
                    head,
                    available: self.next_available,
                    writable,
                    segments,
                    length,
                }));
            }
            index = u16::from_le_bytes([bytes[14], bytes[15]]);
        }
        Err(Error::Invalid(Invalid::Descriptor))
    }

    /// Completes exactly the chain most recently peeked at this ring position.
    /// On any memory error the caller must stop the device, because partially
    /// published guest state cannot safely be retried as a fresh request.
    pub fn complete<M: GuestMemory>(
        &mut self,
        memory: &M,
        chain: Chain,
        written: usize,
    ) -> Result<bool, Error<M::Error>> {
        if chain.queue != self.queue
            || chain.available != self.next_available
            || written > chain.length
            || written > u32::MAX as usize
            || (!chain.writable && written != 0)
        {
            return Err(Error::Invalid(Invalid::Length));
        }
        let mut element = [0; 8];
        element[..4].copy_from_slice(&u32::from(chain.head).to_le_bytes());
        element[4..].copy_from_slice(&(written as u32).to_le_bytes());
        let slot = u64::from(self.next_used) % u64::from(self.queue.size);
        memory
            .write(self.queue.used + 4 + 8 * slot, &element)
            .map_err(Error::Memory)?;
        let next = self.next_used.wrapping_add(1);
        memory
            .store_u16_release(self.queue.used + 2, next)
            .map_err(Error::Memory)?;
        self.next_used = next;
        self.next_available = self.next_available.wrapping_add(1);
        let flags = memory
            .load_u16_acquire(self.queue.available)
            .map_err(Error::Memory)?;
        Ok(flags & 1 == 0)
    }
}

impl Chain {
    pub fn len(&self) -> usize {
        self.length
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn read<M: GuestMemory>(
        &self,
        memory: &M,
        bytes: &mut [u8],
    ) -> Result<(), Error<M::Error>> {
        self.read_at(memory, 0, bytes)
    }

    /// Partial device-readable transfer. The chain remains owned until its
    /// complete payload has been accepted, with bounded intermediate storage.
    pub fn read_at<M: GuestMemory>(
        &self,
        memory: &M,
        offset: usize,
        bytes: &mut [u8],
    ) -> Result<(), Error<M::Error>> {
        if self.writable {
            return Err(Error::Invalid(Invalid::Direction));
        }
        self.copy(
            memory,
            offset,
            bytes.len(),
            |memory, address, offset, length| {
                memory.read(address, &mut bytes[offset..offset + length])
            },
        )
    }

    pub fn write<M: GuestMemory>(&self, memory: &M, bytes: &[u8]) -> Result<(), Error<M::Error>> {
        if !self.writable {
            return Err(Error::Invalid(Invalid::Direction));
        }
        self.copy(memory, 0, bytes.len(), |memory, address, offset, length| {
            memory.write(address, &bytes[offset..offset + length])
        })
    }

    fn copy<M: GuestMemory>(
        &self,
        memory: &M,
        offset: usize,
        length: usize,
        mut copy: impl FnMut(&M, u64, usize, usize) -> Result<(), M::Error>,
    ) -> Result<(), Error<M::Error>> {
        if offset
            .checked_add(length)
            .is_none_or(|end| end > self.length)
        {
            return Err(Error::Invalid(Invalid::Length));
        }
        let mut done = 0;
        let mut skip = offset;
        for segment in &self.segments {
            let start = skip.min(segment.length);
            skip -= start;
            let count = (segment.length - start).min(length - done);
            let mut local = 0;
            while local < count {
                let chunk = (count - local).min(COPY_BYTES);
                copy(
                    memory,
                    segment.address + (start + local) as u64,
                    done + local,
                    chunk,
                )
                .map_err(Error::Memory)?;
                local += chunk;
            }
            done += count;
            if done == length {
                break;
            }
        }
        Ok(())
    }
}

fn regions(queue: Queue) -> [(u64, u64, u64); 3] {
    let size = u64::from(queue.size);
    [
        (queue.descriptor, 16 * size, 16),
        (queue.available, 6 + 2 * size, 2),
        (queue.used, 6 + 8 * size, 4),
    ]
}

#[cfg(test)]
#[path = "../tests/virtqueue.rs"]
mod tests;
