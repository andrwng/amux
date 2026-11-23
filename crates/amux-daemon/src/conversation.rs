//! Each worktree's conversation log: the prompts and closing replies `amux hook` forwards, kept
//! by the daemon in one bounded file per worktree. Never in `state.json` (which is rewritten whole
//! on every save) and never logged: the files hold what the user typed, which can include a
//! secret they pasted, so they are mode `0600` in a directory of mode `0700`, and no line of them
//! reaches `tracing` at any level. See `docs/DESIGN.md` §5.4.
//!
//! The file is JSON lines, one [`Entry`] each, appended. Sequence numbers start at 1 for a
//! worktree, rise by one, and are never reused: once a log spans more than [`MAX_ENTRIES`] (plus
//! [`TRIM_SLACK`], so a full log is not rewritten on every turn) the file is rewritten without its
//! oldest entries, written beside and renamed over so a crash leaves one whole file, and the first
//! entry left says where the log now starts. A stored line is at most [`MAX_LINE_BYTES`], so the
//! file is bounded in bytes as well as entries.
//!
//! The calls here do blocking file I/O: the daemon makes them off its async workers. Each
//! worktree's log has its own lock, so a slow read of one never holds up another. In memory the
//! daemon keeps per worktree a [`Summary`] (read at start from the file's two ends alone) and,
//! once a read has needed it, an index of each entry's byte offset, so a page reads only its own
//! lines.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use amux_core::agent::AgentId;
use amux_core::conversation::{clip_text_for_json, Speaker, Turn, MAX_TURN_TEXT_BYTES};

/// How many entries a trim keeps for a worktree.
pub const MAX_ENTRIES: usize = 2000;
/// How far past [`MAX_ENTRIES`] a log may grow before it is trimmed back. Rewriting on every turn
/// once a log is full would copy the whole file each time; this amortises it to one rewrite per
/// `TRIM_SLACK` turns, so a log holds between `MAX_ENTRIES` and `MAX_ENTRIES + TRIM_SLACK`.
pub const TRIM_SLACK: usize = 200;
/// The most a stored session id takes, JSON-encoded. A real one is a few dozen bytes.
pub const MAX_SESSION_BYTES: usize = 256;
/// The most bytes one stored line can take: the text, clipped to [`MAX_TURN_TEXT_BYTES`] as JSON
/// encodes it, the session clipped to [`MAX_SESSION_BYTES`], and the other fields.
pub const MAX_LINE_BYTES: usize = 25 * 1024;
/// How much of a file's end start reads at a time when looking for its newest entry: more than a
/// line, so one read normally finds it.
const TAIL_WINDOW: u64 = 64 * 1024;

#[cfg(test)]
thread_local! {
    /// How many entries this thread has decoded, for tests of what a read touches.
    static DECODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn decode_entry(line: &[u8]) -> Option<Entry> {
    #[cfg(test)]
    DECODED.with(|d| d.set(d.get() + 1));
    serde_json::from_slice(line).ok()
}

/// One line of a worktree's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: u64,
    pub by: Speaker,
    /// The daemon's clock when the hook arrived.
    pub at: DateTime<Utc>,
    pub text: String,
    pub clipped: bool,
    pub session: Option<String>,
}

/// An entry without its text — what the daemon keeps in memory for the newest one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryHead {
    pub seq: u64,
    pub by: Speaker,
    pub at: DateTime<Utc>,
    pub session: Option<String>,
}

/// Where a worktree's log starts and its newest entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub first_seq: u64,
    pub last: EntryHead,
    /// Trim once the log spans more entries than this: `MAX_ENTRIES + TRIM_SLACK`, or later after
    /// a failed trim.
    trim_past: u64,
}

impl Summary {
    fn new(first_seq: u64, last: EntryHead) -> Self {
        Self {
            first_seq,
            last,
            trim_past: (MAX_ENTRIES + TRIM_SLACK) as u64,
        }
    }

