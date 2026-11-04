//! Hand-rolled RFC 6455 framing for the fake Miniserver.
//!
//! The fake deliberately does not reuse `fastwebsockets` for its own side of
//! the wire. Two reasons:
//!
//! * Several tests have to put a *partial* frame on the socket — a header plus
//!   the first few hundred payload bytes, then a pause — which no framing
//!   library exposes. Owning the bytes is the whole point.
//! * The frame reader here is written to be cancel-safe (the accumulating
//!   buffer lives outside the read future and a frame is only consumed once it
//!   is complete), so the fake can sit in a `select!` without acquiring the
//!   very defect these tests exist to guard against.

use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xa;

/// One frame received from the client, already unmasked.
#[derive(Debug, Clone)]
pub struct ClientFrame {
    pub opcode: u8,
    pub fin: bool,
    pub payload: Vec<u8>,
}

impl ClientFrame {
    /// Payload as UTF-8; the client only ever sends text commands.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.payload).into_owned()
    }

    /// Close code of a close frame, if it carried one.
    pub fn close_code(&self) -> Option<u16> {
        (self.payload.len() >= 2).then(|| u16::from_be_bytes([self.payload[0], self.payload[1]]))
    }
}

/// The two header bytes plus the extended length of an unmasked frame.
pub fn frame_header(opcode: u8, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    out.push(0x80 | opcode);
    if len < 126 {
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out
}

/// A complete unmasked frame, ready to be written to the socket.
pub fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = frame_header(opcode, payload.len());
    out.extend_from_slice(payload);
    out
}

/// Close frame carrying `code`.
pub fn close_frame(code: u16) -> Vec<u8> {
    frame(OP_CLOSE, &code.to_be_bytes())
}

/// Take one complete frame out of `buf`, leaving partial data untouched.
///
/// Returning `None` means "not enough bytes yet"; nothing is consumed in that
/// case, which is what makes [`read_client_frame`] safe to cancel.
pub fn take_frame(buf: &mut Vec<u8>) -> Option<ClientFrame> {
    if buf.len() < 2 {
        return None;
    }
    let fin = buf[0] & 0x80 != 0;
    let opcode = buf[0] & 0x0f;
    let masked = buf[1] & 0x80 != 0;
    let length_code = buf[1] & 0x7f;

    let mut at = 2usize;
    let payload_len = match length_code {
        126 => {
            if buf.len() < at + 2 {
                return None;
            }
            let n = u16::from_be_bytes([buf[at], buf[at + 1]]) as usize;
            at += 2;
            n
        }
        127 => {
            if buf.len() < at + 8 {
                return None;
            }
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&buf[at..at + 8]);
            at += 8;
            u64::from_be_bytes(raw) as usize
        }
        n => n as usize,
    };

    let mask = if masked {
        if buf.len() < at + 4 {
            return None;
        }
        let m = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
        at += 4;
        Some(m)
    } else {
        None
    };

    if buf.len() < at + payload_len {
        return None;
    }
    let mut payload = buf[at..at + payload_len].to_vec();
    if let Some(m) = mask {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= m[i % 4];
        }
    }
    buf.drain(..at + payload_len);
    Some(ClientFrame {
        opcode,
        fin,
        payload,
    })
}

/// Read one frame, buffering whatever else arrives with it.
///
/// `Ok(None)` is end of stream. Cancel-safe: `buf` is owned by the caller and
/// `AsyncReadExt::read` loses nothing when dropped.
pub async fn read_client_frame(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
) -> io::Result<Option<ClientFrame>> {
    loop {
        if let Some(frame) = take_frame(buf) {
            return Ok(Some(frame));
        }
        let mut chunk = [0u8; 16 * 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..read]);
    }
}

/// Write every blob with its own `write_all` + `flush`, waiting `gap` in between.
///
/// Splitting a single frame across blobs is exactly how the cancel-safety
/// scenario is reproduced against the real client.
pub async fn write_blobs(
    stream: &mut TcpStream,
    blobs: &[Vec<u8>],
    gap: std::time::Duration,
) -> io::Result<()> {
    for (index, blob) in blobs.iter().enumerate() {
        if index > 0 && !gap.is_zero() {
            tokio::time::sleep(gap).await;
        }
        stream.write_all(blob).await?;
        stream.flush().await?;
    }
    Ok(())
}

/// `Sec-WebSocket-Accept` for a client key, per RFC 6455.
pub fn accept_key(client_key: &str) -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;
    use sha1::{Digest, Sha1};

    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(GUID.as_bytes());
    B64.encode(hasher.finalize())
}
