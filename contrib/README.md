# amux contrib — editor integration

## `amux.vim` — seamless `Ctrl+hjkl` between vim splits and amux panes

This is the amux counterpart to [`vim-tmux-navigator`](https://github.com/christoomey/vim-tmux-navigator).
With it, `Ctrl+h/j/k/l` moves between vim's splits and amux's tiled panes as one grid — the same
muscle memory you have under tmux.

### How it works

- amux normally intercepts `Ctrl+hjkl` to move between its own panes (and the sidebar).
- When vim starts in an amux pane, `amux.vim` runs `amux passthrough on`, which tells the daemon
  "a vim-like app is foreground here." amux then **passes `Ctrl+hjkl` through** to vim while that
  pane is focused.
- Inside vim, the plugin moves between vim's own splits. At vim's edge (no split that way), it
  runs `amux nav <dir>`, handing focus back to amux, which moves to the adjacent pane (or the
  sidebar). On `VimLeavePre` it runs `amux passthrough off`.

Nothing polls; the plugin is the single integration point. Layout/focus stays in the amux client;
the daemon only relays the announce/nav intents.

### Install

The plugin self-disables unless `$AMUX_TERMINAL_ID` is set (i.e. only inside an amux pane), so it's
safe to always load.

- **Symlink (simplest, stays in sync with the repo):**
  ```sh
  mkdir -p ~/.vim/after/plugin          # or ~/.config/nvim/after/plugin for neovim
  ln -s /path/to/amux/contrib/plugin/amux.vim ~/.vim/after/plugin/amux.vim
  ```
  It must be `after/plugin`, not `plugin`: vim sources `~/.vim/plugin/` **before** plugin-manager
  directories (they come later on `runtimepath`), so from there `vim-tmux-navigator` would override
  `amux.vim`'s `Ctrl+hjkl` mappings — and outside tmux its fallback is a bare `wincmd`, which moves
  inside vim but silently does nothing at the edge. `~/.vim/after/` is last on `runtimepath`, so
  `amux.vim`'s mappings win inside amux.

- **vim-plug (from a local checkout):** add to your vimrc **after** any `vim-tmux-navigator` line,
  then `:PlugInstall`:
  ```vim
  Plug '/path/to/amux', { 'rtp': 'contrib' }
  ```

If you also use `vim-tmux-navigator`, load `amux.vim` **after** it so its mappings win inside amux.
Outside amux, `amux.vim` is inert and `vim-tmux-navigator` behaves normally.
