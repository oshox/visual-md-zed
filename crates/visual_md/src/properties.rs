//! The properties of a note: the `key: value` entries of its YAML front matter,
//! as the properties panel shows them, and the edits that change one of them.
//!
//! The front matter is never written out again from a model of it. Every edit is
//! a replacement of the bytes of the thing that changed, so comments, quoting,
//! ordering and the layout of the properties that were not touched stay exactly
//! as they were. Offsets are bytes of the front matter's body: the text between
//! its two `---` lines.

use std::ops::Range;

use tree_sitter::{Node, Parser};

/// The most of a front matter body that is parsed for the panel; a longer one
/// stays source.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Keys whose entries are lists even while they have no value yet.
const LIST_KEYS: [&str; 3] = ["tags", "aliases", "cssclasses"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Text(String),
    /// A number, as written.
    Number(String),
    Bool(bool),
    /// `2026-10-08`.
    Date(String),
    /// `2026-10-08T10:30` or with seconds.
    DateTime(String),
    /// A list of plain or quoted scalars.
    List {
        items: Vec<ListItem>,
        style: ListStyle,
    },
    /// Anything else: a nested mapping, a block scalar, an anchor, a tag, a list
    /// of lists or a scalar over several lines. It is shown, not edited, in the
    /// panel.
    Complex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    pub text: String,
    pub range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListStyle {
    /// `[a, b]`, from the `[` to after the `]`.
    Flow { range: Range<usize> },
    /// Lines of `- a`, with the column of the dashes.
    Block { indent: usize },
    /// A key that has no value yet.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    pub key: String,
    pub key_range: Range<usize>,
    pub value: Value,
    /// Where the value is written, or `None` when the key has none.
    pub value_range: Option<Range<usize>>,
    /// The whole entry: its key, its value and the lines nested in it.
    pub row_range: Range<usize>,
}

/// A replacement of a range of the body.
pub type Edit = (Range<usize>, String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("not a number")]
    NotANumber,
    #[error("not a date, write it as 2026-10-08")]
    NotADate,
    #[error("not a date and time, write it as 2026-10-08T10:30")]
    NotADateTime,
    #[error("a property needs a name")]
    EmptyName,
    #[error("there is a property named {0} already")]
    DuplicateName(String),
    #[error("a name or value is on one line")]
    MultipleLines,
    #[error("that cannot be written in the front matter")]
    NotRepresentable,
    #[error("that property cannot be edited here")]
    NotEditable,
}

/// The properties of `body`, or `None` when it is not a YAML mapping of plain or
/// quoted keys, in which case it is left as source.
pub fn parse(body: &str) -> Option<Vec<Property>> {
    if body.len() > MAX_BODY_BYTES {
        return None;
    }
    if body
        .lines()
        .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
    {
        return Some(Vec::new());
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_yaml::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(body, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }

    let mut cursor = root.walk();
    let mut documents = root
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "document");
    let document = documents.next()?;
    if documents.next().is_some() {
        return None;
    }
    let mut cursor = document.walk();
    let node = document
        .named_children(&mut cursor)
        .find(|node| node.kind() == "block_node")?;
    let mut cursor = node.walk();
    let mapping = node
        .named_children(&mut cursor)
        .find(|node| node.kind() == "block_mapping")?;

    let mut properties = Vec::new();
    let mut cursor = mapping.walk();
    for pair in mapping.named_children(&mut cursor) {
        if pair.kind() != "block_mapping_pair" {
            continue;
        }
        properties.push(property_of(pair, body)?);
    }
    Some(properties)
}

fn property_of(pair: Node, body: &str) -> Option<Property> {
    let key_node = pair.child_by_field_name("key")?;
    let (key, _) = scalar_of(key_node, body)?;
    let value_node = pair.child_by_field_name("value");
    let value = match value_node {
        None if LIST_KEYS.contains(&key.as_str()) => Value::List {
            items: Vec::new(),
            style: ListStyle::Missing,
        },
        None => Value::Text(String::new()),
        Some(node) => value_of(node, body),
    };
    Some(Property {
        key,
        key_range: key_node.byte_range(),
        value,
        value_range: value_node.map(|node| node.byte_range()),
        row_range: pair.start_byte()..row_end(body, pair.start_byte(), pair.end_byte())?,
    })
}

/// Where the entry that is parsed as `start..end` ends. The parser takes the
/// line break at the end of the text, and the comments that follow an entry at
/// the start of a line, for part of it. A comment there is not the entry's.
fn row_end(body: &str, start: usize, end: usize) -> Option<usize> {
    let mut end = start + body.get(start..end)?.trim_end().len();
    loop {
        let entry = body.get(start..end)?;
        let line_start = entry.rfind('\n').map_or(0, |newline| newline + 1);
        let last_line = entry.get(line_start..)?;
        if line_start == 0 || !last_line.starts_with('#') {
            return Some(end);
        }
        end = start + entry.get(..line_start)?.trim_end().len();
    }
}

fn value_of(node: Node, body: &str) -> Value {
    match node.kind() {
        "flow_node" => flow_value(node, body),
        "block_node" => {
            let mut cursor = node.walk();
            let children: Vec<Node> = node.named_children(&mut cursor).collect();
            match children.as_slice() {
                [sequence] if sequence.kind() == "block_sequence" => {
                    block_list(*sequence, body).unwrap_or(Value::Complex)
                }
                _ => Value::Complex,
            }
        }
        _ => Value::Complex,
    }
}

