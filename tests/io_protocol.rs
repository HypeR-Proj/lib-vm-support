// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::virtio_mmio::Queue;

fn reset() -> Request {
    Request {
        binding: 1,
        epoch: 7,
        transaction: 19,
        command: Command::Device(BackendOperation::Reset),
    }
}

fn reply(request: Request, status: u32) -> Result<Vec<u8>, Error> {
    let mut record = [0; MAX_RECORD];
    request.encode(&mut record)?;
    let length: u32 = if request.command == Command::Hello {
        64
    } else {
        48
    };
    record[8..12].copy_from_slice(&length.to_le_bytes());
    record[12..16].copy_from_slice(&1u32.to_le_bytes());
    record[40..48].fill(0);
    record[40..44].copy_from_slice(&status.to_le_bytes());
    Ok(record[..length as usize].to_vec())
}

#[test]
fn stale_reply_cannot_authorize_quiescence() -> Result<(), Error> {
    let request = reset();
    let record = reply(request, 0)?;
    assert_eq!(Reply::decode(&record, request)?.status, Status::Success);
    for offset in [6, 16, 24, 32] {
        let mut stale = record.clone();
        stale[offset] ^= 1;
        assert_eq!(Reply::decode(&stale, request), Err(Error::MismatchedReply));
    }
    for length in 0..record.len() {
        assert_eq!(
            Reply::decode(&record[..length], request),
            Err(Error::InvalidRecord)
        );
    }
    assert_eq!(
        Reply::decode(&reply(request, 5)?, request)?.status,
        Status::QuiescenceFailed
    );
    Ok(())
}

#[test]
fn activation_wire_has_fixed_linux_layout() -> Result<(), Error> {
    let queues = [Queue {
        size: 128,
        descriptor: 0x4010_0000,
        available: 0x4010_1000,
        used: 0x4010_2000,
        ready: true,
    }; QUEUES];
    let request = Request {
        command: Command::Device(BackendOperation::Activate {
            features: VERSION_1,
            queues,
        }),
        ..reset()
    };
    let mut bytes = [0xff; MAX_RECORD];
    assert_eq!(request.encode(&mut bytes)?, 48 + QUEUES * 32);
    assert_eq!(u64_at(&bytes, 40)?, VERSION_1);
    for offset in (48..48 + QUEUES * 32).step_by(32) {
        assert_eq!(u32_at(&bytes, offset)?, 128);
        assert_eq!(u32_at(&bytes, offset + 4)?, 0);
        assert_eq!(u64_at(&bytes, offset + 8)?, 0x4010_0000);
        assert_eq!(u64_at(&bytes, offset + 16)?, 0x4010_1000);
        assert_eq!(u64_at(&bytes, offset + 24)?, 0x4010_2000);
    }
    assert_eq!(&bytes[12..16], &[0; 4]);
    Ok(())
}

#[test]
fn optional_queues_are_canonical_and_old_bridge_versions_are_rejected() -> Result<(), Error> {
    let queues = core::array::from_fn(|index| Queue {
        size: 8,
        descriptor: 0x4010_0000 + index as u64 * 4096,
        available: 0x4010_0100 + index as u64 * 4096,
        used: 0x4010_0200 + index as u64 * 4096,
        ready: index < 3,
    });
    let request = Request {
        command: Command::Device(BackendOperation::Activate {
            features: VERSION_1,
            queues,
        }),
        ..reset()
    };
    let mut bytes = [0; MAX_RECORD];
    let length = request.encode(&mut bytes)?;
    assert!(bytes[48 + 3 * 32..length].iter().all(|byte| *byte == 0));
    assert!(Request::decode(&bytes[..length]).is_ok());
    bytes[48 + 3 * 32 + 8] = 1;
    assert_eq!(Request::decode(&bytes[..length]), Err(Error::InvalidRecord));
    bytes[48 + 3 * 32 + 8] = 0;
    bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
    assert_eq!(Request::decode(&bytes[..length]), Err(Error::InvalidRecord));
    Ok(())
}

#[test]
fn hello_rejects_incompatible_queue_contract() -> Result<(), Error> {
    let request = Request {
        command: Command::Hello,
        ..reset()
    };
    let mut bytes = reply(request, 0)?;
    bytes[48..56].copy_from_slice(&VERSION_1.to_le_bytes());
    bytes[56..60].copy_from_slice(&(QUEUES as u32).to_le_bytes());
    bytes[60..64].copy_from_slice(&QUEUE_MAX.to_le_bytes());
    assert_eq!(Reply::decode(&bytes, request)?.features, Some(VERSION_1));
    bytes[56] = 4;
    assert_eq!(
        Reply::decode(&bytes, request),
        Err(Error::UnsupportedBackend)
    );
    Ok(())
}

