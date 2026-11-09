//! Laengenpraefix-Framing fuer Nachrichten auf Streams: `[len u32 BE][postcard]`.

use app_proto::MAX_MESSAGE_LEN;
use iroh::endpoint::{RecvStream, SendStream};
use serde::{de::DeserializeOwned, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("Stream error: {0}")]
    Io(String),
    #[error("Frame too large ({0} bytes)")]
    TooLarge(usize),
    #[error("Invalid message: {0}")]
    Wire(#[from] app_proto::WireError),
    #[error("Stream ended")]
    Eof,
}

pub async fn write_frame<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<(), FrameError> {
    let body = app_proto::encode(msg)?;
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(&body);
    send.write_all(&buf)
        .await
        .map_err(|e| FrameError::Io(e.to_string()))
}

/// Liest einen Frame; `Err(Eof)`, wenn der Stream sauber vor einem Frame endet.
pub async fn read_frame<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T, FrameError> {
    read_frame_max(recv, MAX_MESSAGE_LEN).await
}

/// Wie [`read_frame`], mit eigener Obergrenze (z. B. fuer den Handshake, den auch Fremde
/// schicken duerfen: der Puffer wird vor dem Empfang angelegt).
pub async fn read_frame_max<T: DeserializeOwned>(
    recv: &mut RecvStream,
    max: usize,
) -> Result<T, FrameError> {
    let mut len = [0u8; 4];
    match recv.read_exact(&mut len).await {
        Ok(()) => {}
        Err(iroh::endpoint::ReadExactError::FinishedEarly(0)) => return Err(FrameError::Eof),
        Err(e) => return Err(FrameError::Io(e.to_string())),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > max.min(MAX_MESSAGE_LEN) {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body)
        .await
        .map_err(|e| FrameError::Io(e.to_string()))?;
    Ok(app_proto::decode(&body)?)
}
