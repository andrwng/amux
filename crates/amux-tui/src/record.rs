//! A diagnostic tap: when `AMUX_RECORD` names a path, the client appends every byte the daemon
//! sends for a pane — the attach snapshot and each live `Output` chunk — to that file, framed with a
//! timestamp and the terminal id. Off unless the env var is set, and it only ever *observes* the
//! messages already flowing through the client, so it adds no connection and cannot resize a shared
//! PTY (which a second observer client would). Replay the capture with `replay_capture` (an ignored
//! test) to reconstruct exactly what a pane's parser saw and find where it diverged.
//!
//! Frame: `[u64 millis-since-start LE][36B terminal id, hyphenated uuid ASCII][u8 kind][u32 len LE][len payload]`.
//! `kind` is 0 for a snapshot (start a fresh parser) and 1 for live output (feed the running one).

use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::time::Instant;

use amux_core::agent::TerminalId;

/// Bytes of a terminal id in a frame: a hyphenated uuid is always 36 ASCII characters.
const TERM_ID_LEN: usize = 36;
/// Fixed frame header: 8 (millis) + 36 (terminal) + 1 (kind) + 4 (len).
const HEADER_LEN: usize = 8 + TERM_ID_LEN + 1 + 4;

/// A live output chunk vs. an attach snapshot — a replayer starts a new parser on `Snapshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Snapshot,
    Output,
}

impl Kind {
    fn tag(self) -> u8 {
        match self {
            Kind::Snapshot => 0,
            Kind::Output => 1,
        }
    }

    #[cfg(test)]
    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Kind::Snapshot),
            1 => Some(Kind::Output),
            _ => None,
        }
    }
}

/// One decoded frame. Decoding exists only for the replay tool, hence test-only.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub millis: u64,
    pub terminal: TerminalId,
    pub kind: Kind,
    pub payload: Vec<u8>,
}

/// Append-only capture file. `record` is best-effort: a diagnostic must never take down the client,
/// so every I/O error is swallowed. `notes` is a human-readable companion log (`<path>.notes`) that
/// records *sizes* — the size the daemon reports in each snapshot, and the size the client requests
/// on each Attach/Resize — which the binary frame stream does not carry, but which pins a
/// blank-from-shrink (a resize to a size smaller than the content's row destroys it; see
/// `Session::resize` and DESIGN.md §5.3).
pub struct Recorder {
    file: Mutex<File>,
    notes: Mutex<File>,
    start: Instant,
}

impl Recorder {
    /// A recorder if `AMUX_RECORD` is set to a writable path, else `None`.
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("AMUX_RECORD")?;
        let file = File::create(&path).ok()?;
        let notes = File::create(format!("{}.notes", path.to_string_lossy())).ok()?;
        Some(Self {
            file: Mutex::new(file),
            notes: Mutex::new(notes),
            start: Instant::now(),
        })
    }

    pub fn record(&self, terminal: TerminalId, kind: Kind, payload: &[u8]) {
        let millis = self.start.elapsed().as_millis() as u64;
        let frame = encode_frame(millis, terminal, kind, payload);
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(&frame);
            let _ = file.flush();
        }
    }

    /// Append a size event to the `.notes` sidecar. `what` is the event ("SNAP"/"ATTACH"/"RESIZE").
    pub fn note_size(&self, terminal: TerminalId, what: &str, cols: u16, rows: u16) {
        let millis = self.start.elapsed().as_millis() as u64;
        if let Ok(mut f) = self.notes.lock() {
            let short = &terminal.to_full_string()[..8];
            let _ = writeln!(f, "{millis} {short} {what} {cols}x{rows}");
            let _ = f.flush();
        }
    }
}

