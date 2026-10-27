//! The imesh API: length-prefixed JSON frames over any byte stream.
//!
//! One call per stream. The client sends one [`Request`], and the server answers with one
//! [`Reply`]. The same bytes travel over the
//! unix socket, over `ssh ... imesh dial-stdio`, and over an iroh stream on [`API_ALPN`].

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use iroh::EndpointId;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

pub const API_ALPN: &[u8] = b"imesh/api/0";
const MAX_FRAME: usize = 16 << 20;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Io for T {}

pub type Stream = Framed<Box<dyn Io>, LengthDelimitedCodec>;

pub fn framed(io: impl Io) -> Stream {
    let codec = LengthDelimitedCodec::builder().max_frame_length(MAX_FRAME).new_codec();
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
    /// Set by the entry machine on a call it forwards for a CLI key: that key. The target
    /// trusts it only from a member, and serves the call as that key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<EndpointId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reply {
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

/// Sends a request and returns the reply.
pub async fn call(mut s: Stream, req: &Request) -> Result<Value> {
    send(&mut s, req).await?;
    match recv::<Reply>(&mut s).await?.context("stream closed before reply")? {
        Reply::Ok(v) => Ok(v),
        Reply::Err(e) => Err(anyhow!(e)),
    }
}

/// Decodes call params into a typed struct.
pub fn params<T: DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(if v.is_null() { Value::Object(Default::default()) } else { v }).context("bad params")
}

/// `machine.inspect`: what a machine says about itself. Never holds a secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inspect {
    pub id: EndpointId,
    /// Name, address, version, platform, and labels.
    pub info: crate::state::MachineInfo,
    pub cluster: Option<String>,
    pub member: bool,
    /// The `--relay` list the machine started with.
    pub relays: Vec<iroh::RelayUrl>,
    pub no_n0: bool,
}

/// One row of `cluster.machines`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineRow {
    pub id: EndpointId,
    pub name: String,
    pub info: Option<crate::state::MachineInfo>,
    pub status: crate::daemon::PeerStatus,
}

/// What a new machine needs to join a cluster: the result of `cluster.add`
/// ([`crate::daemon::Machine::admit`]) and the params of `cluster.join`. As text it is
/// `imeshjoin` and base64url JSON. It holds no secret unless the cluster relay setting has a token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JoinTicket {
    pub cluster: String,
    pub peers: Vec<iroh::EndpointAddr>,
    #[serde(default)]
    pub relays: crate::state::RelaySettings,
}

const JOIN_PREFIX: &str = "imeshjoin";

impl std::fmt::Display for JoinTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let json = serde_json::to_vec(self).map_err(|_| std::fmt::Error)?;
        write!(f, "{JOIN_PREFIX}{}", data_encoding::BASE64URL_NOPAD.encode(&json))
    }
}

impl std::str::FromStr for JoinTicket {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let body = s.trim().strip_prefix(JOIN_PREFIX).context("not an imesh join ticket")?;
        let bytes = data_encoding::BASE64URL_NOPAD.decode(body.as_bytes()).context("bad join ticket")?;
        serde_json::from_slice(&bytes).context("bad join ticket")
    }
}

/// The result of `cluster.join`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Joined {
    pub cluster: String,
    pub members: usize,
}

/// One row of `client.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRow {
    pub key: EndpointId,
    #[serde(default)]
    pub name: String,
}

/// The result of `relays.get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelaysInfo {
    /// The cluster setting. Empty means every machine uses the relays it started with.
    pub cluster: Vec<iroh::RelayUrl>,
    pub token_set: bool,
    /// Relays that members serve.
    pub served: Vec<Served>,
    /// The relays the answering machine uses now.
    pub in_use: Vec<iroh::RelayUrl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Served {
    pub machine: String,
    pub url: iroh::RelayUrl,
}
