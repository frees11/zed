//! Hardware keyboard input from UIKit's `UIKey`.
//!
//! UIKit identifies a key by its usage on the HID keyboard page
//! (`UIKey.keyCode`). GPUI names the key after its position on a US ANSI
//! keyboard, the ASCII-equivalent naming GPUI uses for layouts without ASCII
//! keys, so a keystroke names the same physical key on every layout. The text
//! the key types on the active layout comes from UIKit as `key_char`.

use gpui::{
    Capslock, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, PlatformInput,
};
use std::ops::RangeInclusive;

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

/// Tracks the hardware keyboard across presses: the modifier keys held, and
/// the keys whose key down GPUI handled, so UIKit never saw them pressed.
#[derive(Debug, Default)]
pub(crate) struct HardwareKeyboard {
    held: HeldModifiers,
    claimed: Vec<HidUsage>,
}

impl HardwareKeyboard {
    /// Converts a press into the GPUI event it produces, if any.
    pub(crate) fn input(
        &mut self,
        press: &KeyPress,
        transition: KeyTransition,
    ) -> Option<PlatformInput> {
        if press.usage.0 == KEYBOARD_CAPS_LOCK {
            return Some(self.modifiers_changed(press.capslock));
        }
        if let Some(modifier) = ModifierKey::from_usage(press.usage) {
            match transition {
                KeyTransition::Down => self.held.press(modifier),
                KeyTransition::Up => self.held.release(modifier),
            }
            return Some(self.modifiers_changed(press.capslock));
        }
        self.held.release_absent(press.modifiers);
        let keystroke = keystroke(press)?;
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
    /// down did, so UIKit never sees a key stay down or come up unpressed.
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
                false
            }
            KeyTransition::Down => true,
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
    /// A key with a fixed name that types layout text, such as letters.
    Printable,
    /// A US ANSI symbol key; with shift held GPUI names it after its shifted
    /// symbol and clears shift, as on macOS.
    Symbol { shifted: &'static str },
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

const fn printable(usage: u16, name: &'static str) -> Key {
    Key {
        usage,
        name,
        kind: KeyKind::Printable,
    }
}

const fn symbol(usage: u16, name: &'static str, shifted: &'static str) -> Key {
    Key {
        usage,
        name,
        kind: KeyKind::Symbol { shifted },
    }
}

/// Keys by HID usage, named for a US ANSI layout. Keypad operators use
/// distinct names so they stay apart from the main keys typing the same text.
const KEYS: &[Key] = &[
    printable(0x04, "a"),
    printable(0x05, "b"),
    printable(0x06, "c"),
    printable(0x07, "d"),
    printable(0x08, "e"),
    printable(0x09, "f"),
    printable(0x0A, "g"),
    printable(0x0B, "h"),
    printable(0x0C, "i"),
    printable(0x0D, "j"),
    printable(0x0E, "k"),
    printable(0x0F, "l"),
    printable(0x10, "m"),
    printable(0x11, "n"),
    printable(0x12, "o"),
    printable(0x13, "p"),
    printable(0x14, "q"),
    printable(0x15, "r"),
    printable(0x16, "s"),
    printable(0x17, "t"),
    printable(0x18, "u"),
    printable(0x19, "v"),
    printable(0x1A, "w"),
    printable(0x1B, "x"),
    printable(0x1C, "y"),
    printable(0x1D, "z"),
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
    printable(0x54, "divide"),
    printable(0x55, "multiply"),
    printable(0x56, "subtract"),
    printable(0x57, "add"),
    control(0x58, "enter", Some("\n")),
    printable(0x59, "1"),
    printable(0x5A, "2"),
    printable(0x5B, "3"),
    printable(0x5C, "4"),
    printable(0x5D, "5"),
    printable(0x5E, "6"),
    printable(0x5F, "7"),
    printable(0x60, "8"),
    printable(0x61, "9"),
    printable(0x62, "0"),
    printable(0x63, "decimal"),
    control(0x65, "menu", None),
    printable(0x67, "equal"),
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
        KeyKind::Control { typed } => (key.name, typed.map(str::to_string)),
        KeyKind::Printable => (key.name, typed),
        KeyKind::Symbol { shifted } if modifiers.shift => {
            modifiers.shift = false;
            (shifted, typed)
        }
        KeyKind::Symbol { .. } => (key.name, typed),
    };
    Some(Keystroke {
        modifiers,
        key: name.to_string(),
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
    fn keys_are_named_by_position_on_other_layouts() {
        let azerty_a = 0x14;
        assert_eq!(
            down(&press(azerty_a, Modifiers::none(), "a", "a")),
            Some(keystroke(Modifiers::none(), "q", Some("a")))
        );
        let thai = down(&press(azerty_a, Modifiers::none(), "ๆ", "ๆ")).unwrap();
        assert_eq!(thai, keystroke(Modifiers::none(), "q", Some("ๆ")));
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