/// Encode one frame (see the module docs for the layout).
fn encode_frame(millis: u64, terminal: TerminalId, kind: Kind, payload: &[u8]) -> Vec<u8> {
    let id = terminal.to_full_string();
    debug_assert_eq!(id.len(), TERM_ID_LEN);
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&millis.to_le_bytes());
    out.extend_from_slice(id.as_bytes());
    out.push(kind.tag());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Decode every frame in a capture, stopping at the first truncated trailer (a capture cut off
/// mid-write is expected — the client was killed — so a short tail is not an error).
#[cfg(test)]
pub fn decode_frames(mut bytes: &[u8]) -> Vec<Frame> {
    let mut frames = Vec::new();
    while bytes.len() >= HEADER_LEN {
        let millis = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let Some(terminal) = std::str::from_utf8(&bytes[8..8 + TERM_ID_LEN])
            .ok()
            .and_then(TerminalId::parse)
        else {
            break;
        };
        let Some(kind) = Kind::from_tag(bytes[8 + TERM_ID_LEN]) else {
            break;
        };
        let len_at = 8 + TERM_ID_LEN + 1;
        let len = u32::from_le_bytes(bytes[len_at..len_at + 4].try_into().unwrap()) as usize;
        if bytes.len() < HEADER_LEN + len {
            break;
        }
        frames.push(Frame {
            millis,
            terminal,
            kind,
            payload: bytes[HEADER_LEN..HEADER_LEN + len].to_vec(),
        });
        bytes = &bytes[HEADER_LEN + len..];
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let t1 = TerminalId::new();
        let t2 = TerminalId::new();
        let mut buf = Vec::new();
        buf.extend_from_slice(&encode_frame(0, t1, Kind::Snapshot, b"\x1b[2Jhello"));
        buf.extend_from_slice(&encode_frame(42, t2, Kind::Output, b"world"));
        buf.extend_from_slice(&encode_frame(99, t1, Kind::Output, b"")); // empty payload is legal

        let frames = decode_frames(&buf);
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames[0],
            Frame {
                millis: 0,
                terminal: t1,
                kind: Kind::Snapshot,
                payload: b"\x1b[2Jhello".to_vec()
            }
        );
        assert_eq!(
            frames[1],
            Frame {
                millis: 42,
                terminal: t2,
                kind: Kind::Output,
                payload: b"world".to_vec()
            }
        );
        assert_eq!(frames[2].payload, Vec::<u8>::new());
    }

    /// Replay a capture (`AMUX_REPLAY=<file>`) into a fresh parser, exactly as the client would, and
    /// report the first frame after which the pane looks broken — blank, or "condensed" (rows the
    /// app clearly meant to separate collapsed together). This is the diagnostic that turns "it went
    /// blank after a while" into "frame N, these bytes". Ignored: it is a hand-run tool, not CI.
    ///
    /// `AMUX_REPLAY_TERM` picks the terminal (default: the one with the most frames);
    /// `AMUX_REPLAY_COLS`/`ROWS` set the parser size (default 332x57).
    #[test]
    #[ignore]
    fn replay_capture() {
        let path = std::env::var("AMUX_REPLAY").expect("set AMUX_REPLAY=<file>");
        let bytes = std::fs::read(&path).expect("read capture");
        let frames = decode_frames(&bytes);
        assert!(!frames.is_empty(), "no frames decoded");

        // Which terminal to replay.
        let term = std::env::var("AMUX_REPLAY_TERM")
            .ok()
            .and_then(|s| TerminalId::parse(&s));
        let term = term.unwrap_or_else(|| {
            let mut counts: std::collections::HashMap<TerminalId, usize> =
                std::collections::HashMap::new();
            for f in &frames {
                *counts.entry(f.terminal).or_default() += 1;
            }
            *counts
                .iter()
                .max_by_key(|(_, n)| **n)
                .map(|(t, _)| t)
                .unwrap()
        });
        let cols: u16 = std::env::var("AMUX_REPLAY_COLS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(332);
        let rows: u16 = std::env::var("AMUX_REPLAY_ROWS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(57);
        println!("replaying {term} at {cols}x{rows}");

        let nonblank = |p: &vt100::Parser| {
            p.screen()
                .rows(0, cols)
                .filter(|r| !r.trim().is_empty())
                .count()
        };
        let mut parser: Option<vt100::Parser> = None;
        let (mut restarts, mut fed) = (0usize, 0usize);
        let mut min_nonblank = usize::MAX;
        let mut min_at = 0usize;
        for (i, f) in frames.iter().filter(|f| f.terminal == term).enumerate() {
            match f.kind {
                Kind::Snapshot => {
                    parser = Some(vt100::Parser::new(rows, cols, 0));
                    restarts += 1;
                }
                Kind::Output => {}
            }
            if let Some(p) = parser.as_mut() {
                p.process(&f.payload);
                fed += 1;
                let nb = nonblank(p);
                if nb < min_nonblank {
                    min_nonblank = nb;
                    min_at = i;
                }
            }
        }
        let p = parser.expect("no snapshot in capture for this terminal");
        println!(
            "frames={} snapshots={} min_nonblank={} @frame{} final_nonblank={}",
            fed,
            restarts,
            min_nonblank,
            min_at,
            nonblank(&p)
        );
        println!("--- final screen ---");
        for (i, row) in p.screen().rows(0, cols).enumerate() {
            let t = row.trim_end();
            if !t.is_empty() {
                println!("{i:>3}: {:.120}", t);
            }
        }
    }

    /// End-to-end: a recorded snapshot + live chunks replay into the same screen a client parser
    /// would show. Proves the capture format and the replay reconstruction are faithful, so a
    /// capture from a real session can be trusted as evidence.
    #[test]
    fn a_recording_replays_to_the_same_screen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cap");
        std::env::set_var("AMUX_RECORD", &path);
        let rec = Recorder::from_env().expect("recorder");
        std::env::remove_var("AMUX_RECORD");

        let term = TerminalId::new();
        // A snapshot that sets a scroll region and paints, then live output inside the region.
        let snapshot = b"\x1b[2J\x1b[3;6r\x1b[1;1HHEAD\x1b[3;1Halpha".to_vec();
        rec.record(term, Kind::Snapshot, &snapshot);
        let chunks: [&[u8]; 3] = [b"\r\nbeta", b"\r\ngamma", b"\r\ndelta"];
        for c in chunks {
            rec.record(term, Kind::Output, c);
        }
        drop(rec);

        // Reference: what a client parser gets fed, in order.
        let mut reference = vt100::Parser::new(10, 20, 0);
        reference.process(&snapshot);
        for c in chunks {
            reference.process(c);
        }

        // Replay from the capture file.
        let bytes = std::fs::read(&path).unwrap();
        let frames = decode_frames(&bytes);
        assert_eq!(frames.len(), 4);
        let mut replayed = vt100::Parser::new(10, 20, 0);
        for f in &frames {
            assert_eq!(f.terminal, term);
            replayed.process(&f.payload);
        }
        assert_eq!(
            replayed.screen().contents(),
            reference.screen().contents(),
            "replay must reconstruct the client's screen byte-for-byte"
        );
    }

    /// A capture truncated mid-frame (the client was killed) decodes the whole frames and drops the
    /// partial tail rather than panicking.
    #[test]
    fn a_truncated_tail_is_dropped() {
        let t = TerminalId::new();
        let mut buf = encode_frame(0, t, Kind::Output, b"complete");
        let full = buf.len();
        buf.extend_from_slice(&encode_frame(1, t, Kind::Output, b"cut here"));
        buf.truncate(full + 20); // header + part of the payload of the second frame

        let frames = decode_frames(&buf);
        assert_eq!(frames.len(), 1, "only the complete frame survives");
        assert_eq!(frames[0].payload, b"complete");
    }
}
