//! Chat-component → legacy-text conversion.
//!
//! [`StatusResponse::description`](crate::StatusResponse::description) stays
//! raw JSON because a MOTD is either a plain string or a Chat-component object
//! (`{"text": …}`, `{"extra": […]}`). [`to_legacy_text`] renders both shapes
//! into the `§`-coded legacy format understood by server lists, MOTD tooling
//! and plain logs; [`to_plain_text`] drops the styling instead.
//!
//! Supported:
//!
//! * every component shape — string, array and object, with nested `extra`
//!   children and `with` arguments;
//! * colors — the 16 legacy names, `"reset"` and `"#rrggbb"`, the latter
//!   downgraded to the nearest legacy color (vanilla behavior: the format has
//!   no 24-bit color);
//! * styles — `bold`, `italic`, `underlined`, `strikethrough` and
//!   `obfuscated`, inherited from the parent component unless overridden.
//!
//! Both functions drop legacy codes embedded in literal text (`"§cred"` →
//! `"red"`), so server text cannot override the styles written around it.
//!
//! Anything the legacy format cannot express is skipped: `score`, `selector`,
//! `keybind`, `nbt` and the 1.21.9+ `object` sprites contribute no text.
//! `translate` is rendered best-effort from `fallback` or the `with`
//! arguments, since the real text lives in the client's language files.
//!
//! See <https://minecraft.wiki/w/Text_component_format>.

use serde_json::{Map, Value};

/// The 16 legacy colors in code order (`§0`…`§f`), with their RGB values.
const COLORS: [(char, u32); 16] = [
    ('0', 0x000000), // black
    ('1', 0x0000AA), // dark_blue
    ('2', 0x00AA00), // dark_green
    ('3', 0x00AAAA), // dark_aqua
    ('4', 0xAA0000), // dark_red
    ('5', 0xAA00AA), // dark_purple
    ('6', 0xFFAA00), // gold
    ('7', 0xAAAAAA), // gray
    ('8', 0x555555), // dark_gray
    ('9', 0x5555FF), // blue
    ('a', 0x55FF55), // green
    ('b', 0x55FFFF), // aqua
    ('c', 0xFF5555), // red
    ('d', 0xFF55FF), // light_purple
    ('e', 0xFFFF55), // yellow
    ('f', 0xFFFFFF), // white
];

/// Convert a Chat component into legacy `§`-coded text.
///
/// Never fails: shapes and fields the legacy format cannot express are
/// skipped, so this is safe to call on a raw server response. Typical use is
/// `to_legacy_text(&result.status.description)`.
///
/// # Example
///
/// ```
/// use lite_mc_ping::chat::to_legacy_text;
/// use serde_json::json;
///
/// // A plain-string MOTD is passed through unchanged …
/// assert_eq!(to_legacy_text(&json!("Hello")), "Hello");
///
/// // … while a component object is rendered with legacy codes. The child
/// // inherits `bold` unless it overrides it, so turning it off costs a reset
/// // before the gray code.
/// let motd = json!({
///     "text": "My Minecraft Server\n",
///     "color": "#55FF55",
///     "bold": true,
///     "extra": [{ "text": "Survival & PvP", "color": "gray", "bold": false }],
/// });
/// assert_eq!(
///     to_legacy_text(&motd),
///     "§a§lMy Minecraft Server\n§r§7Survival & PvP"
/// );
/// ```
pub fn to_legacy_text(component: &Value) -> String {
    let mut writer = LegacyWriter {
        out: String::new(),
        active: Style::default(),
    };
    walk(component, Style::default(), &mut |text, style| {
        writer.push(text, style)
    });
    writer.out
}

/// Convert a Chat component into plain text: the content of
/// [`to_legacy_text`] with the styling left out.
///
/// # Example
///
/// ```
/// use lite_mc_ping::chat::to_plain_text;
/// use serde_json::json;
///
/// let motd = json!({"text": "Line one", "color": "red", "extra": ["\nLine two"]});
/// assert_eq!(to_plain_text(&motd), "Line one\nLine two");
/// ```
pub fn to_plain_text(component: &Value) -> String {
    // Still resolves the style per segment so both functions share one
    // traversal; the style is simply not written out.
    let mut out = String::new();
    walk(component, Style::default(), &mut |text, _| {
        push_text(&mut out, text)
    });
    out
}

