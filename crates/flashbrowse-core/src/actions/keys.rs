//! Key name -> CDP key event mapping.
//!
//! `Input.dispatchKeyEvent` wants the key *value* in `key` and the DOM
//! `KeyboardEvent.code` in `code` (e.g. "a" -> "KeyA"). Sending the key
//! name in `code` breaks layout-dependent keys.

/// A resolved key: value, DOM code and (optional) virtual key code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySpec {
    /// `KeyboardEvent.key`
    pub key: String,
    /// `KeyboardEvent.code`
    pub code: String,
    /// `windowsVirtualKeyCode` (Windows/Linux)
    pub vk: Option<i64>,
}

/// Resolve a user-facing key name ("Enter", "a", "ArrowDown", ...).
pub fn resolve(key: &str) -> Option<KeySpec> {
    let k = key.trim();
    if k.is_empty() {
        return None;
    }
    let spec = match k {
        "Enter" | "Return" => KeySpec {
            key: "Enter".into(),
            code: "Enter".into(),
            vk: Some(13),
        },
        "Tab" => KeySpec {
            key: "Tab".into(),
            code: "Tab".into(),
            vk: Some(9),
        },
        "Escape" | "Esc" => KeySpec {
            key: "Escape".into(),
            code: "Escape".into(),
            vk: Some(27),
        },
        "Backspace" => KeySpec {
            key: "Backspace".into(),
            code: "Backspace".into(),
            vk: Some(8),
        },
        "Delete" | "Del" => KeySpec {
            key: "Delete".into(),
            code: "Delete".into(),
            vk: Some(46),
        },
        "Space" | " " => KeySpec {
            key: " ".into(),
            code: "Space".into(),
            vk: Some(32),
        },
        "ArrowUp" | "Up" => KeySpec {
            key: "ArrowUp".into(),
            code: "ArrowUp".into(),
            vk: Some(38),
        },
        "ArrowDown" | "Down" => KeySpec {
            key: "ArrowDown".into(),
            code: "ArrowDown".into(),
            vk: Some(40),
        },
        "ArrowLeft" | "Left" => KeySpec {
            key: "ArrowLeft".into(),
            code: "ArrowLeft".into(),
            vk: Some(37),
        },
        "ArrowRight" | "Right" => KeySpec {
            key: "ArrowRight".into(),
            code: "ArrowRight".into(),
            vk: Some(39),
        },
        "PageUp" => KeySpec {
            key: "PageUp".into(),
            code: "PageUp".into(),
            vk: Some(33),
        },
        "PageDown" => KeySpec {
            key: "PageDown".into(),
            code: "PageDown".into(),
            vk: Some(34),
        },
        "Home" => KeySpec {
            key: "Home".into(),
            code: "Home".into(),
            vk: Some(36),
        },
        "End" => KeySpec {
            key: "End".into(),
            code: "End".into(),
            vk: Some(35),
        },
        "Shift" => KeySpec {
            key: "Shift".into(),
            code: "ShiftLeft".into(),
            vk: Some(16),
        },
        "Control" | "Ctrl" => KeySpec {
            key: "Control".into(),
            code: "ControlLeft".into(),
            vk: Some(17),
        },
        "Alt" => KeySpec {
            key: "Alt".into(),
            code: "AltLeft".into(),
            vk: Some(18),
        },
        "Meta" | "Command" | "Cmd" => KeySpec {
            key: "Meta".into(),
            code: "MetaLeft".into(),
            vk: Some(91),
        },
        _ => {
            // Single printable character -> letter/digit layout codes.
            let mut chars = k.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            if c.is_ascii_alphabetic() {
                let upper = c.to_ascii_uppercase();
                KeySpec {
                    key: c.to_string(),
                    code: format!("Key{upper}"),
                    vk: Some(upper as i64),
                }
            } else if c.is_ascii_digit() {
                KeySpec {
                    key: c.to_string(),
                    code: format!("Digit{c}"),
                    vk: Some(c as i64),
                }
            } else {
                return None;
            }
        }
    };
    Some(spec)
}

/// A resolved key chord (optional modifiers + primary key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChord {
    /// CDP modifier bitmask: Alt=1, Control=2, Meta=4, Shift=8.
    pub modifiers: i64,
    /// Modifier key specs to press before and release after the primary key.
    pub modifier_specs: Vec<KeySpec>,
    /// Primary key spec.
    pub key: KeySpec,
}

/// Resolve a single key or `+`-delimited key chord (e.g. `"Control+K"`, `"Shift+Tab"`, `"Meta+Enter"`).
pub fn resolve_chord(input: &str) -> Option<KeyChord> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed.contains('+') {
        let key = resolve(trimmed)?;
        return Some(KeyChord {
            modifiers: 0,
            modifier_specs: Vec::new(),
            key,
        });
    }
    let parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();
    if parts.len() < 2 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let (&primary_raw, chord_modifiers) = parts.split_last()?;
    let mut modifiers = 0i64;
    let mut modifier_specs = Vec::with_capacity(chord_modifiers.len());
    for &part in chord_modifiers {
        let (bit, canonical) = match part.to_ascii_lowercase().as_str() {
            "alt" | "option" => (1, "Alt"),
            "control" | "ctrl" => (2, "Control"),
            "meta" | "command" | "cmd" => (4, "Meta"),
            "shift" => (8, "Shift"),
            _ => return None,
        };
        if modifiers & bit == 0 {
            modifiers |= bit;
            modifier_specs.push(resolve(canonical)?);
        }
    }
    let key = resolve(primary_raw)?;
    Some(KeyChord {
        modifiers,
        modifier_specs,
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::{resolve, resolve_chord};

    #[test]
    fn named_keys() {
        let e = resolve("Enter").unwrap();
        assert_eq!(e.key, "Enter");
        assert_eq!(e.code, "Enter");
        assert_eq!(e.vk, Some(13));
    }

    #[test]
    fn letters_use_dom_code() {
        let a = resolve("a").unwrap();
        assert_eq!(a.code, "KeyA");
        assert_eq!(a.key, "a");
        assert_eq!(a.vk, Some(65));
        let z = resolve("Z").unwrap();
        assert_eq!(z.code, "KeyZ");
        assert_eq!(z.key, "Z");
    }

    #[test]
    fn digits_use_digit_code() {
        let d = resolve("7").unwrap();
        assert_eq!(d.code, "Digit7");
        assert_eq!(d.vk, Some(55));
    }

    #[test]
    fn rejects_empty_and_multi_char() {
        assert!(resolve("").is_none());
        assert!(resolve("   ").is_none());
        assert!(resolve("Ctrl+C").is_none());
    }

    #[test]
    fn parses_modifier_chords() {
        let c = resolve_chord("Control+k").unwrap();
        assert_eq!(c.modifiers, 2);
        assert_eq!(c.modifier_specs.len(), 1);
        assert_eq!(c.key.code, "KeyK");

        let combo = resolve_chord("Control+Shift+Z").unwrap();
        assert_eq!(combo.modifiers, 2 | 8);
        assert_eq!(combo.modifier_specs.len(), 2);
        assert_eq!(combo.key.code, "KeyZ");

        let tab = resolve_chord("Shift+Tab").unwrap();
        assert_eq!(tab.modifiers, 8);
        assert_eq!(tab.key.code, "Tab");

        let single = resolve_chord("Enter").unwrap();
        assert_eq!(single.modifiers, 0);
        assert!(single.modifier_specs.is_empty());

        assert!(resolve_chord("Bogus+K").is_none());
        assert!(resolve_chord("Control+").is_none());
    }
}
