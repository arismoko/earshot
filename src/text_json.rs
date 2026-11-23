//! Chat lines between the mod API's `Text` and the JSON components `earshot_core::chat`
//! rewrites. Only what voice chat lines use goes across: literal text, translations
//! with their arguments, children, colours, decorations and click actions. Hover text is
//! left out: `cargo sunrise-mod` reads any use of `TextHover::Text` as building
//! `text::Text`, which made the launcher refuse Earshot on every later game build.
//! Anything else (scores, selectors, NBT, objects) becomes plain text, which a voice
//! chat line never holds.
use mod_api::Text;
use mod_api::text::{TextClick, TextContent, TextStyle};
use serde_json::{Map, Value, json};

/// `text` as a JSON component.
pub fn to_json(text: &Text) -> Value {
    let mut o = Map::new();
    match &text.content {
        TextContent::Literal(s) => {
            o.insert("text".into(), json!(s));
        }
        TextContent::Translatable {
            key,
            fallback,
            arguments,
        } => {
            o.insert("translate".into(), json!(key));
            if let Some(f) = fallback {
                o.insert("fallback".into(), json!(f));
            }
            if !arguments.is_empty() {
                o.insert(
                    "with".into(),
                    Value::Array(arguments.iter().map(to_json).collect()),
                );
            }
        }
        _ => {
            o.insert("text".into(), json!(""));
        }
    }
    style_to_json(&text.style, &mut o);
    if !text.children.is_empty() {
        o.insert(
            "extra".into(),
            Value::Array(text.children.iter().map(to_json).collect()),
        );
    }
    Value::Object(o)
}

fn style_to_json(style: &TextStyle, o: &mut Map<String, Value>) {
    if let Some(c) = &style.color {
        o.insert("color".into(), json!(c));
    }
    for (key, value) in [
        ("bold", style.bold),
        ("italic", style.italic),
        ("underlined", style.underlined),
        ("strikethrough", style.strikethrough),
        ("obfuscated", style.obfuscated),
    ] {
        if let Some(v) = value {
            o.insert(key.into(), json!(v));
        }
    }
    if let Some(i) = &style.insertion {
        o.insert("insertion".into(), json!(i));
    }
    if let Some(click) = &style.click {
        let (action, field, value) = match click {
            TextClick::RunCommand(c) => ("run_command", "command", json!(c)),
            TextClick::SuggestCommand(c) => ("suggest_command", "command", json!(c)),
            TextClick::OpenUrl(u) => ("open_url", "url", json!(u)),
            TextClick::CopyToClipboard(v) => ("copy_to_clipboard", "value", json!(v)),
            TextClick::ChangePage(p) => ("change_page", "page", json!(p)),
            _ => ("", "", Value::Null),
        };
        if !action.is_empty() {
            o.insert(
                "click_event".into(),
                json!({ "action": action, field: value }),
            );
        }
    }
}

/// A JSON component as `Text`; a bare string or number is literal text, a list is its
/// first element with the rest as children.
pub fn from_json(value: &Value) -> Text {
    match value {
        Value::String(s) => Text::literal(s.as_str()),
        Value::Array(list) => {
            let mut iter = list.iter();
            let mut first = iter
                .next()
                .map(from_json)
                .unwrap_or_else(|| Text::literal(""));
            first.children.extend(iter.map(from_json));
            first
        }
        Value::Object(o) => object_to_text(o),
        other => Text::literal(other.to_string()),
    }
}

fn object_to_text(o: &Map<String, Value>) -> Text {
    let mut text = if let Some(key) = o.get("translate").and_then(Value::as_str) {
        let arguments = o
            .get("with")
            .and_then(Value::as_array)
            .map(|args| args.iter().map(from_json).collect())
            .unwrap_or_default();
        let mut t = Text::translate(key, arguments);
        if let (TextContent::Translatable { fallback, .. }, Some(f)) =
            (&mut t.content, o.get("fallback").and_then(Value::as_str))
        {
            *fallback = Some(f.to_owned());
        }
        t
    } else {
        let s = match o.get("text") {
            Some(Value::String(s)) => s.clone(),
            Some(other) if !other.is_null() => other.to_string(),
            _ => String::new(),
        };
        Text::literal(s)
    };
    let style = &mut text.style;
    style.color = o.get("color").and_then(Value::as_str).map(str::to_owned);
    let flag = |k: &str| o.get(k).and_then(Value::as_bool);
    style.bold = flag("bold");
    style.italic = flag("italic");
    style.underlined = flag("underlined");
    style.strikethrough = flag("strikethrough");
    style.obfuscated = flag("obfuscated");
    style.insertion = o
        .get("insertion")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let click = o.get("click_event").or_else(|| o.get("clickEvent"));
    style.click = click.and_then(Value::as_object).and_then(|c| {
        let action = c.get("action")?.as_str()?;
        let s = |k: &str| {
            c.get(k)
                .or_else(|| c.get("value"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        Some(match action {
            "run_command" => TextClick::RunCommand(s("command")?),
            "suggest_command" => TextClick::SuggestCommand(s("command")?),
            "open_url" => TextClick::OpenUrl(s("url")?),
            "copy_to_clipboard" => TextClick::CopyToClipboard(s("value")?),
            "change_page" => {
                TextClick::ChangePage(c.get("page").or_else(|| c.get("value"))?.as_i64()? as i32)
            }
            _ => return None,
        })
    });
    if let Some(extra) = o.get("extra").and_then(Value::as_array) {
        text.children.extend(extra.iter().map(from_json));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invite_line_goes_across_and_back() {
        let line = json!({
            "translate": "message.voicechat.invite",
            "with": [
                "muhnip",
                { "text": "e", "color": "gray" },
                {
                    "translate": "message.voicechat.accept_invite",
                    "color": "green",
                    "click_event": { "action": "run_command", "command": "/voicechat join 1234" },
                    "hover_event": { "action": "show_text", "value": { "translate": "message.voicechat.accept_invite.hover" } }
                }
            ]
        });
        let text = from_json(&line);
        let TextContent::Translatable { key, arguments, .. } = &text.content else {
            panic!("a translation");
        };
        assert_eq!(key, "message.voicechat.invite");
        assert_eq!(arguments.len(), 3);
        assert_eq!(arguments[1].style.color.as_deref(), Some("gray"));
        assert_eq!(
            arguments[2].style.click,
            Some(TextClick::RunCommand("/voicechat join 1234".into()))
        );
        // Hover text doesn't go across (see the module comment): the join button keeps its click.
        assert_eq!(arguments[2].style.hover, None);
        // And back: what earshot_core::chat reads.
        let back = to_json(&text);
        assert_eq!(back["translate"], "message.voicechat.invite");
        assert_eq!(back["with"][0]["text"], "muhnip");
        assert_eq!(
            back["with"][2]["click_event"]["command"],
            "/voicechat join 1234"
        );
        assert!(back["with"][2].get("hover_event").is_none());
    }

    #[test]
    fn lists_and_plain_values() {
        let t = from_json(&json!(["a", { "text": "b", "bold": true }]));
        assert_eq!(t.content, TextContent::Literal("a".into()));
        assert_eq!(t.children.len(), 1);
        assert_eq!(t.children[0].style.bold, Some(true));
        assert_eq!(
            from_json(&json!(7)).content,
            TextContent::Literal("7".into())
        );
    }
}
