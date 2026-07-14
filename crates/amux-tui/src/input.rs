//! Encode crossterm key events as the byte sequences a PTY expects. Promoted from the Phase
//! 0.1 spike and extended with DECCKM handling. See `docs/DESIGN.md` §7.3, §11 gotcha 3.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Encode a key press as PTY bytes. `app_cursor` is DECCKM application-cursor mode: when the
/// child enables it, cursor keys must send `ESC O x` rather than `ESC [ x` (needed for vim,
/// Claude's TUI, etc.).
pub fn key_to_bytes(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let cursor = |final_byte: u8| -> Vec<u8> {
        let intro = if app_cursor { b'O' } else { b'[' };
        vec![0x1b, intro, final_byte]
    };
    let bytes = match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let up = c.to_ascii_uppercase();
                match up {
                    '@'..='_' => vec![(up as u8) - 0x40], // Ctrl-A = 1 … Ctrl-_ = 31
                    ' ' => vec![0],
                    _ => vec![c as u8],
                }
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor(b'A'),
        KeyCode::Down => cursor(b'B'),
        KeyCode::Right => cursor(b'C'),
        KeyCode::Left => cursor(b'D'),
        KeyCode::Home => cursor(b'H'),
        KeyCode::End => cursor(b'F'),
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        _ => return None,
    };
    Some(bytes)
}

/// Bracketed-paste markers — what a terminal wraps pasted text in once an app enables DECSET 2004.
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Encode a whole paste as one PTY write. Newlines are normalized to `\r` (what a real terminal
/// sends for a line break in pasted text). When `bracketed` — the child asked for bracketed paste
/// (Claude's TUI, vim, a shell with it on) — the payload is wrapped in the start/end markers so the
/// child inserts it as a single blob; without the wrapper each embedded newline reads as Enter and
/// a multi-line paste would submit line-by-line. See `docs/DESIGN.md` §7.3.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let body = text.replace("\r\n", "\r").replace('\n', "\r").into_bytes();
    if !bracketed {
        return body;
    }
    let mut out = Vec::with_capacity(PASTE_START.len() + body.len() + PASTE_END.len());
    out.extend_from_slice(PASTE_START);
    out.extend_from_slice(&body);
    out.extend_from_slice(PASTE_END);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn ctrl_c_is_etx() {
        let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(key_to_bytes(k, false), Some(vec![3]));
    }

    #[test]
    fn arrows_respect_decckm() {
        assert_eq!(
            key_to_bytes(press(KeyCode::Up), false),
            Some(vec![0x1b, b'[', b'A'])
        );
        assert_eq!(
            key_to_bytes(press(KeyCode::Up), true),
            Some(vec![0x1b, b'O', b'A'])
        );
    }

    #[test]
    fn plain_char_enter_and_backspace() {
        assert_eq!(
            key_to_bytes(press(KeyCode::Char('x')), false),
            Some(vec![b'x'])
        );
        assert_eq!(
            key_to_bytes(press(KeyCode::Enter), false),
            Some(vec![b'\r'])
        );
        assert_eq!(
            key_to_bytes(press(KeyCode::Backspace), false),
            Some(vec![0x7f])
        );
    }

    #[test]
    fn paste_wraps_only_when_child_wants_bracketed() {
        assert_eq!(encode_paste("hi", false), b"hi".to_vec());
        assert_eq!(encode_paste("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
    }

    #[test]
    fn paste_normalizes_newlines_to_cr() {
        assert_eq!(encode_paste("a\r\nb\nc", false), b"a\rb\rc".to_vec());
        assert_eq!(
            encode_paste("a\nb", true),
            b"\x1b[200~a\rb\x1b[201~".to_vec()
        );
    }
}
