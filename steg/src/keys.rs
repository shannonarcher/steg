//! Parsing of human-friendly key names ("Enter", "Escape", "CTRL|C") into
//! the key events that the mux understands.

use anyhow::{anyhow, bail};
use termwiz::input::{KeyCode, KeyEvent, Modifiers};

/// Parse a key description into a `KeyEvent`.
///
/// `key` is either a single character (`"c"`, `"/"`, `"?"`) or one of the
/// named keys below (case-insensitive, with a few aliases):
/// Enter, Escape, Tab, Backspace, Delete, Insert, Home, End, PageUp,
/// PageDown, Up, Down, Left, Right, Space, F1..F24.
///
/// `mods` is an optional `|`-separated list of modifiers: SHIFT, CTRL,
/// ALT (aka OPT/META), SUPER (aka CMD/WIN).
pub fn parse_key_event(key: &str, mods: Option<&str>) -> anyhow::Result<KeyEvent> {
    let modifiers = match mods {
        None => Modifiers::NONE,
        Some(m) => Modifiers::try_from(m.to_ascii_uppercase())
            .map_err(|e| anyhow!("invalid modifiers {m:?}: {e}"))?,
    };

    let key = parse_key_code(key)?.normalize_shift_to_upper_case(modifiers);

    Ok(KeyEvent { key, modifiers })
}

fn parse_key_code(key: &str) -> anyhow::Result<KeyCode> {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Ok(KeyCode::Char(c));
    }

    let lowered = key.to_ascii_lowercase();
    let code = match lowered.as_str() {
        "enter" | "return" | "cr" => KeyCode::Enter,
        "escape" | "esc" => KeyCode::Escape,
        "tab" => KeyCode::Tab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
        "up" | "uparrow" => KeyCode::UpArrow,
        "down" | "downarrow" => KeyCode::DownArrow,
        "left" | "leftarrow" => KeyCode::LeftArrow,
        "right" | "rightarrow" => KeyCode::RightArrow,
        "space" => KeyCode::Char(' '),
        _ => {
            if let Some(n) = lowered.strip_prefix('f') {
                if let Ok(n) = n.parse::<u8>() {
                    if (1..=24).contains(&n) {
                        return Ok(KeyCode::Function(n));
                    }
                }
            }
            bail!(
                "unknown key {key:?}; use a single character or one of \
                 Enter, Escape, Tab, Backspace, Delete, Insert, Home, End, \
                 PageUp, PageDown, Up, Down, Left, Right, Space, F1-F24"
            );
        }
    };
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_chars_and_names() {
        assert_eq!(parse_key_code("c").unwrap(), KeyCode::Char('c'));
        assert_eq!(parse_key_code("/").unwrap(), KeyCode::Char('/'));
        assert_eq!(parse_key_code("Enter").unwrap(), KeyCode::Enter);
        assert_eq!(parse_key_code("ESC").unwrap(), KeyCode::Escape);
        assert_eq!(parse_key_code("f12").unwrap(), KeyCode::Function(12));
        assert!(parse_key_code("nope").is_err());
    }

    #[test]
    fn modifiers() {
        let ev = parse_key_event("c", Some("ctrl")).unwrap();
        assert_eq!(ev.key, KeyCode::Char('c'));
        assert_eq!(ev.modifiers, Modifiers::CTRL);

        let ev = parse_key_event("Tab", Some("SHIFT")).unwrap();
        assert_eq!(ev.key, KeyCode::Tab);
        assert_eq!(ev.modifiers, Modifiers::SHIFT);

        // SHIFT + lowercase letter normalizes to the upper case char
        let ev = parse_key_event("a", Some("shift")).unwrap();
        assert_eq!(ev.key, KeyCode::Char('A'));

        assert!(parse_key_event("c", Some("hyperdrive")).is_err());
    }
}
