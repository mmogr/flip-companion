use std::time::Duration;

use async_trait::async_trait;
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};
use tokio::sync::Mutex;

use crate::platform::input::InputInjector;

const KEY_DELAY: Duration = Duration::from_millis(20);

/// Build the shared key set used by both async and sync injectors.
fn all_key_codes() -> AttributeSet<KeyCode> {
    let mut keys = AttributeSet::<KeyCode>::new();
    let codes = [
        // Letters
        KeyCode::KEY_A, KeyCode::KEY_B, KeyCode::KEY_C, KeyCode::KEY_D,
        KeyCode::KEY_E, KeyCode::KEY_F, KeyCode::KEY_G, KeyCode::KEY_H,
        KeyCode::KEY_I, KeyCode::KEY_J, KeyCode::KEY_K, KeyCode::KEY_L,
        KeyCode::KEY_M, KeyCode::KEY_N, KeyCode::KEY_O, KeyCode::KEY_P,
        KeyCode::KEY_Q, KeyCode::KEY_R, KeyCode::KEY_S, KeyCode::KEY_T,
        KeyCode::KEY_U, KeyCode::KEY_V, KeyCode::KEY_W, KeyCode::KEY_X,
        KeyCode::KEY_Y, KeyCode::KEY_Z,
        // Digits
        KeyCode::KEY_0, KeyCode::KEY_1, KeyCode::KEY_2, KeyCode::KEY_3,
        KeyCode::KEY_4, KeyCode::KEY_5, KeyCode::KEY_6, KeyCode::KEY_7,
        KeyCode::KEY_8, KeyCode::KEY_9,
        // Special keys
        KeyCode::KEY_ENTER, KeyCode::KEY_BACKSPACE, KeyCode::KEY_SPACE,
        KeyCode::KEY_TAB, KeyCode::KEY_ESC, KeyCode::KEY_CAPSLOCK,
        // Modifiers
        KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTSHIFT,
        KeyCode::KEY_LEFTCTRL, KeyCode::KEY_RIGHTCTRL,
        KeyCode::KEY_LEFTALT, KeyCode::KEY_RIGHTALT,
        // Punctuation / symbols
        KeyCode::KEY_MINUS, KeyCode::KEY_EQUAL,
        KeyCode::KEY_LEFTBRACE, KeyCode::KEY_RIGHTBRACE,
        KeyCode::KEY_SEMICOLON, KeyCode::KEY_APOSTROPHE,
        KeyCode::KEY_GRAVE, KeyCode::KEY_BACKSLASH,
        KeyCode::KEY_COMMA, KeyCode::KEY_DOT, KeyCode::KEY_SLASH,
    ];
    for key in codes {
        keys.insert(key);
    }
    keys
}

/// Real input injector using Linux evdev uinput.
pub struct EvdevInputInjector {
    device: Mutex<VirtualDevice>,
}

impl EvdevInputInjector {
    pub fn try_new() -> anyhow::Result<Self> {
        let keys = all_key_codes();

        let device = VirtualDevice::builder()?
            .name("flip-companion-kb")
            .with_keys(&keys)?
            .build()?;

        Ok(Self {
            device: Mutex::new(device),
        })
    }
}

/// Create a SYN_REPORT event.
fn syn_report() -> InputEvent {
    InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0)
}

/// Emit a single keystroke (with optional shift wrapping) on the given device.
async fn emit_keystroke(dev: &mut VirtualDevice, code: KeyCode, shift: bool) -> anyhow::Result<()> {
    if shift {
        dev.emit(&[
            InputEvent::new(EventType::KEY.0, KeyCode::KEY_LEFTSHIFT.0, 1),
            syn_report(),
        ])?;
        tokio::time::sleep(KEY_DELAY).await;
    }

    dev.emit(&[InputEvent::new(EventType::KEY.0, code.0, 1), syn_report()])?;
    tokio::time::sleep(KEY_DELAY).await;

    dev.emit(&[InputEvent::new(EventType::KEY.0, code.0, 0), syn_report()])?;

    if shift {
        tokio::time::sleep(KEY_DELAY).await;
        dev.emit(&[
            InputEvent::new(EventType::KEY.0, KeyCode::KEY_LEFTSHIFT.0, 0),
            syn_report(),
        ])?;
    }

    Ok(())
}

// ── Synchronous input injector for DRM (Game Mode) ──────────────────────

/// Synchronous input injector for the DRM render loop (no tokio runtime).
pub struct SyncEvdevInputInjector {
    device: std::sync::Mutex<VirtualDevice>,
}