/// A style with inheritance applied: `color` is an index into [`COLORS`]
/// (`None` = no color code, i.e. the client's default), the flags are concrete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Style {
    color: Option<usize>,
    bold: bool,
    italic: bool,
    underlined: bool,
    strikethrough: bool,
    obfuscated: bool,
}

impl Style {
    /// Resolve a component object's style against its parent's.
    ///
    /// `"color": "reset"` clears the inherited color and styles; values that
    /// are not understood (unknown color names, non-boolean flags) are ignored
    /// and the inherited value carries over, matching the crate's lenient
    /// parsing elsewhere.
    fn resolve(parent: Self, obj: &Map<String, Value>) -> Self {
        let mut style = parent;
        match obj.get("color") {
            Some(Value::String(name)) if name == "reset" => style = Self::default(),
            Some(Value::String(name)) => {
                if let Some(index) = color_index(name) {
                    style.color = Some(index);
                }
            }
            _ => {}
        }
        if let Some(value) = flag(obj, "bold") {
            style.bold = value;
        }
        if let Some(value) = flag(obj, "italic") {
            style.italic = value;
        }
        if let Some(value) = flag(obj, "underlined") {
            style.underlined = value;
        }
        if let Some(value) = flag(obj, "strikethrough") {
            style.strikethrough = value;
        }
        if let Some(value) = flag(obj, "obfuscated") {
            style.obfuscated = value;
        }
        style
    }
}

/// Index into [`COLORS`] for a color name, or `None` if it is not one of the
/// 16 legacy colors. `grey` spellings are accepted alongside `gray`.
fn color_index(name: &str) -> Option<usize> {
    Some(match name {
        "black" => 0,
        "dark_blue" => 1,
        "dark_green" => 2,
        "dark_aqua" => 3,
        "dark_red" => 4,
        "dark_purple" => 5,
        "gold" => 6,
        "gray" | "grey" => 7,
        "dark_gray" | "dark_grey" => 8,
        "blue" => 9,
        "green" => 10,
        "aqua" => 11,
        "red" => 12,
        "light_purple" => 13,
        "yellow" => 14,
        "white" => 15,
        _ => return parse_hex(name).map(nearest_color),
    })
}

