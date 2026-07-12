//! Git worktree management — the one capability lifted (in spirit) from grove. Each agent gets
//! an isolated worktree on its own branch. Worktrees live globally by default
//! (`~/.amux/worktrees/<repo>-<hash>/<branch>`) so they never clutter the project tree. All git
//! operations use libgit2 in-process (no `git` binary required). See `docs/DESIGN.md` §4.4.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use git2::{Repository, WorktreeAddOptions, WorktreePruneOptions};

/// A git-tracked worktree that no live agent holds — a candidate for `doctor` to prune.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    /// The worktree's git name (the sanitized branch, e.g. `feat-login`).
    pub name: String,
    pub path: PathBuf,
    /// Uncommitted changes in the worktree dir (0 if the dir is already gone).
    pub dirty: usize,
}

/// Where a repo's worktrees are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeLocation {
    /// `~/.amux/worktrees/<repo>-<hash>/` — out of the project tree (default).
    Global,
    /// `<repo>/.amux/worktrees/` — beside the code (must be gitignored).
    InRepo,
}

/// Creates, removes, and lists git worktrees for a single repository. Cheap to clone (two
/// paths) so the daemon can hold one per registered repo and hand out copies without a lock.
#[derive(Debug, Clone)]
pub struct WorktreeService {
    repo: PathBuf,
    base: PathBuf,
}

impl WorktreeService {
    /// Resolve the worktree base for `repo_path` according to `location`.
    pub fn new(repo_path: impl AsRef<Path>, location: WorktreeLocation) -> Result<Self> {
        let repo = canonical_repo(repo_path.as_ref())?;
        let base = match location {
            WorktreeLocation::Global => global_base(&repo)?,
            WorktreeLocation::InRepo => repo.join(".amux").join("worktrees"),
        };
        Ok(Self { repo, base })
    }

    /// Use an explicit base directory (for the daemon to override, or for tests).
    pub fn with_base(repo_path: impl AsRef<Path>, base: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            repo: canonical_repo(repo_path.as_ref())?,
            base: base.into(),
        })
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    /// The canonical path of the repository this service manages.
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// The path a branch's worktree would occupy (whether or not it exists).
    pub fn path_for(&self, branch: &str) -> PathBuf {
        self.base.join(sanitize(branch))
    }

    pub fn exists(&self, branch: &str) -> bool {
        self.path_for(branch).exists()
    }

    /// Create a worktree for `branch` (creating the branch off HEAD if it doesn't exist).
    /// Idempotent: returns the existing path if already present.
    pub fn create(&self, branch: &str) -> Result<PathBuf> {
        let repo = Repository::open(&self.repo).context("open repository")?;
        std::fs::create_dir_all(&self.base).context("create worktree base directory")?;

        let name = sanitize(branch);
        let path = self.base.join(&name);
        if path.exists() {
            return Ok(path);
        }

        let reference = match repo.find_reference(&format!("refs/heads/{branch}")) {
            Ok(reference) => reference,
            Err(_) => {
                let commit = repo
                    .head()
                    .context("get HEAD")?
                    .peel_to_commit()
                    .context("resolve HEAD commit")?;
                repo.branch(branch, &commit, false)
                    .context("create branch")?
                    .into_reference()
            }
        };

        let mut opts = WorktreeAddOptions::new();
        opts.reference(Some(&reference));
        if let Err(e) = repo.worktree(&name, &path, Some(&opts)) {
            // The branch is checked out elsewhere (the main repo, or a leftover worktree git
            // still tracks). Surface a plain-language message instead of the raw libgit2 error.
            if e.class() == git2::ErrorClass::Worktree {
                anyhow::bail!(
                    "branch '{branch}' is already checked out (in the main repo or another agent) \
                     — choose a different branch"
                );
            }
            return Err(e).with_context(|| format!("create worktree for {branch}"));
        }
        Ok(path)
    }

    /// Prune the worktree metadata and delete its directory.
    pub fn remove(&self, branch: &str) -> Result<()> {
        self.prune_worktree(&sanitize(branch))
    }

    /// Prune a worktree by its git name (already sanitized): prune metadata + remove its dir.
    pub fn prune_worktree(&self, name: &str) -> Result<()> {
        let repo = Repository::open(&self.repo).context("open repository")?;
        if let Ok(worktree) = repo.find_worktree(name) {
            let mut opts = WorktreePruneOptions::new();
            opts.valid(true).working_tree(true);
            worktree.prune(Some(&mut opts)).context("prune worktree")?;
        }
        let path = self.base.join(name);
        if path.exists() {
            std::fs::remove_dir_all(&path).context("remove worktree directory")?;
        }
        Ok(())
    }

    /// Names of all worktrees git knows about for this repo.
    pub fn list(&self) -> Result<Vec<String>> {
        let repo = Repository::open(&self.repo).context("open repository")?;
        let worktrees = repo.worktrees().context("list worktrees")?;
        Ok(worktrees
            .iter()
            .filter_map(|entry| entry.ok().flatten())
            .map(String::from)
            .collect())
    }

    /// Orphaned worktrees: git-tracked worktrees **under our base** that no live agent holds
    /// (`keep_branches` are the branches of live agents). These are what wedge a branch as
    /// "already checked out" after a crash or an out-of-band deletion. Each reports its dirty
    /// count so the caller can spare one with uncommitted work.
    pub fn orphans(&self, keep_branches: &[String]) -> Result<Vec<Orphan>> {
        let keep: Vec<String> = keep_branches.iter().map(|b| sanitize(b)).collect();
        let repo = Repository::open(&self.repo).context("open repository")?;
        // Compare canonicalized paths: git records a worktree's path resolved (e.g. `/private/…`
        // on macOS), which won't share a prefix with a non-canonical base otherwise.
        let base = std::fs::canonicalize(&self.base).unwrap_or_else(|_| self.base.clone());
        let mut out = Vec::new();
        for name in self.list()? {
            if keep.contains(&name) {
                continue;
            }
            let Ok(worktree) = repo.find_worktree(&name) else {
                continue;
            };
            let path = worktree.path().to_path_buf();
            let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            // Safety: only ever touch worktrees that live under amux's own base directory, so a
            // user's hand-made worktree elsewhere is never a prune candidate.
            if !canon.starts_with(&base) {
                continue;
            }
            let dirty = if path.exists() {
                dirty_at(&path).unwrap_or(0)
            } else {
                0
            };
            out.push(Orphan { name, path, dirty });
        }
        Ok(out)
    }

    /// Count uncommitted changes (staged, unstaged, and untracked) in a branch's worktree.
    pub fn dirty_count(&self, branch: &str) -> Result<usize> {
        dirty_at(&self.path_for(branch))
    }

    /// Symlink shared files (e.g. `node_modules`, `.env`) from the repo into a worktree.
    pub fn link_shared(&self, branch: &str, files: &[String]) -> Result<()> {
        let worktree = self.path_for(branch);
        for file in files {
            let source = self.repo.join(file);
            let target = worktree.join(file);
            if !source.exists() || target.exists() {
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&source, &target)
                .with_context(|| format!("symlink shared file {file}"))?;
        }
        Ok(())
    }
}

