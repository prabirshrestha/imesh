//! Replication between two members on ALPN `imesh/sync/1`.
//!
//! Every stream starts with an [`Open`] frame that says what it carries:
//! - `Ping`: liveness and path. The other side answers `Pong`.
//! - `Push`: records this machine wrote. No answer.
//! - `Exchange`: anti-entropy. Both sides end with the winner for every key.
//!
//! Frames are postcard, length-prefixed like the API. Summaries and records travel in chunks, so
//! the state can grow past one frame.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use iroh::endpoint::Connection;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    api::{self, Stream},
    state::{Record, Stamp, State},
};

pub const SYNC_ALPN: &[u8] = b"imesh/sync/1";

/// A chunk stays well under the frame limit.
const CHUNK_BYTES: usize = 256 << 10;

#[derive(Debug, Serialize, Deserialize)]
enum Open {
    Ping,
    Push(Vec<Record>),
    Exchange,
}

#[derive(Debug, Serialize, Deserialize)]
enum Msg {
    Pong,
    Summary(Vec<(String, Stamp)>),
    Records(Vec<Record>),
    Done,
}

async fn send<T: Serialize>(s: &mut Stream, v: &T) -> Result<()> {
    s.send(postcard::to_stdvec(v)?.into()).await?;
    Ok(())
}

async fn recv<T: DeserializeOwned>(s: &mut Stream) -> Result<T> {
    let frame = s.next().await.context("stream closed")??;
    Ok(postcard::from_bytes(&frame)?)
}

async fn open(conn: &Connection, first: &Open) -> Result<Stream> {
    let (send_half, recv_half) = conn.open_bi().await?;
    let mut s = api::framed(tokio::io::join(recv_half, send_half));
    send(&mut s, first).await?;
    Ok(s)
}

/// Splits items into chunks of about [`CHUNK_BYTES`].
fn chunks<T: Serialize>(items: Vec<T>) -> Vec<Vec<T>> {
    let mut out = vec![Vec::new()];
    let mut size = 0;
    for item in items {
        let n = postcard::to_stdvec(&item).map(|v| v.len()).unwrap_or(CHUNK_BYTES);
        if size + n > CHUNK_BYTES && !out.last().unwrap().is_empty() {
            out.push(Vec::new());
            size = 0;
        }
        size += n;
        out.last_mut().unwrap().push(item);
    }
    out
}

async fn send_all<T: Serialize>(s: &mut Stream, items: Vec<T>, wrap: fn(Vec<T>) -> Msg) -> Result<()> {
    for chunk in chunks(items) {
        send(s, &wrap(chunk)).await?;
    }
    send(s, &Msg::Done).await
}

async fn recv_summary(s: &mut Stream) -> Result<Vec<(String, Stamp)>> {
    let mut all = Vec::new();
    loop {
        match recv(s).await? {
            Msg::Summary(part) => all.extend(part),
            Msg::Done => return Ok(all),
            other => bail!("expected a summary, got {other:?}"),
        }
    }
}

async fn recv_records(s: &mut Stream, state: &State) -> Result<usize> {
    let mut applied = 0;
    loop {
        match recv(s).await? {
            Msg::Records(part) => applied += state.merge_all(part),
            Msg::Done => return Ok(applied),
            other => bail!("expected records, got {other:?}"),
        }
    }
}

pub async fn ping(conn: &Connection) -> Result<()> {
    let mut s = open(conn, &Open::Ping).await?;
    match recv(&mut s).await? {
        Msg::Pong => Ok(()),
        other => bail!("expected pong, got {other:?}"),
    }
}

pub async fn push(conn: &Connection, records: Vec<Record>) -> Result<()> {
    for chunk in chunks(records) {
        let mut s = open(conn, &Open::Push(chunk)).await?;
        s.close().await?;
    }
    Ok(())
}

/// The longest one anti-entropy exchange may take.
pub const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(120);

/// Anti-entropy with one peer. Returns how many records this side applied.
pub async fn exchange(state: &State, conn: &Connection) -> Result<usize> {
    tokio::time::timeout(EXCHANGE_TIMEOUT, exchange_once(state, conn))
        .await
        .map_err(|_| anyhow!("exchange took over {EXCHANGE_TIMEOUT:?}"))?
}

async fn exchange_once(state: &State, conn: &Connection) -> Result<usize> {
    let mut s = open(conn, &Open::Exchange).await?;
    send_all(&mut s, state.summary(), Msg::Summary).await?;
    let applied = recv_records(&mut s, state).await?;
    let theirs = recv_summary(&mut s).await?;
    send_all(&mut s, state.newer_than(&theirs), Msg::Records).await?;
    s.close().await?;
    Ok(applied)
}

/// Serves one incoming stream.
pub async fn serve(state: &State, mut s: Stream) -> Result<()> {
    match recv::<Open>(&mut s).await? {
        Open::Ping => send(&mut s, &Msg::Pong).await?,
        Open::Push(records) => {
            state.merge_all(records);
        }
        Open::Exchange => {
            let serve = async {
                let theirs = recv_summary(&mut s).await?;
                send_all(&mut s, state.newer_than(&theirs), Msg::Records).await?;
                send_all(&mut s, state.summary(), Msg::Summary).await?;
                recv_records(&mut s, state).await
            };
            tokio::time::timeout(EXCHANGE_TIMEOUT, serve)
                .await
                .map_err(|_| anyhow!("exchange took over {EXCHANGE_TIMEOUT:?}"))??;
        }
    }
    s.close().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_split_by_size_and_keep_order() {
        let items: Vec<Vec<u8>> = (0..10).map(|i| vec![i as u8; 100 << 10]).collect();
        let parts = chunks(items.clone());
        assert!(parts.len() >= 4, "10 x 100 KiB should need several 256 KiB chunks");
        assert_eq!(parts.concat(), items);
        assert_eq!(chunks(Vec::<u8>::new()), vec![Vec::<u8>::new()]);
    }
}
