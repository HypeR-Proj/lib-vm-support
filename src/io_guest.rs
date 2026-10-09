// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Shared image preparation and installation for physical-I/O deployments.

use hyper_os::handle::{Rights, VirtualCpuObject, VirtualMachineObject};
use hyper_os::memory::WritableVmo;
use hyper_os::startup::{self, Startup};
use hyper_os::virtual_serial::Output;
use hyper_os::vm;
use hyper_os::{device, virtual_serial};
use hyper_vm_image::guest_fdt::io::IoDevices;
use hyper_vm_image::{Payload, ReadAt, guest_fdt, linux};
use std::fs::File;
use std::io;
use std::os::hyper::fs::FileExt;
use std::sync::Arc;

type Result<T> = std::result::Result<T, String>;
pub const RAM_BASE: u64 = 0x4000_0000;
/// RAM budget for the resident, multi-client I/O appliance.
pub const RAM_BYTES: u64 = 128 * 1024 * 1024;
pub const PHYSICAL_MMIO: u64 = 0x0b00_0000;
pub const PHYSICAL_NET_MMIO: u64 = 0x0b01_0000;
pub const PHYSICAL_NET_IRQ: u32 = 59;
/// Generic ECAM, MSI frame and BAR resources occupy one disjoint PCI aperture.
pub const PHYSICAL_PCI_MMIO: u64 = 0x0b80_0000;
pub const PHYSICAL_PCI_IRQ: u32 = 128;
fn show(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

pub struct InstalledGuest {
    name: String,
    image: String,
    pub machine: Arc<hyper_os::OwnedHandle<VirtualMachineObject>>,
    pub cpus: Vec<Arc<hyper_os::OwnedHandle<VirtualCpuObject>>>,
    pub output: Output,
}

impl InstalledGuest {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn image(&self) -> &str {
        &self.image
    }
}

pub struct Image {
    name: String,
    path: String,
    pub memory: WritableVmo,
    pub plan: linux::BootPlan,
    pub arguments: String,
    affinity: Vec<hyper_vm_policy::affinity::Affinity>,
}
struct Source {
    file: File,
    length: u64,
}
impl ReadAt for Source {
    type Error = io::Error;
    fn length(&self) -> io::Result<u64> {
        Ok(self.length)
    }
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(output, offset)
    }
}

impl Image {
    pub fn load(definition: &hyper_vm_policy::fleet::Definition) -> Result<Self> {
        definition.validate()?;
        let path = &definition.image;
        let file = File::open(path).map_err(|error| format!("open {path}: {error}"))?;
        let length = file
            .metadata()
            .map_err(|error| format!("stat {path}: {error}"))?
            .len();
        let source =
            hyper_vm_policy::image::CachedSource::new(Source { file, length }).map_err(show)?;
        let image = hyper_vm_image::parse(&source).map_err(show)?;
        let image = hyper_vm_policy::image::configure(image, &definition.configuration)?;
        let plan = linux::validate_reference(&source, image).map_err(show)?;
        let source = source.into_inner();
        if plan.memory_base() != RAM_BASE
            || ![64 * 1024 * 1024, RAM_BYTES].contains(&plan.memory_size())
            || plan.architecture() != hyper_vm_image::Architecture::Aarch64
        {
            return Err("I/O deployment requires 64 or 128 MiB AArch64 reference images".into());
        }
        println!("HypeR I/O loader: validated {path}, allocating guest RAM");
        let memory = WritableVmo::create_contiguous(plan.memory_size()).map_err(show)?;
        println!("HypeR I/O loader: copying {path} kernel");
        copy_payload(&source, &memory, image.kernel)?;
        if let Some(payload) = image.initramfs {
            copy_payload(&source, &memory, payload)?;
        }
        Ok(Self {
            name: definition.name.clone(),
            path: definition.image.clone(),
            memory,
            plan,
            arguments: image.boot_arguments.as_str().into(),
            affinity: definition.configuration.affinity.clone(),
        })
    }
    pub fn device_tree(&self, gic_version: u32, devices: IoDevices<'_>) -> Result<()> {
        let capacity =
            usize::try_from(self.plan.device_tree().end() - self.plan.device_tree().start())
                .map_err(|_| "device tree reservation is too large")?;
        let mut structure = vec![0; capacity];
        let mut strings = vec![0; capacity];
        let mut output = vec![0; capacity];
        let length = guest_fdt::build_aarch64_linux_with_io(
            guest_fdt::Aarch64LinuxBoot {
                memory_base: RAM_BASE,
                memory_size: self.plan.memory_size(),
                vcpu_count: self.plan.vcpu_count(),
                gic_version,
                initramfs: self
                    .plan
                    .initramfs()
                    .map(|range| (range.start(), range.end())),
                boot_arguments: &self.arguments,
            },
            devices,
            &mut structure,
            &mut strings,
            &mut output,
        )
        .map_err(show)?;
        self.memory
            .write_all_at(
                self.plan.device_tree().start() - RAM_BASE,
                &output[..length],
            )
            .map_err(show)
    }
}

fn copy_payload(source: &Source, memory: &WritableVmo, payload: Payload) -> Result<()> {
    let offset = payload
        .load_address
        .checked_sub(RAM_BASE)
        .ok_or("payload below RAM")?;
    crate::image_io::copy(
        payload.file_offset,
        payload.length,
        |offset, bytes| source.read_exact_at(offset, bytes),
        |relative, bytes| {
            let destination = offset
                .checked_add(relative)
                .ok_or("payload offset overflow")?;
            memory.write_all_at(destination, bytes).map_err(show)
        },
    )
    .map_err(show)?;
    Ok(())
}

