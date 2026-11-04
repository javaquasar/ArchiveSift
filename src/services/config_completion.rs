//! Completion against an incomplete JSON prefix, without requiring a valid document.
use archive_sift_scraper_api::settings_schema::SettingsSchema;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::ops::Range;

#[derive(Clone, Debug)]
pub struct Completion {
    pub label: String,
    pub detail: String,
    /// Unicode scalar positions, matching egui's text cursor.
    pub range: Range<usize>,
    pub insert: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Expect {
    Key,
    Colon,
    Value,
    Done,
}
struct Frame {
    path: Vec<String>,
    array: bool,
    expect: Expect,
    key: String,
    index: usize,
    seen: HashSet<String>,
}
struct Context {
    path: Vec<String>,
    key: bool,
    prefix: String,
    range: Range<usize>,
    has_colon: bool,
    seen: HashSet<String>,
}

pub fn complete(text: &str, cursor: usize, schema: &SettingsSchema) -> Vec<Completion> {
    let caret = text
        .char_indices()
        .nth(cursor)
        .map_or(text.len(), |(i, _)| i);
    let Some(context) = context(text, caret) else {
        return vec![];
    };
    let Some(node) = schema.at_path(&context.path) else {
        return vec![];
    };
    let mut result = Vec::new();
    if context.key {
        if let Some(properties) = node["properties"].as_object() {
            for (key, child) in properties {
                if !key.starts_with(&context.prefix) || context.seen.contains(key) {
                    continue;
                }
                let required = node["required"]
                    .as_array()
                    .is_some_and(|r| r.iter().any(|v| v == key));
                let mut insert = serde_json::to_string(key).unwrap();
                if !context.has_colon {
                    insert.push_str(": ");
                    insert.push_str(&serde_json::to_string(&placeholder(child, 0)).unwrap());
                }
                result.push(Completion {
                    label: key.clone(),
                    detail: format!(
                        "{}{}{}",
                        if required { "required · " } else { "" },
                        child["type"].as_str().unwrap_or("value"),
                        child["description"]
                            .as_str()
                            .map(|d| format!(" · {d}"))
                            .unwrap_or_default()
                    ),
                    range: char_range(text, context.range.clone()),
                    insert,
                });
            }
        }
    } else {
        let values = if let Some(value) = node.get("const") {
            vec![value.clone()]
        } else if let Some(values) = node["enum"].as_array() {
            values.clone()
        } else {
            let mut values = vec![placeholder(node, 0)];
            if node["type"] == "boolean" {
                values = vec![json!(true), json!(false)];
            }
            for name in ["minimum", "maximum"] {
                if let Some(value) = node.get(name) {
                    if !values.contains(value) {
                        values.push(value.clone());
                    }
                }
            }
            if node["type"]
                .as_array()
                .is_some_and(|t| t.iter().any(|t| t == "null"))
            {
                values.push(Value::Null);
            }
            values
        };
        for value in values {
            let insert = serde_json::to_string(&value).unwrap();
            let label = value
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(&insert)
                .to_string();
            if !context.prefix.is_empty()
                && !label.starts_with(&context.prefix)
                && !insert.starts_with(&context.prefix)
            {
                continue;
            }
            result.push(Completion {
                label,
                detail: node["description"]
                    .as_str()
                    .unwrap_or("Declared value")
                    .into(),
                range: char_range(text, context.range.clone()),
                insert,
            });
        }
    }
    result.truncate(40);
    result
}

pub fn apply(text: &str, completion: &Completion) -> (String, usize) {
    let mut result = text
        .chars()
        .take(completion.range.start)
        .collect::<String>();
    result.push_str(&completion.insert);
    result.extend(text.chars().skip(completion.range.end));
    (
        result,
        completion.range.start + completion.insert.chars().count(),
    )
}
fn char_range(text: &str, range: Range<usize>) -> Range<usize> {
    text[..range.start].chars().count()..text[..range.end].chars().count()
}
fn placeholder(schema: &Value, depth: usize) -> Value {
    if let Some(v) = schema
        .get("default")
        .or_else(|| schema.get("const"))
        .or_else(|| schema["enum"].as_array().and_then(|v| v.first()))
    {
        return v.clone();
    }
    if depth > 16 {
        return Value::Null;
    }
    match schema["type"].as_str().or_else(|| {
        schema["type"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(Value::as_str)
    }) {
        Some("object") => Value::Object(
            schema["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter_map(|k| {
                    schema["properties"]
                        .get(k)
                        .map(|s| (k.into(), placeholder(s, depth + 1)))
                })
                .collect(),
        ),
        Some("array") => json!([]),
        Some("string") => json!(""),
        Some("integer" | "number") => schema.get("minimum").cloned().unwrap_or(json!(0)),
        Some("boolean") => json!(false),
        _ => Value::Null,
    }
}
fn path_for_value(frames: &[Frame]) -> Vec<String> {
    let Some(frame) = frames.last() else {
        return vec![];
    };
    let mut path = frame.path.clone();
    path.push(if frame.array {
        frame.index.to_string()
    } else {
        frame.key.clone()
    });
    path
}
fn string_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut i = start + 1;
    let mut escape = false;
    while i < bytes.len() {
        if !escape && bytes[i] == b'"' {
            return i + 1;
        }
        if !escape && bytes[i] == b'\\' {
            escape = true;
        } else {
            escape = false;
        }
        i += 1;
    }
    text.len()
}
fn context(text: &str, caret: usize) -> Option<Context> {
    let bytes = text.as_bytes();
    let mut frames = Vec::<Frame>::new();
    let mut i = 0;
    while i < caret {
        match bytes[i] {
            b'{' | b'[' => {
                let array = bytes[i] == b'[';
                let path = if frames.is_empty() {
                    vec![]
                } else {
                    path_for_value(&frames)
                };
                if let Some(f) = frames.last_mut() {
                    f.expect = Expect::Done;
                }
                frames.push(Frame {
                    path,
                    array,
                    expect: if array { Expect::Value } else { Expect::Key },
                    key: String::new(),
                    index: 0,
                    seen: HashSet::new(),
                });
                i += 1;
            }
            b'}' | b']' => {
                frames.pop()?;
                i += 1;
            }
            b':' => {
                frames.last_mut()?.expect = Expect::Value;
                i += 1;
            }
            b',' => {
                let f = frames.last_mut()?;
                f.expect = if f.array { Expect::Value } else { Expect::Key };
                if f.array {
                    f.index += 1;
                }
                i += 1;
            }
            b'"' => {
                let end = string_end(text, i);
                let closed = end <= caret && bytes.get(end.saturating_sub(1)) == Some(&b'"');
                if !closed || end == caret {
                    let f = frames.last()?;
                    let key = f.expect == Expect::Key;
                    if !key && f.expect != Expect::Value {
                        return None;
                    }
                    let prefix_end = if closed { caret - 1 } else { caret };
                    let prefix = serde_json::from_str::<String>(&format!(
                        "\"{}\"",
                        &text[i + 1..prefix_end]
                    ))
                    .unwrap_or_else(|_| text[i + 1..prefix_end].into());
                    let end = if closed || bytes.get(end.saturating_sub(1)) == Some(&b'"') {
                        end
                    } else {
                        caret
                    };
                    let next = text[end..].trim_start();
                    return Some(Context {
                        path: if key {
                            f.path.clone()
                        } else {
                            path_for_value(&frames)
                        },
                        key,
                        prefix,
                        range: i..end,
                        has_colon: next.starts_with(':'),
                        seen: f.seen.clone(),
                    });
                }
                let value = serde_json::from_str::<String>(&text[i..end]).ok()?;
                let f = frames.last_mut()?;
                if f.expect == Expect::Key {
                    f.key = value.clone();
                    f.seen.insert(value);
                    f.expect = Expect::Colon;
                } else {
                    f.expect = Expect::Done;
                }
                i = end;
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                let start = i;
                while i < caret
                    && !bytes[i].is_ascii_whitespace()
                    && !matches!(bytes[i], b',' | b'}' | b']')
                {
                    i += 1;
                }
                if i == caret {
                    let mut end = i;
                    while end < bytes.len()
                        && !bytes[end].is_ascii_whitespace()
                        && !matches!(bytes[end], b',' | b'}' | b']')
                    {
                        end += 1;
                    }
                    return Some(Context {
                        path: path_for_value(&frames),
                        key: false,
                        prefix: text[start..i].into(),
                        range: start..end,
                        has_colon: false,
                        seen: HashSet::new(),
                    });
                }
                frames.last_mut()?.expect = Expect::Done;
            }
        }
    }
    let f = frames.last()?;
    match f.expect {
        Expect::Key => Some(Context {
            path: f.path.clone(),
            key: true,
            prefix: String::new(),
            range: caret..caret,
            has_colon: false,
            seen: f.seen.clone(),
        }),
        Expect::Value => Some(Context {
            path: path_for_value(&frames),
            key: false,
            prefix: String::new(),
            range: caret..caret,
            has_colon: false,
            seen: HashSet::new(),
        }),
        _ => None,
    }
}
