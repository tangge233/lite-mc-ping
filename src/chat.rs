//! Chat-component → legacy-text conversion.
//!
//! [`StatusResponse::description`](crate::StatusResponse::description) stays
//! raw JSON because a MOTD is either a plain string or a Chat-component object
//! (`{"text": …}`, `{"extra": […]}`). [`to_legacy_text`] renders both shapes
//! into the `§`-coded legacy format understood by server lists, MOTD tooling
//! and plain logs (see <https://minecraft.wiki/w/Text_component_format>);
//! [`to_plain_text`] drops the styling instead.
//!
//! Supported:
//!
//! * every component shape — string, array and object, with nested `extra`
//!   children and `with` arguments;
//! * colors — the 16 legacy names, `"reset"` and `"#rrggbb"`, the latter
//!   downgraded to the nearest legacy color, since the format carries no 24-bit
//!   color;
//! * styles — `bold`, `italic`, `underlined`, `strikethrough` and
//!   `obfuscated`, inherited from the parent component unless overridden.
//!
//! Legacy codes inside a component's text are that component's own business: a
//! client applies them over the style around them, so [`to_legacy_text`] writes
//! the text verbatim — `{"text":"§x§F§F§5§5§5§5hi","color":"green"}` renders
//! dark purple, not green, and dropping the codes would repaint it. The writer
//! can therefore not assume a client that read such text is still in the state
//! it left it in. [`to_plain_text`] drops the codes with the rest of the
//! styling, since a client shows no formatting at all.
//!
//! Anything the legacy format cannot express is skipped: `score`, `selector`,
//! `keybind`, `nbt` and the 1.21.9+ `object` sprites contribute no text.
//! `translate` is rendered best-effort from `fallback` or the `with`
//! arguments, since the real text lives in the client's language files.

use serde_json::{Map, Value};

// ─── Conversion ──────────────────────────────────────────────────────────────

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
        active: Some(Style::default()),
    };
    walk(component, Style::default(), &mut |text, style| {
        writer.push(text, style)
    });
    writer.out
}

/// Convert a Chat component into plain text: the text [`to_legacy_text`]
/// renders, with the styling left out — including the codes embedded in the
/// text, which a client reads as formatting (a `§` and the character after it,
/// valid code or not) and therefore never shows.
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
    // Shares its traversal with `to_legacy_text`, which resolves the style of
    // every segment; here that style is simply not written out.
    let mut out = String::new();
    walk(component, Style::default(), &mut |text, _| {
        push_plain_text(&mut out, text)
    });
    out
}