pub fn install(
    startup: &Startup<'_>,
    image: &Image,
    own: &hyper_os::OwnedHandle<hyper_os::handle::GuestMemoryObject>,
    shared: Option<&hyper_os::OwnedHandle<hyper_os::handle::GuestMemoryObject>>,
    physical: Option<&hyper_os::OwnedHandle<hyper_os::handle::PhysicalDeviceObject>>,
    serial_address: u64,
) -> Result<InstalledGuest> {
    let ram_bytes = image.plan.memory_size();
    let mapping = shared.map(|memory| SharedGrant {
        memory: memory.as_handle_ref(),
        guest_offset: ram_bytes,
        memory_offset: 0,
        size: ram_bytes,
    });
    let physical = physical.map(|device| PhysicalAssignment {
        device: device.as_handle_ref(),
        base: PHYSICAL_MMIO,
        irq: 40,
    });
    install_mapped(
        startup,
        image,
        own,
        mapping.as_slice(),
        if shared.is_some() {
            ram_bytes * 2
        } else {
            ram_bytes
        },
        physical.as_slice(),
        serial_address,
    )
}

#[derive(Clone, Copy)]
pub struct SharedGrant<'a> {
    pub memory: hyper_os::HandleRef<'a, hyper_os::handle::GuestMemoryObject>,
    pub guest_offset: u64,
    pub memory_offset: u64,
    pub size: u64,
}

/// A physical controller and its guest trap aperture. All assigned controllers
/// retain the same installed VM and its DMA backing until retirement completes.
#[derive(Clone, Copy)]
pub struct PhysicalAssignment<'a> {
    pub device: hyper_os::HandleRef<'a, hyper_os::handle::PhysicalDeviceObject>,
    pub base: u64,
    pub irq: u32,
}

pub fn install_mapped(
    startup: &Startup<'_>,
    image: &Image,
    own: &hyper_os::OwnedHandle<hyper_os::handle::GuestMemoryObject>,
    shared: &[SharedGrant<'_>],
    address_space_bytes: u64,
    physical: &[PhysicalAssignment<'_>],
    serial_address: u64,
) -> Result<InstalledGuest> {
    if address_space_bytes < image.plan.memory_size() {
        return Err("I/O VM address space cannot truncate its boot RAM".into());
    }
    let lease = vm::derive_creation_lease(
        startup
            .borrow(startup::VIRTUAL_MACHINE_CREATION_AUTHORITY)
            .map_err(show)?,
        startup.borrow(startup::RESOURCE_DOMAIN).map_err(show)?,
    )
    .map_err(show)?;
    let pending = vm::create(
        lease,
        vm::Configuration {
            guest_physical_base: RAM_BASE,
            memory_size: address_space_bytes,
            vcpu_count: image.plan.vcpu_count(),
            architecture: vm::Architecture::Aarch64,
            platform_profile: vm::PlatformProfile::Aarch64Reference,
        },
    )
    .map_err(|error| show(error.error()))?;
    vm::map_guest_memory(
        pending.as_handle_ref(),
        own.as_handle_ref(),
        0,
        0,
        image.plan.memory_size(),
    )
    .map_err(show)?;
    for shared in shared {
        vm::map_guest_memory(
            pending.as_handle_ref(),
            shared.memory,
            shared.guest_offset,
            shared.memory_offset,
            shared.size,
        )
        .map_err(show)?;
    }
    for physical in physical {
        device::assign(
            pending.as_handle_ref(),
            physical.device,
            physical.base,
            physical.irq,
        )
        .map_err(show)?;
    }
    let serial = virtual_serial::create().map_err(show)?;
    let output = Output::register(
        &serial,
        startup.borrow(startup::ROOT_VMAR).map_err(show)?,
        serial_address,
        WritableVmo::create(virtual_serial::BUFFER_BYTES).map_err(show)?,
    )
    .map_err(show)?;
    let binding = serial
        .duplicate(Rights::ASSIGN_DEVICE.union(Rights::TRANSFER))
        .map_err(show)?;
    vm::set_virtual_serial(pending.as_handle_ref(), binding)
        .map_err(|error| show(error.error()))?;
    vm::set_bootstrap(
        pending.as_handle_ref(),
        vm::VirtualCpuBootstrap {
            entry: image.plan.kernel_entry(),
            stack: 0,
            arguments: image.plan.bootstrap_arguments(),
        },
    )
    .map_err(show)?;
    vm::seal(pending.as_handle_ref()).map_err(show)?;
    let (machine, cpu) = vm::install(pending).map_err(|error| show(error.error()))?;
    let machine = Arc::new(machine);
    let mut cpus = vec![Arc::new(cpu)];
    for index in 1..image.plan.vcpu_count() {
        cpus.push(Arc::new(
            vm::open_vcpu(machine.as_handle_ref(), index).map_err(show)?,
        ));
    }
    hyper_vm_policy::affinity::apply(&image.affinity, image.plan.vcpu_count(), |index, words| {
        let cpu = cpus
            .get(index as usize)
            .ok_or(hyper_os::Error::InvalidResponse)?;
        vm::set_vcpu_affinity(cpu.as_handle_ref(), words)
    })?;
    Ok(InstalledGuest {
        name: image.name.clone(),
        image: image.path.clone(),
        machine,
        cpus,
        output,
    })
}
