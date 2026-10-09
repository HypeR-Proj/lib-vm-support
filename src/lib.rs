// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Shared image, virtual-device and I/O appliance mechanisms for VM services.
//! Process supervision, console sessions and crash fixtures belong to consumers.

pub mod image_io;
pub mod io_backend;
pub mod io_protocol;
pub mod profile;
pub mod virtio_mmio;
pub mod virtio_net;

#[cfg(all(target_os = "hyper", target_arch = "aarch64"))]
pub mod io_guest;
