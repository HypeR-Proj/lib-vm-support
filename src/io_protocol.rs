// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Versioned control records shared with the independently built I/O appliance.
//! No pointers, file descriptors or Rust layout cross this boundary.

use crate::virtio_mmio::{BackendOperation, DeviceKind, QUEUE_MAX, QUEUES, VERSION_1};
use crate::virtio_net;

const MAGIC: u32 = 0x314f_4948;
const VERSION: u16 = 3;
const HEADER: usize = 40;
pub const MAX_RECORD: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidIdentity,
    InvalidRecord,
    MismatchedReply,
    UnsupportedBackend,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    Hello,
    NetworkHello,
    Prepare {
        alias: u64,
        guest_base: u64,
        length: u64,
        mapping_token: u64,
    },
    Release,
    Device(BackendOperation),
    NetworkDevice(BackendOperation),
}

impl Command {
    const fn opcode(self) -> u16 {
        match self {
            Self::Hello => 1,
            Self::NetworkHello => 7,
            Self::Prepare { .. } => 5,
            Self::Release => 6,
            Self::Device(BackendOperation::Activate { .. }) => 2,
            Self::Device(BackendOperation::Reset) => 3,
            Self::Device(BackendOperation::StopQueue { .. }) => 4,
            Self::NetworkDevice(BackendOperation::Activate { .. }) => 8,
            Self::NetworkDevice(BackendOperation::Reset) => 9,
            Self::NetworkDevice(BackendOperation::StopQueue { .. }) => 10,
        }
    }

