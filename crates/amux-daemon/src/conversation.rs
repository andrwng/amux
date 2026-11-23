//! Each worktree's conversation log: the prompts and closing replies `amux hook` forwards, kept
//! by the daemon in one bounded file per worktree. Never in `state.json` (which is rewritten whole
//! on every save) and never logged: the files hold what the user typed, which can include a
//! secret they pasted, so they are mode `0600` in a directory of mode `0700`, and no line of them
//! reaches `tracing` at any level. See `docs/DESIGN.md` §5.4.
//!
//! The file is JSON lines, one [`Entry`] each, appended. Sequence numbers start at 1 for a
//! worktree, rise by one, and are never reused: past [`MAX_ENTRIES`] (plus [`TRIM_SLACK`], so a
//! full log is not rewritten on every turn) the file is rewritten without its oldest entries,
//! written beside and renamed over so a crash leaves one whole file, and the first entry left says
//! where the log now starts. In memory the daemon keeps only a [`Summary`] per worktree, rebuilt
//! from the file at start.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use amux_core::agent::AgentId;
use amux_core::conversation::{Speaker, Turn};

/// How many entries a trim keeps for a worktree.
pub const MAX_ENTRIES: usize = 2000;
/// How far past [`MAX_ENTRIES`] a log may grow before it is trimmed back. Rewriting on every turn
/// once a log is full would copy the whole file each time; this amortises it to one rewrite per
/// `TRIM_SLACK` turns, so a log holds between `MAX_ENTRIES` and `MAX_ENTRIES + TRIM_SLACK`.
pub const TRIM_SLACK: usize = 200;

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

/// Where a worktree's log starts, its newest entry, and how many entries the file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub first_seq: u64,
    pub last: EntryHead,
    entries: usize,
}

