//! Outer records (magic `NVXS`), which OpenVMM brokers between one host client and the guest
//! agent.
//!
//! A record is a 44-byte header (the magic, the version, the record type, a zero flags byte, the
//! session's 16-byte instance ID, its epoch, the record's sequence number, and the payload length)
//! followed by the payload.

use super::{ProtocolError, array, violation};

/// Magic of every outer record.
pub const OUTER_MAGIC: [u8; 4] = *b"NVXS";
/// Version of the outer record format.
pub const OUTER_VERSION: u16 = 1;
/// Length of an outer record header.
pub const OUTER_HEADER_LEN: usize = 44;
/// Largest payload of one outer record.
pub const OUTER_MAX_PAYLOAD: usize = 65_536;

/// Record with which the host client attaches, carrying the launch capability.
pub const OUTER_HOST_ATTACH: u8 = 2;
/// Record with which OpenVMM resets the session.
pub const OUTER_RESET: u8 = 3;
/// Record that carries an application frame.
pub const OUTER_DATA: u8 = 5;
/// Record with which OpenVMM asks an attaching host client to wait, because another client holds
/// the broker's single host slot.
pub const OUTER_WAIT: u8 = 6;
/// Record with which OpenVMM accepts the host client. It carries the session's instance ID and
/// epoch, which every data record repeats.
pub const OUTER_READY: u8 = 7;
/// Record with which OpenVMM rejects the host client's capability.
pub const OUTER_ERROR: u8 = 8;

/// Length of the launch capability that authenticates the host client.
pub const CAPABILITY_LEN: usize = 32;

/// Decoded outer record header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OuterHeader {
    /// One of the `OUTER_*` record types.
    pub record_type: u8,
    /// ID of the session instance, all zero before the session is established.
    pub instance_id: [u8; 16],
    /// Epoch of the session, zero before the session is established.
    pub epoch: u64,
    /// Sequence number of the record within its direction of the session.
    pub sequence: u64,
    /// Length of the payload that follows the header.
    pub length: usize,
}

/// Encodes one outer record.
pub fn encode_outer(
    record_type: u8,
    instance_id: &[u8; 16],
    epoch: u64,
    sequence: u64,
    payload: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    let length = u32::try_from(payload.len())
        .ok()
        .filter(|&length| length as usize <= OUTER_MAX_PAYLOAD)
        .ok_or_else(|| violation("control payload exceeds the outer protocol limit"))?;
    let mut record = Vec::with_capacity(OUTER_HEADER_LEN + payload.len());
    record.extend_from_slice(&OUTER_MAGIC);
    record.extend_from_slice(&OUTER_VERSION.to_le_bytes());
    record.push(record_type);
    record.push(0);
    record.extend_from_slice(instance_id);
    record.extend_from_slice(&epoch.to_le_bytes());
    record.extend_from_slice(&sequence.to_le_bytes());
    record.extend_from_slice(&length.to_le_bytes());
    record.extend_from_slice(payload);
    Ok(record)
}

/// Decodes and validates an outer record header.
pub fn decode_outer_header(header: &[u8; OUTER_HEADER_LEN]) -> Result<OuterHeader, ProtocolError> {
    let version = u16::from_le_bytes([header[4], header[5]]);
    let flags = header[7];
    let length = u32::from_le_bytes(array(&header[40..44])) as usize;
    if header[..4] != OUTER_MAGIC || version != OUTER_VERSION || flags != 0 {
        return Err(violation(
            "control endpoint returned an invalid outer record",
        ));
    }
    if length > OUTER_MAX_PAYLOAD {
        return Err(violation(
            "control endpoint returned an oversized outer record",
        ));
    }
    Ok(OuterHeader {
        record_type: header[6],
        instance_id: array(&header[8..24]),
        epoch: u64::from_le_bytes(array(&header[24..32])),
        sequence: u64::from_le_bytes(array(&header[32..40])),
        length,
    })
}
