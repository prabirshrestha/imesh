//! The imesh API: length-prefixed JSON frames over any byte stream.
//!
//! One call per stream. The client sends one [`Request`], the server sends zero or more
//! [`Reply::Item`] and ends with [`Reply::Ok`] or [`Reply::Err`]. The same bytes travel over the
//! unix socket, over `ssh ... imesh dial-stdio`, and over an iroh stream on [`API_ALPN`].

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use iroh::EndpointId;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

pub const API_ALPN: &[u8] = b"imesh/api/0";
pub const LIVE_ALPN: &[u8] = b"imesh/live/0";
pub const MAX_FRAME: usize = 16 << 20;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Io for T {}

pub type Stream = Framed<Box<dyn Io>, LengthDelimitedCodec>;

pub fn framed(io: impl Io) -> Stream {
    let codec = LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME)
        .new_codec();
    Framed::new(Box::new(io) as Box<dyn Io>, codec)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Machine that should serve the call. `None` means the machine that receives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<EndpointId>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reply {
    Item(Value),
    Ok(Value),
    Err(String),
}

pub async fn send<T: Serialize>(s: &mut Stream, v: &T) -> Result<()> {
    s.send(serde_json::to_vec(v)?.into()).await?;
    Ok(())
}

pub async fn recv<T: DeserializeOwned>(s: &mut Stream) -> Result<Option<T>> {
    match s.next().await {
        None => Ok(None),
        Some(frame) => Ok(Some(serde_json::from_slice(&frame?)?)),
    }
}

/// Sends a request and collects items and the final value.
pub async fn call(mut s: Stream, req: &Request) -> Result<(Vec<Value>, Value)> {
    send(&mut s, req).await?;
    let mut items = Vec::new();
    loop {
        match recv::<Reply>(&mut s).await?.context("stream closed before reply")? {
            Reply::Item(v) => items.push(v),
            Reply::Ok(v) => return Ok((items, v)),
            Reply::Err(e) => return Err(anyhow!(e)),
        }
    }
}

/// Decodes call params into a typed struct.
pub fn params<T: DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(if v.is_null() { Value::Object(Default::default()) } else { v })
        .context("bad params")
}
