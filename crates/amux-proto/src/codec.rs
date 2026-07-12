//! A length-prefixed (`u32` big-endian) `postcard` codec. `S` is the type this side sends,
//! `R` the type it receives, so the same struct serves both peers via the `ClientCodec` /
//! `ServerCodec` aliases. See `docs/DESIGN.md` §6 and §11 (postcard over bincode).

use std::marker::PhantomData;

use bytes::{Buf, BufMut, BytesMut};
use serde::{de::DeserializeOwned, Serialize};
use tokio_util::codec::{Decoder, Encoder};

use crate::{ClientMsg, DaemonMsg, PROTO_VERSION};

/// Hard cap on a single frame (4 MiB) — guards against a corrupt or hostile length prefix.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Errors from framing / (de)serialization / version negotiation.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("frame of {0} bytes exceeds the maximum frame size")]
    FrameTooLarge(usize),
    #[error("(de)serialization failed: {0}")]
    Serde(#[from] postcard::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol version mismatch: ours={ours}, theirs={theirs}")]
    VersionMismatch { ours: u32, theirs: u32 },
}

/// Verify a peer's advertised protocol version against ours.
pub fn check_version(theirs: u32) -> Result<(), ProtoError> {
    if theirs == PROTO_VERSION {
        Ok(())
    } else {
        Err(ProtoError::VersionMismatch {
            ours: PROTO_VERSION,
            theirs,
        })
    }
}

/// Length-prefixed postcard codec. `S` = sent type, `R` = received type.
///
/// The `PhantomData<fn(S) -> R>` keeps the codec `Send + Sync + 'static` regardless of the
/// message types, with the right variance, without owning either.
pub struct WireCodec<S, R> {
    _marker: PhantomData<fn(S) -> R>,
}

impl<S, R> WireCodec<S, R> {
    pub fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

impl<S, R> Default for WireCodec<S, R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Serialize, R> Encoder<S> for WireCodec<S, R> {
    type Error = ProtoError;

    fn encode(&mut self, item: S, dst: &mut BytesMut) -> Result<(), ProtoError> {
        let body = postcard::to_stdvec(&item)?;
        if body.len() > MAX_FRAME_BYTES {
            return Err(ProtoError::FrameTooLarge(body.len()));
        }
        dst.reserve(4 + body.len());
        dst.put_u32(body.len() as u32);
        dst.extend_from_slice(&body);
        Ok(())
    }
}

impl<S, R: DeserializeOwned> Decoder for WireCodec<S, R> {
    type Item = R;
    type Error = ProtoError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<R>, ProtoError> {
        if src.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]) as usize;
        if len > MAX_FRAME_BYTES {
            return Err(ProtoError::FrameTooLarge(len));
        }
        if src.len() < 4 + len {
            // Reserve the rest of the frame so the next read can complete it.
            src.reserve(4 + len - src.len());
            return Ok(None);
        }
        src.advance(4);
        let body = src.split_to(len);
        Ok(Some(postcard::from_bytes(&body)?))
    }
}