/// Find the working directory of the git repository containing `from` (walking up).
pub fn discover_repo(from: &Path) -> Result<PathBuf> {
    let repo = Repository::discover(from)
        .with_context(|| format!("not inside a git repository: {}", from.display()))?;
    repo.workdir()
        .map(Path::to_path_buf)
        .context("bare repository has no working directory")
}

fn canonical_repo(repo_path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(repo_path)
        .with_context(|| format!("repository path does not exist: {}", repo_path.display()))
}

fn sanitize(branch: &str) -> String {
    branch.replace('/', "-")
}

/// Count uncommitted changes (staged, unstaged, untracked) in the worktree at `path`, excluding
/// amux's own injected artifacts (the hook settings we write) so they never trip the delete
/// guard — only real user work counts as dirty.
fn dirty_at(path: &Path) -> Result<usize> {
    let repo = Repository::open(path).context("open worktree")?;
    let mut opts = git2::StatusOptions::new();
    // Recurse untracked dirs so a fully-untracked `.claude/` is reported as the file path (not
    // collapsed to the directory), letting us exclude exactly our injected settings.
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    let statuses = repo.statuses(Some(&mut opts)).context("git status")?;
    let count = statuses
        .iter()
        .filter(|e| e.path().ok() != Some(AMUX_SETTINGS_PATH))
        .count();
    Ok(count)
}

/// The hook settings file amux writes into each worktree (relative to its root).
const AMUX_SETTINGS_PATH: &str = ".claude/settings.local.json";

/// `~/.amux/worktrees/<basename>-<hash>` — stable per repo, disambiguated by path hash.
fn global_base(repo: &Path) -> Result<PathBuf> {
    let home = directories::BaseDirs::new()
        .context("cannot determine home directory")?
        .home_dir()
        .to_path_buf();
    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let key = format!("{name}-{}", fnv1a(&repo.to_string_lossy()));
    Ok(home.join(".amux").join("worktrees").join(key))
}