impl SyncEvdevInputInjector {
    pub fn try_new() -> anyhow::Result<Self> {
        let keys = all_key_codes();

        let device = VirtualDevice::builder()?
            .name("flip-companion-kb")
            .with_keys(&keys)?
            .build()?;

        eprintln!("[input] created uinput virtual keyboard (sync)");
        Ok(Self {
            device: std::sync::Mutex::new(device),
        })
    }

    /// Press and release a key by name (called from the DRM render loop).
    pub fn press_key_sync(&self, key: &str) {
        let (code, shift) = match map_key_name(key) {
            Some(v) => v,
            None => {
                eprintln!("[input] unknown key: {key:?}");
                return;
            }
        };

        let mut dev = self.device.lock().unwrap();
        if let Err(e) = emit_keystroke_sync(&mut dev, code, shift) {
            eprintln!("[input] error pressing {key:?}: {e}");
        }
    }
}

fn emit_keystroke_sync(
    dev: &mut VirtualDevice,
    code: KeyCode,
    shift: bool,
) -> anyhow::Result<()> {
    if shift {
        dev.emit(&[
            InputEvent::new(EventType::KEY.0, KeyCode::KEY_LEFTSHIFT.0, 1),
            syn_report(),
        ])?;
        std::thread::sleep(KEY_DELAY);
    }

    dev.emit(&[InputEvent::new(EventType::KEY.0, code.0, 1), syn_report()])?;
    std::thread::sleep(KEY_DELAY);

    dev.emit(&[InputEvent::new(EventType::KEY.0, code.0, 0), syn_report()])?;

    if shift {
        std::thread::sleep(KEY_DELAY);
        dev.emit(&[
            InputEvent::new(EventType::KEY.0, KeyCode::KEY_LEFTSHIFT.0, 0),
            syn_report(),
        ])?;
    }

    Ok(())
}

/// Map a character to (KeyCode, needs_shift).
fn map_char(ch: char) -> Option<(KeyCode, bool)> {
    match ch {
        // Lowercase letters
        'a' => Some((KeyCode::KEY_A, false)),
        'b' => Some((KeyCode::KEY_B, false)),
        'c' => Some((KeyCode::KEY_C, false)),
        'd' => Some((KeyCode::KEY_D, false)),
        'e' => Some((KeyCode::KEY_E, false)),
        'f' => Some((KeyCode::KEY_F, false)),
        'g' => Some((KeyCode::KEY_G, false)),
        'h' => Some((KeyCode::KEY_H, false)),
        'i' => Some((KeyCode::KEY_I, false)),
        'j' => Some((KeyCode::KEY_J, false)),
        'k' => Some((KeyCode::KEY_K, false)),
        'l' => Some((KeyCode::KEY_L, false)),
        'm' => Some((KeyCode::KEY_M, false)),
        'n' => Some((KeyCode::KEY_N, false)),
        'o' => Some((KeyCode::KEY_O, false)),
        'p' => Some((KeyCode::KEY_P, false)),
        'q' => Some((KeyCode::KEY_Q, false)),
        'r' => Some((KeyCode::KEY_R, false)),
        's' => Some((KeyCode::KEY_S, false)),
        't' => Some((KeyCode::KEY_T, false)),
        'u' => Some((KeyCode::KEY_U, false)),
        'v' => Some((KeyCode::KEY_V, false)),
        'w' => Some((KeyCode::KEY_W, false)),
        'x' => Some((KeyCode::KEY_X, false)),
        'y' => Some((KeyCode::KEY_Y, false)),
        'z' => Some((KeyCode::KEY_Z, false)),
        // Uppercase letters → shifted
        'A'..='Z' => map_char(ch.to_ascii_lowercase()).map(|(k, _)| (k, true)),
        // Digits
        '0' => Some((KeyCode::KEY_0, false)),
        '1' => Some((KeyCode::KEY_1, false)),
        '2' => Some((KeyCode::KEY_2, false)),
        '3' => Some((KeyCode::KEY_3, false)),
        '4' => Some((KeyCode::KEY_4, false)),
        '5' => Some((KeyCode::KEY_5, false)),
        '6' => Some((KeyCode::KEY_6, false)),
        '7' => Some((KeyCode::KEY_7, false)),
        '8' => Some((KeyCode::KEY_8, false)),
        '9' => Some((KeyCode::KEY_9, false)),
        // Whitespace
        ' ' => Some((KeyCode::KEY_SPACE, false)),
        '\n' => Some((KeyCode::KEY_ENTER, false)),
        '\t' => Some((KeyCode::KEY_TAB, false)),
        // Unshifted symbols
        '-' => Some((KeyCode::KEY_MINUS, false)),
        '=' => Some((KeyCode::KEY_EQUAL, false)),
        '[' => Some((KeyCode::KEY_LEFTBRACE, false)),
        ']' => Some((KeyCode::KEY_RIGHTBRACE, false)),
        '\\' => Some((KeyCode::KEY_BACKSLASH, false)),
        ';' => Some((KeyCode::KEY_SEMICOLON, false)),
        '\'' => Some((KeyCode::KEY_APOSTROPHE, false)),
        '`' => Some((KeyCode::KEY_GRAVE, false)),
        ',' => Some((KeyCode::KEY_COMMA, false)),
        '.' => Some((KeyCode::KEY_DOT, false)),
        '/' => Some((KeyCode::KEY_SLASH, false)),
        // Shifted symbols
        '!' => Some((KeyCode::KEY_1, true)),
        '@' => Some((KeyCode::KEY_2, true)),
        '#' => Some((KeyCode::KEY_3, true)),
        '$' => Some((KeyCode::KEY_4, true)),
        '%' => Some((KeyCode::KEY_5, true)),
        '^' => Some((KeyCode::KEY_6, true)),
        '&' => Some((KeyCode::KEY_7, true)),
        '*' => Some((KeyCode::KEY_8, true)),
        '(' => Some((KeyCode::KEY_9, true)),
        ')' => Some((KeyCode::KEY_0, true)),
        '_' => Some((KeyCode::KEY_MINUS, true)),
        '+' => Some((KeyCode::KEY_EQUAL, true)),
        '{' => Some((KeyCode::KEY_LEFTBRACE, true)),
        '}' => Some((KeyCode::KEY_RIGHTBRACE, true)),
        '|' => Some((KeyCode::KEY_BACKSLASH, true)),
        ':' => Some((KeyCode::KEY_SEMICOLON, true)),
        '"' => Some((KeyCode::KEY_APOSTROPHE, true)),
        '~' => Some((KeyCode::KEY_GRAVE, true)),
        '<' => Some((KeyCode::KEY_COMMA, true)),
        '>' => Some((KeyCode::KEY_DOT, true)),
        '?' => Some((KeyCode::KEY_SLASH, true)),
        _ => None,
    }
}

