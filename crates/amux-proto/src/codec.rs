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
    use crate::{AgentInfo, Layout, RepoInfo, Size};
    use amux_core::agent::{AgentId, AgentState, RepoId, TerminalId};
    use chrono::Utc;
    use proptest::prelude::*;
    use std::path::PathBuf;

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

    fn sample_layout() -> Layout {
        Layout::Split {
            axis: amux_core::nav::Axis::LeftRight,
            ratio: 0.5,
            first: Box::new(Layout::Leaf {
                terminal: Some(TerminalId::new()),
            }),
            second: Box::new(Layout::Leaf {
                terminal: Some(TerminalId::new()),
            }),
        }
    }

    fn sample_repo() -> RepoInfo {
        RepoInfo {
            id: RepoId::from_canonical_path(&PathBuf::from("/repos/amux")),
            name: "amux".into(),
            path: PathBuf::from("/repos/amux"),
        }
    }

    fn sample_info() -> AgentInfo {
        AgentInfo {
            id: AgentId::new(),
            repo: RepoId::from_canonical_path(&PathBuf::from("/repos/amux")),
            name: "auth".into(),
            branch: Some("feat/auth".into()),
            state: AgentState::Working,
            last_activity: Utc::now(),
            last_opened: Utc::now(),
            unread: true,
            primary_terminal: TerminalId::new(),
        }
    }

    #[test]
    fn client_messages_roundtrip() {
        let id = AgentId::new();
        let t = TerminalId::new();
        roundtrip(ClientMsg::Hello {
            proto_version: PROTO_VERSION,
        });
        roundtrip(ClientMsg::ListAgents);
        roundtrip(ClientMsg::AddRepo {
            path: "/repos/amux".into(),
        });
        roundtrip(ClientMsg::CreateAgent {
            repo: RepoId::from_canonical_path(&PathBuf::from("/repos/amux")),
            branch: "feat/x".into(),
            prompt: None,
        });
        // Dispatch-with-a-task: the prompt rides the same message.
        roundtrip(ClientMsg::CreateAgent {
            repo: RepoId::from_canonical_path(&PathBuf::from("/repos/amux")),
            branch: "feat/x".into(),
            prompt: Some("fix the flaky config_home test".into()),
        });
        roundtrip(ClientMsg::CreateAgentAt {
            path: "/repos/other".into(),
            branch: "feat/y".into(),
        });
        roundtrip(ClientMsg::CreateHeadAgent {
            repo: RepoId::from_canonical_path(&PathBuf::from("/repos/amux")),
        });
        roundtrip(ClientMsg::CreateHeadAgentAt {
            path: "/repos/other".into(),
        });
        roundtrip(ClientMsg::Scroll {
            terminal: t,
            lines: 3,
        });
        // The extremes are meaningful: to the oldest line, and back to live.
        roundtrip(ClientMsg::Scroll {
            terminal: t,
            lines: i32::MAX,
        });
        roundtrip(ClientMsg::Scroll {
            terminal: t,
            lines: i32::MIN,
        });
        roundtrip(ClientMsg::DeleteAgent { id, force: true });
        roundtrip(ClientMsg::ResumeAgent { id });
        roundtrip(ClientMsg::SpawnShell {
            terminal: t,
            like: TerminalId::new(),
        });
        roundtrip(ClientMsg::CloseTerminal { terminal: t });
        roundtrip(ClientMsg::DoctorRepo {
            repo: RepoId::from_canonical_path(&PathBuf::from("/repos/amux")),
        });
        roundtrip(ClientMsg::Focus { agent: Some(id) });
        roundtrip(ClientMsg::Focus { agent: None });
        roundtrip(ClientMsg::SetLayout {
            agent: id,
            layout: Some(sample_layout()),
        });
        roundtrip(ClientMsg::SetLayout {
            agent: id,
            layout: None,
        });
        roundtrip(ClientMsg::SetMinis(vec![id, AgentId::new()]));
        roundtrip(ClientMsg::SetActive(Some(id)));
        roundtrip(ClientMsg::SetActive(None));
        roundtrip(ClientMsg::SetPrevious(Some(id)));
        roundtrip(ClientMsg::SetPrevious(None));
        roundtrip(ClientMsg::Attach {
            terminal: t,
            size: Size {
                cols: 120,
                rows: 40,
            },
        });
        roundtrip(ClientMsg::Detach { terminal: t });
        roundtrip(ClientMsg::Input {
            terminal: t,
            bytes: vec![0x1b, b'[', b'A'],
        });
        roundtrip(ClientMsg::Resize {
            terminal: t,
            size: Size { cols: 80, rows: 24 },
        });
    }

    #[test]
    fn daemon_messages_roundtrip() {
        let id = AgentId::new();
        let t = TerminalId::new();
        roundtrip(DaemonMsg::Hello {
            proto_version: PROTO_VERSION,
        });
        roundtrip(DaemonMsg::Repos(vec![sample_repo()]));
        roundtrip(DaemonMsg::RepoAdded(sample_repo()));
        roundtrip(DaemonMsg::Agents(vec![sample_info(), sample_info()]));
        roundtrip(DaemonMsg::Layouts(vec![(id, sample_layout())]));
        roundtrip(DaemonMsg::Minis(vec![id]));
        roundtrip(DaemonMsg::Active(Some(id)));
        roundtrip(DaemonMsg::Active(None));
        roundtrip(DaemonMsg::Previous(Some(id)));
        roundtrip(DaemonMsg::Previous(None));
        roundtrip(DaemonMsg::AgentAdded(sample_info()));
        roundtrip(DaemonMsg::AgentRemoved { id });
        roundtrip(DaemonMsg::StateChanged {
            id,
            state: AgentState::Exited { code: Some(0) },
        });
        roundtrip(DaemonMsg::UnreadChanged { id, unread: true });
        roundtrip(DaemonMsg::OpenedChanged { id, at: Utc::now() });
        roundtrip(DaemonMsg::TerminalApp {
            terminal: t,
            passthrough: true,
        });
        roundtrip(DaemonMsg::Navigate {
            terminal: t,
            dir: amux_core::nav::Dir::Left,
        });
        roundtrip(DaemonMsg::DeleteNeedsConfirm {
            id,
            message: "2 uncommitted changes".into(),
        });
        roundtrip(DaemonMsg::OutputSnapshot {
            terminal: t,
            bytes: vec![1, 2, 3],
        });
        roundtrip(DaemonMsg::ScrollView {
            terminal: t,
            offset: 42,
            available: 4096,
            bytes: b"\x1b[H\x1b[Jline 0".to_vec(),
        });
        roundtrip(DaemonMsg::Output {
            terminal: t,
            bytes: b"hi\r\n".to_vec(),
        });
        roundtrip(DaemonMsg::TerminalExited {
            terminal: t,
            code: Some(0),
        });
        roundtrip(DaemonMsg::DoctorReport {
            repo: RepoId::from_canonical_path(&PathBuf::from("/repos/amux")),
            pruned: vec!["feat-a".into()],
            skipped: vec![("feat-b".into(), 3)],
        });
        roundtrip(DaemonMsg::Error {
            message: "boom".into(),
        });
    }

    #[test]
    fn agent_info_headless_branch_roundtrips() {
        // A branchless HEAD session carries `branch: None` over the wire.
        let mut info = sample_info();
        info.branch = None;
        roundtrip(DaemonMsg::AgentAdded(info));
    }

    #[test]
    fn partial_frame_yields_none_until_complete() {
        let mut codec = WireCodec::<ClientMsg, ClientMsg>::new();
        let mut full = BytesMut::new();
        let msg = ClientMsg::Input {
            terminal: TerminalId::new(),
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
        let repo = RepoId::from_canonical_path(&PathBuf::from("/repos/amux"));
        codec
            .encode(
                ClientMsg::CreateAgent {
                    repo,
                    branch: "x".into(),
                    prompt: None,
                },
                &mut buf,
            )
            .unwrap();

        assert_eq!(codec.decode(&mut buf).unwrap(), Some(ClientMsg::ListAgents));
        assert_eq!(
            codec.decode(&mut buf).unwrap(),
            Some(ClientMsg::CreateAgent {
                repo,
                branch: "x".into(),
                prompt: None
            })
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
            roundtrip(ClientMsg::Input { terminal: TerminalId::new(), bytes });
        }

        #[test]
        fn output_bytes_roundtrip(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
            roundtrip(DaemonMsg::Output { terminal: TerminalId::new(), bytes });
        }
    }
}
