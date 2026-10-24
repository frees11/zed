//! Hardware keyboard input from UIKit's `UIKey`.
//!
//! UIKit identifies a key by its usage on the HID keyboard page
//! (`UIKey.keyCode`). Keys of the main block are named like gpui_macos names
//! them: after the ASCII character they type without modifiers on the active
//! layout (`UIKey.charactersIgnoringModifiers`), so a keybinding fires on the
//! key labelled with it. Where the layout types no ASCII character on a key,
//! as on Cyrillic or Thai layouts, the key is named after its position on a US
//! ANSI keyboard instead. Keypad and control keys have fixed names. The text
//! the key types comes from UIKit as `key_char`.
//!
//! UIKit reports a held key as one press. A key down GPUI handled is withheld
//! from UIKit's text input, which repeats only the keys it receives, so the
//! keyboard repeats it as `is_held` key downs itself.

use gpui::{
    Capslock, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, PlatformInput,
};
use std::{ops::RangeInclusive, time::Duration};

/// How long a key GPUI handled is held before it repeats, and how often it repeats then. UIKit
/// exposes no key repeat settings, so these are gpui_linux's Wayland defaults.
pub(crate) const KEY_REPEAT_DELAY: Duration = Duration::from_millis(500);
pub(crate) const KEY_REPEAT_INTERVAL: Duration = Duration::from_micros(62_500);

/// A key's usage on the HID keyboard page, as `UIKey.keyCode` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HidUsage(pub(crate) u16);

/// Whether a key went down or came back up. A cancelled press counts as up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyTransition {
    Down,
    Up,
}

/// One phase of a hardware key press, as UIKit reports it.
#[derive(Clone, Debug)]
pub(crate) struct KeyPress {
    pub(crate) usage: HidUsage,
    /// The modifiers held in addition to this key, from `UIKey.modifierFlags`.
    pub(crate) modifiers: Modifiers,
    pub(crate) capslock: Capslock,
    /// The text the key types with the held modifiers, `UIKey.characters`.
    pub(crate) characters: String,
    /// The text the key types without modifiers, `UIKey.charactersIgnoringModifiers`.
    pub(crate) characters_ignoring_modifiers: String,
}

/// Tracks the hardware keyboard across presses: the modifier keys held, the
/// keys whose key down GPUI handled, so UIKit never saw them pressed, and the
/// held key that repeats.
#[derive(Debug, Default)]
pub(crate) struct HardwareKeyboard {
    held: HeldModifiers,
    claimed: Vec<HidUsage>,
    last_key_down: Option<(HidUsage, Keystroke)>,
    repeating: Option<(HidUsage, Keystroke)>,
    /// Changes whenever a repeat starts or stops, so a timer of an earlier repeat stops.
    repeat_generation: u64,
}

impl HardwareKeyboard {
    /// Converts a press into the GPUI event it produces, if any.
    pub(crate) fn input(
        &mut self,
        press: &KeyPress,
        transition: KeyTransition,
    ) -> Option<PlatformInput> {
        if press.usage.0 == KEYBOARD_CAPS_LOCK {
            self.stop_repeat();
            return Some(self.modifiers_changed(press.capslock));
        }
        if let Some(modifier) = ModifierKey::from_usage(press.usage) {
            match transition {
                KeyTransition::Down => self.held.press(modifier),
                KeyTransition::Up => self.held.release(modifier),
            }
            self.stop_repeat();
            return Some(self.modifiers_changed(press.capslock));
        }
        self.held.release_absent(press.modifiers);
        if transition == KeyTransition::Up
            && self
                .repeating
                .as_ref()
                .is_some_and(|(usage, _)| *usage == press.usage)
        {
            self.stop_repeat();
        }
        let keystroke = keystroke(press);
        if transition == KeyTransition::Down {
            self.last_key_down = keystroke.clone().map(|keystroke| (press.usage, keystroke));
        }
        let keystroke = keystroke?;
        Some(match transition {
            KeyTransition::Down => PlatformInput::KeyDown(KeyDownEvent {
                keystroke,
                is_held: false,
                prefer_character_input: false,
            }),
            KeyTransition::Up => PlatformInput::KeyUp(KeyUpEvent { keystroke }),
        })
    }

    /// Records whether GPUI handled a key down and returns whether UIKit should
    /// still receive the press. A key up reaches UIKit exactly when its key
    /// down did, so UIKit never sees a key stay down or come up unpressed. A
    /// key down GPUI handled starts repeating; any other key down stops the
    /// repeat, since UIKit repeats the keys it receives itself.
    pub(crate) fn forward(
        &mut self,
        usage: HidUsage,
        transition: KeyTransition,
        handled: bool,
    ) -> bool {
        match transition {
            KeyTransition::Down if handled => {
                if !self.claimed.contains(&usage) {
                    self.claimed.push(usage);
                }
                match self.last_key_down.take() {
                    Some((pressed, keystroke)) if pressed == usage => {
                        self.repeat_generation += 1;
                        self.repeating = Some((usage, keystroke));
                    }
                    _ => self.stop_repeat(),
                }
                false
            }
            KeyTransition::Down => {
                self.stop_repeat();
                true
            }
            KeyTransition::Up => {
                let claimed = self.claimed.len();
                self.claimed.retain(|claimed| *claimed != usage);
                self.claimed.len() == claimed
            }
        }
    }