/// FNV-1a 32-bit — a tiny, version-stable hash (unlike `DefaultHasher`) for directory naming.
fn fnv1a(s: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in s.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a temp repo with one commit, entirely via libgit2 (no `git` binary needed).
    fn init_repo(dir: &Path) {
        let repo = Repository::init(dir).unwrap();
        {
            let mut config = repo.config().unwrap();
            config.set_str("user.name", "amux test").unwrap();
            config.set_str("user.email", "test@amux.local").unwrap();
        }
        std::fs::write(dir.join("README.md"), "hello").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = repo.signature().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }

    #[test]
    fn create_list_and_remove_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repo");
        std::fs::create_dir(&repo_path).unwrap();
        init_repo(&repo_path);

        let svc = WorktreeService::with_base(&repo_path, tmp.path().join("wt")).unwrap();

        let path = svc.create("feature/login").unwrap();
        assert!(path.exists(), "worktree dir should exist");
        assert!(
            path.ends_with("feature-login"),
            "slashes sanitized to dashes"
        );
        assert!(svc.exists("feature/login"));
        assert!(svc.list().unwrap().iter().any(|n| n == "feature-login"));

        // Idempotent.
        assert_eq!(svc.create("feature/login").unwrap(), path);

        svc.remove("feature/login").unwrap();
        assert!(!path.exists(), "worktree dir should be gone");
        assert!(!svc.exists("feature/login"));
    }

    #[test]
    fn checked_out_branch_yields_a_friendly_error() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repo");
        std::fs::create_dir(&repo_path).unwrap();
        init_repo(&repo_path);

        // The repo's own HEAD branch is checked out in the main worktree, so making a linked
        // worktree for it must fail — but with our message, not the raw libgit2 one.
        let repo = Repository::open(&repo_path).unwrap();
        let head = repo.head().unwrap();
        let branch = head.shorthand().unwrap().to_string();

        let svc = WorktreeService::with_base(&repo_path, tmp.path().join("wt")).unwrap();
        let err = svc.create(&branch).unwrap_err().to_string();
        assert!(
            err.contains("already checked out") && err.contains("different branch"),
            "expected a friendly checkout error, got: {err}"
        );
    }

    #[test]
    fn injected_hook_settings_do_not_count_as_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repo");
        std::fs::create_dir(&repo_path).unwrap();
        init_repo(&repo_path);
        let svc = WorktreeService::with_base(&repo_path, tmp.path().join("wt")).unwrap();
        let wt = svc.create("feat/x").unwrap();

        // amux's own hook settings must not read as user changes.
        std::fs::create_dir_all(wt.join(".claude")).unwrap();
        std::fs::write(wt.join(".claude/settings.local.json"), "{}").unwrap();
        assert_eq!(svc.dirty_count("feat/x").unwrap(), 0);

        // A real user file does count.
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
        assert_eq!(svc.dirty_count("feat/x").unwrap(), 1);
    }

    #[test]
    fn doctor_prunes_orphans_but_keeps_live_and_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repo");
        std::fs::create_dir(&repo_path).unwrap();
        init_repo(&repo_path);
        let svc = WorktreeService::with_base(&repo_path, tmp.path().join("wt")).unwrap();

        // Three worktrees: one stays live, one is a clean orphan, one is a dirty orphan.
        svc.create("live").unwrap();
        svc.create("orphan").unwrap();
        let dirty = svc.create("dirty").unwrap();
        std::fs::write(dirty.join("scratch.txt"), "wip").unwrap();

        // Only "live" is held by an agent; the other two are orphans.
        let orphans = svc.orphans(&["live".to_string()]).unwrap();
        let names: Vec<&str> = orphans.iter().map(|o| o.name.as_str()).collect();
        assert!(names.contains(&"orphan") && names.contains(&"dirty"));
        assert!(
            !names.contains(&"live"),
            "a live worktree is never an orphan"
        );
        assert_eq!(
            orphans.iter().find(|o| o.name == "dirty").unwrap().dirty,
            1,
            "the dirty orphan reports its uncommitted change"
        );

        // Prune the clean orphan; the branch (and dir) are gone, live + dirty survive.
        svc.prune_worktree("orphan").unwrap();
        assert!(!svc.exists("orphan"));
        assert!(svc.exists("live"));
        assert!(svc.exists("dirty"));
    }

    #[test]
    fn global_base_is_stable_and_repo_specific() {
        let a1 = global_base(Path::new("/home/u/proj")).unwrap();
        let a2 = global_base(Path::new("/home/u/proj")).unwrap();
        let b = global_base(Path::new("/home/u/other")).unwrap();
        assert_eq!(a1, a2, "same repo path → same base");
        assert_ne!(a1, b, "different repo path → different base");
        assert!(a1.to_string_lossy().contains(".amux/worktrees/proj-"));
    }
}
