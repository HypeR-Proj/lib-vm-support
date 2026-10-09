// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Network device configuration. Packet buffers belong to the vhost backend.

pub const QUEUES: usize = 2;
pub const MAC_FEATURE: u64 = 1 << 5;
pub const MTU: u16 = 1500;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Configuration {
    pub mac: [u8; 6],
    pub mtu: u16,
}

impl Configuration {
    pub fn valid(self) -> bool {
        // Deployment assigns a stable, locally administered unicast address.
        self.mac[0] & 3 == 2 && self.mtu == MTU
    }
}

#[cfg(test)]
#[path = "../tests/virtio_net.rs"]
mod tests;
