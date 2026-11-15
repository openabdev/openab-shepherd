//! The wire between a holder and its front (issue #76).
//!
//! A frame is `u32 hdr_len | u32 payload_len | JSON header | raw payload`,
//! both lengths big-endian. The payload carries raw bytes (terminal output,
//! input, meta), so nothing is base64-encoded. The header and the payload
//! together are at most [`MAX_FRAME`] bytes; a longer announced length is
//! refused before any of it is read.
//!
//! The link is two socketpairs (see `server.rs` for why): `ops` carries
//! requests from the front and their replies from the holder, `events`
//! carries the holder's output, idle, exit and gap events. The first frame on
//! each link is the holder's `hello`; the front's first frame on `ops` is its
//! own. The versions must be equal.
//!
//! Errors in this module never include header or payload text: a spawn
//! request carries profile secrets in its environment, and a log line made
//! from a wire error must not repeat them.

use serde_json::{json, Value};
use std::io::{self, Read, Write};

/// The protocol version. A holder and a front talk only when theirs are
/// equal; bump it for any change to a frame, an op or an event.
pub(crate) const HOLDER_VERSION: u32 = 1;

/// The most bytes of header plus payload in one frame.
pub(crate) const MAX_FRAME: usize = 16 * 1024 * 1024;

/// The most output bytes in one `output` event.
pub(crate) const OUTPUT_CHUNK: usize = 64 * 1024;

/// One frame: a JSON object and raw bytes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Frame {
    pub header: Value,
    pub payload: Vec<u8>,
}

impl Frame {
    #[cfg_attr(not(test), allow(dead_code))] // the contract tests build frames with it
    pub(crate) fn new(header: Value, payload: Vec<u8>) -> Self {
        Self { header, payload }
    }

    /// The header's string field `key`.
    pub(crate) fn str(&self, key: &str) -> Option<&str> {
        self.header.get(key).and_then(Value::as_str)
    }

    pub(crate) fn u64(&self, key: &str) -> Option<u64> {
        self.header.get(key).and_then(Value::as_u64)
    }
}

