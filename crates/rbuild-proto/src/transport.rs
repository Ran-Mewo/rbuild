//! Length-prefixed, multiplexed frame transport.
//!
//! Everything between client and daemon travels as `Frame`s over a single
//! byte stream — in production that stream is the stdin/stdout pair of an
//! `ssh host rbuildd serve` child process, so no extra ports are ever opened.
//!
//! Each frame carries a `stream` id so independent logical channels (file
//! sync, build I/O) can share the one connection. Control frames carry a
//! JSON-encoded [`Message`]; data frames carry opaque bytes (already
//! compressed by the caller) to avoid the cost of encoding bulk payloads.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::proto::Message;

/// Largest frame payload accepted off the wire, a guard against a corrupt
/// or hostile length prefix forcing a huge allocation. Bulk file data is
/// chunked well below this.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Control,
    Data,
    Close,
}

impl FrameKind {
    fn to_byte(self) -> u8 {
        match self {
            FrameKind::Control => 1,
            FrameKind::Data => 2,
            FrameKind::Close => 3,
        }
    }

    fn from_byte(b: u8) -> io::Result<Self> {
        match b {
            1 => Ok(FrameKind::Control),
            2 => Ok(FrameKind::Data),
            3 => Ok(FrameKind::Close),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown frame kind {other}"),
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub stream: u32,
    pub kind: FrameKind,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn control(stream: u32, msg: &Message) -> anyhow::Result<Self> {
        Ok(Frame {
            stream,
            kind: FrameKind::Control,
            payload: serde_json::to_vec(msg)?,
        })
    }

    pub fn data(stream: u32, payload: Vec<u8>) -> Self {
        Frame {
            stream,
            kind: FrameKind::Data,
            payload,
        }
    }

    pub fn close(stream: u32) -> Self {
        Frame {
            stream,
            kind: FrameKind::Close,
            payload: Vec::new(),
        }
    }

    pub fn as_message(&self) -> anyhow::Result<Message> {
        Ok(serde_json::from_slice(&self.payload)?)
    }
}

/// Wire layout: `len: u32` (covers everything after it) · `kind: u8` ·
/// `stream: u32` · `payload`. All integers big-endian.
pub async fn write_frame<W>(w: &mut W, frame: &Frame) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let body_len = 1 + 4 + frame.payload.len();
    if body_len as u64 > MAX_FRAME_LEN as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame exceeds MAX_FRAME_LEN",
        ));
    }
    w.write_u32(body_len as u32).await?;
    w.write_u8(frame.kind.to_byte()).await?;
    w.write_u32(frame.stream).await?;
    w.write_all(&frame.payload).await?;
    w.flush().await?;
    Ok(())
}

/// Reads one frame. Returns `Ok(None)` on a clean EOF at a frame boundary,
/// which is how a peer signals it closed the connection.
pub async fn read_frame<R>(r: &mut R) -> io::Result<Option<Frame>>
where
    R: AsyncRead + Unpin,
{
    let body_len = match r.read_u32().await {
        Ok(n) => n,
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    };
    if !(5..=MAX_FRAME_LEN).contains(&body_len) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid frame length {body_len}"),
        ));
    }
    let kind = FrameKind::from_byte(r.read_u8().await?)?;
    let stream = r.read_u32().await?;
    let mut payload = vec![0u8; (body_len - 5) as usize];
    r.read_exact(&mut payload).await?;
    Ok(Some(Frame {
        stream,
        kind,
        payload,
    }))
}