    /// The modifiers whose keys are held.
    pub(crate) fn modifiers(&self) -> Modifiers {
        self.held.modifiers()
    }

    /// The generation of the running key repeat, which its timer passes to [`Self::repeat`].
    pub(crate) fn repeat_generation(&self) -> Option<u64> {
        self.repeating.as_ref().map(|_| self.repeat_generation)
    }

    /// The `is_held` key down a timer of the repeat `generation` delivers, while that repeat
    /// still runs.
    pub(crate) fn repeat(&self, generation: u64) -> Option<PlatformInput> {
        let (_, keystroke) = self
            .repeating
            .as_ref()
            .filter(|_| self.repeat_generation == generation)?;
        Some(PlatformInput::KeyDown(KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: true,
            prefer_character_input: false,
        }))
    }

    /// Stops the key repeat, for example when the window stops receiving keys.
    pub(crate) fn stop_repeat(&mut self) {
        if self.repeating.take().is_some() {
            self.repeat_generation += 1;
        }
    }

    fn modifiers_changed(&self, capslock: Capslock) -> PlatformInput {
        PlatformInput::ModifiersChanged(ModifiersChangedEvent {
            modifiers: self.held.modifiers(),
            capslock,
        })
    }
}

const KEYBOARD_CAPS_LOCK: u16 = 0x39;
const MODIFIER_USAGES: RangeInclusive<u16> = 0xE0..=0xE7;

/// One of the eight HID modifier keys, from left control (0xE0) to right GUI (0xE7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModifierKey(u8);

impl ModifierKey {
    fn from_usage(usage: HidUsage) -> Option<Self> {
        MODIFIER_USAGES
            .contains(&usage.0)
            .then(|| Self(1 << (usage.0 - MODIFIER_USAGES.start())))
    }
}

/// The held modifier keys, one bit per HID modifier usage, so releasing one
/// of two held shift keys keeps shift held.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HeldModifiers(u8);

const CONTROL_KEYS: u8 = 0b0001_0001;
const SHIFT_KEYS: u8 = 0b0010_0010;
const ALT_KEYS: u8 = 0b0100_0100;
const GUI_KEYS: u8 = 0b1000_1000;

impl HeldModifiers {
    fn press(&mut self, key: ModifierKey) {
        self.0 |= key.0;
    }

    fn release(&mut self, key: ModifierKey) {
        self.0 &= !key.0;
    }

    /// Drops modifiers a key reports as not held, when their release reached
    /// another responder.
    fn release_absent(&mut self, reported: Modifiers) {
        for (held, keys) in [
            (reported.control, CONTROL_KEYS),
            (reported.shift, SHIFT_KEYS),
            (reported.alt, ALT_KEYS),
            (reported.platform, GUI_KEYS),
        ] {
            if !held {
                self.0 &= !keys;
            }
        }
    }

    fn modifiers(self) -> Modifiers {
        Modifiers {
            control: self.0 & CONTROL_KEYS != 0,
            alt: self.0 & ALT_KEYS != 0,
            shift: self.0 & SHIFT_KEYS != 0,
            platform: self.0 & GUI_KEYS != 0,
            function: false,
        }
    }
}

