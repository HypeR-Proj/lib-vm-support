// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Owner-only block control. These operations are intentionally absent from
//! the guest runtime request decoder, so broker forwarding cannot grant them.

use crate::io_protocol::{self, MAX_RECORD};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    Hello,
    Enumerate {
        cursor: u32,
    },
    Read {
        disk: u64,
        offset: u64,
        length: u64,
        scratch: u64,
    },
    Bind {
        client: u32,
        read_only: bool,
        disk: u64,
        first: u64,
        sectors: u64,
    },
}

impl Command {
    fn opcode(self) -> u16 {
        match self {
            Self::Hello => 11,
            Self::Enumerate { .. } => 12,
            Self::Read { .. } => 13,
            Self::Bind { .. } => 14,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Request {
    pub transaction: u64,
    pub command: Command,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Disk {
    pub id: u64,
    pub bytes: u64,
    pub sector_bytes: u32,
    pub physical_sector_bytes: u32,
    pub read_only: bool,
    pub next_cursor: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reply {
    Hello { max_read: u32 },
    Disk(Option<Disk>),
    Done,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Rejected(u32),
}

impl Request {
    pub fn encode(self, output: &mut [u8; MAX_RECORD]) -> Result<usize, Error> {
        let base = io_protocol::Request {
            binding: 1,
            epoch: 1,
            transaction: self.transaction,
            command: io_protocol::Command::Hello,
        };
        base.encode(output).map_err(|_| Error::Invalid)?;
        let length = match self.command {
            Command::Hello => 40,
            Command::Enumerate { cursor } => {
                output[40..44].copy_from_slice(&cursor.to_le_bytes());
                48
            }
            Command::Read {
                disk,
                offset,
                length,
                scratch,
            } => {
                if disk == 0
                    || length == 0
                    || offset.checked_add(length).is_none()
                    || scratch.checked_add(length).is_none()
                {
                    return Err(Error::Invalid);
                }
                for (field, value) in output[40..72]
                    .chunks_exact_mut(8)
                    .zip([disk, offset, length, scratch])
                {
                    field.copy_from_slice(&value.to_le_bytes());
                }
                72
            }
            Command::Bind {
                client,
                read_only,
                disk,
                first,
                sectors,
            } => {
                if client == 0 || disk == 0 || sectors == 0 || first.checked_add(sectors).is_none()
                {
                    return Err(Error::Invalid);
                }
                output[40..44].copy_from_slice(&client.to_le_bytes());
                output[44..48].copy_from_slice(&u32::from(read_only).to_le_bytes());
                for (field, value) in output[48..72]
                    .chunks_exact_mut(8)
                    .zip([disk, first, sectors])
                {
                    field.copy_from_slice(&value.to_le_bytes());
                }
                72
            }
        };
        output[6..8].copy_from_slice(&self.command.opcode().to_le_bytes());
        output[8..12].copy_from_slice(&(length as u32).to_le_bytes());
        Ok(length)
    }

    pub fn decode_reply(self, bytes: &[u8]) -> Result<Reply, Error> {
        let status = u32_at(bytes, 40)?;
        let expected = if status == 0 {
            match self.command {
                Command::Hello => 64,
                Command::Enumerate { .. } => 80,
                _ => 48,
            }
        } else {
            48
        };
        let mut request = [0; MAX_RECORD];
        self.encode(&mut request)?;
        if bytes.len() != expected
            || bytes[..8] != request[..8]
            || u32_at(bytes, 8)? as usize != expected
            || u32_at(bytes, 12)? != 1
            || bytes[16..40] != request[16..40]
            || u32_at(bytes, 44)? != 0
            || status > 5
        {
            return Err(Error::Invalid);
        }
        if status != 0 {
            return Err(Error::Rejected(status));
        }
        match self.command {
            Command::Hello => {
                let max_read = u32_at(bytes, 56)?;
                if u64_at(bytes, 48)? & 7 != 7
                    || !(512..=1024 * 1024).contains(&max_read)
                    || u32_at(bytes, 60)? != 0
                {
                    return Err(Error::Invalid);
                }
                Ok(Reply::Hello { max_read })
            }
            Command::Enumerate { cursor } => {
                let id = u64_at(bytes, 48)?;
                if id == 0 {
                    return if bytes[48..].iter().all(|byte| *byte == 0) {
                        Ok(Reply::Disk(None))
                    } else {
                        Err(Error::Invalid)
                    };
                }
                let disk = Disk {
                    id,
                    bytes: u64_at(bytes, 56)?,
                    sector_bytes: u32_at(bytes, 64)?,
                    physical_sector_bytes: u32_at(bytes, 68)?,
                    read_only: u32_at(bytes, 72)? == 1,
                    next_cursor: u32_at(bytes, 76)?,
                };
                if u32_at(bytes, 72)? > 1
                    || !(512..=65536).contains(&disk.sector_bytes)
                    || !disk.sector_bytes.is_power_of_two()
                    || disk.physical_sector_bytes < disk.sector_bytes
                    || !disk.physical_sector_bytes.is_power_of_two()
                    || disk.bytes == 0
                    || !disk.bytes.is_multiple_of(u64::from(disk.sector_bytes))
                    || disk.next_cursor <= cursor
                {
                    return Err(Error::Invalid);
                }
                Ok(Reply::Disk(Some(disk)))
            }
            _ => Ok(Reply::Done),
        }
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    bytes
        .get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(Error::Invalid)
}

#[cfg(test)]
#[path = "../tests/block_protocol.rs"]
mod tests;

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, Error> {
    bytes
        .get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(Error::Invalid)
}
