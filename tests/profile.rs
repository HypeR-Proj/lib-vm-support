// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::*;

fn metadata(
    architecture: vm::Architecture,
    platform_profile: vm::PlatformProfile,
) -> vm::VirtualMachinePlatformInfo {
    vm::VirtualMachinePlatformInfo {
        aarch64_gic_version: 3,
        architecture,
        platform_profile,
        counter_frequency_hz: 24_000_000,
        riscv_isa: 0x1ff,
    }
}

#[test]
fn native_profile_query_preserves_image_identity() {
    assert_eq!(
        native_profile(PlatformProfile::Aarch64Reference),
        Ok(vm::PlatformProfile::Aarch64Reference)
    );
    assert_eq!(
        native_profile(PlatformProfile::Riscv64Reference),
        Ok(vm::PlatformProfile::Riscv64Reference)
    );
    assert_eq!(
        native_profile(PlatformProfile::X86_64Reference),
        Err(SelectionError::PlatformProfile)
    );
}

#[test]
fn metadata_does_not_substitute_a_different_platform_or_architecture() {
    assert!(matches!(
        validate_metadata(
            Architecture::Aarch64,
            PlatformProfile::Aarch64Reference,
            metadata(
                vm::Architecture::Aarch64,
                vm::PlatformProfile::Aarch64Reference
            )
        ),
        Ok(GuestHardwareMetadata::Aarch64 { gic_version: 3 })
    ));
    assert!(matches!(
        validate_metadata(
            Architecture::Riscv64,
            PlatformProfile::Riscv64Reference,
            metadata(
                vm::Architecture::Riscv64,
                vm::PlatformProfile::Riscv64Reference
            )
        ),
        Ok(GuestHardwareMetadata::Riscv64 {
            counter_frequency_hz: 24_000_000,
            riscv_isa: 0x1ff
        })
    ));
    assert!(matches!(
        validate_metadata(
            Architecture::Riscv64,
            PlatformProfile::Riscv64Reference,
            metadata(
                vm::Architecture::Aarch64,
                vm::PlatformProfile::Riscv64Reference
            )
        ),
        Err(SelectionError::Architecture)
    ));
    assert!(matches!(
        validate_metadata(
            Architecture::Riscv64,
            PlatformProfile::Riscv64Reference,
            metadata(
                vm::Architecture::Riscv64,
                vm::PlatformProfile::Aarch64Reference
            )
        ),
        Err(SelectionError::PlatformProfile)
    ));
}

#[test]
fn aarch64_guest_revision_is_taken_from_the_lease() {
    let mut info = metadata(
        vm::Architecture::Aarch64,
        vm::PlatformProfile::Aarch64Reference,
    );
    info.aarch64_gic_version = 2;
    assert_eq!(
        validate_metadata(
            Architecture::Aarch64,
            PlatformProfile::Aarch64Reference,
            info
        ),
        Ok(GuestHardwareMetadata::Aarch64 { gic_version: 2 })
    );
    info.aarch64_gic_version = 0;
    assert_eq!(
        validate_metadata(
            Architecture::Aarch64,
            PlatformProfile::Aarch64Reference,
            info
        ),
        Err(SelectionError::InterruptController)
    );
}
