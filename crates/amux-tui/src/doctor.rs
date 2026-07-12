//! `amux doctor`: a headless client that asks the daemon to prune orphaned worktrees in the
//! current repo, prints the result, and exits. Useful when a branch is wedged ("already checked
//! out") after a crash and you want to reclaim it without opening the TUI.

use anyhow::{bail, Result};
use futures::{SinkExt, StreamExt};

use amux_core::agent::RepoId;
use amux_proto::{ClientMsg, DaemonMsg};

pub async fn run() -> Result<()> {
    let (mut framed, repo) = crate::client::connect().await?;
    // The daemon derives a repo's id from its canonical path; match that so our report lands.
    let canonical = std::fs::canonicalize(&repo).unwrap_or_else(|_| repo.clone());
    let repo_id = RepoId::from_canonical_path(&canonical);

    // Ensure the repo is registered (no-op if it already is), then request the prune.
    framed.send(ClientMsg::AddRepo { path: repo }).await?;
    framed.send(ClientMsg::DoctorRepo { repo: repo_id }).await?;

    while let Some(frame) = framed.next().await {
        match frame? {
            DaemonMsg::DoctorReport {
                repo,
                pruned,
                skipped,
            } if repo == repo_id => {
                if pruned.is_empty() && skipped.is_empty() {
                    println!("No orphaned worktrees — nothing to prune.");
                } else {
                    for name in &pruned {
                        println!("pruned   {name}");
                    }
                    for (name, dirty) in &skipped {
                        let plural = if *dirty == 1 { "" } else { "s" };
                        println!("skipped  {name}  ({dirty} uncommitted change{plural})");
                    }
                    println!("\n{} pruned, {} skipped.", pruned.len(), skipped.len());
                }
                return Ok(());
            }
            DaemonMsg::Error { message } => bail!("daemon error: {message}"),
            _ => {}
        }
    }
    bail!("daemon closed the connection before reporting");
}