/// Parse a `#rrggbb` color into RGB, case-insensitively.
fn parse_hex(name: &str) -> Option<u32> {
    let digits = name.strip_prefix('#')?;
    if digits.len() != 6 {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// Index of the legacy color closest to `rgb` by squared sRGB distance; an
/// exact match has distance 0 and therefore always wins, ties go to the
/// earlier entry in [`COLORS`].
///
/// This mirrors what vanilla clients fall back to for 24-bit colors rather
/// than the BungeeCord `§x§r§r§g§g§b§b` extension, which vanilla does not
/// understand.
fn nearest_color(rgb: u32) -> usize {
    let (r, g, b) = channels(rgb);
    let mut best = 0;
    let mut best_distance = u32::MAX;
    for (index, &(_, value)) in COLORS.iter().enumerate() {
        let (other_r, other_g, other_b) = channels(value);
        let distance = square(r, other_r) + square(g, other_g) + square(b, other_b);
        if distance < best_distance {
            best_distance = distance;
            best = index;
        }
    }
    best
}

/// Split an RGB value into its three channels.
fn channels(rgb: u32) -> (i32, i32, i32) {
    (
        ((rgb >> 16) & 0xFF) as i32,
        ((rgb >> 8) & 0xFF) as i32,
        (rgb & 0xFF) as i32,
    )
}

/// Squared difference between two channel values.
fn square(a: i32, b: i32) -> u32 {
    let diff = (a - b).unsigned_abs();
    diff * diff
}

/// A boolean style flag, if present as a JSON boolean.
fn flag(obj: &Map<String, Value>, key: &str) -> Option<bool> {
    obj.get(key)?.as_bool()
}

/// Walk a component tree, handing every literal text run to `emit` along with
/// the style resolved for it. Returns the style the component's own content
/// used, which children inherit.
fn walk(component: &Value, parent: Style, emit: &mut impl FnMut(&str, Style)) -> Style {
    match component {
        // A bare string is shorthand for `{"text": …}`.
        Value::String(text) => {
            emit(text, parent);
            parent
        }
        // A list is shorthand for `{text: <first>, extra: [<rest>]}`, so every
        // element after the first inherits the first one's style.
        Value::Array(items) => {
            let mut inherited = parent;
            for (index, item) in items.iter().enumerate() {
                let style = walk(item, inherited, emit);
                if index == 0 {
                    inherited = style;
                }
            }
            inherited
        }
        Value::Object(obj) => {
            let style = Style::resolve(parent, obj);
            // Content, in the order of vanilla's auto-detection when an
            // object carries several content keys.
            if let Some(text) = obj.get("text") {
                emit_literal(text, style, emit);
            } else if obj.get("translate").is_some_and(Value::is_string) {
                emit_translation(obj, style, emit);
            }
            if let Some(Value::Array(extra)) = obj.get("extra") {
                for child in extra {
                    walk(child, style, emit);
                }
            }
            style
        }
        // Numbers and booleans are shorthand for their string form; `null` and
        // unknown shapes are not text at all.
        Value::Number(number) => {
            emit(&number.to_string(), parent);
            parent
        }
        Value::Bool(value) => {
            emit(if *value { "true" } else { "false" }, parent);
            parent
        }
        Value::Null => parent,
    }
}

/// Emit a `text` value (or a shorthand element).
fn emit_literal(value: &Value, style: Style, emit: &mut impl FnMut(&str, Style)) {
    match value {
        Value::String(text) => emit(text, style),
        Value::Number(number) => emit(&number.to_string(), style),
        Value::Bool(value) => emit(if *value { "true" } else { "false" }, style),
        _ => {}
    }
}

/// Best-effort rendering of a `translate` component.
///
/// The displayed text is the client's translation of `key`, which a ping crate
/// has no table for: `fallback` is used when the server supplied one,
/// otherwise the `with` arguments are joined by a space. The `%s` slots live
/// in the language file, so a translation that reorders or drops arguments
/// cannot be reproduced here, and a key with no arguments renders nothing.
fn emit_translation(obj: &Map<String, Value>, style: Style, emit: &mut impl FnMut(&str, Style)) {
    if let Some(Value::String(fallback)) = obj.get("fallback") {
        emit(fallback, style);
        return;
    }
    if let Some(Value::Array(args)) = obj.get("with") {
        for (index, arg) in args.iter().enumerate() {
            if index > 0 {
                emit(" ", style);
            }
            walk(arg, style, emit);
        }
    }
}

/// Renders resolved segments into `§`-coded legacy text.
///
/// Legacy codes are stateful: a code applies until it is changed, and a style
/// can only be switched off by `§r`, which also clears the color. `active`
/// tracks the state a client would be in after reading everything written so
/// far, so each segment writes only the codes it needs, plus one reset when a
/// previously written attribute has to be dropped.
struct LegacyWriter {
    out: String,
    active: Style,
}

impl LegacyWriter {
    /// Append a text run that carries `style`.
    fn push(&mut self, text: &str, style: Style) {
        // Empty runs carry no visible styling; skipping them lets the next run
        // diff against the state that is actually on the wire.
        if text.is_empty() {
            return;
        }
        let needs_reset = (self.active.color.is_some() && style.color.is_none())
            || (self.active.bold && !style.bold)
            || (self.active.italic && !style.italic)
            || (self.active.underlined && !style.underlined)
            || (self.active.strikethrough && !style.strikethrough)
            || (self.active.obfuscated && !style.obfuscated);
        if needs_reset {
            self.out.push_str("§r");
            self.active = Style::default();
        }
        if let Some(index) = style.color
            && self.active.color != style.color
        {
            self.out.push('§');
            self.out.push(COLORS[index].0);
        }
        for (wanted, active, code) in [
            (style.bold, self.active.bold, 'l'),
            (style.italic, self.active.italic, 'o'),
            (style.underlined, self.active.underlined, 'n'),
            (style.strikethrough, self.active.strikethrough, 'm'),
            (style.obfuscated, self.active.obfuscated, 'k'),
        ] {
            if wanted && !active {
                self.out.push('§');
                self.out.push(code);
            }
        }
        self.active = style;
        push_text(&mut self.out, text);
    }
}

/// Append `text` to `out`, dropping the legacy codes embedded in it.
///
/// Component text should carry no codes — styling lives in the component — but
/// plugins do embed them, and the legacy format has no escape for a `§`: left
/// in place they would override the codes [`LegacyWriter`] believes it has
/// written, and show up verbatim in plain text.
fn push_text(out: &mut String, text: &str) {
    if text.contains('§') {
        strip_codes(out, text);
    } else {
        out.push_str(text);
    }
}

/// Append `text` to `out` one character at a time, dropping its legacy codes.
///
/// A code is `§` plus one character, typically introduced by a plugin that
/// embedded legacy formatting in a component's text. Both characters are
/// dropped so the words survive ("§cred" → "red"); a `§` followed by anything
/// that is not a code only loses the `§`, since it carries no formatting.
/// BungeeCord's `§x§r§r§g§g§b§b` RGB extension is spotted and dropped whole.
fn strip_codes(out: &mut String, text: &str) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '§' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('x' | 'X') => {
                for _ in 0..6 {
                    if chars.peek() != Some(&'§') {
                        break;
                    }
                    chars.next();
                    if chars.peek().is_some_and(char::is_ascii_hexdigit) {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            Some(code) if is_code(code) => {}
            Some(other) => out.push(other),
            None => {}
        }
    }
}

/// Whether `c` is the second character of a legacy formatting code: `0`–`9`
/// and `a`–`f` colors, `k`–`o` styles, `r` reset.
fn is_code(c: char) -> bool {
    matches!(c, '0'..='9' | 'a'..='f' | 'k'..='o' | 'r')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_string_is_passed_through() {
        assert_eq!(to_legacy_text(&json!("Hello")), "Hello");
        assert_eq!(to_legacy_text(&json!({"text": "Hello"})), "Hello");
    }

    #[test]
    fn object_style_becomes_codes() {
        let motd = json!({"text": "Welcome", "color": "aqua", "bold": true});
        assert_eq!(to_legacy_text(&motd), "§b§lWelcome");
    }

    #[test]
    fn every_style_flag_has_a_code() {
        let motd = json!({
            "text": "x",
            "italic": true,
            "underlined": true,
            "strikethrough": true,
            "obfuscated": true,
        });
        assert_eq!(to_legacy_text(&motd), "§o§n§m§kx");
    }

    #[test]
    fn extra_builds_a_two_line_motd() {
        let motd = json!({
            "text": "",
            "extra": [
                {"text": "My Minecraft Server", "color": "#55FF55", "bold": true},
                {"text": "\n"},
                {"text": "Survival & PvP & Events", "color": "gray"},
            ],
        });
        // The middle component inherits nothing, so the green bold state is
        // reset before the newline and the gray second line.
        assert_eq!(
            to_legacy_text(&motd),
            "§a§lMy Minecraft Server§r\n§7Survival & PvP & Events"
        );
        assert_eq!(
            to_plain_text(&motd),
            "My Minecraft Server\nSurvival & PvP & Events"
        );
    }

    #[test]
    fn children_inherit_parent_style() {
        let motd = json!({
            "text": "a",
            "color": "red",
            "bold": true,
            "extra": [{"text": "b"}, {"text": "c", "bold": false}],
        });
        // "b" needs no codes; turning bold off requires a reset, then the
        // color is re-applied.
        assert_eq!(to_legacy_text(&motd), "§c§lab§r§cc");
    }

    #[test]
    fn color_reset_clears_inherited_color_and_style() {
        let motd = json!({
            "text": "a",
            "color": "red",
            "bold": true,
            "extra": [{"text": "b", "color": "reset"}],
        });
        assert_eq!(to_legacy_text(&motd), "§c§la§rb");
    }

    #[test]
    fn array_shorthand_inherits_first_element() {
        let motd = json!([{"text": "A", "color": "red"}, "B", "C"]);
        assert_eq!(to_legacy_text(&motd), "§cABC");
    }

    #[test]
    fn hex_colors_downgrade_to_the_nearest_legacy_color() {
        assert_eq!(
            to_legacy_text(&json!({"text": "x", "color": "#000000"})),
            "§0x"
        );
        assert_eq!(
            to_legacy_text(&json!({"text": "x", "color": "#010101"})),
            "§0x"
        );
        assert_eq!(
            to_legacy_text(&json!({"text": "x", "color": "#FF0000"})),
            "§4x"
        );
    }

    #[test]
    fn unknown_and_malformed_styles_are_ignored() {
        let motd = json!({"text": "x", "color": "chartreuse", "bold": "yes"});
        assert_eq!(to_legacy_text(&motd), "x");
        let motd = json!({"text": "x", "color": 12});
        assert_eq!(to_legacy_text(&motd), "x");
    }

    #[test]
    fn legacy_codes_in_text_are_stripped() {
        // Otherwise server text could inject codes that override the styles
        // this writer believes it has emitted.
        let motd = json!({"text": "§cboom", "color": "green"});
        assert_eq!(to_legacy_text(&motd), "§aboom");
        assert_eq!(to_plain_text(&motd), "boom");

        // BungeeCord's RGB extension goes too, as does a stray `§`.
        let motd = json!({"text": "§x§F§F§5§5§5§5hi", "color": "green"});
        assert_eq!(to_legacy_text(&motd), "§ahi");
        let motd = json!({"text": "50§ off"});
        assert_eq!(to_legacy_text(&motd), "50 off");
    }

    #[test]
    fn translated_text_uses_fallback_then_with_arguments() {
        let fallback = json!({"translate": "chat.type.text", "fallback": "Steve: hi"});
        assert_eq!(to_legacy_text(&fallback), "Steve: hi");

        let args = json!({
            "translate": "multiplayer.player.joined",
            "with": [{"text": "Steve", "color": "yellow"}, "!"],
        });
        // Arguments replaced by the key's `%s` slots cannot be re-ordered or
        // re-spaced without the language file, so they are joined by a space.
        assert_eq!(to_legacy_text(&args), "§eSteve§r !");

        // Nothing renderable: no fallback, no arguments.
        assert_eq!(to_legacy_text(&json!({"translate": "chat.type.text"})), "");
    }

    #[test]
    fn non_text_content_contributes_nothing() {
        assert_eq!(
            to_legacy_text(&json!({"score": {"name": "x", "objective": "o"}})),
            ""
        );
        assert_eq!(to_legacy_text(&json!({"selector": "@p"})), "");
        assert_eq!(
            to_legacy_text(
                &json!({"type": "object", "object": "atlas", "sprite": "block/emerald_block"})
            ),
            ""
        );
        // ... while sibling components still render.
        let motd = json!({
            "extra": [
                {"type": "object", "object": "atlas", "sprite": "block/emerald_block"},
                {"text": "hi"},
            ],
        });
        assert_eq!(to_legacy_text(&motd), "hi");
    }

    #[test]
    fn shorthand_numbers_and_booleans_are_stringified() {
        assert_eq!(to_plain_text(&json!([1, true, 2.5])), "1true2.5");
    }

    #[test]
    fn empty_components_produce_empty_text() {
        assert_eq!(to_legacy_text(&json!({})), "");
        assert_eq!(to_legacy_text(&json!([])), "");
        assert_eq!(to_legacy_text(&json!(null)), "");
    }
}
