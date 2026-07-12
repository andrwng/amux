# amux

A terminal UI for multiplexing AI coding agents in isolated git worktrees — a persistent
sidebar of agents, one focused main window, and several floating live mini-terminals you can
answer without losing your place.

- **Design & architecture:** [`docs/DESIGN.md`](docs/DESIGN.md)
- **Phase 0 build plan:** [`docs/PHASE-0.md`](docs/PHASE-0.md)

Status: **pre-alpha**, Phase 0 (the spine). Unix only (macOS + Linux/Ubuntu).

## Try the Phase 0.1 spike

Proves the PTY↔render loop — a live `$SHELL` inside a ratatui frame:

```sh
cargo run --example spike   # quit with Ctrl-Q
```

On Ubuntu you need a C toolchain first: `sudo apt-get install -y build-essential pkg-config`.
