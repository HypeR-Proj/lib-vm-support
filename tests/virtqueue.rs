// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::*;
#[path = "virtqueue_memory.rs"]
mod memory;
use memory::{BASE, Memory, SIZE};

fn ring(index: usize) -> Result<Ring, String> {
    Ring::new(
        Memory::queue(index),
        MemoryRange::new(BASE, SIZE).map_err(|e| format!("{e:?}"))?,
    )
    .map_err(|e| format!("{e:?}"))
}
fn show<E: std::fmt::Debug>(error: E) -> String {
    format!("{error:?}")
}

#[test]
fn descriptor_snapshots_scatter_and_notification_suppression() -> Result<(), String> {
    let memory = Memory::new();
    let queue = Memory::queue(0);
    let mut ring = ring(0)?;
    ring.initialize(&memory).map_err(show)?;
    memory
        .descriptor(queue, 0, BASE + 0x10000, 3, NEXT, 1)
        .map_err(show)?;
    memory
        .descriptor(queue, 1, BASE + 0x20000, 2, 0, 0)
        .map_err(show)?;
    memory.write(BASE + 0x10000, b"abc").map_err(show)?;
    memory.write(BASE + 0x20000, b"de").map_err(show)?;
    memory.publish(queue, 0, 0).map_err(show)?;
    let chain = ring
        .peek(&memory, false)
        .map_err(show)?
        .ok_or("missing chain")?;
    // A peer mutating descriptors after admission does not redirect this copy.
    memory
        .descriptor(queue, 0, u64::MAX, 10, 0, 0)
        .map_err(show)?;
    let mut bytes = [0; 5];
    chain.read(&memory, &mut bytes).map_err(show)?;
    assert_eq!(&bytes, b"abcde");
    memory.store_u16_release(queue.available, 1).map_err(show)?;
    assert!(!ring.complete(&memory, chain, 0).map_err(show)?);
    assert_eq!(memory.used(queue), Ok(1));
    assert!(ring.peek(&memory, false).map_err(show)?.is_none());
    Ok(())
}

#[test]
fn index_wrap_is_distinct_from_invalid_available_jump() -> Result<(), String> {
    let memory = Memory::new();
    let queue = Memory::queue(0);
    let mut ring = ring(0)?;
    ring.next_available = u16::MAX;
    ring.next_used = u16::MAX;
    memory
        .descriptor(queue, 0, BASE + 0x10000, 1, WRITE, 0)
        .map_err(show)?;
    memory.publish(queue, u16::MAX, 0).map_err(show)?;
    let chain = ring
        .peek(&memory, true)
        .map_err(show)?
        .ok_or("missing wrap chain")?;
    chain.write(&memory, b"a").map_err(show)?;
    assert!(ring.complete(&memory, chain, 1).map_err(show)?);
    assert_eq!(memory.used(queue), Ok(0));
    memory
        .store_u16_release(queue.available + 2, 9)
        .map_err(show)?;
    assert!(matches!(
        ring.peek(&memory, true),
        Err(Error::Invalid(Invalid::AvailableIndex))
    ));
    Ok(())
}

#[test]
fn untrusted_chains_reject_cycles_indirect_direction_and_bounds() -> Result<(), String> {
    let memory = Memory::new();
    let queue = Memory::queue(0);
    let ring = ring(0)?;
    memory.publish(queue, 0, 0).map_err(show)?;
    for (address, length, flags, next, failure) in [
        (BASE, 1, NEXT, 0, Invalid::Descriptor),
        (BASE, 1, 4, 0, Invalid::Descriptor),
        (BASE, 1, WRITE, 0, Invalid::Direction),
        (BASE - 1, 1, 0, 0, Invalid::Descriptor),
        (u64::MAX - 1, 4, 0, 0, Invalid::Descriptor),
        (BASE + SIZE - 1, 2, 0, 0, Invalid::Descriptor),
        (BASE, 1, NEXT, 8, Invalid::Descriptor),
    ] {
        memory
            .descriptor(queue, 0, address, length, flags, next)
            .map_err(show)?;
        assert!(
            matches!(ring.peek(&memory, false), Err(Error::Invalid(actual)) if actual == failure)
        );
        assert_eq!(memory.used(queue), Ok(0));
    }
    Ok(())
}

#[test]
fn bounded_copies_cover_large_payload_and_reject_wrong_ring_completion() -> Result<(), String> {
    const MAX_TRANSFER: usize = 128 * 1024;
    let memory = Memory::new();
    let queue = Memory::queue(0);
    let ring = ring(0)?;
    memory
        .descriptor(queue, 0, BASE + 0x10000, MAX_TRANSFER as u32, WRITE, 0)
        .map_err(show)?;
    memory.publish(queue, 0, 0).map_err(show)?;
    let chain = ring
        .peek(&memory, true)
        .map_err(show)?
        .ok_or("missing chain")?;
    let bytes = vec![0xa5; MAX_TRANSFER];
    chain.write(&memory, &bytes).map_err(show)?;
    assert_eq!(
        memory.bytes(BASE + 0x10000, MAX_TRANSFER).map_err(show)?,
        bytes
    );
    let mut other = self::ring(1)?;
    assert_eq!(
        other.complete(&memory, chain, MAX_TRANSFER),
        Err(Error::Invalid(Invalid::Length))
    );
    Ok(())
}

#[test]
fn layout_rejects_metadata_overlap_before_guest_memory_access() -> Result<(), String> {
    let memory = MemoryRange::new(BASE, SIZE).map_err(show)?;
    let mut queue = Memory::queue(0);
    queue.available = queue.descriptor + 2;
    assert!(matches!(Ring::new(queue, memory), Err(Invalid::Layout)));
    Ok(())
}
