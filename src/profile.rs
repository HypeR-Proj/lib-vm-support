// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Conversion between validated image identity and Native platform metadata.

use hyper_os::vm;
use hyper_vm_image::guest_fdt::GuestHardwareMetadata;
use hyper_vm_image::{Architecture, PlatformProfile};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionError {
    Architecture,
    PlatformProfile,
    InterruptController,
}

/// Translate an image profile into its Native query identity. Image layout and
/// CPU-count policy remain in the shared Linux boot-plan validator.
pub const fn native_profile(
    profile: PlatformProfile,
) -> Result<vm::PlatformProfile, SelectionError> {
    match profile {
        PlatformProfile::Aarch64Reference => Ok(vm::PlatformProfile::Aarch64Reference),
        PlatformProfile::Riscv64Reference => Ok(vm::PlatformProfile::Riscv64Reference),
        PlatformProfile::X86_64Reference => Err(SelectionError::PlatformProfile),
    }
}

/// Reject a mismatched kernel response before using any of its hardware facts.
pub fn validate_metadata(
    architecture: Architecture,
    profile: PlatformProfile,
    metadata: vm::VirtualMachinePlatformInfo,
) -> Result<GuestHardwareMetadata, SelectionError> {
    if metadata.platform_profile != native_profile(profile)? {
        return Err(SelectionError::PlatformProfile);
    }
    match (architecture, metadata.architecture) {
        (Architecture::Aarch64, vm::Architecture::Aarch64) => match metadata.aarch64_gic_version {
            2 | 3 => Ok(GuestHardwareMetadata::Aarch64 {
                gic_version: metadata.aarch64_gic_version as u32,
            }),
            _ => Err(SelectionError::InterruptController),
        },
        (Architecture::Riscv64, vm::Architecture::Riscv64) => Ok(GuestHardwareMetadata::Riscv64 {
            counter_frequency_hz: metadata.counter_frequency_hz,
            riscv_isa: metadata.riscv_isa,
        }),
        _ => Err(SelectionError::Architecture),
    }
}

#[cfg(test)]
#[path = "../tests/profile.rs"]
mod tests;
