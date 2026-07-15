//! Encode crossterm key events as the byte sequences a PTY expects. Promoted from the Phase
//! 0.1 spike and extended with DECCKM handling. See `docs/DESIGN.md` §7.3, §11 gotcha 3.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Encode a key press as PTY bytes. `app_cursor` is DECCKM application-cursor mode: when the
/// child enables it, cursor keys must send `ESC O x` rather than `ESC [ x` (needed for vim,
/// Claude's TUI, etc.).
pub fn key_to_bytes(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let cursor = |final_byte: u8| -> Vec<u8> {
        // xterm's function-key modifier param: 1 + Shift(1) + Alt(2) + Ctrl(4). A held modifier
        // forces the CSI form `ESC [ 1 ; <param> <final>` *regardless* of DECCKM — the SS3
        // (`ESC O`) form is only ever used for a bare cursor key. Without this the modifier is
        // dropped and the child sees a plain arrow, so e.g. Ctrl+Left can't move by a word.
        let m = key.modifiers;
        let param = 1
            + u8::from(m.contains(KeyModifiers::SHIFT))
            + 2 * u8::from(m.contains(KeyModifiers::ALT))
            + 4 * u8::from(m.contains(KeyModifiers::CONTROL));
        if param != 1 {
            return vec![0x1b, b'[', b'1', b';', b'0' + param, final_byte];
        }
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
    fn ctrl_arrows_send_modified_csi() {
        // Ctrl held → xterm modifier param 5 (1 + Ctrl(4)); e.g. Ctrl+Left = ESC [ 1 ; 5 D.
        // This is what a line editor (readline, Claude's TUI) reads as "move one word".
        let ctrl = |code| KeyEvent::new(code, KeyModifiers::CONTROL);
        assert_eq!(
            key_to_bytes(ctrl(KeyCode::Left), false),
            Some(b"\x1b[1;5D".to_vec())
        );
        assert_eq!(
            key_to_bytes(ctrl(KeyCode::Right), false),
            Some(b"\x1b[1;5C".to_vec())
        );
        assert_eq!(
            key_to_bytes(ctrl(KeyCode::Up), false),
            Some(b"\x1b[1;5A".to_vec())
        );
        assert_eq!(
            key_to_bytes(ctrl(KeyCode::Down), false),
            Some(b"\x1b[1;5B".to_vec())
        );
    }

    #[test]
    fn alt_arrows_send_modified_csi() {
        // Alt/Option held → modifier param 3 (1 + Alt(2)); the macOS "Option as Meta" path.
        let alt = |code| KeyEvent::new(code, KeyModifiers::ALT);
        assert_eq!(
            key_to_bytes(alt(KeyCode::Left), false),
            Some(b"\x1b[1;3D".to_vec())
        );
        assert_eq!(
            key_to_bytes(alt(KeyCode::Right), false),
            Some(b"\x1b[1;3C".to_vec())
        );
    }

    #[test]
    fn combined_modifiers_sum_into_the_param() {
        // param = 1 + Shift(1) + Alt(2) + Ctrl(4).
        assert_eq!(
            key_to_bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT), false),
            Some(b"\x1b[1;2A".to_vec()) // Shift = 2
        );
        assert_eq!(
            key_to_bytes(
                KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                false
            ),
            Some(b"\x1b[1;6C".to_vec()) // Ctrl+Shift = 6
        );
    }

    #[test]
    fn modified_arrows_force_csi_even_under_decckm() {
        // With a modifier held, xterm always uses the CSI form — never the SS3 (ESC O) form that
        // application-cursor mode selects for a *bare* arrow. Regression guard for the DECCKM path.
        assert_eq!(
            key_to_bytes(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL), true),
            Some(b"\x1b[1;5D".to_vec())
        );
    }

    #[test]
    fn modified_home_end_encode() {
        // Home/End share the cursor-key encoding: final bytes H and F.
        let ctrl = |code| KeyEvent::new(code, KeyModifiers::CONTROL);
        assert_eq!(
            key_to_bytes(ctrl(KeyCode::Home), false),
            Some(b"\x1b[1;5H".to_vec())
        );
        assert_eq!(
            key_to_bytes(ctrl(KeyCode::End), false),
            Some(b"\x1b[1;5F".to_vec())
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