fn flow_value(node: Node, body: &str) -> Value {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    let [child] = children.as_slice() else {
        // An anchor, a tag, or both with the value.
        return Value::Complex;
    };
    match child.kind() {
        "flow_sequence" => flow_list(*child, body).unwrap_or(Value::Complex),
        "plain_scalar" => {
            let text = body.get(child.byte_range()).unwrap_or_default();
            if text.contains('\n') {
                return Value::Complex;
            }
            let mut cursor = child.walk();
            let kind = child.named_children(&mut cursor).next().map(|n| n.kind());
            match kind {
                Some("boolean_scalar") => match text.to_ascii_lowercase().as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    _ => Value::Complex,
                },
                Some("integer_scalar" | "float_scalar") => Value::Number(text.to_string()),
                Some("string_scalar") if is_datetime(text) => Value::DateTime(text.to_string()),
                Some("string_scalar") if is_date(text) => Value::Date(text.to_string()),
                _ => Value::Text(text.to_string()),
            }
        }
        "double_quote_scalar" | "single_quote_scalar" => match scalar_of(node, body) {
            Some((text, _)) if !text.contains('\n') => Value::Text(text),
            _ => Value::Complex,
        },
        _ => Value::Complex,
    }
}

fn flow_list(sequence: Node, body: &str) -> Option<Value> {
    let mut items = Vec::new();
    let mut cursor = sequence.walk();
    for item in sequence.named_children(&mut cursor) {
        if item.kind() != "flow_node" {
            return None;
        }
        let (text, range) = scalar_of(item, body)?;
        if text.contains('\n') {
            return None;
        }
        items.push(ListItem { text, range });
    }
    Some(Value::List {
        items,
        style: ListStyle::Flow {
            range: sequence.byte_range(),
        },
    })
}

fn block_list(sequence: Node, body: &str) -> Option<Value> {
    let mut items = Vec::new();
    let mut indent = None;
    let mut cursor = sequence.walk();
    for item in sequence.named_children(&mut cursor) {
        if item.kind() != "block_sequence_item" {
            return None;
        }
        let mut item_cursor = item.walk();
        let content: Vec<Node> = item.named_children(&mut item_cursor).collect();
        let [node] = content.as_slice() else {
            return None;
        };
        if node.kind() != "flow_node" {
            return None;
        }
        let (text, range) = scalar_of(*node, body)?;
        if text.contains('\n') {
            return None;
        }
        indent.get_or_insert_with(|| column_of(body, item.start_byte()));
        items.push(ListItem { text, range });
    }
    Some(Value::List {
        items,
        style: ListStyle::Block {
            indent: indent.unwrap_or(2),
        },
    })
}

/// The text of a plain or quoted scalar and where it is written. `None` for
/// anything that has more to it, such as an alias or a flow collection.
fn scalar_of(node: Node, body: &str) -> Option<(String, Range<usize>)> {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    let [scalar] = children.as_slice() else {
        return None;
    };
    let written = body.get(scalar.byte_range())?;
    let text = match scalar.kind() {
        "plain_scalar" => written.to_string(),
        "single_quote_scalar" => written
            .strip_prefix('\'')?
            .strip_suffix('\'')?
            .replace("''", "'"),
        "double_quote_scalar" => {
            unescape_double_quoted(written.strip_prefix('"')?.strip_suffix('"')?)?
        }
        _ => return None,
    };
    Some((text, node.byte_range()))
}

fn unescape_double_quoted(inner: &str) -> Option<String> {
    let mut text = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            text.push(character);
            continue;
        }
        match characters.next()? {
            '"' => text.push('"'),
            '\\' => text.push('\\'),
            '/' => text.push('/'),
            'n' => text.push('\n'),
            't' => text.push('\t'),
            // Other escapes are left to the source rather than guessed at.
            _ => return None,
        }
    }
    Some(text)
}

fn column_of(body: &str, offset: usize) -> usize {
    let line_start = body.get(..offset).map_or(0, |before| {
        before.rfind('\n').map_or(0, |newline| newline + 1)
    });
    offset - line_start
}

/// Whether `text` is a number as YAML writes one. `f64` also reads `inf` and
/// `nan`, which have no digit.
pub fn is_number(text: &str) -> bool {
    text.chars().any(|character| character.is_ascii_digit()) && text.parse::<f64>().is_ok()
}

pub fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
        return false;
    }
    let digits = |range: Range<usize>| -> Option<u32> { text.get(range)?.parse().ok() };
    let (Some(_), Some(month), Some(day)) = (digits(0..4), digits(5..7), digits(8..10)) else {
        return false;
    };
    text.bytes()
        .enumerate()
        .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
        && (1..=12).contains(&month)
        && (1..=31).contains(&day)
}

pub fn is_datetime(text: &str) -> bool {
    let Some((date, time)) = text.split_once(['T', ' ']) else {
        return false;
    };
    if !is_date(date) {
        return false;
    }
    let mut parts = time.split(':');
    let part = |part: Option<&str>, limit: u32| -> bool {
        part.is_some_and(|part| {
            part.len() == 2 && part.parse::<u32>().is_ok_and(|value| value <= limit)
        })
    };
    let hours = part(parts.next(), 23);
    let minutes = part(parts.next(), 59);
    let seconds = match parts.next() {
        None => true,
        Some(seconds) => part(Some(seconds), 59),
    };
    hours && minutes && seconds && parts.next().is_none()
}