/// How a key turns into a GPUI keystroke.
#[derive(Clone, Copy, Debug)]
enum KeyKind {
    /// A key with a fixed name and fixed text, such as `enter` or `up`.
    Control { typed: Option<&'static str> },
    /// A keypad key: a fixed name that keeps it apart from the main key typing
    /// the same text, and layout text.
    Keypad,
    /// A main-block key, named after the ASCII character it types on the
    /// active layout. The key's name and `shifted` are its unshifted and
    /// shifted US ANSI symbols, used where the layout types no ASCII character
    /// on it. With shift held a key that is not a letter is named after its
    /// shifted symbol and shift is cleared, as on macOS.
    Layout { shifted: Option<&'static str> },
}

struct Key {
    usage: u16,
    name: &'static str,
    kind: KeyKind,
}

const fn control(usage: u16, name: &'static str, typed: Option<&'static str>) -> Key {
    Key {
        usage,
        name,
        kind: KeyKind::Control { typed },
    }
}

const fn keypad(usage: u16, name: &'static str) -> Key {
    Key {
        usage,
        name,
        kind: KeyKind::Keypad,
    }
}

const fn letter(usage: u16, name: &'static str) -> Key {
    Key {
        usage,
        name,
        kind: KeyKind::Layout { shifted: None },
    }
}

const fn symbol(usage: u16, name: &'static str, shifted: &'static str) -> Key {
    Key {
        usage,
        name,
        kind: KeyKind::Layout {
            shifted: Some(shifted),
        },
    }
}

/// Keys by HID usage, with their US ANSI names. Keypad operators use
/// distinct names so they stay apart from the main keys typing the same text.
const KEYS: &[Key] = &[
    letter(0x04, "a"),
    letter(0x05, "b"),
    letter(0x06, "c"),
    letter(0x07, "d"),
    letter(0x08, "e"),
    letter(0x09, "f"),
    letter(0x0A, "g"),
    letter(0x0B, "h"),
    letter(0x0C, "i"),
    letter(0x0D, "j"),
    letter(0x0E, "k"),
    letter(0x0F, "l"),
    letter(0x10, "m"),
    letter(0x11, "n"),
    letter(0x12, "o"),
    letter(0x13, "p"),
    letter(0x14, "q"),
    letter(0x15, "r"),
    letter(0x16, "s"),
    letter(0x17, "t"),
    letter(0x18, "u"),
    letter(0x19, "v"),
    letter(0x1A, "w"),
    letter(0x1B, "x"),
    letter(0x1C, "y"),
    letter(0x1D, "z"),
    symbol(0x1E, "1", "!"),
    symbol(0x1F, "2", "@"),
    symbol(0x20, "3", "#"),
    symbol(0x21, "4", "$"),
    symbol(0x22, "5", "%"),
    symbol(0x23, "6", "^"),
    symbol(0x24, "7", "&"),
    symbol(0x25, "8", "*"),
    symbol(0x26, "9", "("),
    symbol(0x27, "0", ")"),
    control(0x28, "enter", Some("\n")),
    control(0x29, "escape", None),
    control(0x2A, "backspace", None),
    control(0x2B, "tab", Some("\t")),
    control(0x2C, "space", Some(" ")),
    symbol(0x2D, "-", "_"),
    symbol(0x2E, "=", "+"),
    symbol(0x2F, "[", "{"),
    symbol(0x30, "]", "}"),
    symbol(0x31, "\\", "|"),
    symbol(0x33, ";", ":"),
    symbol(0x34, "'", "\""),
    symbol(0x35, "`", "~"),
    symbol(0x36, ",", "<"),
    symbol(0x37, ".", ">"),
    symbol(0x38, "/", "?"),
    control(0x3A, "f1", None),
    control(0x3B, "f2", None),
    control(0x3C, "f3", None),
    control(0x3D, "f4", None),
    control(0x3E, "f5", None),
    control(0x3F, "f6", None),
    control(0x40, "f7", None),
    control(0x41, "f8", None),
    control(0x42, "f9", None),
    control(0x43, "f10", None),
    control(0x44, "f11", None),
    control(0x45, "f12", None),
    control(0x49, "insert", None),
    control(0x4A, "home", None),
    control(0x4B, "pageup", None),
    control(0x4C, "delete", None),
    control(0x4D, "end", None),
    control(0x4E, "pagedown", None),
    control(0x4F, "right", None),
    control(0x50, "left", None),
    control(0x51, "down", None),
    control(0x52, "up", None),
    keypad(0x54, "divide"),
    keypad(0x55, "multiply"),
    keypad(0x56, "subtract"),
    keypad(0x57, "add"),
    control(0x58, "enter", Some("\n")),
    keypad(0x59, "1"),
    keypad(0x5A, "2"),
    keypad(0x5B, "3"),
    keypad(0x5C, "4"),
    keypad(0x5D, "5"),
    keypad(0x5E, "6"),
    keypad(0x5F, "7"),
    keypad(0x60, "8"),
    keypad(0x61, "9"),
    keypad(0x62, "0"),
    keypad(0x63, "decimal"),
    control(0x65, "menu", None),
    keypad(0x67, "equal"),
    control(0x68, "f13", None),
    control(0x69, "f14", None),
    control(0x6A, "f15", None),
    control(0x6B, "f16", None),
    control(0x6C, "f17", None),
    control(0x6D, "f18", None),
    control(0x6E, "f19", None),
    control(0x6F, "f20", None),
    control(0x70, "f21", None),
    control(0x71, "f22", None),
    control(0x72, "f23", None),
    control(0x73, "f24", None),
];

/// UIKit reports some keys with private-use characters rather than text.
const PRIVATE_USE_CHARACTERS: RangeInclusive<char> = '\u{E000}'..='\u{F8FF}';

fn is_text(text: &str) -> bool {
    !text.is_empty()
        && text.chars().all(|character| {
            !character.is_control() && !PRIVATE_USE_CHARACTERS.contains(&character)
        })
}

/// A key outside the US ANSI table, such as the ISO key beside left shift, is
/// named after the single character it types without modifiers.
fn fallback_name(characters_ignoring_modifiers: &str) -> Option<String> {
    let mut characters = characters_ignoring_modifiers.chars();
    match (characters.next(), characters.next()) {
        (Some(character), None) if is_text(characters_ignoring_modifiers) => {
            Some(character.to_string())
        }
        _ => None,
    }
}

/// The single printable ASCII character of `text`, if that is all it holds.
fn ascii_character(text: &str) -> Option<char> {
    let mut characters = text.chars();
    match (characters.next(), characters.next()) {
        (Some(character), None) if character.is_ascii_graphic() => Some(character),
        _ => None,
    }
}

/// Names the main-block key `key` after the ASCII character it types on the
/// layout, or after its US ANSI symbols where the layout types no ASCII
/// character on it. With shift held, a key that is not a letter is named after
/// the character it types with shift, and shift is cleared.
fn layout_name(
    key: &Key,
    shifted: Option<&'static str>,
    press: &KeyPress,
    modifiers: &mut Modifiers,
) -> String {
    let unshifted = ascii_character(&press.characters_ignoring_modifiers)
        .map(|character| character.to_ascii_lowercase());
    if unshifted.is_some_and(|character| character.is_ascii_lowercase()) || !modifiers.shift {
        return unshifted.map_or_else(|| key.name.to_string(), String::from);
    }
    let shifted_name = match unshifted {
        Some(character) if character.to_string() != key.name => {
            ascii_character(&press.characters).filter(|shifted| *shifted != character)
        }
        Some(_) => shifted.and_then(ascii_character),
        None => ascii_character(&press.characters).or_else(|| shifted.and_then(ascii_character)),
    };
    match shifted_name {
        Some(shifted_name) => {
            modifiers.shift = false;
            shifted_name.to_string()
        }
        None => unshifted.map_or_else(|| key.name.to_string(), String::from),
    }
}

fn keystroke(press: &KeyPress) -> Option<Keystroke> {
    let mut modifiers = press.modifiers;
    let typed = (!modifiers.control && !modifiers.platform && is_text(&press.characters))
        .then(|| press.characters.clone());
    let Some(key) = KEYS.iter().find(|key| key.usage == press.usage.0) else {
        return Some(Keystroke {
            modifiers,
            key: fallback_name(&press.characters_ignoring_modifiers)?,
            key_char: typed,
        });
    };
    let (name, key_char) = match key.kind {
        KeyKind::Control { typed } => (key.name.to_string(), typed.map(str::to_string)),
        KeyKind::Keypad => (key.name.to_string(), typed),
        KeyKind::Layout { shifted } => (layout_name(key, shifted, press, &mut modifiers), typed),
    };
    Some(Keystroke {
        modifiers,
        key: name,
        key_char,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `core/ultra-keymap` `GPUI_KEY_NAMES` of the Remote Ultra repository, the
    /// table its GPUI clients map keystrokes back to HID usages with.
    const ULTRA_KEYMAP_NAMES: &[(u16, &str)] = &[
        (0x04, "a"),
        (0x05, "b"),
        (0x06, "c"),
        (0x07, "d"),
        (0x08, "e"),
        (0x09, "f"),
        (0x0A, "g"),
        (0x0B, "h"),
        (0x0C, "i"),
        (0x0D, "j"),
        (0x0E, "k"),
        (0x0F, "l"),
        (0x10, "m"),
        (0x11, "n"),
        (0x12, "o"),
        (0x13, "p"),
        (0x14, "q"),
        (0x15, "r"),
        (0x16, "s"),
        (0x17, "t"),
        (0x18, "u"),
        (0x19, "v"),
        (0x1A, "w"),
        (0x1B, "x"),
        (0x1C, "y"),
        (0x1D, "z"),
        (0x1E, "1"),
        (0x1F, "2"),
        (0x20, "3"),
        (0x21, "4"),
        (0x22, "5"),
        (0x23, "6"),
        (0x24, "7"),
        (0x25, "8"),
        (0x26, "9"),
        (0x27, "0"),
        (0x28, "enter"),
        (0x29, "escape"),
        (0x2A, "backspace"),
        (0x2B, "tab"),
        (0x2C, "space"),
        (0x2D, "-"),
        (0x2E, "="),
        (0x2F, "["),
        (0x30, "]"),
        (0x31, "\\"),
        (0x33, ";"),
        (0x34, "'"),
        (0x35, "`"),
        (0x36, ","),
        (0x37, "."),
        (0x38, "/"),
        (0x3A, "f1"),
        (0x3B, "f2"),
        (0x3C, "f3"),
        (0x3D, "f4"),
        (0x3E, "f5"),
        (0x3F, "f6"),
        (0x40, "f7"),
        (0x41, "f8"),
        (0x42, "f9"),
        (0x43, "f10"),
        (0x44, "f11"),
        (0x45, "f12"),
        (0x49, "insert"),
        (0x4A, "home"),
        (0x4B, "pageup"),
        (0x4C, "delete"),
        (0x4D, "end"),
        (0x4E, "pagedown"),
        (0x4F, "right"),
        (0x50, "left"),
        (0x51, "down"),
        (0x52, "up"),
        (0x54, "divide"),
        (0x55, "multiply"),
        (0x56, "subtract"),
        (0x57, "add"),
        (0x63, "decimal"),
        (0x65, "menu"),
        (0x67, "equal"),
        (0x68, "f13"),
        (0x69, "f14"),
        (0x6A, "f15"),
        (0x6B, "f16"),
        (0x6C, "f17"),
        (0x6D, "f18"),
        (0x6E, "f19"),
        (0x6F, "f20"),
        (0x70, "f21"),
        (0x71, "f22"),
        (0x72, "f23"),
        (0x73, "f24"),
    ];

    /// `core/ultra-keymap` `GPUI_SHIFTED_SYMBOL_NAMES`.
    const ULTRA_KEYMAP_SHIFTED_NAMES: &[(u16, &str)] = &[
        (0x1E, "!"),
        (0x1F, "@"),
        (0x20, "#"),
        (0x21, "$"),
        (0x22, "%"),
        (0x23, "^"),
        (0x24, "&"),
        (0x25, "*"),
        (0x26, "("),
        (0x27, ")"),
        (0x2D, "_"),
        (0x2E, "+"),
        (0x2F, "{"),
        (0x30, "}"),
        (0x31, "|"),
        (0x33, ":"),
        (0x34, "\""),
        (0x35, "~"),
        (0x36, "<"),
        (0x37, ">"),
        (0x38, "?"),
    ];

    const LEFT_SHIFT: u16 = 0xE1;
    const RIGHT_SHIFT: u16 = 0xE5;
    const LEFT_GUI: u16 = 0xE3;
    const KEYBOARD_A: u16 = 0x04;
    const KEYBOARD_S: u16 = 0x16;
    const KEYBOARD_1: u16 = 0x1E;
    const KEYBOARD_RETURN: u16 = 0x28;
    const KEYPAD_ENTER: u16 = 0x58;
    const KEYPAD_1: u16 = 0x59;
    const KEYPAD_ASTERISK: u16 = 0x55;
    const NON_US_BACKSLASH: u16 = 0x64;
    const KEYBOARD_UP_ARROW: u16 = 0x52;

    fn press(usage: u16, modifiers: Modifiers, characters: &str, ignoring: &str) -> KeyPress {
        KeyPress {
            usage: HidUsage(usage),
            modifiers,
            capslock: Capslock::default(),
            characters: characters.to_string(),
            characters_ignoring_modifiers: ignoring.to_string(),
        }
    }

    fn down(press: &KeyPress) -> Option<Keystroke> {
        match HardwareKeyboard::default().input(press, KeyTransition::Down)? {
            PlatformInput::KeyDown(event) => Some(event.keystroke),
            other => panic!("expected a key down, got {other:?}"),
        }
    }

    fn keystroke(modifiers: Modifiers, key: &str, key_char: Option<&str>) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.to_string(),
            key_char: key_char.map(str::to_string),
        }
    }

    fn modifiers_changed(input: Option<PlatformInput>) -> ModifiersChangedEvent {
        match input {
            Some(PlatformInput::ModifiersChanged(event)) => event,
            other => panic!("expected modifiers changed, got {other:?}"),
        }
    }

    #[test]
    fn every_key_has_one_entry() {
        for (index, key) in KEYS.iter().enumerate() {
            assert!(
                KEYS[index + 1..]
                    .iter()
                    .all(|other| other.usage != key.usage),
                "usage {:#04x} appears twice",
                key.usage
            );
        }
    }

    #[test]
    fn names_match_ultra_keymap_us_ansi_table() {
        for &(usage, name) in ULTRA_KEYMAP_NAMES {
            let stroke = down(&press(usage, Modifiers::none(), "", ""))
                .unwrap_or_else(|| panic!("usage {usage:#04x} has no keystroke"));
            assert_eq!(stroke.key, name, "usage {usage:#04x}");
        }
    }

    #[test]
    fn shifted_symbols_match_ultra_keymap_and_clear_shift() {
        for &(usage, name) in ULTRA_KEYMAP_SHIFTED_NAMES {
            let stroke = down(&press(usage, Modifiers::shift(), name, "")).unwrap();
            assert_eq!(stroke, keystroke(Modifiers::none(), name, Some(name)));
        }
    }

    #[test]
    fn letters_keep_shift_and_type_layout_text() {
        assert_eq!(
            down(&press(KEYBOARD_A, Modifiers::none(), "a", "a")),
            Some(keystroke(Modifiers::none(), "a", Some("a")))
        );
        assert_eq!(
            down(&press(KEYBOARD_A, Modifiers::shift(), "A", "a")),
            Some(keystroke(Modifiers::shift(), "a", Some("A")))
        );
    }

    #[test]
    fn keys_are_named_after_the_ascii_character_of_their_layout() {
        let azerty_a = 0x14;
        let azerty_z = 0x1A;
        let qwertz_z = 0x1C;
        assert_eq!(
            down(&press(azerty_a, Modifiers::none(), "a", "a")),
            Some(keystroke(Modifiers::none(), "a", Some("a")))
        );
        assert_eq!(
            down(&press(azerty_z, Modifiers::command(), "z", "z")),
            Some(keystroke(Modifiers::command(), "z", None))
        );
        assert_eq!(
            down(&press(azerty_a, Modifiers::command(), "a", "a")),
            Some(keystroke(Modifiers::command(), "a", None))
        );
        assert_eq!(
            down(&press(qwertz_z, Modifiers::command(), "z", "z")),
            Some(keystroke(Modifiers::command(), "z", None))
        );
        assert_eq!(
            down(&press(azerty_a, Modifiers::shift(), "A", "a")),
            Some(keystroke(Modifiers::shift(), "a", Some("A")))
        );
    }

    #[test]
    fn layout_symbols_name_their_key_and_shift_names_the_shifted_character() {
        let azerty_1 = KEYBOARD_1;
        assert_eq!(
            down(&press(azerty_1, Modifiers::none(), "&", "&")),
            Some(keystroke(Modifiers::none(), "&", Some("&")))
        );
        assert_eq!(
            down(&press(azerty_1, Modifiers::shift(), "1", "&")),
            Some(keystroke(Modifiers::none(), "1", Some("1")))
        );
        let azerty_2 = 0x1F;
        assert_eq!(
            down(&press(azerty_2, Modifiers::shift(), "2", "é")),
            Some(keystroke(Modifiers::none(), "2", Some("2")))
        );
    }

    #[test]
    fn keys_without_an_ascii_layout_character_are_named_by_position() {
        let azerty_a = 0x14;
        let thai = down(&press(azerty_a, Modifiers::none(), "ๆ", "ๆ")).unwrap();
        assert_eq!(thai, keystroke(Modifiers::none(), "q", Some("ๆ")));
        let russian_a = KEYBOARD_A;
        assert_eq!(
            down(&press(russian_a, Modifiers::command(), "ф", "ф")),
            Some(keystroke(Modifiers::command(), "a", None))
        );
        assert_eq!(
            down(&press(russian_a, Modifiers::shift(), "Ф", "ф")),
            Some(keystroke(Modifiers::shift(), "a", Some("Ф")))
        );
        let azerty_2 = 0x1F;
        assert_eq!(
            down(&press(azerty_2, Modifiers::none(), "é", "é")),
            Some(keystroke(Modifiers::none(), "2", Some("é")))
        );
    }

    #[test]
    fn alt_types_layout_text() {
        let alt = Modifiers::alt();
        assert_eq!(
            down(&press(KEYBOARD_S, alt, "ß", "s")),
            Some(keystroke(alt, "s", Some("ß")))
        );
    }

    #[test]
    fn command_and_control_type_nothing() {
        assert_eq!(
            down(&press(KEYBOARD_S, Modifiers::command(), "s", "s")),
            Some(keystroke(Modifiers::command(), "s", None))
        );
        assert_eq!(
            down(&press(KEYBOARD_S, Modifiers::control(), "\u{13}", "s")),
            Some(keystroke(Modifiers::control(), "s", None))
        );
        let command_shift = Modifiers::command_shift();
        assert_eq!(
            down(&press(KEYBOARD_1, command_shift, "!", "1")),
            Some(keystroke(Modifiers::command(), "!", None))
        );
    }

    #[test]
    fn control_keys_have_fixed_text() {
        assert_eq!(
            down(&press(KEYBOARD_RETURN, Modifiers::none(), "\r", "\r")),
            Some(keystroke(Modifiers::none(), "enter", Some("\n")))
        );
        assert_eq!(
            down(&press(KEYPAD_ENTER, Modifiers::none(), "\r", "\r")),
            Some(keystroke(Modifiers::none(), "enter", Some("\n")))
        );
        assert_eq!(
            down(&press(
                KEYBOARD_UP_ARROW,
                Modifiers::shift(),
                "UIKeyInputUpArrow",
                "UIKeyInputUpArrow"
            )),
            Some(keystroke(Modifiers::shift(), "up", None))
        );
        assert_eq!(
            down(&press(0x2B, Modifiers::shift(), "\t", "\t")),
            Some(keystroke(Modifiers::shift(), "tab", Some("\t")))
        );
    }

    #[test]
    fn keypad_keys_keep_their_own_names() {
        assert_eq!(
            down(&press(KEYPAD_ASTERISK, Modifiers::none(), "*", "*")),
            Some(keystroke(Modifiers::none(), "multiply", Some("*")))
        );
        assert_eq!(
            down(&press(KEYPAD_1, Modifiers::shift(), "1", "1")),
            Some(keystroke(Modifiers::shift(), "1", Some("1")))
        );
    }

    #[test]
    fn keys_outside_the_table_use_their_single_character() {
        assert_eq!(
            down(&press(NON_US_BACKSLASH, Modifiers::shift(), ">", "<")),
            Some(keystroke(Modifiers::shift(), "<", Some(">")))
        );
        let print_screen = 0x46;
        assert_eq!(down(&press(print_screen, Modifiers::none(), "", "")), None);
        assert_eq!(
            down(&press(
                print_screen,
                Modifiers::none(),
                "UIKeyInputPrintScreen",
                "UIKeyInputPrintScreen"
            )),
            None
        );
        assert_eq!(
            down(&press(
                print_screen,
                Modifiers::none(),
                "\u{F72E}",
                "\u{F72E}"
            )),
            None
        );
    }

    #[test]
    fn dead_keys_type_nothing() {
        let alt = Modifiers::alt();
        assert_eq!(
            down(&press(0x08, alt, "", "e")),
            Some(keystroke(alt, "e", None))
        );
    }

    #[test]
    fn key_up_carries_the_keystroke() {
        let mut keyboard = HardwareKeyboard::default();
        let input = keyboard.input(
            &press(KEYBOARD_A, Modifiers::none(), "a", "a"),
            KeyTransition::Up,
        );
        assert_eq!(
            input.map(|input| match input {
                PlatformInput::KeyUp(event) => event.keystroke,
                other => panic!("expected a key up, got {other:?}"),
            }),
            Some(keystroke(Modifiers::none(), "a", Some("a")))
        );
    }

    #[test]
    fn both_shift_keys_must_be_released() {
        let mut keyboard = HardwareKeyboard::default();
        let shift = |usage| press(usage, Modifiers::none(), "", "");
        let pressed = modifiers_changed(keyboard.input(&shift(LEFT_SHIFT), KeyTransition::Down));
        assert_eq!(pressed.modifiers, Modifiers::shift());
        keyboard.input(&shift(RIGHT_SHIFT), KeyTransition::Down);
        let one_left = modifiers_changed(keyboard.input(&shift(LEFT_SHIFT), KeyTransition::Up));
        assert_eq!(one_left.modifiers, Modifiers::shift());
        let none = modifiers_changed(keyboard.input(&shift(RIGHT_SHIFT), KeyTransition::Up));
        assert_eq!(none.modifiers, Modifiers::none());
        assert_eq!(keyboard.modifiers(), Modifiers::none());
    }

    #[test]
    fn modifier_keys_combine() {
        let mut keyboard = HardwareKeyboard::default();
        keyboard.input(
            &press(LEFT_GUI, Modifiers::none(), "", ""),
            KeyTransition::Down,
        );
        let event = modifiers_changed(keyboard.input(
            &press(RIGHT_SHIFT, Modifiers::command(), "", ""),
            KeyTransition::Down,
        ));
        assert_eq!(event.modifiers, Modifiers::command_shift());
    }

    #[test]
    fn caps_lock_reports_its_state() {
        let mut keyboard = HardwareKeyboard::default();
        let caps_lock = KeyPress {
            capslock: Capslock { on: true },
            ..press(KEYBOARD_CAPS_LOCK, Modifiers::none(), "", "")
        };
        let event = modifiers_changed(keyboard.input(&caps_lock, KeyTransition::Down));
        assert_eq!(event.capslock, Capslock { on: true });
        assert_eq!(event.modifiers, Modifiers::none());
    }

    #[test]
    fn a_key_without_shift_drops_a_lost_shift_release() {
        let mut keyboard = HardwareKeyboard::default();
        keyboard.input(
            &press(LEFT_SHIFT, Modifiers::none(), "", ""),
            KeyTransition::Down,
        );
        keyboard.input(
            &press(LEFT_GUI, Modifiers::shift(), "", ""),
            KeyTransition::Down,
        );
        keyboard.input(
            &press(KEYBOARD_A, Modifiers::command(), "a", "a"),
            KeyTransition::Down,
        );
        assert_eq!(keyboard.modifiers(), Modifiers::command());
    }

    fn handled_key_down(keyboard: &mut HardwareKeyboard, press: &KeyPress) -> Option<u64> {
        keyboard.input(press, KeyTransition::Down);
        assert!(!keyboard.forward(press.usage, KeyTransition::Down, true));
        keyboard.repeat_generation()
    }

    fn repeated(keyboard: &HardwareKeyboard, generation: u64) -> Option<Keystroke> {
        match keyboard.repeat(generation)? {
            PlatformInput::KeyDown(event) => {
                assert!(event.is_held, "a repeat is a held key down");
                Some(event.keystroke)
            }
            other => panic!("expected a key down, got {other:?}"),
        }
    }

    #[test]
    fn a_handled_key_down_repeats_until_its_key_up() {
        let mut keyboard = HardwareKeyboard::default();
        let backspace = press(0x2A, Modifiers::none(), "\u{8}", "\u{8}");
        let generation = handled_key_down(&mut keyboard, &backspace).expect("the key repeats");
        for _ in 0..3 {
            assert_eq!(
                repeated(&keyboard, generation),
                Some(keystroke(Modifiers::none(), "backspace", None))
            );
        }
        keyboard.input(&backspace, KeyTransition::Up);
        assert_eq!(keyboard.repeat_generation(), None);
        assert_eq!(repeated(&keyboard, generation), None);
    }

    #[test]
    fn a_key_down_left_to_uikit_does_not_repeat_and_stops_the_repeat() {
        let mut keyboard = HardwareKeyboard::default();
        let left = press(0x50, Modifiers::none(), "", "");
        let generation = handled_key_down(&mut keyboard, &left).expect("the key repeats");
        let letter = press(KEYBOARD_A, Modifiers::none(), "a", "a");
        keyboard.input(&letter, KeyTransition::Down);
        assert!(keyboard.forward(letter.usage, KeyTransition::Down, false));
        assert_eq!(keyboard.repeat_generation(), None);
        assert_eq!(repeated(&keyboard, generation), None);
    }

    #[test]
    fn the_last_handled_key_down_takes_over_the_repeat() {
        let mut keyboard = HardwareKeyboard::default();
        let left = press(0x50, Modifiers::none(), "", "");
        let right = press(0x4F, Modifiers::none(), "", "");
        let first = handled_key_down(&mut keyboard, &left).expect("left repeats");
        let second = handled_key_down(&mut keyboard, &right).expect("right repeats");
        assert_ne!(first, second);
        assert_eq!(repeated(&keyboard, first), None);
        assert_eq!(
            repeated(&keyboard, second),
            Some(keystroke(Modifiers::none(), "right", None))
        );
        keyboard.input(&left, KeyTransition::Up);
        assert_eq!(
            keyboard.repeat_generation(),
            Some(second),
            "releasing another key keeps the repeat"
        );
    }

    #[test]
    fn modifier_and_caps_lock_keys_stop_the_repeat() {
        for modifier in [LEFT_SHIFT, LEFT_GUI, KEYBOARD_CAPS_LOCK] {
            let mut keyboard = HardwareKeyboard::default();
            let down_arrow = press(0x51, Modifiers::none(), "", "");
            let generation = handled_key_down(&mut keyboard, &down_arrow).expect("the key repeats");
            keyboard.input(
                &press(modifier, Modifiers::none(), "", ""),
                KeyTransition::Down,
            );
            assert_eq!(repeated(&keyboard, generation), None, "{modifier:#04x}");
        }
    }

    #[test]
    fn stopping_the_repeat_ends_its_timer_generation() {
        let mut keyboard = HardwareKeyboard::default();
        let up_arrow = press(KEYBOARD_UP_ARROW, Modifiers::none(), "", "");
        let generation = handled_key_down(&mut keyboard, &up_arrow).expect("the key repeats");
        keyboard.stop_repeat();
        assert_eq!(repeated(&keyboard, generation), None);
        assert_eq!(keyboard.repeat_generation(), None);
    }

    #[test]
    fn repeat_timing_is_the_wayland_default() {
        assert_eq!(KEY_REPEAT_DELAY, Duration::from_millis(500));
        assert_eq!(KEY_REPEAT_INTERVAL * 16, Duration::from_secs(1));
    }

    #[test]
    fn key_up_reaches_uikit_only_when_its_key_down_did() {
        let mut keyboard = HardwareKeyboard::default();
        let handled_key = HidUsage(KEYBOARD_A);
        let forwarded_key = HidUsage(KEYBOARD_S);
        assert!(!keyboard.forward(handled_key, KeyTransition::Down, true));
        assert!(keyboard.forward(forwarded_key, KeyTransition::Down, false));
        assert!(!keyboard.forward(handled_key, KeyTransition::Up, false));
        assert!(keyboard.forward(forwarded_key, KeyTransition::Up, true));
        assert!(keyboard.forward(handled_key, KeyTransition::Up, false));
    }
}