/// The client's view: sends `ClientMsg`, receives `DaemonMsg`.
pub type ClientCodec = WireCodec<ClientMsg, DaemonMsg>;
/// The daemon's view: sends `DaemonMsg`, receives `ClientMsg`.
pub type ServerCodec = WireCodec<DaemonMsg, ClientMsg>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentInfo, Size};
    use amux_core::agent::{AgentId, AgentState};
    use chrono::Utc;
    use proptest::prelude::*;

    /// Encode then decode with a symmetric codec; assert we get the same value back and the
    /// buffer is fully consumed.
    fn roundtrip<T>(msg: T)
    where
        T: Serialize + DeserializeOwned + Clone + PartialEq + std::fmt::Debug,
    {
        let mut codec = WireCodec::<T, T>::new();
        let mut buf = BytesMut::new();
        codec.encode(msg.clone(), &mut buf).unwrap();
        let got = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(got, msg);
        assert!(buf.is_empty(), "buffer not fully consumed");
    }

    fn sample_info() -> AgentInfo {
        AgentInfo {
            id: AgentId::new(),
            name: "auth".into(),
            branch: "feat/auth".into(),
            state: AgentState::Working,
            last_activity: Utc::now(),
        }
    }

    #[test]
    fn client_messages_roundtrip() {
        let id = AgentId::new();
        roundtrip(ClientMsg::Hello {
            proto_version: PROTO_VERSION,
        });
        roundtrip(ClientMsg::ListAgents);
        roundtrip(ClientMsg::CreateAgent {
            branch: "feat/x".into(),
        });
        roundtrip(ClientMsg::DeleteAgent { id });
        roundtrip(ClientMsg::ResumeAgent { id });
        roundtrip(ClientMsg::Attach {
            id,
            size: Size {
                cols: 120,
                rows: 40,
            },
        });
        roundtrip(ClientMsg::Input {
            id,
            bytes: vec![0x1b, b'[', b'A'],
        });
        roundtrip(ClientMsg::Resize {
            id,
            size: Size { cols: 80, rows: 24 },
        });
    }

    #[test]
    fn daemon_messages_roundtrip() {
        let id = AgentId::new();
        roundtrip(DaemonMsg::Hello {
            proto_version: PROTO_VERSION,
        });
        roundtrip(DaemonMsg::Agents(vec![sample_info(), sample_info()]));
        roundtrip(DaemonMsg::AgentAdded(sample_info()));
        roundtrip(DaemonMsg::AgentRemoved { id });
        roundtrip(DaemonMsg::StateChanged {
            id,
            state: AgentState::Exited { code: Some(0) },
        });
        roundtrip(DaemonMsg::OutputSnapshot {
            id,
            bytes: vec![1, 2, 3],
        });
        roundtrip(DaemonMsg::Output {
            id,
            bytes: b"hello\r\n".to_vec(),
        });
        roundtrip(DaemonMsg::Error {
            message: "boom".into(),
        });
    }

    #[test]
    fn partial_frame_yields_none_until_complete() {
        let mut codec = WireCodec::<ClientMsg, ClientMsg>::new();
        let mut full = BytesMut::new();
        let msg = ClientMsg::Input {
            id: AgentId::new(),
            bytes: vec![1, 2, 3, 4, 5],
        };
        codec.encode(msg.clone(), &mut full).unwrap();
        let bytes = full.to_vec();

        let mut partial = BytesMut::new();
        for (i, b) in bytes.iter().enumerate() {
            partial.put_u8(*b);
            let res = codec.decode(&mut partial).unwrap();
            if i + 1 < bytes.len() {
                assert!(res.is_none(), "decoded early after {} bytes", i + 1);
            } else {
                assert_eq!(res, Some(msg.clone()));
            }
        }
    }

    #[test]
    fn decodes_two_frames_from_one_buffer() {
        let mut codec = WireCodec::<ClientMsg, ClientMsg>::new();
        let mut buf = BytesMut::new();
        codec.encode(ClientMsg::ListAgents, &mut buf).unwrap();
        codec
            .encode(ClientMsg::CreateAgent { branch: "x".into() }, &mut buf)
            .unwrap();

        assert_eq!(codec.decode(&mut buf).unwrap(), Some(ClientMsg::ListAgents));
        assert_eq!(
            codec.decode(&mut buf).unwrap(),
            Some(ClientMsg::CreateAgent { branch: "x".into() })
        );
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn oversized_length_prefix_is_rejected() {
        let mut codec = WireCodec::<ClientMsg, ClientMsg>::new();
        let mut buf = BytesMut::new();
        buf.put_u32((MAX_FRAME_BYTES + 1) as u32);
        assert!(matches!(
            codec.decode(&mut buf),
            Err(ProtoError::FrameTooLarge(_))
        ));
    }

    #[test]
    fn version_check_accepts_match_and_rejects_mismatch() {
        assert!(check_version(PROTO_VERSION).is_ok());
        assert!(matches!(
            check_version(PROTO_VERSION.wrapping_add(1)),
            Err(ProtoError::VersionMismatch { .. })
        ));
    }

    proptest! {
        #[test]
        fn input_bytes_roundtrip(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
            roundtrip(ClientMsg::Input { id: AgentId::new(), bytes });
        }

        #[test]
        fn output_bytes_roundtrip(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
            roundtrip(DaemonMsg::Output { id: AgentId::new(), bytes });
        }
    }
}