/// `text` as it is written for a value or a name: as it is when that reads back
/// as the same text, and in double quotes when it would not.
pub fn scalar_text(text: &str) -> String {
    if needs_quotes(text) {
        double_quoted(text)
    } else {
        text.to_string()
    }
}

fn double_quoted(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn needs_quotes(text: &str) -> bool {
    let Some(first) = text.chars().next() else {
        return true;
    };
    if text.trim() != text {
        return true;
    }
    if "[]{},#&*!|>'\"%@`".contains(first) {
        return true;
    }
    if "-?:".contains(first) && text.chars().nth(1).is_none_or(char::is_whitespace) {
        return true;
    }
    if text.contains(": ") || text.contains(" #") || text.ends_with(':') {
        return true;
    }
    if text.chars().any(char::is_control) {
        return true;
    }
    matches!(
        text.to_ascii_lowercase().as_str(),
        "true" | "false" | "null" | "~" | "yes" | "no" | "on" | "off"
    ) || is_number(text)
}

fn line_break(body: &str) -> &'static str {
    if body.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Applies `edits`, which do not overlap, to `body`.
pub fn apply(body: &str, edits: &[Edit]) -> String {
    let mut sorted: Vec<&Edit> = edits.iter().collect();
    sorted.sort_by_key(|(range, _)| range.start);
    let mut result = String::with_capacity(body.len());
    let mut position = 0;
    for (range, replacement) in sorted {
        result.push_str(body.get(position..range.start).unwrap_or_default());
        result.push_str(replacement);
        position = range.end;
    }
    result.push_str(body.get(position..).unwrap_or_default());
    result
}

/// `edits` when applying them leaves a body that parses to properties for which
/// `expected` holds, and an error when it would not.
fn checked(
    body: &str,
    edits: Vec<Edit>,
    expected: impl Fn(&[Property]) -> bool,
) -> Result<Vec<Edit>, EditError> {
    let edited = apply(body, &edits);
    match parse(&edited) {
        Some(after) if expected(&after) => Ok(edits),
        _ => Err(EditError::NotRepresentable),
    }
}

fn property_at(properties: &[Property], index: usize) -> Result<&Property, EditError> {
    properties.get(index).ok_or(EditError::NotEditable)
}

/// Sets the value of a text, number, date or date and time property to what was
/// typed. A number or a date that is not one is refused, so that a property does
/// not change its type by a typing mistake.
pub fn set_value(
    body: &str,
    properties: &[Property],
    index: usize,
    typed: &str,
) -> Result<Vec<Edit>, EditError> {
    let property = property_at(properties, index)?;
    let typed = typed.trim();
    if typed.contains(['\n', '\r']) {
        return Err(EditError::MultipleLines);
    }
    let keeps_text = matches!(property.value, Value::Text(_));
    let attempt = |written: String| {
        let edit = match &property.value_range {
            Some(range) => (range.clone(), written),
            None => (
                property.row_range.end..property.row_range.end,
                format!(" {written}"),
            ),
        };
        let key = property.key.clone();
        let wanted = typed.to_string();
        checked(body, vec![edit], move |after| {
            after.get(index).is_some_and(|property| {
                property.key == key
                    && match &property.value {
                        Value::Text(text) | Value::Date(text) | Value::DateTime(text) => {
                            *text == wanted
                        }
                        // Text that reads as a number is quoted to stay text.
                        Value::Number(text) => !keeps_text && *text == wanted,
                        _ => false,
                    }
            })
        })
    };
    match &property.value {
        Value::Number(_) if is_number(typed) => attempt(typed.to_string()),
        Value::Number(_) => Err(EditError::NotANumber),
        Value::Date(_) if is_date(typed) => attempt(typed.to_string()),
        Value::Date(_) => Err(EditError::NotADate),
        Value::DateTime(_) if is_datetime(typed) => attempt(typed.to_string()),
        Value::DateTime(_) => Err(EditError::NotADateTime),
        Value::Text(_) => attempt(scalar_text(typed)).or_else(|_| attempt(double_quoted(typed))),
        _ => Err(EditError::NotEditable),
    }
}

/// Turns a `true` into a `false` and the other way round.
pub fn toggle_bool(
    body: &str,
    properties: &[Property],
    index: usize,
) -> Result<Vec<Edit>, EditError> {
    let property = property_at(properties, index)?;
    let (Value::Bool(current), Some(range)) = (&property.value, &property.value_range) else {
        return Err(EditError::NotEditable);
    };
    let new = !*current;
    checked(body, vec![(range.clone(), new.to_string())], move |after| {
        after
            .get(index)
            .is_some_and(|property| property.value == Value::Bool(new))
    })
}

/// Adds `item` to the end of a list.
pub fn list_add(
    body: &str,
    properties: &[Property],
    index: usize,
    item: &str,
) -> Result<Vec<Edit>, EditError> {
    let property = property_at(properties, index)?;
    let Value::List { items, style } = &property.value else {
        return Err(EditError::NotEditable);
    };
    let item = item.trim();
    if item.is_empty() {
        return Err(EditError::EmptyName);
    }
    if item.contains(['\n', '\r']) {
        return Err(EditError::MultipleLines);
    }
    let written = scalar_text(item);
    let line_break = line_break(body);

    let edit = match style {
        ListStyle::Flow { range } => {
            let close = range.end.saturating_sub(1);
            match items.last() {
                Some(last) => (last.range.end..last.range.end, format!(", {written}")),
                None => (close..close, written),
            }
        }
        ListStyle::Block { indent } => {
            let end = items
                .last()
                .map_or(property.row_range.end, |last| last.range.end);
            (
                end..end,
                format!("{line_break}{}- {written}", " ".repeat(*indent)),
            )
        }
        ListStyle::Missing => {
            let end = property.row_range.end;
            (end..end, format!("{line_break}  - {written}"))
        }
    };

    let key = property.key.clone();
    let mut wanted: Vec<String> = items.iter().map(|item| item.text.clone()).collect();
    wanted.push(item.to_string());
    checked(body, vec![edit], move |after| {
        after.get(index).is_some_and(|property| {
            property.key == key
                && matches!(
                    &property.value,
                    Value::List { items, .. }
                        if items.iter().map(|item| &item.text).eq(wanted.iter())
                )
        })
    })
}

/// Removes the item at `item_index` of a list.
pub fn list_remove(
    body: &str,
    properties: &[Property],
    index: usize,
    item_index: usize,
) -> Result<Vec<Edit>, EditError> {
    let property = property_at(properties, index)?;
    let Value::List { items, style } = &property.value else {
        return Err(EditError::NotEditable);
    };
    let item = items.get(item_index).ok_or(EditError::NotEditable)?;

    let edit = match style {
        ListStyle::Flow { range } => {
            let (start, end) = if items.len() == 1 {
                (range.start + 1, range.end.saturating_sub(1))
            } else if let Some(next) = items.get(item_index + 1) {
                (item.range.start, next.range.start)
            } else {
                let previous = items
                    .get(item_index.wrapping_sub(1))
                    .ok_or(EditError::NotEditable)?;
                (previous.range.end, item.range.end)
            };
            (start..end, String::new())
        }
        ListStyle::Block { .. } => (line_range(body, &item.range), String::new()),
        ListStyle::Missing => return Err(EditError::NotEditable),
    };

    let key = property.key.clone();
    let mut wanted: Vec<String> = items.iter().map(|item| item.text.clone()).collect();
    wanted.remove(item_index);
    checked(body, vec![edit], move |after| {
        after.get(index).is_some_and(|property| {
            property.key == key
                && match &property.value {
                    Value::List { items, .. } => {
                        items.iter().map(|item| &item.text).eq(wanted.iter())
                    }
                    // A block list whose last item went has no value left.
                    Value::Text(text) => wanted.is_empty() && text.is_empty(),
                    _ => false,
                }
        })
    })
}

/// The lines `range` is on, with the line break that ends the last of them.
fn line_range(body: &str, range: &Range<usize>) -> Range<usize> {
    let start = body.get(..range.start).map_or(0, |before| {
        before.rfind('\n').map_or(0, |newline| newline + 1)
    });
    let end = body.get(range.end..).map_or(body.len(), |after| {
        after
            .find('\n')
            .map_or(body.len(), |newline| range.end + newline + 1)
    });
    start..end
}

/// Adds a property with no value at the end.
pub fn add_property(
    body: &str,
    properties: &[Property],
    name: &str,
) -> Result<Vec<Edit>, EditError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(EditError::EmptyName);
    }
    if name.contains(['\n', '\r']) {
        return Err(EditError::MultipleLines);
    }
    if properties.iter().any(|property| property.key == name) {
        return Err(EditError::DuplicateName(name.to_string()));
    }
    let line_break = line_break(body);
    let mut line = String::new();
    if !body.is_empty() && !body.ends_with('\n') {
        line.push_str(line_break);
    }
    line.push_str(&scalar_text(name));
    line.push(':');
    line.push_str(line_break);

    let end = body.len();
    let name = name.to_string();
    let count = properties.len();
    checked(body, vec![(end..end, line)], move |after| {
        after.len() == count + 1 && after.last().is_some_and(|property| property.key == name)
    })
}

