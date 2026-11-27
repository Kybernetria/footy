//! Conversions between Footy's shortcut strings and the XDG portal formats.

/// Convert a Footy binding such as `ctrl+shift+space` into the XDG shortcut
/// trigger format (`CTRL+SHIFT+space`) used as a `preferred_trigger` hint by
/// the GlobalShortcuts portal.
///
/// Returns `None` when the binding cannot be expressed (no main key, the `fn`
/// key, or an unknown key name). The compositor then lets the user choose.
pub fn xdg_trigger(binding: &str) -> Option<String> {
    let mut modifiers: Vec<&'static str> = Vec::new();
    let mut key: Option<String> = None;

    for part in binding.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let lower = part.to_ascii_lowercase();
        let modifier = match lower.as_str() {
            "ctrl" | "control" => Some("CTRL"),
            "shift" => Some("SHIFT"),
            "alt" | "option" => Some("ALT"),
            "super" | "meta" | "cmd" | "command" | "win" | "windows" | "logo" => Some("LOGO"),
            _ => None,
        };
        if let Some(modifier) = modifier {
            if !modifiers.contains(&modifier) {
                modifiers.push(modifier);
            }
            continue;
        }
        if key.is_some() {
            // XDG triggers have exactly one non-modifier key.
            return None;
        }
        key = Some(xkb_key_name(&lower)?);
    }

    let key = key?;
    // Keep a stable modifier order so equal bindings produce equal triggers.
    let order = ["CTRL", "ALT", "SHIFT", "LOGO"];
    modifiers.sort_by_key(|m| order.iter().position(|o| o == m));
    let mut trigger = modifiers.join("+");
    if !trigger.is_empty() {
        trigger.push('+');
    }
    trigger.push_str(&key);
    Some(trigger)
}

fn xkb_key_name(lower: &str) -> Option<String> {
    let named = match lower {
        "space" => "space",
        "enter" | "return" => "Return",
        "escape" | "esc" => "Escape",
        "tab" => "Tab",
        "backspace" => "BackSpace",
        "delete" | "del" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "Page_Up",
        "pagedown" => "Page_Down",
        "up" | "arrowup" => "Up",
        "down" | "arrowdown" => "Down",
        "left" | "arrowleft" => "Left",
        "right" | "arrowright" => "Right",
        "minus" => "minus",
        "equal" => "equal",
        "comma" => "comma",
        "period" => "period",
        "slash" => "slash",
        "backslash" => "backslash",
        "semicolon" => "semicolon",
        "quote" => "apostrophe",
        "backquote" => "grave",
        "bracketleft" => "bracketleft",
        "bracketright" => "bracketright",
        "pause" => "Pause",
        "printscreen" => "Print",
        "fn" | "function" => return None,
        _ => "",
    };
    if !named.is_empty() {
        return Some(named.to_string());
    }

    let stripped = lower
        .strip_prefix("key")
        .or_else(|| lower.strip_prefix("digit"))
        .filter(|rest| rest.len() == 1)
        .unwrap_or(lower);
    let mut chars = stripped.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphanumeric() {
            return Some(c.to_ascii_lowercase().to_string());
        }
        return None;
    }

    if let Some(number) = stripped.strip_prefix('f') {
        if let Ok(n) = number.parse::<u8>() {
            if (1..=24).contains(&n) {
                return Some(format!("F{n}"));
            }
        }
    }
    None
}

/// X11 keysyms used for the RemoteDesktop portal. Keysyms (unlike keycodes)
/// let the compositor resolve the key through the user's current layout.
pub mod keysym {
    pub const CONTROL_L: i32 = 0xffe3;
    pub const SHIFT_L: i32 = 0xffe1;
    pub const SUPER_L: i32 = 0xffeb;
    pub const INSERT: i32 = 0xff63;
    pub const RETURN: i32 = 0xff0d;
    pub const LOWER_V: i32 = 0x0076;
}

#[cfg(test)]
mod tests {
    use super::xdg_trigger;

    #[test]
    fn converts_default_bindings() {
        assert_eq!(xdg_trigger("ctrl+space").as_deref(), Some("CTRL+space"));
        assert_eq!(
            xdg_trigger("ctrl+shift+space").as_deref(),
            Some("CTRL+SHIFT+space")
        );
    }

    #[test]
    fn normalises_modifier_aliases_and_order() {
        assert_eq!(
            xdg_trigger("shift+super+alt+KeyR").as_deref(),
            Some("ALT+SHIFT+LOGO+r")
        );
        assert_eq!(xdg_trigger("Control+Digit5").as_deref(), Some("CTRL+5"));
        assert_eq!(xdg_trigger("meta+F12").as_deref(), Some("LOGO+F12"));
    }

    #[test]
    fn rejects_unrepresentable_bindings() {
        assert_eq!(xdg_trigger("ctrl+shift"), None);
        assert_eq!(xdg_trigger("fn+space"), None);
        assert_eq!(xdg_trigger("ctrl+a+b"), None);
        assert_eq!(xdg_trigger("ctrl+mediaplay"), None);
        assert_eq!(xdg_trigger(""), None);
    }
}
