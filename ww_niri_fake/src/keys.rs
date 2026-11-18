//! Key names as the real server takes them, without xkb: the modifiers, the printable ASCII
//! characters and a fixed list of keysym names stand in for the us keymap.

use ww_niri_api::InputErrorOwned;

/// Keysym names the fake knows besides single characters, `F1` to `F24` and the modifiers.
const KEYSYMS: &[&str] = &[
    "Return",
    "Escape",
    "Tab",
    "ISO_Left_Tab",
    "BackSpace",
    "Delete",
    "Insert",
    "Home",
    "End",
    "Page_Up",
    "Page_Down",
    "Prior",
    "Next",
    "Up",
    "Down",
    "Left",
    "Right",
    "space",
    "plus",
    "minus",
    "equal",
    "comma",
    "period",
    "slash",
    "backslash",
    "semicolon",
    "apostrophe",
    "grave",
    "bracketleft",
    "bracketright",
    "Menu",
    "Print",
    "Pause",
    "Caps_Lock",
    "Control_L",
    "Control_R",
    "Shift_L",
    "Shift_R",
    "Alt_L",
    "Alt_R",
    "Super_L",
    "Super_R",
    "XF86AudioPlay",
    "XF86AudioPause",
    "XF86AudioStop",
    "XF86AudioNext",
    "XF86AudioPrev",
    "XF86AudioMute",
    "XF86AudioRaiseVolume",
    "XF86AudioLowerVolume",
    "XF86Back",
    "XF86Forward",
    "XF86HomePage",
    "XF86Search",
    "XF86Reload",
];

/// Function keys go up to this one.
const FUNCTION_KEY_MAX: u8 = 24;

/// The modifiers by name, as `press_keys` takes them.
fn is_modifier(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "ctrl" | "control" | "shift" | "alt" | "super" | "logo" | "mod4"
    )
}

/// A modifier or a keysym name: exact first, then in any case (`return` is `Return`).
pub(crate) fn is_key(name: &str) -> bool {
    if is_modifier(name) {
        return true;
    }
    let mut chars = name.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        return ch.is_ascii_graphic();
    }
    if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
        return (1..=FUNCTION_KEY_MAX).contains(&n);
    }
    KEYSYMS.iter().any(|k| k.eq_ignore_ascii_case(name))
}

/// Checks a combination like `ctrl+shift+Tab`: modifiers, then one key; `ctrl++` is ctrl with
/// plus.
pub(crate) fn check_combo(combo: &str) -> Result<(), InputErrorOwned> {
    let parts: Vec<&str> = combo.split('+').map(str::trim).collect();
    let parts: Vec<&str> = match parts.as_slice() {
        [head @ .., "", ""] => head.iter().copied().chain(["plus"]).collect(),
        _ => parts,
    };
    if parts.iter().any(|p| p.is_empty()) {
        return Err(InputErrorOwned::UnknownKey(combo.to_owned()));
    }
    let (last, mods) = parts.split_last().expect("a split has at least one part");
    if let Some(bad) = mods.iter().find(|m| !is_modifier(m)) {
        return Err(InputErrorOwned::UnknownKey((*bad).to_owned()));
    }
    if !is_key(last) {
        return Err(InputErrorOwned::UnknownKey((*last).to_owned()));
    }
    Ok(())
}

/// The first character of `text` on no key of the us keymap, as `NotTypeable` says it.
pub(crate) fn check_text(text: &str) -> Result<(), InputErrorOwned> {
    match text
        .chars()
        .find(|&ch| !(ch.is_ascii_graphic() || matches!(ch, ' ' | '\n' | '\t')))
    {
        Some(ch) => Err(InputErrorOwned::NotTypeable(u32::from(ch))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos() {
        assert_eq!(check_combo("Return"), Ok(()));
        assert_eq!(check_combo("Ctrl+Shift+Tab"), Ok(()));
        assert_eq!(check_combo("ctrl+l"), Ok(()));
        assert_eq!(check_combo("ctrl++"), Ok(()));
        assert_eq!(check_combo("XF86AudioPlay"), Ok(()));
        assert_eq!(check_combo("return"), Ok(()));
        assert_eq!(check_combo("F12"), Ok(()));
        assert_eq!(
            check_combo("hyper+x"),
            Err(InputErrorOwned::UnknownKey("hyper".to_owned()))
        );
        assert_eq!(
            check_combo("ctrl+Nope"),
            Err(InputErrorOwned::UnknownKey("Nope".to_owned()))
        );
        assert_eq!(
            check_combo("F25"),
            Err(InputErrorOwned::UnknownKey("F25".to_owned()))
        );
        assert_eq!(
            check_combo(""),
            Err(InputErrorOwned::UnknownKey(String::new()))
        );
    }

    #[test]
    fn text() {
        assert_eq!(check_text("Hello, world!\n"), Ok(()));
        assert_eq!(
            check_text("привет"),
            Err(InputErrorOwned::NotTypeable(u32::from('п')))
        );
    }
}