/// Why a frame could not be read or written.
#[derive(Debug)]
pub(crate) enum WireError {
    Io(io::Error),
    /// The peer closed the link between two frames.
    Eof,
    /// The peer closed the link inside a frame.
    Truncated,
    /// A frame announced (or was asked to carry) more than [`MAX_FRAME`].
    Oversize(usize),
    /// The frame is not a prefix, a JSON object and bytes.
    Malformed(&'static str),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "link error: {err}"),
            Self::Eof => write!(f, "the peer closed the link"),
            Self::Truncated => write!(f, "the peer closed the link inside a frame"),
            Self::Oversize(len) => write!(f, "a frame of {len} bytes exceeds {MAX_FRAME}"),
            Self::Malformed(why) => write!(f, "malformed frame: {why}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<io::Error> for WireError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Write one frame with a single `write_all`, so a caller that holds the
/// stream's mutex keeps frames whole.
pub(crate) fn write_frame(
    w: &mut impl Write,
    header: &Value,
    payload: &[u8],
) -> Result<(), WireError> {
    let header = serde_json::to_vec(header).map_err(|_| WireError::Malformed("header"))?;
    let total = header.len() + payload.len();
    if total > MAX_FRAME {
        return Err(WireError::Oversize(total));
    }
    let mut out = Vec::with_capacity(8 + total);
    out.extend_from_slice(&(header.len() as u32).to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(payload);
    w.write_all(&out)?;
    Ok(())
}

/// Read exactly `buf.len()` bytes. `Ok(false)` for a clean end before the
/// first byte, `Truncated` for an end after it.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> Result<bool, WireError> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) if got == 0 => return Ok(false),
            Ok(0) => return Err(WireError::Truncated),
            Ok(n) => got += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(WireError::Io(err)),
        }
    }
    Ok(true)
}

/// Read one frame. `Eof` when the peer closed before a frame began.
pub(crate) fn read_frame(r: &mut impl Read) -> Result<Frame, WireError> {
    let mut prefix = [0u8; 8];
    if !read_full(r, &mut prefix)? {
        return Err(WireError::Eof);
    }
    let hdr_len = u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
    let payload_len = u32::from_be_bytes([prefix[4], prefix[5], prefix[6], prefix[7]]) as usize;
    let total = hdr_len + payload_len;
    if total > MAX_FRAME {
        return Err(WireError::Oversize(total));
    }
    let mut body = vec![0u8; total];
    if total > 0 && !read_full(r, &mut body)? {
        return Err(WireError::Truncated);
    }
    let payload = body.split_off(hdr_len);
    let header: Value =
        serde_json::from_slice(&body).map_err(|_| WireError::Malformed("header is not JSON"))?;
    if !header.is_object() {
        return Err(WireError::Malformed("header is not an object"));
    }
    Ok(Frame { header, payload })
}

/// The `hello` each side sends first.
pub(crate) fn hello(version: u32, link: &str) -> Value {
    json!({"t": "hello", "v": version, "link": link})
}

/// What a peer's first frame says about its version.
pub(crate) enum Greeting {
    Hello(u32),
    /// The peer refused ours: `error{version}` with both numbers.
    Refused {
        holder: u32,
        front: u32,
    },
}

pub(crate) fn greeting(frame: &Frame) -> Result<Greeting, WireError> {
    match frame.str("t") {
        Some("hello") => frame
            .u64("v")
            .and_then(|v| u32::try_from(v).ok())
            .map(Greeting::Hello)
            .ok_or(WireError::Malformed("hello has no version")),
        Some("error") if frame.str("kind") == Some("version") => {
            let num = |key| frame.u64(key).and_then(|v| u32::try_from(v).ok());
            match (num("holder"), num("front")) {
                (Some(holder), Some(front)) => Ok(Greeting::Refused { holder, front }),
                _ => Err(WireError::Malformed("version error has no numbers")),
            }
        }
        _ => Err(WireError::Malformed("the first frame is not a hello")),
    }
}

/// The refusal a side writes before it closes on a version mismatch.
pub(crate) fn version_error(holder: u32, front: u32) -> Value {
    json!({"t": "error", "kind": "version", "holder": holder, "front": front})
}

// ---------------------------------------------------------------------------
// Errors carried in replies
// ---------------------------------------------------------------------------

/// An `io::Error` as a reply header's `err` object: the kind by name, and the
/// OS error number when there is one, so the front sees the same
/// `raw_os_error` (EAGAIN, EIO) the holder did.
pub(crate) fn io_error_json(err: &io::Error) -> Value {
    let kind = match err.kind() {
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::WouldBlock => "would_block",
        io::ErrorKind::InvalidInput => "invalid_input",
        io::ErrorKind::Unsupported => "unsupported",
        io::ErrorKind::Interrupted => "interrupted",
        io::ErrorKind::BrokenPipe => "broken_pipe",
        _ => "other",
    };
    json!({"k": kind, "os": err.raw_os_error(), "m": err.to_string()})
}

pub(crate) fn io_error_from_json(err: &Value) -> io::Error {
    if let Some(os) = err.get("os").and_then(Value::as_i64) {
        return io::Error::from_raw_os_error(os as i32);
    }
    let msg = err
        .get("m")
        .and_then(Value::as_str)
        .unwrap_or("the holder refused the request")
        .to_string();
    let kind = match err.get("k").and_then(Value::as_str) {
        Some("not_found") => io::ErrorKind::NotFound,
        Some("would_block") => io::ErrorKind::WouldBlock,
        Some("invalid_input") => io::ErrorKind::InvalidInput,
        Some("unsupported") => io::ErrorKind::Unsupported,
        Some("interrupted") => io::ErrorKind::Interrupted,
        Some("broken_pipe") => io::ErrorKind::BrokenPipe,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, msg)
}

use crate::backend::{BackendError, ErrorKind};

/// A spawn failure as a reply header's `berr` object.
pub(crate) fn backend_error_json(err: &BackendError) -> Value {
    let (kind, extra) = match &err.kind {
        ErrorKind::Unavailable => ("unavailable", None),
        ErrorKind::Timeout => ("timeout", None),
        ErrorKind::NotFound => ("not_found", None),
        ErrorKind::NameTaken => ("name_taken", None),
        ErrorKind::InvalidInput => ("invalid_input", None),
        ErrorKind::Protocol => ("protocol", None),
        ErrorKind::Unsupported(what) => ("unsupported", Some(what.clone())),
        _ => ("other", None),
    };
    json!({"k": kind, "m": err.detail, "u": extra})
}

pub(crate) fn backend_error_from_json(err: &Value) -> BackendError {
    let detail = err
        .get("m")
        .and_then(Value::as_str)
        .unwrap_or("the holder refused the request")
        .to_string();
    let kind = match err.get("k").and_then(Value::as_str) {
        Some("unavailable") => ErrorKind::Unavailable,
        Some("timeout") => ErrorKind::Timeout,
        Some("not_found") => ErrorKind::NotFound,
        Some("name_taken") => ErrorKind::NameTaken,
        Some("invalid_input") => ErrorKind::InvalidInput,
        Some("protocol") => ErrorKind::Protocol,
        Some("unsupported") => ErrorKind::Unsupported(
            err.get("u")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        _ => ErrorKind::Other,
    };
    BackendError::new(kind, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn encode(header: &Value, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_frame(&mut out, header, payload).unwrap();
        out
    }

    #[test]
    fn a_frame_round_trips_with_a_binary_payload() {
        let payload: Vec<u8> = (0..=255u8).chain(0..=255u8).collect();
        let header = json!({"op": "write", "id": 7, "pane": "n:1"});
        let bytes = encode(&header, &payload);
        let frame = read_frame(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(frame.header, header);
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn the_prefix_is_two_big_endian_lengths() {
        let bytes = encode(&json!({}), b"abc");
        assert_eq!(&bytes[..8], &[0, 0, 0, 2, 0, 0, 0, 3]);
        assert_eq!(&bytes[8..], b"{}abc");
    }

    #[test]
    fn frames_follow_each_other_and_a_clean_end_is_eof() {
        let mut bytes = encode(&json!({"a": 1}), b"x");
        bytes.extend(encode(&json!({"b": 2}), b""));
        let mut cur = Cursor::new(bytes);
        assert_eq!(read_frame(&mut cur).unwrap().header, json!({"a": 1}));
        assert_eq!(read_frame(&mut cur).unwrap().header, json!({"b": 2}));
        assert!(matches!(read_frame(&mut cur), Err(WireError::Eof)));
    }

    #[test]
    fn a_frame_cut_anywhere_is_truncated_not_eof() {
        let whole = encode(&json!({"op": "list", "id": 1}), b"payload");
        for cut in 1..whole.len() {
            let got = read_frame(&mut Cursor::new(whole[..cut].to_vec()));
            assert!(
                matches!(got, Err(WireError::Truncated)),
                "cut at {cut}: {got:?}"
            );
        }
    }

    #[test]
    fn an_oversize_length_is_refused_before_it_is_read() {
        // Announces 16 MiB + 1 and has none of it: refused, not Truncated.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&(MAX_FRAME as u32 - 1).to_be_bytes());
        let got = read_frame(&mut Cursor::new(bytes));
        assert!(matches!(got, Err(WireError::Oversize(n)) if n == MAX_FRAME + 1));

        // u32::MAX in either field cannot overflow the sum.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u32::MAX.to_be_bytes());
        bytes.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            read_frame(&mut Cursor::new(bytes)),
            Err(WireError::Oversize(_))
        ));
    }

    #[test]
    fn exactly_the_limit_is_accepted_and_one_more_is_refused_on_write() {
        let header = json!({});
        let payload = vec![7u8; MAX_FRAME - 2];
        let bytes = encode(&header, &payload);
        assert_eq!(
            read_frame(&mut Cursor::new(bytes)).unwrap().payload.len(),
            payload.len()
        );
        let too_big = vec![7u8; MAX_FRAME - 1];
        let mut out = Vec::new();
        assert!(matches!(
            write_frame(&mut out, &header, &too_big),
            Err(WireError::Oversize(_))
        ));
        assert!(out.is_empty(), "a refused frame writes nothing");
    }

    #[test]
    fn a_header_that_is_not_a_json_object_is_malformed() {
        for body in [&b"not json"[..], b"[1,2]", b"3", b""] {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&0u32.to_be_bytes());
            bytes.extend_from_slice(body);
            assert!(
                matches!(
                    read_frame(&mut Cursor::new(bytes)),
                    Err(WireError::Malformed(_))
                ),
                "{body:?}"
            );
        }
    }

    #[test]
    fn a_wire_error_never_repeats_the_frame() {
        let secret = "sk-SECRET-VALUE";
        let mut bytes = Vec::new();
        let body = format!("{{\"env\": \"{secret}\" nonsense");
        bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(body.as_bytes());
        let err = read_frame(&mut Cursor::new(bytes)).unwrap_err();
        assert!(!err.to_string().contains(secret), "{err}");
        assert!(!format!("{err:?}").contains(secret), "{err:?}");
    }

    #[test]
    fn io_errors_keep_their_kind_and_os_number() {
        for err in [
            io::Error::from_raw_os_error(libc::EAGAIN),
            io::Error::from_raw_os_error(libc::EIO),
            io::Error::new(io::ErrorKind::NotFound, "no such pane"),
            io::Error::new(io::ErrorKind::InvalidInput, "too big"),
            io::Error::new(io::ErrorKind::Unsupported, "no tail"),
        ] {
            let back = io_error_from_json(&io_error_json(&err));
            assert_eq!(back.kind(), err.kind(), "{err}");
            assert_eq!(back.raw_os_error(), err.raw_os_error(), "{err}");
        }
        assert_eq!(
            io_error_from_json(&io_error_json(&io::Error::from_raw_os_error(libc::EAGAIN))).kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn hello_and_refusal_are_told_apart() {
        let frame = Frame::new(hello(3, "ops"), Vec::new());
        assert!(matches!(greeting(&frame), Ok(Greeting::Hello(3))));
        let frame = Frame::new(version_error(1, 2), Vec::new());
        assert!(matches!(
            greeting(&frame),
            Ok(Greeting::Refused {
                holder: 1,
                front: 2
            })
        ));
        let frame = Frame::new(json!({"op": "list"}), Vec::new());
        assert!(greeting(&frame).is_err());
    }
}