/// The per-worktree logs under one directory, plus the summaries of those loaded.
pub struct ConversationLog {
    dir: PathBuf,
    summaries: HashMap<AgentId, Summary>,
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
            summaries: HashMap::new(),
        })
    }

    fn path(&self, agent: AgentId) -> PathBuf {
        self.dir.join(format!("{}.jsonl", agent.to_full_string()))
    }

    fn tmp_path(&self, agent: AgentId) -> PathBuf {
        self.dir
            .join(format!("{}.jsonl.tmp", agent.to_full_string()))
    }

    /// Rebuild `agent`'s summary from its file, at daemon start. A torn last line (a crash during
    /// an append) is cut off so the next append starts on a line of its own, and a rewrite left
    /// half-done (its temporary file) is discarded: the file it was to replace is still whole.
    pub fn load(&mut self, agent: AgentId) -> Result<()> {
        std::fs::remove_file(self.tmp_path(agent)).ok();
        let path = self.path(agent);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
        };
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut whole_bytes: u64 = 0;
        let mut first_seq = None;
        let mut last = None;
        let mut entries = 0;
        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .with_context(|| format!("read {}", path.display()))?;
            if n == 0 {
                break;
            }
            if !line.ends_with('\n') {
                break; // torn: the append never finished
            }
            whole_bytes += n as u64;
            if let Ok(entry) = serde_json::from_str::<Entry>(&line) {
                first_seq.get_or_insert(entry.seq);
                last = Some(head(&entry));
                entries += 1;
            }
        }
        let len = std::fs::metadata(&path)?.len();
        if len != whole_bytes {
            OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_len(whole_bytes))
                .with_context(|| format!("cut the torn tail of {}", path.display()))?;
        }
        if let (Some(first_seq), Some(last)) = (first_seq, last) {
            self.summaries.insert(
                agent,
                Summary {
                    first_seq,
                    last,
                    entries,
                },
            );
        }
        Ok(())
    }

    pub fn summary(&self, agent: AgentId) -> Option<&Summary> {
        self.summaries.get(&agent)
    }

    /// Append `turn` to `agent`'s log as the next entry, stamped `at`, and trim the file if it has
    /// grown past its bound. Returns the entry's sequence number.
    pub fn append(&mut self, agent: AgentId, turn: Turn, at: DateTime<Utc>) -> Result<u64> {
        let seq = self.summaries.get(&agent).map_or(1, |s| s.last.seq + 1);
        let entry = Entry {
            seq,
            by: turn.by,
            at,
            text: turn.text,
            clipped: turn.clipped,
            session: turn.session,
        };
        let mut line = serde_json::to_vec(&entry).context("encode entry")?;
        line.push(b'\n');
        let path = self.path(agent);
        let mut file = open_private(&path, OpenOptions::new().create(true).append(true))?;
        let before = file.metadata()?.len();
        if let Err(e) = file.write_all(&line) {
            // Leave no partial line for the next append to run into.
            file.set_len(before).ok();
            return Err(e).with_context(|| format!("append to {}", path.display()));
        }
        let summary = self.summaries.entry(agent).or_insert(Summary {
            first_seq: seq,
            last: head(&entry),
            entries: 0,
        });
        summary.last = head(&entry);
        summary.entries += 1;
        if summary.entries > MAX_ENTRIES + TRIM_SLACK {
            self.trim(agent)?;
        }
        Ok(seq)
    }

    /// Rewrite `agent`'s log keeping only its newest [`MAX_ENTRIES`], beside the file and renamed
    /// over it.
    fn trim(&mut self, agent: AgentId) -> Result<()> {
        let path = self.path(agent);
        let tmp = self.tmp_path(agent);
        let all = read_entries(&path)?;
        let keep = &all[all.len().saturating_sub(MAX_ENTRIES)..];
        {
            let mut out = open_private(
                &tmp,
                OpenOptions::new().create(true).truncate(true).write(true),
            )?;
            for entry in keep {
                let mut line = serde_json::to_vec(entry).context("encode entry")?;
                line.push(b'\n');
                out.write_all(&line)
                    .with_context(|| format!("write {}", tmp.display()))?;
            }
            out.sync_all().ok();
        }
        std::fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
        if let (Some(first), Some(summary)) = (keep.first(), self.summaries.get_mut(&agent)) {
            summary.first_seq = first.seq;
            summary.entries = keep.len();
        }
        Ok(())
    }

    /// `agent`'s entries with a sequence number greater than `after`, oldest first: at most
    /// `limit` of them, and fewer if their text would pass `max_bytes` (at least one is always
    /// returned when there is one, so a reader paging by sequence number always moves forward).
    pub fn read_after(
        &self,
        agent: AgentId,
        after: u64,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Vec<Entry>> {
        if !self.summaries.contains_key(&agent) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut bytes = 0;
        for entry in read_entries(&self.path(agent))? {
            if entry.seq <= after {
                continue;
            }
            if out.len() >= limit {
                break;
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

    /// Remove `agent`'s log: its worktree is gone.
    pub fn remove(&mut self, agent: AgentId) {
        self.summaries.remove(&agent);
        std::fs::remove_file(self.path(agent)).ok();
        std::fs::remove_file(self.tmp_path(agent)).ok();
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

/// Every whole, decodable entry in the file at `path`. A torn or damaged line is skipped.
fn read_entries(path: &Path) -> Result<Vec<Entry>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("read {}", path.display()))?;
        if let Ok(entry) = serde_json::from_str::<Entry>(&line) {
            out.push(entry);
        }
    }
    Ok(out)
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
        let mut log = ConversationLog::open(tmp.path()).unwrap();
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
            let mut log = ConversationLog::open(tmp.path()).unwrap();
            for i in 0..3 {
                log.append(agent, turn(Speaker::User, "q"), at(i)).unwrap();
            }
        }
        let mut log = ConversationLog::open(tmp.path()).unwrap();
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
        let mut log = ConversationLog::open(tmp.path()).unwrap();
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
        let mut reloaded = ConversationLog::open(tmp.path()).unwrap();
        reloaded.load(agent).unwrap();
        assert_eq!(reloaded.summary(agent).unwrap().first_seq, s.first_seq);
        assert_eq!(reloaded.summary(agent).unwrap().last.seq, total + 1);
    }

    #[test]
    fn a_crash_between_write_and_rename_leaves_the_old_file_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = AgentId::new();
        let mut log = ConversationLog::open(tmp.path()).unwrap();
        for i in 0..5 {
            log.append(agent, turn(Speaker::User, "q"), at(i)).unwrap();
        }
        // A rewrite that got as far as its temporary file, then the daemon died.
        std::fs::write(log.tmp_path(agent), b"{\"seq\":99,\"half").unwrap();
        let mut reloaded = ConversationLog::open(tmp.path()).unwrap();
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
            let mut log = ConversationLog::open(tmp.path()).unwrap();
            log.append(agent, turn(Speaker::User, "q"), at(0)).unwrap();
            log.path(agent)
        };
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"seq\":2,\"by\":\"agent\",\"te").unwrap();
        drop(f);
        let mut log = ConversationLog::open(tmp.path()).unwrap();
        log.load(agent).unwrap();
        assert_eq!(log.summary(agent).unwrap().last.seq, 1);
        assert_eq!(
            log.append(agent, turn(Speaker::Agent, "a"), at(1)).unwrap(),
            2
        );
        let all = log.read_after(agent, 0, 200, usize::MAX).unwrap();
        assert_eq!(seqs(&all), [1, 2], "the new entry is a line of its own");
    }

    #[test]
    fn files_are_private() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("conversations");
        let mut log = ConversationLog::open(&dir).unwrap();
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

    #[test]
    fn a_trimmed_file_is_private_too() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = ConversationLog::open(tmp.path()).unwrap();
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
        let mut log = ConversationLog::open(tmp.path()).unwrap();
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
        let mut log = ConversationLog::open(tmp.path()).unwrap();
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
