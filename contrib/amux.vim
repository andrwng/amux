" amux.vim — seamless Ctrl+h/j/k/l between vim splits and amux panes.
"
" The mirror of christoomey/vim-tmux-navigator, for amux instead of tmux. Inside an amux pane,
" amux passes Ctrl+hjkl through to vim (because this plugin announces vim is running here); this
" plugin then moves between vim's own splits, and hands navigation back to amux at the edge via
" `amux nav`.
"
" Install (vim-plug):   Plug 'you/amux', { 'rtp': 'contrib' }
"   or copy this file to ~/.vim/plugin/amux.vim (or ~/.config/nvim/plugin/).
" If you also use vim-tmux-navigator, load amux.vim AFTER it so these mappings win inside amux.
"
" It self-disables outside amux (keyed on $AMUX_TERMINAL_ID), so it's inert under plain tmux/shell.

if exists('g:loaded_amux_navigator') || !exists('$AMUX_TERMINAL_ID')
  finish
endif
let g:loaded_amux_navigator = 1

" Move within vim in `vim_dir` (h/j/k/l); if already at the edge, hand back to amux in `amux_dir`.
function! s:AmuxNavigate(vim_dir, amux_dir) abort
  let l:prev = winnr()
  silent! execute 'wincmd ' . a:vim_dir
  if winnr() == l:prev
    call system('amux nav ' . a:amux_dir)
    redraw!
  endif
endfunction

" Tell amux this pane is (and later is no longer) driven by vim, so it routes Ctrl+hjkl to us.
augroup amux_navigator
  autocmd!
  autocmd VimEnter,VimResume * call system('amux passthrough on')
  autocmd VimLeavePre        * call system('amux passthrough off')
augroup END

nnoremap <silent> <C-h> :call <SID>AmuxNavigate('h', 'h')<CR>
nnoremap <silent> <C-j> :call <SID>AmuxNavigate('j', 'j')<CR>
nnoremap <silent> <C-k> :call <SID>AmuxNavigate('k', 'k')<CR>
nnoremap <silent> <C-l> :call <SID>AmuxNavigate('l', 'l')<CR>
