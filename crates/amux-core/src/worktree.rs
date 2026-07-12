//! Git worktree management — the one capability lifted (in spirit) from grove. Each agent gets
//! an isolated worktree on its own branch. Worktrees live globally by default
//! (`~/.amux/worktrees/<repo>-<hash>/<branch>`) so they never clutter the project tree. All git
//! operations use libgit2 in-process (no `git` binary required). See `docs/DESIGN.md` §4.4.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use git2::{Repository, WorktreeAddOptions, WorktreePruneOptions};

/// Where a repo's worktrees are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeLocation {
    /// `~/.amux/worktrees/<repo>-<hash>/` — out of the project tree (default).
    Global,
    /// `<repo>/.amux/worktrees/` — beside the code (must be gitignored).
    InRepo,
}

/// Creates, removes, and lists git worktrees for a single repository.
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
        repo.worktree(&name, &path, Some(&opts))
            .with_context(|| format!("create worktree for {branch}"))?;
        Ok(path)
    }

    /// Prune the worktree metadata and delete its directory.
    pub fn remove(&self, branch: &str) -> Result<()> {
        let repo = Repository::open(&self.repo).context("open repository")?;
        let name = sanitize(branch);
        if let Ok(worktree) = repo.find_worktree(&name) {
            let mut opts = WorktreePruneOptions::new();
            opts.valid(true).working_tree(true);
            worktree.prune(Some(&mut opts)).context("prune worktree")?;
        }
        let path = self.base.join(&name);
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
    fn global_base_is_stable_and_repo_specific() {
        let a1 = global_base(Path::new("/home/u/proj")).unwrap();
        let a2 = global_base(Path::new("/home/u/proj")).unwrap();
        let b = global_base(Path::new("/home/u/other")).unwrap();
        assert_eq!(a1, a2, "same repo path → same base");
        assert_ne!(a1, b, "different repo path → different base");
        assert!(a1.to_string_lossy().contains(".amux/worktrees/proj-"));
    }
}
