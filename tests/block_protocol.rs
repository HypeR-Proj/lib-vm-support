// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::*;

fn reply(request: Request, length: usize) -> Vec<u8> {
    let mut bytes = [0; MAX_RECORD];
    request.encode(&mut bytes).unwrap();
    bytes[8..12].copy_from_slice(&(length as u32).to_le_bytes());
    bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
    bytes[40..].fill(0);
    bytes[..length].to_vec()
}

#[test]
fn management_records_cannot_enter_guest_forwarding() {
    for command in [
        Command::Hello,
        Command::Enumerate { cursor: 0 },
        Command::Read {
            disk: 1,
            offset: 512,
            length: 512,
            scratch: 0,
        },
        Command::Bind {
            client: 1,
            read_only: true,
            disk: 1,
            first: 2048,
            sectors: 4096,
        },
    ] {
        let request = Request {
            transaction: 7,
            command,
        };
        let mut bytes = [0xff; MAX_RECORD];
        let length = request.encode(&mut bytes).unwrap();
        assert!(io_protocol::Request::decode(&bytes[..length]).is_err());
        assert!(bytes[length..].iter().all(|byte| *byte == 0));
    }
}

#[test]
fn owner_reply_requires_identity_and_canonical_payload() {
    let request = Request {
        transaction: 7,
        command: Command::Hello,
    };
    let mut bytes = reply(request, 64);
    bytes[48..56].copy_from_slice(&7u64.to_le_bytes());
    bytes[56..60].copy_from_slice(&1048576u32.to_le_bytes());
    assert_eq!(
        request.decode_reply(&bytes),
        Ok(Reply::Hello { max_read: 1048576 })
    );
    for index in [0, 4, 6, 8, 12, 16, 24, 32, 36, 44, 60] {
        let mut changed = bytes.clone();
        changed[index] ^= 1;
        assert_eq!(
            request.decode_reply(&changed),
            Err(Error::Invalid),
            "offset {index}"
        );
    }
    for length in 0..64 {
        assert!(request.decode_reply(&bytes[..length]).is_err());
    }
    bytes[48] = 3;
    assert!(request.decode_reply(&bytes).is_err());
    let mut failure = reply(request, 48);
    failure[40] = 2;
    assert_eq!(request.decode_reply(&failure), Err(Error::Rejected(2)));
    failure[40] = 6;
    assert_eq!(request.decode_reply(&failure), Err(Error::Invalid));
}

#[test]
fn enumeration_rejects_nonprogress_and_invalid_geometry() {
    let request = Request {
        transaction: 3,
        command: Command::Enumerate { cursor: 2 },
    };
    let mut bytes = reply(request, 80);
    assert_eq!(request.decode_reply(&bytes), Ok(Reply::Disk(None)));
    bytes[48..56].copy_from_slice(&3u64.to_le_bytes());
    bytes[56..64].copy_from_slice(&(1024u64 * 1024).to_le_bytes());
    bytes[64..68].copy_from_slice(&512u32.to_le_bytes());
    bytes[68..72].copy_from_slice(&4096u32.to_le_bytes());
    bytes[76..80].copy_from_slice(&3u32.to_le_bytes());
    assert!(matches!(
        request.decode_reply(&bytes),
        Ok(Reply::Disk(Some(_)))
    ));
    for (offset, value) in [(64, 513u32), (68, 256), (72, 2), (76, 2)] {
        let mut changed = bytes.clone();
        changed[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        assert_eq!(request.decode_reply(&changed), Err(Error::Invalid));
    }
    bytes[48..56].fill(0);
    assert_eq!(request.decode_reply(&bytes), Err(Error::Invalid));
}

#[test]
fn owner_cannot_encode_wrapping_ranges() {
    for command in [
        Command::Read {
            disk: 1,
            offset: u64::MAX,
            length: 512,
            scratch: 0,
        },
        Command::Read {
            disk: 1,
            offset: 0,
            length: 512,
            scratch: u64::MAX,
        },
        Command::Bind {
            client: 0,
            read_only: false,
            disk: 1,
            first: 0,
            sectors: 1,
        },
        Command::Bind {
            client: 1,
            read_only: false,
            disk: 1,
            first: u64::MAX,
            sectors: 1,
        },
    ] {
        assert!(
            Request {
                transaction: 1,
                command
            }
            .encode(&mut [0; MAX_RECORD])
            .is_err()
        );
    }
}