    /// How many entries the log spans. Sequence numbers are consecutive, so this is how many it
    /// holds, less any line that was damaged.
    fn span(&self) -> u64 {
        self.last.seq + 1 - self.first_seq
    }
}

/// One worktree's log. `file` is held by whatever reads or writes the file; `summary` is apart so
/// a listing never waits for a read.
#[derive(Default)]
struct WorktreeLog {
    file: Mutex<FileState>,
    summary: Mutex<Option<Summary>>,
}

#[derive(Default)]
struct FileState {
    /// Each indexed entry's sequence number and the byte offset of its line, oldest first. Built
    /// by the first read or trim that needs it, extended by appends, rebuilt by a trim; bounded
    /// like the file, by the entries a log may span.
    index: Option<Vec<(u64, u64)>>,
}

/// The per-worktree logs under one directory.
pub struct ConversationLog {
    dir: PathBuf,
    worktrees: Mutex<HashMap<AgentId, Arc<WorktreeLog>>>,
}

impl ConversationLog {
    /// Use `dir` for the logs, creating it with mode `0700` (and tightening it if it exists).
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict {}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            worktrees: Mutex::new(HashMap::new()),
        })
    }

    fn path(&self, agent: AgentId) -> PathBuf {
        self.dir.join(format!("{}.jsonl", agent.to_full_string()))
    }

    fn tmp_path(&self, agent: AgentId) -> PathBuf {
        self.dir
            .join(format!("{}.jsonl.tmp", agent.to_full_string()))
    }

    /// `agent`'s log, made on first use. The map's lock is only held to look it up, never while
    /// a worktree's own lock is taken.
    fn worktree(&self, agent: AgentId) -> Arc<WorktreeLog> {
        Arc::clone(self.worktrees.lock().unwrap().entry(agent).or_default())
    }

    fn existing(&self, agent: AgentId) -> Option<Arc<WorktreeLog>> {
        self.worktrees.lock().unwrap().get(&agent).cloned()
    }

    /// Rebuild `agent`'s summary from its file, at daemon start, reading only the file's two ends.
    /// A torn last line (a crash during an append) is cut off so the next append starts on a line
    /// of its own, and a rewrite left half-done (its temporary file) is discarded: the file it was
    /// to replace is still whole.
    pub fn load(&self, agent: AgentId) -> Result<()> {
        std::fs::remove_file(self.tmp_path(agent)).ok();
        let worktree = self.worktree(agent);
        let mut file = worktree.file.lock().unwrap();
        let summary = load_ends(&self.path(agent))?;
        file.index = None;
        *worktree.summary.lock().unwrap() = summary;
        Ok(())
    }

    pub fn summary(&self, agent: AgentId) -> Option<Summary> {
        self.existing(agent)?.summary.lock().unwrap().clone()
    }

    /// Append `turn` to `agent`'s log as the next entry, stamped `at`, and trim the file if it has
    /// grown past its bound. Returns the entry's sequence number. The text is clipped again here,
    /// by what it takes once JSON-encoded, so a stored line stays within [`MAX_LINE_BYTES`].
    pub fn append(&self, agent: AgentId, turn: Turn, at: DateTime<Utc>) -> Result<u64> {
        let worktree = self.worktree(agent);
        let mut file = worktree.file.lock().unwrap();
        let (text, clipped_here) = clip_text_for_json(&turn.text, MAX_TURN_TEXT_BYTES);
        let session = turn
            .session
            .map(|s| clip_text_for_json(&s, MAX_SESSION_BYTES).0);
        let seq = worktree
            .summary
            .lock()
            .unwrap()
            .as_ref()
            .map_or(1, |s| s.last.seq + 1);
        let entry = Entry {
            seq,
            by: turn.by,
            at,
            text,
            clipped: turn.clipped || clipped_here,
            session,
        };
        let mut line = serde_json::to_vec(&entry).context("encode entry")?;
        line.push(b'\n');
        debug_assert!(
            line.len() <= MAX_LINE_BYTES,
            "a line of {} bytes",
            line.len()
        );
        let path = self.path(agent);
        let mut out = open_private(&path, OpenOptions::new().create(true).append(true))?;
        let before = out.metadata()?.len();
        if let Err(e) = out.write_all(&line) {
            // Leave no partial line for the next append to run into.
            out.set_len(before).ok();
            return Err(e).with_context(|| format!("append to {}", path.display()));
        }
        if let Some(index) = &mut file.index {
            index.push((seq, before));
        }
        let due = {
            let mut summary = worktree.summary.lock().unwrap();
            let summary = summary.get_or_insert_with(|| Summary::new(seq, head(&entry)));
            summary.last = head(&entry);
            summary.span() > summary.trim_past
        };
        if due {
            // The entry is written whatever becomes of the trim, so a failed trim is reported as
            // that, and not retried on every turn after it: only once another slack's worth of
            // entries has arrived.
            if let Err(e) = self.trim(agent, &worktree, &mut file) {
                tracing::warn!("could not trim the conversation log of {agent}: {e:#}");
                if let Some(summary) = worktree.summary.lock().unwrap().as_mut() {
                    summary.trim_past = summary.span() + TRIM_SLACK as u64;
                }
            }
        }
        Ok(seq)
    }

    /// Rewrite `agent`'s log keeping only its newest [`MAX_ENTRIES`] sequence numbers, beside the
    /// file and renamed over it. On failure the temporary file is removed: the log it was to
    /// replace is whole.
    fn trim(&self, agent: AgentId, worktree: &WorktreeLog, file: &mut FileState) -> Result<()> {
        let result = self.rewrite_newest(agent, worktree, file);
        if result.is_err() {
            std::fs::remove_file(self.tmp_path(agent)).ok();
        }
        result
    }

    fn rewrite_newest(
        &self,
        agent: AgentId,
        worktree: &WorktreeLog,
        file: &mut FileState,
    ) -> Result<()> {
        let path = self.path(agent);
        let tmp = self.tmp_path(agent);
        let Some(last_seq) = worktree
            .summary
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.last.seq)
        else {
            return Ok(());
        };
        let index = indexed(file, &path)?;
        let cutoff = last_seq.saturating_sub(MAX_ENTRIES as u64);
        let keep_from = index.partition_point(|&(seq, _)| seq <= cutoff);
        let Some(&(first_kept, start)) = index.get(keep_from) else {
            return Ok(());
        };
        // The kept lines are copied as they are, not decoded.
        {
            let mut src = File::open(&path).with_context(|| format!("open {}", path.display()))?;
            src.seek(SeekFrom::Start(start))?;
            let mut out = open_private(
                &tmp,
                OpenOptions::new().create(true).truncate(true).write(true),
            )?;
            std::io::copy(&mut src, &mut out)
                .with_context(|| format!("write {}", tmp.display()))?;
            out.sync_all().ok();
        }
        std::fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
        let rebased = index[keep_from..]
            .iter()
            .map(|&(seq, offset)| (seq, offset - start))
            .collect();
        file.index = Some(rebased);
        if let Some(summary) = worktree.summary.lock().unwrap().as_mut() {
            *summary = Summary::new(first_kept, summary.last.clone());
        }
        Ok(())
    }

    /// `agent`'s entries with a sequence number greater than `after`, oldest first: at most
    /// `limit` of them, and fewer if their text would pass `max_bytes` (at least one is always
    /// returned when there is one, so a reader paging by sequence number always moves forward).
    /// Reads from the first wanted entry's offset; only the entries returned are decoded.
    pub fn read_after(
        &self,
        agent: AgentId,
        after: u64,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Vec<Entry>> {
        let Some(worktree) = self.existing(agent) else {
            return Ok(Vec::new());
        };
        if worktree.summary.lock().unwrap().is_none() {
            return Ok(Vec::new());
        }
        let mut file = worktree.file.lock().unwrap();
        let path = self.path(agent);
        let index = indexed(&mut file, &path)?;
        let from = index.partition_point(|&(seq, _)| seq <= after);
        let Some(&(_, start)) = index.get(from) else {
            return Ok(Vec::new());
        };
        let mut reader =
            BufReader::new(File::open(&path).with_context(|| format!("open {}", path.display()))?);
        reader.seek(SeekFrom::Start(start))?;
        let mut out = Vec::new();
        let mut bytes = 0;
        let mut line = Vec::new();
        while out.len() < limit {
            line.clear();
            let n = reader
                .read_until(b'\n', &mut line)
                .with_context(|| format!("read {}", path.display()))?;
            if n == 0 {
                break;
            }
            let Some(entry) = decode_entry(&line) else {
                continue; // damaged: skipped
            };
            if entry.seq <= after {
                continue;
            }
            let size = entry_size(&entry);
            if !out.is_empty() && bytes + size > max_bytes {
                break;
            }
            bytes += size;
            out.push(entry);
        }
        Ok(out)
    }

    /// Remove `agent`'s log: its worktree is gone. Waits for a read or append in progress on it.
    pub fn remove(&self, agent: AgentId) {
        let worktree = self.worktrees.lock().unwrap().remove(&agent);
        let _file = worktree.as_ref().map(|w| w.file.lock().unwrap());
        std::fs::remove_file(self.path(agent)).ok();
        std::fs::remove_file(self.tmp_path(agent)).ok();
        if let Some(w) = &worktree {
            *w.summary.lock().unwrap() = None;
        }
    }

    /// Test seam: hold `agent`'s file lock — as a slow read would — on the calling thread, saying
    /// so on `held`, until `release` is sent to or dropped.
    #[cfg(test)]
    pub(crate) fn hold(
        &self,
        agent: AgentId,
        held: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) {
        let worktree = self.worktree(agent);
        let _file = worktree.file.lock().unwrap();
        let _ = held.send(());
        let _ = release.recv();
    }
}

