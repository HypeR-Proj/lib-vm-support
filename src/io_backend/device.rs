// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Transport selection around one authoritative virtio state machine.

use crate::virtio::{Error, Transaction};
use crate::{virtio_mmio, virtio_pci};

pub(super) enum Device {
    Mmio(virtio_mmio::Device),
    Pci(virtio_pci::Device),
}

impl Device {
    pub(super) fn new(features: u64, base: u64, size: u64, queues: usize) -> Result<Self, Error> {
        virtio_mmio::Device::scsi_with_queues(features, base, size, queues).map(Self::Mmio)
    }
    pub(super) fn network(
        features: u64,
        base: u64,
        size: u64,
        config: crate::virtio_net::Configuration,
    ) -> Result<Self, Error> {
        virtio_mmio::Device::network(features, base, size, config).map(Self::Mmio)
    }
    pub(super) fn into_pci(self, bar: u32) -> Result<Self, Error> {
        match self {
            Self::Mmio(device) => Ok(Self::Pci(virtio_pci::Device::new(device.into_core(), bar))),
            Self::Pci(_) => Err(Error::InvalidStatus),
        }
    }
    pub(super) fn read(&self, offset: u64, width: u8) -> Result<u64, Error> {
        match self {
            Self::Mmio(device) => device.read(offset, width),
            Self::Pci(device) => device.read(offset, width),
        }
    }
    pub(super) fn write(
        &mut self,
        offset: u64,
        width: u8,
        value: u64,
    ) -> Result<Option<Transaction>, Error> {
        match self {
            Self::Mmio(device) => device.write(offset, width, value),
            Self::Pci(device) => device.write(offset, width, value),
        }
    }
    pub(super) fn complete(&mut self, id: u64, success: bool) -> Result<(), Error> {
        match self {
            Self::Mmio(device) => device.complete(id, success),
            Self::Pci(device) => device.complete(id, success),
        }
    }
    pub(super) fn backend_lost(&mut self) {
        match self {
            Self::Mmio(device) => device.backend_lost(),
            Self::Pci(device) => device.backend_lost(),
        }
    }
}
