//! Detach the daemon from the launching terminal via the classic double-fork + `setsid`.
//!
//! CRITICAL (see `docs/DESIGN.md` §11 gotcha 2): this MUST run before the tokio runtime is
//! built. `fork()` only clones the calling thread, so forking after a multi-threaded runtime
//! exists leaves other threads' locks held forever in the child (silent deadlock on Linux,
//! outright abort on macOS). Call this, then call [`crate::run_blocking`].

use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result};

/// Fork twice, start a new session, detach from the controlling terminal, and redirect
/// stdio to `/dev/null`. Returns only in the final daemon (grand)child; the intermediate
/// parents `exit(0)`.
pub fn daemonize() -> Result<()> {
    use nix::unistd::{chdir, fork, setsid, ForkResult};

    // First fork: guarantee we are not a process-group leader, so `setsid` can succeed.
    if let ForkResult::Parent { .. } = unsafe { fork() }.context("first fork")? {
        std::process::exit(0);
    }

    setsid().context("setsid")?;

    // Second fork: as a non-session-leader we can never re-acquire a controlling terminal.
    if let ForkResult::Parent { .. } = unsafe { fork() }.context("second fork")? {
        std::process::exit(0);
    }

    chdir("/").context("chdir /")?;

    // Point stdio at /dev/null so the detached daemon never reads or writes the old terminal.
    if let Ok(devnull) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        let fd = devnull.as_raw_fd();
        unsafe {
            nix::libc::dup2(fd, 0);
            nix::libc::dup2(fd, 1);
            nix::libc::dup2(fd, 2);
        }
    }

    Ok(())
}