/// Map a key name (from the keyboard UI) to (KeyCode, needs_shift).
fn map_key_name(key: &str) -> Option<(KeyCode, bool)> {
    match key {
        "Return" => Some((KeyCode::KEY_ENTER, false)),
        "BackSpace" => Some((KeyCode::KEY_BACKSPACE, false)),
        "space" => Some((KeyCode::KEY_SPACE, false)),
        "Tab" => Some((KeyCode::KEY_TAB, false)),
        "Ctrl" => Some((KeyCode::KEY_LEFTCTRL, false)),
        "Alt" => Some((KeyCode::KEY_LEFTALT, false)),
        "Shift" => Some((KeyCode::KEY_LEFTSHIFT, false)),
        "Escape" => Some((KeyCode::KEY_ESC, false)),
        _ => {
            // Single character — delegate to map_char
            let mut chars = key.chars();
            if let Some(ch) = chars.next() {
                if chars.next().is_none() {
                    return map_char(ch);
                }
            }
            None
        }
    }
}

#[async_trait]
impl InputInjector for EvdevInputInjector {
    async fn press_key(&self, key: &str) -> anyhow::Result<()> {
        let (code, shift) =
            map_key_name(key).ok_or_else(|| anyhow::anyhow!("unknown key: {key:?}"))?;

        let mut dev = self.device.lock().await;
        emit_keystroke(&mut dev, code, shift).await
    }