/// The file's index, building it first if no read has yet.
fn indexed<'a>(file: &'a mut FileState, path: &Path) -> Result<&'a [(u64, u64)]> {
    if file.index.is_none() {
        file.index = Some(build_index(path)?);
    }
    Ok(file.index.as_deref().unwrap_or_default())
}

/// Each whole line's sequence number and offset, read from the line's start (`{"seq":N,` — how
/// every entry is written) without decoding the rest. A line without one is left out.
fn build_index(path: &Path) -> Result<Vec<(u64, u64)>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
    };
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    let mut offset = 0u64;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 || !line.ends_with(b"\n") {
            break;
        }
        if let Some(seq) = leading_seq(&line) {
            out.push((seq, offset));
        }
        offset += n as u64;
    }
    Ok(out)
}

/// The sequence number a line starts with, if it starts like an entry.
fn leading_seq(line: &[u8]) -> Option<u64> {
    let digits = line.strip_prefix(b"{\"seq\":")?;
    let end = digits.iter().position(|b| !b.is_ascii_digit())?;
    std::str::from_utf8(&digits[..end]).ok()?.parse().ok()
}

/// Where the log at `path` starts and its newest entry, from the file's two ends; `None` for a
/// missing or empty log. Cuts a torn last line first.
fn load_ends(path: &Path) -> Result<Option<Summary>> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
    };
    let len = file.metadata()?.len();
    let whole = end_of_last_line(&mut file, len)?;
    if whole != len {
        file.set_len(whole)
            .with_context(|| format!("cut the torn tail of {}", path.display()))?;
    }
    let Some(first) = first_entry(&mut file, whole)? else {
        return Ok(None);
    };
    let Some(last) = last_entry(&mut file, whole)? else {
        return Ok(None);
    };
    Ok(Some(Summary::new(first.seq, head(&last))))
}