    pub const fn device_kind(self) -> DeviceKind {
        match self {
            Self::NetworkHello | Self::NetworkDevice(_) => DeviceKind::Network,
            _ => DeviceKind::Scsi,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    pub binding: u64,
    pub epoch: u32,
    pub transaction: u64,
    pub command: Command,
}

impl Request {
    /// Canonical decoding for the Native broker: callers still restrict which
    /// operations a runtime may request on its already-authorized binding.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let command = match u16_at(bytes, 6)? {
            1 => Command::Hello,
            7 => Command::NetworkHello,
            3 => Command::Device(BackendOperation::Reset),
            9 => Command::NetworkDevice(BackendOperation::Reset),
            4 => Command::Device(BackendOperation::StopQueue {
                queue: u32_at(bytes, 40)?,
            }),
            10 => Command::NetworkDevice(BackendOperation::StopQueue {
                queue: u32_at(bytes, 40)?,
            }),
            5 => Command::Prepare {
                alias: u64_at(bytes, 40)?,
                guest_base: u64_at(bytes, 48)?,
                length: u64_at(bytes, 56)?,
                mapping_token: u64_at(bytes, 64)?,
            },
            6 => Command::Release,
            opcode @ (2 | 8) => {
                let mut queues = [crate::virtio_mmio::Queue {
                    size: 0,
                    descriptor: 0,
                    available: 0,
                    used: 0,
                    ready: false,
                }; QUEUES];
                let count = if opcode == 2 {
                    QUEUES
                } else {
                    virtio_net::QUEUES
                };
                for (index, queue) in queues[..count].iter_mut().enumerate() {
                    let offset = 48 + index * 32;
                    *queue = crate::virtio_mmio::Queue {
                        size: u32_at(bytes, offset)?,
                        descriptor: u64_at(bytes, offset + 8)?,
                        available: u64_at(bytes, offset + 16)?,
                        used: u64_at(bytes, offset + 24)?,
                        ready: u32_at(bytes, offset)? != 0,
                    };
                }
                let operation = BackendOperation::Activate {
                    features: u64_at(bytes, 40)?,
                    queues,
                };
                if opcode == 2 {
                    Command::Device(operation)
                } else {
                    Command::NetworkDevice(operation)
                }
            }
            _ => return Err(Error::InvalidRecord),
        };
        let request = Self {
            binding: u64_at(bytes, 16)?,
            epoch: u32::try_from(u64_at(bytes, 24)?).map_err(|_| Error::InvalidRecord)?,
            transaction: u64_at(bytes, 32)?,
            command,
        };
        let mut canonical = [0; MAX_RECORD];
        let length = request.encode(&mut canonical)?;
        if canonical.get(..length) != Some(bytes) {
            return Err(Error::InvalidRecord);
        }
        Ok(request)
    }
    pub fn encode(self, output: &mut [u8; MAX_RECORD]) -> Result<usize, Error> {
        if self.binding == 0 || self.epoch == 0 || self.transaction == 0 {
            return Err(Error::InvalidIdentity);
        }
        let length = match self.command {
            Command::Hello
            | Command::NetworkHello
            | Command::Release
            | Command::Device(BackendOperation::Reset)
            | Command::NetworkDevice(BackendOperation::Reset) => HEADER,
            Command::Prepare { .. } => 72,
            Command::Device(BackendOperation::Activate { .. }) => 48 + QUEUES * 32,
            Command::NetworkDevice(BackendOperation::Activate { .. }) => {
                48 + virtio_net::QUEUES * 32
            }
            Command::Device(BackendOperation::StopQueue { .. })
            | Command::NetworkDevice(BackendOperation::StopQueue { .. }) => 48,
        };
        output.fill(0);
        output[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        output[4..6].copy_from_slice(&VERSION.to_le_bytes());
        output[6..8].copy_from_slice(&self.command.opcode().to_le_bytes());
        output[8..12].copy_from_slice(&(length as u32).to_le_bytes());
        output[16..24].copy_from_slice(&self.binding.to_le_bytes());
        output[24..32].copy_from_slice(&u64::from(self.epoch).to_le_bytes());
        output[32..40].copy_from_slice(&self.transaction.to_le_bytes());
        match self.command {
            Command::Prepare {
                alias,
                guest_base,
                length,
                mapping_token,
            } => {
                if length == 0
                    || !alias.is_multiple_of(4096)
                    || !guest_base.is_multiple_of(4096)
                    || !length.is_multiple_of(4096)
                    || alias.checked_add(length).is_none()
                    || guest_base.checked_add(length).is_none()
                {
                    return Err(Error::InvalidRecord);
                }
                output[40..48].copy_from_slice(&alias.to_le_bytes());
                output[48..56].copy_from_slice(&guest_base.to_le_bytes());
                output[56..64].copy_from_slice(&length.to_le_bytes());
                output[64..72].copy_from_slice(&mapping_token.to_le_bytes());
            }
            Command::Device(BackendOperation::Activate { features, queues })
            | Command::NetworkDevice(BackendOperation::Activate { features, queues }) => {
                let count = self.command.device_kind().queue_count();
                if queues[count..]
                    .iter()
                    .any(|queue| *queue != crate::virtio_mmio::Queue::default())
                {
                    return Err(Error::InvalidRecord);
                }
                output[40..48].copy_from_slice(&features.to_le_bytes());
                for (queue, chunk) in queues[..count]
                    .iter()
                    .zip(output[48..length].chunks_exact_mut(32))
                {
                    if !queue.ready {
                        continue;
                    }
                    chunk[0..4].copy_from_slice(&queue.size.to_le_bytes());
                    chunk[8..16].copy_from_slice(&queue.descriptor.to_le_bytes());
                    chunk[16..24].copy_from_slice(&queue.available.to_le_bytes());
                    chunk[24..32].copy_from_slice(&queue.used.to_le_bytes());
                }
            }
            Command::Device(BackendOperation::StopQueue { queue })
            | Command::NetworkDevice(BackendOperation::StopQueue { queue }) => {
                output[40..44].copy_from_slice(&queue.to_le_bytes());
            }
            _ => {}
        }
        Ok(length)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Success,
    Unsupported,
    Invalid,
    Busy,
    /// Activation failed, but rollback has quiesced every backend producer.
    BackendFailure,
    /// No acknowledgement permitting memory/queue reuse can be inferred.
    QuiescenceFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reply {
    pub status: Status,
    pub features: Option<u64>,
    pub network: Option<virtio_net::Configuration>,
}

impl Reply {
    /// Accept only the exact outstanding operation, including its epoch. A
    /// stale successful reply must never authorize queue or DMA memory reuse.
    pub fn decode(bytes: &[u8], request: Request) -> Result<Self, Error> {
        let successful = u32_at(bytes, 40)? == 0;
        let expected = match request.command {
            Command::Hello if successful => 64,
            Command::NetworkHello if successful => 80,
            _ => 48,
        };
        if bytes.len() != expected
            || u32_at(bytes, 0)? != MAGIC
            || u16_at(bytes, 4)? != VERSION
            || u32_at(bytes, 8)? as usize != expected
            || u32_at(bytes, 12)? != 1
            || u32_at(bytes, 44)? != 0
        {
            return Err(Error::InvalidRecord);
        }
        if u16_at(bytes, 6)? != request.command.opcode()
            || u64_at(bytes, 16)? != request.binding
            || u64_at(bytes, 24)? != u64::from(request.epoch)
            || u64_at(bytes, 32)? != request.transaction
        {
            return Err(Error::MismatchedReply);
        }
        let status = match u32_at(bytes, 40)? {
            0 => Status::Success,
            1 => Status::Unsupported,
            2 => Status::Invalid,
            3 => Status::Busy,
            4 => Status::BackendFailure,
            5 => Status::QuiescenceFailed,
            _ => return Err(Error::InvalidRecord),
        };
        let features =
            if successful && matches!(request.command, Command::Hello | Command::NetworkHello) {
                let features = u64_at(bytes, 48)?;
                if status != Status::Success
                    || features & VERSION_1 == 0
                    || u32_at(bytes, 56)? != request.command.device_kind().queue_count() as u32
                    || u32_at(bytes, 60)? != QUEUE_MAX
                {
                    return Err(Error::UnsupportedBackend);
                }
                Some(features)
            } else {
                None
            };
        let network = if successful && request.command == Command::NetworkHello {
            let config = virtio_net::Configuration {
                mac: field(bytes, 64)?,
                mtu: u16_at(bytes, 70)?,
            };
            if !config.valid() || bytes[72..80] != [0; 8] {
                return Err(Error::UnsupportedBackend);
            }
            Some(config)
        } else {
            None
        };
        Ok(Self {
            status,
            features,
            network,
        })
    }
}

fn field<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], Error> {
    bytes
        .get(offset..offset + N)
        .and_then(|value| value.try_into().ok())
        .ok_or(Error::InvalidRecord)
}
fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, Error> {
    field(bytes, offset).map(u16::from_le_bytes)
}
fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    field(bytes, offset).map(u32::from_le_bytes)
}
fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, Error> {
    field(bytes, offset).map(u64::from_le_bytes)
}

#[cfg(test)]
#[path = "../tests/io_protocol.rs"]
mod tests;