/// Removes a property with the lines nested in it.
pub fn remove_property(
    body: &str,
    properties: &[Property],
    index: usize,
) -> Result<Vec<Edit>, EditError> {
    let property = property_at(properties, index)?;
    let edit = (line_range(body, &property.row_range), String::new());
    let count = properties.len();
    checked(body, vec![edit], move |after| after.len() + 1 == count)
}

/// Gives a property another name.
pub fn rename_property(
    body: &str,
    properties: &[Property],
    index: usize,
    name: &str,
) -> Result<Vec<Edit>, EditError> {
    let property = property_at(properties, index)?;
    let name = name.trim();
    if name.is_empty() {
        return Err(EditError::EmptyName);
    }
    if name.contains(['\n', '\r']) {
        return Err(EditError::MultipleLines);
    }
    if properties
        .iter()
        .enumerate()
        .any(|(other, property)| other != index && property.key == name)
    {
        return Err(EditError::DuplicateName(name.to_string()));
    }
    let wanted = name.to_string();
    let count = properties.len();
    checked(
        body,
        vec![(property.key_range.clone(), scalar_text(name))],
        move |after| {
            after.len() == count
                && after
                    .get(index)
                    .is_some_and(|property| property.key == wanted)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "title: \"Hello: world\"\ntags: [a, b]\naliases:\n  - one\n  - two\ndone: true\ncount: 3\nrating: 4.5\ndate: 2026-10-08\ntime: 2026-10-08T10:30:00\nempty:\nnested:\n  a: 1\nnote: |\n  block\n  text\n# a comment\nanchor: &x value\nref: *x\nplain text here: yes\nurl: https://example.com/a#b\nnull_value: null\nsingle: 'it''s'\n";

    fn properties(body: &str) -> Vec<Property> {
        parse(body).expect("the body parses")
    }

    fn value_of_key(body: &str, key: &str) -> Value {
        properties(body)
            .into_iter()
            .find(|property| property.key == key)
            .map(|property| property.value)
            .expect("the key is there")
    }

    #[test]
    fn test_every_kind_of_value_is_recognised() {
        let text = |text: &str| Value::Text(text.to_string());
        assert_eq!(value_of_key(SAMPLE, "title"), text("Hello: world"));
        assert_eq!(value_of_key(SAMPLE, "done"), Value::Bool(true));
        assert_eq!(value_of_key(SAMPLE, "count"), Value::Number("3".into()));
        assert_eq!(value_of_key(SAMPLE, "rating"), Value::Number("4.5".into()));
        assert_eq!(
            value_of_key(SAMPLE, "date"),
            Value::Date("2026-10-08".into())
        );
        assert_eq!(
            value_of_key(SAMPLE, "time"),
            Value::DateTime("2026-10-08T10:30:00".into())
        );
        assert_eq!(value_of_key(SAMPLE, "empty"), text(""));
        assert_eq!(value_of_key(SAMPLE, "nested"), Value::Complex);
        assert_eq!(value_of_key(SAMPLE, "note"), Value::Complex);
        assert_eq!(value_of_key(SAMPLE, "anchor"), Value::Complex);
        assert_eq!(value_of_key(SAMPLE, "ref"), Value::Complex);
        assert_eq!(value_of_key(SAMPLE, "plain text here"), text("yes"));
        assert_eq!(value_of_key(SAMPLE, "url"), text("https://example.com/a#b"));
        assert_eq!(value_of_key(SAMPLE, "null_value"), text("null"));
        assert_eq!(value_of_key(SAMPLE, "single"), text("it's"));
    }

    #[test]
    fn test_a_value_over_several_lines_is_not_edited() {
        let body = "a: first\n  second\nb: 1\n";
        let found = properties(body);

        assert_eq!(found[0].value, Value::Complex);
        assert_eq!(found[1].value, Value::Number("1".into()));
        assert_eq!(set_value(body, &found, 0, "x"), Err(EditError::NotEditable));
    }

    #[test]
    fn test_lists_are_flow_or_block_and_known_keys_are_lists_while_empty() {
        let texts = |value: Value| match value {
            Value::List { items, .. } => items.into_iter().map(|item| item.text).collect(),
            other => vec![format!("{other:?}")],
        };
        assert_eq!(texts(value_of_key(SAMPLE, "tags")), vec!["a", "b"]);
        assert_eq!(texts(value_of_key(SAMPLE, "aliases")), vec!["one", "two"]);
        assert!(matches!(
            value_of_key("tags:\ntitle: x\n", "tags"),
            Value::List {
                style: ListStyle::Missing,
                ..
            }
        ));
        assert_eq!(
            value_of_key("related: [[a], b]\n", "related"),
            Value::Complex,
            "a list that has a list in it is not edited"
        );
        assert_eq!(
            value_of_key("quoted: [\"a, b\", 'c']\n", "quoted"),
            Value::List {
                items: vec![
                    ListItem {
                        text: "a, b".into(),
                        range: 9..15
                    },
                    ListItem {
                        text: "c".into(),
                        range: 17..20
                    }
                ],
                style: ListStyle::Flow { range: 8..21 }
            }
        );
    }

    #[test]
    fn test_a_body_that_is_not_a_mapping_of_scalar_keys_is_left_as_source() {
        for body in [
            "- a\n- b\n",
            "just text\n",
            "key: [unclosed\n",
            "a: 1\n  b: 2\n",
            "{a: 1}\n",
            "? [a, b]\n: value\n",
            "a: 1\n---\nb: 2\n",
        ] {
            assert_eq!(parse(body), None, "{body:?}");
        }
        assert_eq!(parse(""), Some(Vec::new()));
        assert_eq!(parse("\n# only a comment\n"), Some(Vec::new()));
    }

    #[test]
    fn test_a_very_long_body_is_left_as_source() {
        let body = "key: value\n".repeat(MAX_BODY_BYTES / 11 + 1);

        assert_eq!(parse(&body), None);
    }

    #[test]
    fn test_ranges_are_bytes_after_multibyte_text() {
        let body = "título: ñandú ✓\nz: 1\n";
        let found = properties(body);

        assert_eq!(found[0].key, "título");
        assert_eq!(
            body.get(found[0].value_range.clone().expect("a value")),
            Some("ñandú ✓")
        );
        assert_eq!(body.get(found[1].row_range.clone()), Some("z: 1"));
    }

    #[test]
    fn test_crlf_bodies_are_read_and_written_with_crlf() {
        let body = "a: 1\r\ntags:\r\n  - x\r\n";
        let found = properties(body);
        let edits = list_add(body, &found, 1, "y").expect("an edit");

        assert_eq!(apply(body, &edits), "a: 1\r\ntags:\r\n  - x\r\n  - y\r\n");
        let edits = add_property(body, &found, "b").expect("an edit");
        assert_eq!(apply(body, &edits), "a: 1\r\ntags:\r\n  - x\r\nb:\r\n");
    }

    #[test]
    fn test_setting_a_text_quotes_it_when_it_has_to() {
        let body = "title: old\nother: keep # comment\n";
        for (typed, written) in [
            ("plain words", "plain words"),
            ("has: colon", "\"has: colon\""),
            ("ends:", "\"ends:\""),
            ("true", "\"true\""),
            ("123", "\"123\""),
            ("#hash", "\"#hash\""),
            ("- dash", "\"- dash\""),
            ("say \"hi\"", "say \"hi\""),
            ("\"quoted\"", "\"\\\"quoted\\\"\""),
            ("[brackets]", "\"[brackets]\""),
            ("trailing # note", "\"trailing # note\""),
            ("2026-10-08", "2026-10-08"),
            ("-ok", "-ok"),
            ("", "\"\""),
        ] {
            let found = properties(body);
            let edits = set_value(body, &found, 0, typed).expect("an edit");
            let edited = apply(body, &edits);

            assert_eq!(
                edited,
                format!("title: {written}\nother: keep # comment\n"),
                "{typed:?}"
            );
        }
    }

    #[test]
    fn test_what_is_typed_reads_back_the_same_or_is_refused() {
        let nasty = [
            "é",
            "😀",
            "a\\b",
            "\\n",
            "\"",
            "'",
            "a'b",
            "``",
            "<<",
            "= x",
            "~x",
            "!tag",
            "&a",
            "*a",
            "|",
            ">",
            "%x",
            "@x",
            "a: ",
            ":a",
            "a:b",
            "-",
            "- ",
            "?",
            "? x",
            "x #y",
            "#",
            "[",
            "{",
            "a,b",
            "yes",
            "Null",
            "0x1F",
            "1_000",
            ".inf",
            "1e3",
            "+1",
            "--",
            "---",
            "...",
            " lead",
            "trail ",
            "tab\there",
            "a\u{2028}b",
            "\u{feff}bom",
            "0o7",
        ];
        let body = "key: old\nz: 1\n";
        let found = properties(body);
        for typed in nasty {
            let edits = set_value(body, &found, 0, typed).expect("an edit");
            let after = properties(&apply(body, &edits));

            assert_eq!(after.len(), 2, "{typed:?}");
            assert!(
                matches!(&after[0].value, Value::Text(text) if text == typed.trim()),
                "{typed:?} read back as {:?}",
                after[0].value
            );
            assert_eq!(after[1].value, Value::Number("1".into()), "{typed:?}");
        }
        for typed in ["a\u{0}b", "a\u{7}b"] {
            assert_eq!(
                set_value(body, &found, 0, typed),
                Err(EditError::NotRepresentable),
                "{typed:?} is not written out wrongly"
            );
        }
    }

    #[test]
    fn test_setting_a_value_that_does_not_fit_its_type_is_refused() {
        let body = "n: 3\nd: 2026-10-08\nt: 2026-10-08T10:30\nx: text\n";
        let found = properties(body);

        assert_eq!(
            set_value(body, &found, 0, "three"),
            Err(EditError::NotANumber)
        );
        assert_eq!(
            set_value(body, &found, 0, "inf"),
            Err(EditError::NotANumber)
        );
        assert_eq!(set_value(body, &found, 0, ""), Err(EditError::NotANumber));
        assert_eq!(
            set_value(body, &found, 1, "tomorrow"),
            Err(EditError::NotADate)
        );
        for not_a_date in [
            "2026-13-01",
            "2026-00-10",
            "2026-10-32",
            "2026-10-00",
            "26-10-08",
        ] {
            assert_eq!(
                set_value(body, &found, 1, not_a_date),
                Err(EditError::NotADate),
                "{not_a_date}"
            );
        }
        for not_a_time in [
            "10:30",
            "2026-10-08T24:00",
            "2026-10-08T10:60",
            "2026-10-08T10:30:60",
        ] {
            assert_eq!(
                set_value(body, &found, 2, not_a_time),
                Err(EditError::NotADateTime),
                "{not_a_time}"
            );
        }
        assert_eq!(
            set_value(body, &found, 3, "two\nlines"),
            Err(EditError::MultipleLines)
        );
        assert_eq!(set_value(body, &found, 9, "x"), Err(EditError::NotEditable));
        assert!(set_value(body, &found, 0, " 4.5 ").is_ok());
        assert!(set_value(body, &found, 1, "2027-01-31").is_ok());
        assert!(set_value(body, &found, 2, "2027-01-31T08:05:09").is_ok());
    }

    #[test]
    fn test_a_property_with_no_value_gets_one_after_its_colon() {
        let body = "a: 1\nempty:\nz: 2\n";
        let found = properties(body);
        let edits = set_value(body, &found, 1, "filled").expect("an edit");

        assert_eq!(apply(body, &edits), "a: 1\nempty: filled\nz: 2\n");
    }

    #[test]
    fn test_a_bool_is_flipped_and_nothing_else_is() {
        let body = "a: true  # keep\nb: False\nc: 1\n";
        let found = properties(body);

        assert_eq!(
            apply(body, &toggle_bool(body, &found, 0).expect("an edit")),
            "a: false  # keep\nb: False\nc: 1\n"
        );
        assert_eq!(
            apply(body, &toggle_bool(body, &found, 1).expect("an edit")),
            "a: true  # keep\nb: true\nc: 1\n"
        );
        assert_eq!(toggle_bool(body, &found, 2), Err(EditError::NotEditable));
    }

    #[test]
    fn test_items_are_added_to_flow_block_and_missing_lists() {
        let body = "flow: [a, b]\nempty_flow: []\nblock:\n  - x\n  - y\nnested:\n    - deep\ntags:\nlast: 1\n";
        let found = properties(body);
        let after = |index: usize, item: &str| {
            apply(body, &list_add(body, &found, index, item).expect("an edit"))
        };

        assert!(after(0, "c").starts_with("flow: [a, b, c]\nempty_flow: []\n"));
        assert!(after(1, "c").contains("empty_flow: [c]\n"));
        assert!(after(2, "z").contains("block:\n  - x\n  - y\n  - z\nnested:"));
        assert!(after(3, "z").contains("nested:\n    - deep\n    - z\ntags:"));
        assert!(after(4, "z").contains("tags:\n  - z\nlast: 1\n"));
        assert!(after(0, "needs: quotes").contains("[a, b, \"needs: quotes\"]"));
        assert!(
            after(0, "2024").contains("[a, b, \"2024\"]"),
            "a number in a list would not be text to other readers"
        );
        assert!(after(0, "true").contains("[a, b, \"true\"]"));
        assert!(after(0, "yes").contains("[a, b, \"yes\"]"));
        assert_eq!(list_add(body, &found, 0, "  "), Err(EditError::EmptyName));
        assert_eq!(list_add(body, &found, 5, "x"), Err(EditError::NotEditable));
    }

    #[test]
    fn test_items_are_removed_from_flow_and_block_lists() {
        let body = "flow: [a, b, c]\none: [only]\nblock:\n  - x\n  - y\n  - z\nlast: 1\n";
        let found = properties(body);
        let after = |index: usize, item: usize| {
            apply(
                body,
                &list_remove(body, &found, index, item).expect("an edit"),
            )
        };

        assert!(after(0, 0).starts_with("flow: [b, c]\n"));
        assert!(after(0, 1).starts_with("flow: [a, c]\n"));
        assert!(after(0, 2).starts_with("flow: [a, b]\n"));
        assert!(after(1, 0).contains("one: []\n"));
        assert!(after(2, 0).contains("block:\n  - y\n  - z\nlast"));
        assert!(after(2, 1).contains("block:\n  - x\n  - z\nlast"));
        assert!(after(2, 2).contains("block:\n  - x\n  - y\nlast"));
        assert_eq!(list_remove(body, &found, 0, 3), Err(EditError::NotEditable));
    }

    #[test]
    fn test_removing_the_last_item_of_a_block_list_leaves_a_key_with_no_value() {
        let body = "tags:\n  - only\nz: 1\n";
        let found = properties(body);
        let edits = list_remove(body, &found, 0, 0).expect("an edit");

        assert_eq!(apply(body, &edits), "tags:\nz: 1\n");
    }

    #[test]
    fn test_a_property_is_added_at_the_end_removed_and_renamed() {
        let body = "a: 1\nb:\n  c: 2\nd: 3";
        let found = properties(body);

        assert_eq!(
            apply(
                body,
                &add_property(body, &found, "new key").expect("an edit")
            ),
            "a: 1\nb:\n  c: 2\nd: 3\nnew key:\n"
        );
        assert_eq!(
            apply("", &add_property("", &[], "first").expect("an edit")),
            "first:\n"
        );
        assert_eq!(
            apply(body, &remove_property(body, &found, 1).expect("an edit")),
            "a: 1\nd: 3"
        );
        assert_eq!(
            apply(body, &remove_property(body, &found, 2).expect("an edit")),
            "a: 1\nb:\n  c: 2\n"
        );
        assert_eq!(
            apply(
                body,
                &rename_property(body, &found, 1, "b2").expect("an edit")
            ),
            "a: 1\nb2:\n  c: 2\nd: 3"
        );
        assert_eq!(
            add_property(body, &found, "a"),
            Err(EditError::DuplicateName("a".into()))
        );
        assert_eq!(
            rename_property(body, &found, 1, "d"),
            Err(EditError::DuplicateName("d".into()))
        );
        assert!(rename_property(body, &found, 1, "b").is_ok());
        assert_eq!(add_property(body, &found, " "), Err(EditError::EmptyName));
        assert_eq!(
            add_property(body, &found, "a\nb"),
            Err(EditError::MultipleLines)
        );
    }

    /// The text of every property but `edited`, which edits must leave as it was
    /// written.
    fn untouched_rows(body: &str, edited: Option<usize>) -> Vec<String> {
        properties(body)
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != edited)
            .map(|(_, property)| {
                body.get(property.row_range.clone())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn test_removing_a_property_keeps_the_comment_after_it() {
        let body = "tags:\n  - a\n# Section two\nother: 1\n";
        let found = properties(body);

        assert_eq!(
            body.get(found[0].row_range.clone()),
            Some("tags:\n  - a"),
            "the comment is not part of the entry"
        );
        let edits = remove_property(body, &found, 0).expect("an edit");
        assert_eq!(apply(body, &edits), "# Section two\nother: 1\n");
    }

    #[test]
    fn test_an_edit_changes_only_the_property_it_is_for() {
        let bodies = [
            SAMPLE,
            "# top comment\na: 1   # trailing\n\n\nb: [x,y]\n  # indented comment\nc: 'q'\n",
            "a: 1\r\nb: [x, y]\r\nc:\r\n  - z\r\n",
        ];
        for body in bodies {
            let found = properties(body);
            for index in 0..found.len() {
                let mut candidates: Vec<Result<Vec<Edit>, EditError>> = Vec::new();
                candidates.push(set_value(body, &found, index, "changed"));
                candidates.push(set_value(body, &found, index, "42"));
                candidates.push(toggle_bool(body, &found, index));
                candidates.push(list_add(body, &found, index, "added"));
                candidates.push(list_remove(body, &found, index, 0));
                candidates.push(rename_property(body, &found, index, "renamed"));
                for edits in candidates.into_iter().flatten() {
                    let edited = apply(body, &edits);
                    assert_eq!(
                        untouched_rows(&edited, Some(index)),
                        untouched_rows(body, Some(index)),
                        "{edits:?} on {index} of {body:?}"
                    );
                    for (range, _) in &edits {
                        let row = &found[index].row_range;
                        let line = line_range(body, row);
                        assert!(
                            line.start <= range.start && range.end <= line.end,
                            "{edits:?} reaches outside the lines of property {index} of {body:?}"
                        );
                    }
                }
            }
            let edits = add_property(body, &found, "appended").expect("an edit");
            let edited = apply(body, &edits);
            assert!(edited.starts_with(body.trim_end_matches(['\r', '\n'])));
            assert_eq!(
                untouched_rows(&edited, None)[..found.len()],
                untouched_rows(body, None)[..]
            );
        }
    }

    /// A small deterministic generator, so that a failure can be reproduced.
    struct Random(u64);

    impl Random {
        fn below(&mut self, limit: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % limit.max(1)
        }

        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items
                .get(self.below(items.len()))
                .copied()
                .unwrap_or_default()
        }
    }

    #[test]
    fn test_random_front_matter_and_random_edits_change_only_what_they_name() {
        let keys = [
            "title",
            "tags",
            "aliases",
            "done",
            "count",
            "date",
            "plain key",
            "ünï",
        ];
        let values = [
            "text",
            "\"quoted: text\"",
            "'single'",
            "true",
            "12",
            "3.5",
            "2026-10-08",
            "2026-10-08T09:30",
            "[a, b]",
            "[]",
            "[\"x, y\"]",
            "&a anchored",
            "{k: v}",
            "",
            "# not a comment",
            "|\n  block\n  lines",
        ];
        let words = [
            "word",
            "two words",
            "colon: here",
            "yes",
            "007",
            "#tag",
            "- dash",
            "ünï",
            "say \"hi\"",
            "[x]",
            "",
        ];
        let mut checked_edits = 0;
        for seed in 0..300u64 {
            let mut random = Random(seed.wrapping_mul(0x9E3779B97F4A7C15) ^ 0xABCDEF);
            let mut body = String::new();
            let count = random.below(6);
            for position in 0..count {
                if random.below(5) == 0 {
                    body.push_str("# a comment\n");
                }
                let key = format!("{}{position}", random.pick(&keys));
                let value = random.pick(&values);
                body.push_str(&format!("{key}:"));
                if value.is_empty() {
                    body.push('\n');
                } else if value.starts_with('|') {
                    body.push_str(&format!(" {value}\n"));
                } else if random.below(4) == 0 && value.starts_with('[') {
                    body.push_str("\n  - one\n  - two\n");
                } else {
                    body.push_str(&format!(" {value}\n"));
                }
            }
            let Some(found) = parse(&body) else {
                continue;
            };
            if found.is_empty() {
                continue;
            }
            let index = random.below(found.len());
            let typed = random.pick(&words);
            let attempts = [
                set_value(&body, &found, index, typed),
                toggle_bool(&body, &found, index),
                list_add(&body, &found, index, typed),
                list_remove(&body, &found, index, random.below(3)),
                rename_property(&body, &found, index, typed),
                remove_property(&body, &found, index),
            ];
            for (attempt, result) in attempts.into_iter().enumerate() {
                let Ok(edits) = result else { continue };
                checked_edits += 1;
                let edited = apply(&body, &edits);
                let Some(after) = parse(&edited) else {
                    panic!(
                        "seed {seed} attempt {attempt}: {body:?} became unparseable as {edited:?}"
                    );
                };
                let removed = attempt == 5;
                let expected_count = if removed {
                    found.len() - 1
                } else {
                    found.len()
                };
                assert_eq!(
                    after.len(),
                    expected_count,
                    "seed {seed} attempt {attempt}: {body:?}"
                );
                assert_eq!(
                    untouched_rows(&edited, (!removed).then_some(index)),
                    if removed {
                        let mut rows = untouched_rows(&body, None);
                        rows.remove(index);
                        rows
                    } else {
                        untouched_rows(&body, Some(index))
                    },
                    "seed {seed} attempt {attempt}: {body:?} -> {edited:?}"
                );
            }
        }
        assert!(checked_edits > 300, "only {checked_edits} edits were made");
    }
}