/// The offset just past the file's last newline: where whole lines end.
fn end_of_last_line(file: &mut File, len: u64) -> Result<u64> {
    let mut end = len;
    let mut buf = vec![0u8; TAIL_WINDOW as usize];
    while end > 0 {
        let start = end.saturating_sub(TAIL_WINDOW);
        let chunk = &mut buf[..(end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(chunk)?;
        if let Some(i) = chunk.iter().rposition(|&b| b == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

/// The first decodable entry among the whole lines before `whole`.
fn first_entry(file: &mut File, whole: u64) -> Result<Option<Entry>> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(&mut *file).take(whole);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(None);
        }
        if let Some(entry) = decode_entry(&line) {
            return Ok(Some(entry));
        }
    }
}

/// The last decodable entry among the whole lines before `whole`, reading back from the end in a
/// window that grows only while the lines it holds are all damaged.
fn last_entry(file: &mut File, whole: u64) -> Result<Option<Entry>> {
    let mut window = TAIL_WINDOW;
    loop {
        let start = whole.saturating_sub(window);
        let mut buf = vec![0u8; (whole - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf)?;
        // `buf` ends with a newline; the piece before the window's first newline may be the end
        // of a longer line, unless the window starts the file.
        let body = buf.strip_suffix(b"\n").unwrap_or(&buf);
        let pieces: Vec<&[u8]> = body.split(|&b| b == b'\n').collect();
        for (i, piece) in pieces.iter().enumerate().rev() {
            if i == 0 && start > 0 {
                break;
            }
            if let Some(entry) = decode_entry(piece) {
                return Ok(Some(entry));
            }
        }
        if start == 0 {
            return Ok(None);
        }
        window = window.saturating_mul(2);
    }
}

/// What an entry costs in a reply, roughly: its text and session plus a fixed allowance for the
/// rest of its fields.
pub fn entry_size(entry: &Entry) -> usize {
    entry.text.len() + entry.session.as_ref().map_or(0, String::len) + 64
}

fn head(entry: &Entry) -> EntryHead {
    EntryHead {
        seq: entry.seq,
        by: entry.by,
        at: entry.at,
        session: entry.session.clone(),
    }
}

/// Open `path` with `options`, creating it (if it is created) with mode `0600`.
fn open_private(path: &Path, options: &mut OpenOptions) -> Result<File> {
    options
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_795_000_000 + secs, 0).unwrap()
    }

    fn turn(by: Speaker, text: &str) -> Turn {
        Turn::new(by, text, Some("sess-1".into()))
    }

    fn seqs(entries: &[Entry]) -> Vec<u64> {
        entries.iter().map(|e| e.seq).collect()
    }

    #[test]
    fn entries_are_numbered_from_one_and_read_back_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let log = ConversationLog::open(tmp.path()).unwrap();
        let agent = AgentId::new();
        assert_eq!(
            log.append(agent, turn(Speaker::User, "q"), at(0)).unwrap(),
            1
        );
        assert_eq!(
            log.append(agent, turn(Speaker::Agent, "a"), at(1)).unwrap(),
            2
        );
        let all = log.read_after(agent, 0, 200, usize::MAX).unwrap();
        assert_eq!(seqs(&all), [1, 2]);
        assert_eq!(all[0].text, "q");
        assert_eq!(all[1].by, Speaker::Agent);
        assert_eq!(all[1].at, at(1));
        let s = log.summary(agent).unwrap();
        assert_eq!((s.first_seq, s.last.seq), (1, 2));
        assert_eq!(
            seqs(&log.read_after(agent, 1, 200, usize::MAX).unwrap()),
            [2]
        );
    }

    #[test]
    fn a_restart_rebuilds_the_summary_and_numbering_goes_on() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = AgentId::new();
        {
            let log = ConversationLog::open(tmp.path()).unwrap();
            for i in 0..3 {
                log.append(agent, turn(Speaker::User, "q"), at(i)).unwrap();
            }
        }
        let log = ConversationLog::open(tmp.path()).unwrap();
        assert!(log.summary(agent).is_none(), "nothing until loaded");
        log.load(agent).unwrap();
        let s = log.summary(agent).unwrap().clone();
        assert_eq!((s.first_seq, s.last.seq, s.last.at), (1, 3, at(2)));
        assert_eq!(s.last.session.as_deref(), Some("sess-1"));
        assert_eq!(
            log.append(agent, turn(Speaker::Agent, "a"), at(9)).unwrap(),
            4
        );
    }

    #[test]
    fn the_log_is_bounded_and_numbering_survives_a_trim() {
        let tmp = tempfile::tempdir().unwrap();
        let log = ConversationLog::open(tmp.path()).unwrap();
        let agent = AgentId::new();
        let total = (MAX_ENTRIES + TRIM_SLACK + 1) as u64;
        for i in 0..total {
            log.append(agent, turn(Speaker::User, "q"), at(i as i64))
                .unwrap();
        }
        let all = log.read_after(agent, 0, usize::MAX, usize::MAX).unwrap();
        assert_eq!(all.len(), MAX_ENTRIES, "trimmed back to the bound");
        assert_eq!(all.first().unwrap().seq, total - MAX_ENTRIES as u64 + 1);
        assert_eq!(all.last().unwrap().seq, total);
        let s = log.summary(agent).unwrap().clone();
        assert_eq!(
            s.first_seq,
            total - MAX_ENTRIES as u64 + 1,
            "the log says where it starts"
        );
        assert_eq!(
            log.append(agent, turn(Speaker::Agent, "a"), at(0)).unwrap(),
            total + 1
        );
        // The same after a restart.
        let reloaded = ConversationLog::open(tmp.path()).unwrap();
        reloaded.load(agent).unwrap();
        assert_eq!(reloaded.summary(agent).unwrap().first_seq, s.first_seq);
        assert_eq!(reloaded.summary(agent).unwrap().last.seq, total + 1);
    }

    #[test]
    fn a_crash_between_write_and_rename_leaves_the_old_file_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = AgentId::new();
        let log = ConversationLog::open(tmp.path()).unwrap();
        for i in 0..5 {
            log.append(agent, turn(Speaker::User, "q"), at(i)).unwrap();
        }
        // A rewrite that got as far as its temporary file, then the daemon died.
        std::fs::write(log.tmp_path(agent), b"{\"seq\":99,\"half").unwrap();
        let reloaded = ConversationLog::open(tmp.path()).unwrap();
        reloaded.load(agent).unwrap();
        assert_eq!(
            seqs(&reloaded.read_after(agent, 0, 200, usize::MAX).unwrap()),
            [1, 2, 3, 4, 5]
        );
        assert!(
            !reloaded.tmp_path(agent).exists(),
            "the leftover is discarded"
        );
    }

    #[test]
    fn a_torn_last_line_is_ignored_and_cut_off() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = AgentId::new();
        let path = {
            let log = ConversationLog::open(tmp.path()).unwrap();
            log.append(agent, turn(Speaker::User, "q"), at(0)).unwrap();
            log.path(agent)
        };
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"seq\":2,\"by\":\"agent\",\"te").unwrap();
        drop(f);
        let log = ConversationLog::open(tmp.path()).unwrap();
        log.load(agent).unwrap();
        assert_eq!(log.summary(agent).unwrap().last.seq, 1);
        assert_eq!(
            log.append(agent, turn(Speaker::Agent, "a"), at(1)).unwrap(),
            2
        );
        let all = log.read_after(agent, 0, 200, usize::MAX).unwrap();
        assert_eq!(seqs(&all), [1, 2], "the new entry is a line of its own");
    }

    /// A crash mid-append can tear the last line inside a multi-byte character, leaving bytes that
    /// are not UTF-8. The log stays readable: the torn line is cut, the next entry is numbered
    /// after the last good one and reads back, and a damaged line in the middle is skipped.
    #[test]
    fn a_line_torn_inside_a_multi_byte_character_is_cut_and_the_log_goes_on() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = AgentId::new();
        let path = {
            let log = ConversationLog::open(tmp.path()).unwrap();
            log.append(agent, turn(Speaker::User, "q"), at(0)).unwrap();
            log.path(agent)
        };
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        // A whole line that is not UTF-8, then a line torn after two bytes of a four-byte 🦀.
        f.write_all(b"{\"seq\":2,\"text\":\"\xff\xfe\"}\n").unwrap();
        f.write_all(b"{\"seq\":3,\"by\":\"agent\",\"text\":\"crab \xf0\x9f")
            .unwrap();
        drop(f);
        let log = ConversationLog::open(tmp.path()).unwrap();
        log.load(agent).expect("the log loads");
        assert_eq!(log.summary(agent).unwrap().last.seq, 1);
        assert_eq!(
            log.append(agent, turn(Speaker::Agent, "🦀 after"), at(1))
                .unwrap(),
            2,
            "numbered after the last good entry"
        );
        let all = log.read_after(agent, 0, 200, usize::MAX).expect("readable");
        assert_eq!(seqs(&all), [1, 2]);
        assert_eq!(all[1].text, "🦀 after");
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.ends_with(b"\n"), "the file ends on a whole line");
    }

    #[test]
    fn files_are_private() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("conversations");
        let log = ConversationLog::open(&dir).unwrap();
        let agent = AgentId::new();
        log.append(agent, turn(Speaker::User, "q"), at(0)).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&log.path(agent)), 0o600);
        // A directory that already existed with looser permissions is tightened.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        ConversationLog::open(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
    }

    fn decoded() -> usize {
        DECODED.with(|d| d.get())
    }

    /// JSON escaping inflates a control character sixfold, so a turn of them clipped by raw size
    /// made a line six times the clip. The clip bounds the stored line, and with it the file.
    #[test]
    fn a_turn_of_control_characters_is_stored_within_the_line_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let log = ConversationLog::open(tmp.path()).unwrap();
        let agent = AgentId::new();
        // As a hook would send it: clipped by raw size, so not by what it encodes to.
        let turn = Turn::new(
            Speaker::User,
            &"\x01".repeat(64 * 1024),
            Some("s".repeat(4096)),
        );
        log.append(agent, turn, at(0)).unwrap();
        let line = std::fs::read(log.path(agent)).unwrap();
        assert!(
            line.len() <= MAX_LINE_BYTES,
            "a stored line of {} bytes",
            line.len()
        );
        let entry = &log.read_after(agent, 0, 1, usize::MAX).unwrap()[0];
        assert!(entry.clipped);
        assert!(entry.text.starts_with('\x01'));
    }

    /// Start reads what it needs to know where each log starts and ends, not every entry; a page
    /// decodes the page (once the log's index is built), not the whole file.
    #[test]
    fn start_and_a_page_decode_only_what_they_need() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = AgentId::new();
        {
            let log = ConversationLog::open(tmp.path()).unwrap();
            for i in 0..500 {
                log.append(agent, turn(Speaker::User, "q"), at(i)).unwrap();
            }
        }
        let log = ConversationLog::open(tmp.path()).unwrap();
        let before = decoded();
        log.load(agent).unwrap();
        assert!(
            decoded() - before <= 4,
            "start decoded {}",
            decoded() - before
        );
        let s = log.summary(agent).unwrap().clone();
        assert_eq!((s.first_seq, s.last.seq), (1, 500));
        // The first read may index the log; the next pages read only their own entries.
        assert_eq!(seqs(&log.read_after(agent, 0, 1, usize::MAX).unwrap()), [1]);
        let before = decoded();
        assert_eq!(
            seqs(&log.read_after(agent, 450, 3, usize::MAX).unwrap()),
            [451, 452, 453]
        );
        assert!(
            decoded() - before <= 4,
            "a page decoded {}",
            decoded() - before
        );
        // Appends after the index is built are found too.
        log.append(agent, turn(Speaker::Agent, "a"), at(0)).unwrap();
        assert_eq!(
            seqs(&log.read_after(agent, 499, 5, usize::MAX).unwrap()),
            [500, 501]
        );
    }

    /// A `Write` that appends into a shared buffer, for reading what was logged.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn lines(path: &Path) -> usize {
        std::fs::read(path)
            .unwrap()
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
    }

    /// A trim that fails does not fail the append (the entry was written), says it was the trim
    /// that failed, leaves no temporary file, and is not retried on the very next turn — only
    /// after another slack's worth of turns, when it can succeed.
    #[test]
    fn a_failed_trim_keeps_the_entry_cleans_up_and_backs_off() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let tmp = tempfile::tempdir().unwrap();
        let log = ConversationLog::open(tmp.path()).unwrap();
        let agent = AgentId::new();
        let full = MAX_ENTRIES + TRIM_SLACK;
        for i in 0..full {
            log.append(agent, turn(Speaker::User, "q"), at(i as i64))
                .unwrap();
        }
        // A leftover the trim cannot write over, so this trim fails.
        let leftover = log.tmp_path(agent);
        std::fs::write(&leftover, b"").unwrap();
        std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o400)).unwrap();
        let seq = log
            .append(agent, turn(Speaker::User, "q"), at(0))
            .expect("the entry was written; only the trim failed");
        assert_eq!(seq, full as u64 + 1);
        assert!(!leftover.exists(), "the temporary file is removed");
        let logged = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(
            logged.contains("trim"),
            "reported as a failed trim: {logged}"
        );
        let path = log.path(agent);
        assert_eq!(lines(&path), full + 1);
        // The next turn does not retry it...
        log.append(agent, turn(Speaker::User, "q"), at(0)).unwrap();
        assert_eq!(lines(&path), full + 2, "backed off");
        // ...a slack's worth later it does, and succeeds.
        for _ in 0..TRIM_SLACK {
            log.append(agent, turn(Speaker::User, "q"), at(0)).unwrap();
        }
        assert_eq!(lines(&path), MAX_ENTRIES, "trimmed once it could be");
    }

    #[test]
    fn a_trimmed_file_is_private_too() {
        let tmp = tempfile::tempdir().unwrap();
        let log = ConversationLog::open(tmp.path()).unwrap();
        let agent = AgentId::new();
        for i in 0..(MAX_ENTRIES + TRIM_SLACK + 1) {
            log.append(agent, turn(Speaker::User, "q"), at(i as i64))
                .unwrap();
        }
        let mode = std::fs::metadata(log.path(agent))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn removing_a_log_removes_its_file() {
        let tmp = tempfile::tempdir().unwrap();
        let log = ConversationLog::open(tmp.path()).unwrap();
        let agent = AgentId::new();
        log.append(agent, turn(Speaker::User, "q"), at(0)).unwrap();
        log.remove(agent);
        assert!(!log.path(agent).exists());
        assert!(log.summary(agent).is_none());
        // A worktree made later under another id starts its own log at 1.
        assert_eq!(
            log.append(AgentId::new(), turn(Speaker::User, "q"), at(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn a_read_stops_at_its_limit_and_its_byte_bound_but_always_moves_forward() {
        let tmp = tempfile::tempdir().unwrap();
        let log = ConversationLog::open(tmp.path()).unwrap();
        let agent = AgentId::new();
        let text = "z".repeat(1000);
        for i in 0..10 {
            log.append(agent, turn(Speaker::User, &text), at(i))
                .unwrap();
        }
        assert_eq!(
            seqs(&log.read_after(agent, 2, 3, usize::MAX).unwrap()),
            [3, 4, 5]
        );
        let one = entry_size(&log.read_after(agent, 0, 1, usize::MAX).unwrap()[0]);
        assert_eq!(log.read_after(agent, 0, 200, one * 4).unwrap().len(), 4);
        // A bound smaller than one entry still returns that entry.
        assert_eq!(seqs(&log.read_after(agent, 7, 200, 1).unwrap()), [8]);
        assert!(log
            .read_after(agent, 10, 200, usize::MAX)
            .unwrap()
            .is_empty());
        assert!(log
            .read_after(AgentId::new(), 0, 200, usize::MAX)
            .unwrap()
            .is_empty());
    }
}