    async fn type_text(&self, text: &str) -> anyhow::Result<()> {
        let mut dev = self.device.lock().await;

        for ch in text.chars() {
            let (code, shift) =
                map_char(ch).ok_or_else(|| anyhow::anyhow!("unmappable character: {ch:?}"))?;
            emit_keystroke(&mut dev, code, shift).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_lowercase_letters() {
        assert_eq!(map_key_name("a"), Some((KeyCode::KEY_A, false)));
        assert_eq!(map_key_name("z"), Some((KeyCode::KEY_Z, false)));
    }

    #[test]
    fn map_uppercase_letters_need_shift() {
        assert_eq!(map_key_name("A"), Some((KeyCode::KEY_A, true)));
        assert_eq!(map_key_name("Z"), Some((KeyCode::KEY_Z, true)));
    }

    #[test]
    fn map_digits() {
        assert_eq!(map_key_name("0"), Some((KeyCode::KEY_0, false)));
        assert_eq!(map_key_name("9"), Some((KeyCode::KEY_9, false)));
    }

    #[test]
    fn map_special_keys() {
        assert_eq!(map_key_name("Return"), Some((KeyCode::KEY_ENTER, false)));
        assert_eq!(
            map_key_name("BackSpace"),
            Some((KeyCode::KEY_BACKSPACE, false))
        );
        assert_eq!(map_key_name("space"), Some((KeyCode::KEY_SPACE, false)));
        assert_eq!(map_key_name("Tab"), Some((KeyCode::KEY_TAB, false)));
        assert_eq!(map_key_name("Ctrl"), Some((KeyCode::KEY_LEFTCTRL, false)));
        assert_eq!(map_key_name("Alt"), Some((KeyCode::KEY_LEFTALT, false)));
    }

    #[test]
    fn map_unknown_returns_none() {
        assert_eq!(map_key_name("FooBar"), None);
    }

    #[test]
    fn map_char_shifted_symbols() {
        assert_eq!(map_char('!'), Some((KeyCode::KEY_1, true)));
        assert_eq!(map_char('@'), Some((KeyCode::KEY_2, true)));
        assert_eq!(map_char('#'), Some((KeyCode::KEY_3, true)));
        assert_eq!(map_char('$'), Some((KeyCode::KEY_4, true)));
        assert_eq!(map_char('%'), Some((KeyCode::KEY_5, true)));
        assert_eq!(map_char('^'), Some((KeyCode::KEY_6, true)));
        assert_eq!(map_char('&'), Some((KeyCode::KEY_7, true)));
        assert_eq!(map_char('*'), Some((KeyCode::KEY_8, true)));
        assert_eq!(map_char('('), Some((KeyCode::KEY_9, true)));
        assert_eq!(map_char(')'), Some((KeyCode::KEY_0, true)));
        assert_eq!(map_char('?'), Some((KeyCode::KEY_SLASH, true)));
        assert_eq!(map_char('~'), Some((KeyCode::KEY_GRAVE, true)));
        assert_eq!(map_char('{'), Some((KeyCode::KEY_LEFTBRACE, true)));
        assert_eq!(map_char('}'), Some((KeyCode::KEY_RIGHTBRACE, true)));
        assert_eq!(map_char('|'), Some((KeyCode::KEY_BACKSLASH, true)));
        assert_eq!(map_char(':'), Some((KeyCode::KEY_SEMICOLON, true)));
        assert_eq!(map_char('"'), Some((KeyCode::KEY_APOSTROPHE, true)));
        assert_eq!(map_char('<'), Some((KeyCode::KEY_COMMA, true)));
        assert_eq!(map_char('>'), Some((KeyCode::KEY_DOT, true)));
        assert_eq!(map_char('_'), Some((KeyCode::KEY_MINUS, true)));
        assert_eq!(map_char('+'), Some((KeyCode::KEY_EQUAL, true)));
    }

    #[test]
    fn map_char_unshifted_symbols() {
        assert_eq!(map_char('-'), Some((KeyCode::KEY_MINUS, false)));
        assert_eq!(map_char('='), Some((KeyCode::KEY_EQUAL, false)));
        assert_eq!(map_char('['), Some((KeyCode::KEY_LEFTBRACE, false)));
        assert_eq!(map_char(']'), Some((KeyCode::KEY_RIGHTBRACE, false)));
        assert_eq!(map_char('\\'), Some((KeyCode::KEY_BACKSLASH, false)));
        assert_eq!(map_char(';'), Some((KeyCode::KEY_SEMICOLON, false)));
        assert_eq!(map_char('\''), Some((KeyCode::KEY_APOSTROPHE, false)));
        assert_eq!(map_char('`'), Some((KeyCode::KEY_GRAVE, false)));
        assert_eq!(map_char(','), Some((KeyCode::KEY_COMMA, false)));
        assert_eq!(map_char('.'), Some((KeyCode::KEY_DOT, false)));
        assert_eq!(map_char('/'), Some((KeyCode::KEY_SLASH, false)));
    }

    #[test]
    fn map_char_uppercase() {
        assert_eq!(map_char('A'), Some((KeyCode::KEY_A, true)));
        assert_eq!(map_char('Z'), Some((KeyCode::KEY_Z, true)));
    }

    #[test]
    fn map_char_unknown_returns_none() {
        assert_eq!(map_char('€'), None);
    }
}
