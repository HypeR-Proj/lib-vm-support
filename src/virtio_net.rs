// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Network device configuration. Packet buffers belong to the vhost backend.

pub const QUEUES: usize = 2;
pub const MAC_FEATURE: u64 = 1 << 5;
pub const CSUM: u64 = 1 << 0;
pub const HOST_TSO4: u64 = 1 << 11;
pub const HOST_TSO6: u64 = 1 << 12;
pub const TX_OFFLOADS: u64 = CSUM | HOST_TSO4 | HOST_TSO6;
pub const MTU: u16 = 1500;

/// Guest TX offloads require end-to-end backend support. Segmentation also
/// needs partial checksum completion; never offer TSO without that dependency.
pub(crate) fn supported_offloads(backend_features: u64) -> u64 {
    if backend_features & CSUM == 0 {
        0
    } else {
        backend_features & TX_OFFLOADS
    }
}

pub(crate) fn valid_features(features: u64) -> bool {
    features & (HOST_TSO4 | HOST_TSO6) == 0 || features & CSUM != 0
}

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