// ─── Styles ──────────────────────────────────────────────────────────────────

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
    /// `"color": "reset"` clears the inherited color and styles. Values that are
    /// not understood — unknown color names, non-boolean flags — are ignored and
    /// the inherited value carries over, matching the crate's lenient parsing
    /// elsewhere.
    fn resolve(parent: Self, obj: &Map<String, Value>) -> Self {
        let mut style = parent;
        if let Some(Value::String(name)) = obj.get("color") {
            if name == "reset" {
                style = Self::default();
            } else if let Some(index) = color_index(name) {
                style.color = Some(index);
            }
        }
        for (key, field) in [
            ("bold", &mut style.bold),
            ("italic", &mut style.italic),
            ("underlined", &mut style.underlined),
            ("strikethrough", &mut style.strikethrough),
            ("obfuscated", &mut style.obfuscated),
        ] {
            if let Some(value) = flag(obj, key) {
                *field = value;
            }
        }
        style
    }

    /// Whether a client already in this state renders `other` as it is: nothing
    /// `other` turns off is on here.
    ///
    /// Vanilla can only switch an attribute off with `§r`, which clears the
    /// color too, so a run that drops one costs a reset. Attributes `other`
    /// turns *on* are written as codes when its run is written, and the ones
    /// running here that `other` keeps are simply left on.
    fn covers(&self, other: &Self) -> bool {
        (self.color.is_none() || other.color.is_some())
            && (!self.bold || other.bold)
            && (!self.italic || other.italic)
            && (!self.underlined || other.underlined)
            && (!self.strikethrough || other.strikethrough)
            && (!self.obfuscated || other.obfuscated)
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

/// Index of the legacy color closest to `rgb`.
///
/// [`min_by_key`] keeps the first minimum, so a tie goes to the earlier entry in
/// [`COLORS`], and an exact match has distance 0 and always wins.
///
/// Downgrading is the only option: the format carries no 24-bit color, and the
/// `§x§r§r§g§g§b§b` extension that would carry one is not understood by vanilla
/// clients — they drop the unknown `§x` pair and read the hex digits after it
/// as codes of their own.
fn nearest_color(rgb: u32) -> usize {
    COLORS
        .iter()
        .enumerate()
        .min_by_key(|&(_, &(_, value))| distance(rgb, value))
        .map_or(0, |(index, _)| index)
}

/// Split an RGB value into its three channels.
fn channels(rgb: u32) -> (i32, i32, i32) {
    (
        ((rgb >> 16) & 0xFF) as i32,
        ((rgb >> 8) & 0xFF) as i32,
        (rgb & 0xFF) as i32,
    )
}

/// Squared difference between two 0–255 channel values: at most `255²` per
/// channel, so the three-channel sum in [`distance`] cannot overflow a `u32`.
fn square(a: i32, b: i32) -> u32 {
    let diff = (a - b).unsigned_abs();
    diff * diff
}

/// Squared sRGB distance between two colors.
///
/// Squared, so the comparison needs no square root: it orders distances exactly
/// and keeps equidistant colors tied, which is what [`nearest_color`] resolves.
fn distance(a: u32, b: u32) -> u32 {
    let (a_r, a_g, a_b) = channels(a);
    let (b_r, b_g, b_b) = channels(b);
    square(a_r, b_r) + square(a_g, b_g) + square(a_b, b_b)
}

/// A boolean style flag, if present as a JSON boolean.
fn flag(obj: &Map<String, Value>, key: &str) -> Option<bool> {
    obj.get(key)?.as_bool()
}

// ─── Traversal ───────────────────────────────────────────────────────────────

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
        // A list is shorthand for `{text: <first>, extra: [<rest>]}`, so the
        // first element's style is what the rest inherit.
        Value::Array(items) => {
            let mut rest = items.iter();
            let Some(first) = rest.next() else {
                return parent;
            };
            let inherited = walk(first, parent, emit);
            for item in rest {
                walk(item, inherited, emit);
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
        // Numbers and booleans are shorthand for the text they stringify to;
        // `null` and unknown shapes carry no text at all.
        Value::Number(_) | Value::Bool(_) => {
            emit_literal(component, parent, emit);
            parent
        }
        Value::Null => parent,
    }
}

/// Emit a component position that carries text: a `text` value, or a shorthand
/// string, number or boolean standing in for one.
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

// ─── Legacy output ───────────────────────────────────────────────────────────

/// Accumulates the legacy text for the segments [`walk`] resolves, writing the
/// codes each one needs and no more.
///
/// Legacy codes are stateful: a code applies until it is changed, and a style
/// can only be switched off by `§r`, which also clears the color. `active`
/// tracks the state a client would be in after reading everything written so
/// far, so a segment writes the codes it still needs, plus one reset when a
/// previously written attribute has to be dropped.
struct LegacyWriter {
    out: String,
    /// The style a client is in after reading `out`, or `None` once a segment's
    /// own text carried codes: what those left behind cannot be known here.
    active: Option<Style>,
}

impl LegacyWriter {
    /// Append a text run that carries `style`.
    fn push(&mut self, text: &str, style: Style) {
        // Empty runs carry no visible styling; skipping them lets the next run
        // diff against the state that is actually on the wire.
        if text.is_empty() {
            return;
        }
        let mut active = self.active.unwrap_or_default();
        // `None` means an earlier run's own codes left the client somewhere
        // unknowable, so this run starts from a state it states itself.
        if !self.active.is_some_and(|state| state.covers(&style)) {
            push_code(&mut self.out, 'r');
            active = Style::default();
        }
        if let Some(index) = style.color
            && active.color != style.color
        {
            push_code(&mut self.out, COLORS[index].0);
            // A color code clears the flags as well — vanilla's `§l` and friends
            // only turn attributes on — so the color is all that survives it,
            // and the flags below are written again where they are wanted.
            active = Style {
                color: Some(index),
                ..Style::default()
            };
        }
        for (wanted, on, code) in [
            (style.bold, active.bold, 'l'),
            (style.italic, active.italic, 'o'),
            (style.underlined, active.underlined, 'n'),
            (style.strikethrough, active.strikethrough, 'm'),
            (style.obfuscated, active.obfuscated, 'k'),
        ] {
            if wanted && !on {
                push_code(&mut self.out, code);
            }
        }
        // The text goes out as it came in: a client applies the codes inside it
        // over the style just written, exactly as it applies them over the
        // component's own style, so rewriting them would change the rendering.
        self.out.push_str(text);
        self.active = if text.contains('§') {
            None
        } else {
            Some(style)
        };
    }
}

/// Append the legacy code `§code` to `out`.
fn push_code(out: &mut String, code: char) {
    out.push('§');
    out.push(code);
}

/// Append `text` to `out` the way a client shows it with the styling dropped.
///
/// A `§` in component text starts a formatting code, so the character after it
/// is formatting too and a client never draws either — whether or not the code
/// is one it knows (`§x`, `§z` and even `§ ` are swallowed, and a trailing `§`
/// goes on its own). Keeping the character after an unknown code would show text
/// the client does not: "50§ off" renders as "50off", not "50 off".
fn push_plain_text(out: &mut String, text: &str) {
    // Fast path: text carrying no codes is the common case.
    if !text.contains('§') {
        out.push_str(text);
        return;
    }
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            // Skips the code's second character, or nothing at all.
            chars.next();
        } else {
            out.push(c);
        }
    }
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
    fn an_unchanged_style_costs_no_codes() {
        let motd = json!({
            "text": "a",
            "color": "red",
            "bold": true,
            "extra": [{"text": "b", "color": "red"}],
        });
        // Nothing changes across the child, so it needs no codes at all.
        assert_eq!(to_legacy_text(&motd), "§c§lab");
    }

    #[test]
    fn a_color_code_clears_the_flags_with_it() {
        // `§a` switches the inherited bold off on the client (only `§l` and
        // friends turn attributes on), so writing the color alone would drop it.
        let motd = json!({
            "text": "a",
            "color": "red",
            "bold": true,
            "italic": true,
            "extra": [{"text": "b", "color": "green"}],
        });
        assert_eq!(to_legacy_text(&motd), "§c§l§oa§a§l§ob");
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
        // Parsed case-insensitively …
        assert_eq!(
            to_legacy_text(&json!({"text": "x", "color": "#ff0000"})),
            "§4x"
        );
        // … and equidistant to black and dark_blue, so the earlier entry wins.
        assert_eq!(
            to_legacy_text(&json!({"text": "x", "color": "#000055"})),
            "§0x"
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
    fn text_codes_reach_the_client_that_applies_them() {
        // A client reads the codes inside the text over the component's style,
        // so they are what the MOTD renders in.
        let motd = json!({"text": "§cboom", "color": "green"});
        assert_eq!(to_legacy_text(&motd), "§a§cboom");

        // Vanilla has no `§x` code: it is swallowed, and the six codes behind
        // it are applied, so this MOTD renders dark purple even though its
        // style says green.
        let motd = json!({"text": "§x§F§F§5§5§5§5hi", "color": "green"});
        assert_eq!(to_legacy_text(&motd), "§a§x§F§F§5§5§5§5hi");

        // A `§` that starts no code is not styling, so it stays as it is.
        assert_eq!(to_legacy_text(&json!({"text": "50§ off"})), "50§ off");

        // Plain text is what the client shows, so the codes go with the rest
        // of the styling. A `§` swallows the character after it even when it
        // starts no code, as the client reads it as formatting either way.
        assert_eq!(to_plain_text(&json!({"text": "§cboom"})), "boom");
        assert_eq!(to_plain_text(&json!({"text": "a§zb"})), "ab");
        assert_eq!(to_plain_text(&json!({"text": "50§ off"})), "50off");
        assert_eq!(to_plain_text(&json!({"text": "tail§"})), "tail");
    }

    #[test]
    fn text_codes_do_not_leak_into_the_next_run() {
        // The `§l` inside the first run's text stays on in the client, and the
        // child overrides no flag, so the next run resets and restates its
        // color instead of trusting the state the writer had reached.
        let motd = json!({"text": "§lA", "extra": [{"text": "B", "color": "green"}]});
        assert_eq!(to_legacy_text(&motd), "§lA§r§aB");
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
