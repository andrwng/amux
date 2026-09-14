//! Stamp the build with the current git commit and build time, exposed as env vars the binary
//! reads via `env!`. This makes `amux --version` report exactly which source built the running
//! binary — the one fact that repeatedly went unverifiable when a stale TUI kept running.

use std::process::Command;

fn main() {
    // Short git hash, plus a `-dirty` marker if the working tree has uncommitted changes.
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    let hash = if dirty { format!("{hash}-dirty") } else { hash };
    println!("cargo:rustc-env=AMUX_GIT_HASH={hash}");

    // Build time (UTC seconds since epoch is enough to compare "is this newer than that").
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=AMUX_BUILD_EPOCH={secs}");

    // Rebuild the stamp whenever HEAD moves or the index changes, so it never goes stale.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