#[test]
fn rejected_hello_preserves_backend_status_without_capability_fields() -> Result<(), Error> {
    for command in [Command::Hello, Command::NetworkHello] {
        let request = Request { command, ..reset() };
        let mut bytes = [0; MAX_RECORD];
        request.encode(&mut bytes)?;
        bytes[8..12].copy_from_slice(&48u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
        bytes[40..44].copy_from_slice(&1u32.to_le_bytes());
        let reply = Reply::decode(&bytes[..48], request)?;
        assert_eq!(reply.status, Status::Unsupported);
        assert_eq!(reply.features, None);
        assert_eq!(reply.network, None);
    }
    Ok(())
}

#[test]
fn network_protocol_has_independent_opcodes_and_exact_configuration() -> Result<(), Error> {
    let hello = Request {
        command: Command::NetworkHello,
        ..reset()
    };
    let mut bytes = [0; MAX_RECORD];
    hello.encode(&mut bytes)?;
    bytes[8..12].copy_from_slice(&80u32.to_le_bytes());
    bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
    bytes[48..56].copy_from_slice(&VERSION_1.to_le_bytes());
    bytes[56..60].copy_from_slice(&2u32.to_le_bytes());
    bytes[60..64].copy_from_slice(&QUEUE_MAX.to_le_bytes());
    bytes[64..70].copy_from_slice(&[2, 0x48, 0x59, 0, 0, 1]);
    bytes[70..72].copy_from_slice(&1500u16.to_le_bytes());
    assert_eq!(
        Reply::decode(&bytes[..80], hello)?
            .network
            .map(|config| config.mac),
        Some([2, 0x48, 0x59, 0, 0, 1])
    );
    bytes[72] = 1;
    assert_eq!(
        Reply::decode(&bytes[..80], hello),
        Err(Error::UnsupportedBackend)
    );
    let mut queues = [Queue::default(); QUEUES];
    for (index, queue) in queues[..2].iter_mut().enumerate() {
        *queue = Queue {
            size: 128,
            descriptor: 0x4000_0000 + index as u64 * 0x10000,
            available: 0x4000_1000 + index as u64 * 0x10000,
            used: 0x4000_2000 + index as u64 * 0x10000,
            ready: true,
        };
    }
    let activation = Request {
        command: Command::NetworkDevice(BackendOperation::Activate {
            features: VERSION_1,
            queues,
        }),
        ..reset()
    };
    assert_eq!(activation.encode(&mut bytes)?, 112);
    assert_eq!(Request::decode(&bytes[..112])?, activation);
    assert_eq!(u16_at(&bytes, 6)?, 8);
    let mut storage_reply = reply(reset(), 0)?;
    let net_reset = Request {
        command: Command::NetworkDevice(BackendOperation::Reset),
        ..reset()
    };
    assert_eq!(
        Reply::decode(&storage_reply, net_reset),
        Err(Error::MismatchedReply)
    );
    storage_reply[6..8].copy_from_slice(&9u16.to_le_bytes());
    assert_eq!(
        Reply::decode(&storage_reply, net_reset)?.status,
        Status::Success
    );
    Ok(())
}

#[test]
fn prepare_carries_exact_mapping_identity_and_release_is_header_only() -> Result<(), Error> {
    let request = Request {
        command: Command::Prepare {
            alias: 0x4400_0000,
            guest_base: 0x5000_0000,
            length: 0x20000,
            mapping_token: 123,
        },
        ..reset()
    };
    let mut bytes = [0xff; MAX_RECORD];
    assert_eq!(request.encode(&mut bytes)?, 72);
    assert_eq!(u16_at(&bytes, 6)?, 5);
    assert_eq!(u64_at(&bytes, 40)?, 0x4400_0000);
    assert_eq!(u64_at(&bytes, 48)?, 0x5000_0000);
    assert_eq!(u64_at(&bytes, 56)?, 0x20000);
    assert_eq!(u64_at(&bytes, 64)?, 123);
    assert_eq!(
        Reply::decode(&reply(request, 0)?, request)?.status,
        Status::Success
    );
    let release = Request {
        command: Command::Release,
        ..reset()
    };
    assert_eq!(release.encode(&mut bytes)?, 40);
    assert_eq!(u16_at(&bytes, 6)?, 6);
    for command in [
        Command::Prepare {
            alias: 1,
            guest_base: 0,
            length: 4096,
            mapping_token: 0,
        },
        Command::Prepare {
            alias: 0,
            guest_base: 0,
            length: 0,
            mapping_token: 0,
        },
        Command::Prepare {
            alias: u64::MAX - 4095,
            guest_base: 0,
            length: 4096,
            mapping_token: 0,
        },
    ] {
        assert_eq!(
            Request { command, ..reset() }.encode(&mut bytes),
            Err(Error::InvalidRecord)
        );
    }
    Ok(())
}

#[test]
fn broker_decodes_only_canonical_requests() -> Result<(), Error> {
    let request = Request {
        binding: 9,
        epoch: 2,
        transaction: 7,
        command: Command::Prepare {
            alias: 0,
            guest_base: 0x4000_0000,
            length: 128 * 1024 * 1024,
            mapping_token: 42,
        },
    };
    let mut bytes = [0; MAX_RECORD];
    let length = request.encode(&mut bytes)?;
    assert_eq!(Request::decode(&bytes[..length]), Ok(request));
    bytes[12] = 1;
    assert!(Request::decode(&bytes[..length]).is_err());
    assert!(Request::decode(&bytes[..20]).is_err());
    Ok(())
}
