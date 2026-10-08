//! Footnotes: `[^label]` references and the `[^label]: text` definitions they
//! point to.
//!
//! The Markdown grammar knows nothing of them. A reference parses as a link
//! whose text is `^label`, and a definition as a paragraph, or as a link
//! reference definition when its text is one word. So they are found in the
//! text here, outside code and front matter, once for each parse of a document.
//! A reference is numbered by the order in which labels are first referred to.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use tree_sitter::Tree;

use crate::note_contents;

/// A `[^label]` that has a definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub label: String,
    pub range: Range<usize>,
    pub number: usize,
}

/// A `[^label]: text` at the start of a line, with the lines that continue it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub label: String,
    /// The `[^label]:`.
    pub marker: Range<usize>,
    /// Where its text starts: after the marker and the spaces that follow it.
    pub text_start: usize,
    /// The text as Markdown, without the indentation of continuation lines.
    pub text: String,
    /// The number of its label. A definition nothing refers to has none, and
    /// neither has one that repeats the label of an earlier definition.
    pub number: Option<usize>,
}

/// The footnotes of a document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Footnotes {
    /// In order of position. A reference to a label nothing defines is not here.
    pub references: Vec<Reference>,
    pub definitions: Vec<Definition>,
}

struct Line<'a> {
    start: usize,
    content: &'a str,
}

impl Footnotes {
    /// The footnotes of `text`, whose block parse is `tree` when the caller has
    /// it. What is in code or in front matter is not a footnote.
    pub fn scan(text: &str, tree: Option<&Tree>) -> Self {
        if !text.contains("[^") {
            return Self::default();
        }

        let mut skipped = note_contents::code_ranges_in(text, tree);
        if let Some(tree) = tree {
            let front_matter_end = note_contents::front_matter_end(tree);
            if front_matter_end > 0 {
                skipped.push(0..front_matter_end);
            }
        }
        let skipped = merged(skipped);

        let lines = lines_of(text);
        let mut definitions = Vec::new();
        let mut index = 0;
        while index < lines.len() {
            match definition_at(&lines, index, &skipped) {
                Some((definition, next)) => {
                    definitions.push(definition);
                    index = next;
                }
                None => index += 1,
            }
        }

        let marker_starts: HashSet<usize> = definitions
            .iter()
            .map(|definition| definition.marker.start)
            .collect();
        let mut first_definition: HashMap<String, usize> = HashMap::new();
        for (index, definition) in definitions.iter().enumerate() {
            first_definition
                .entry(normalized(&definition.label))
                .or_insert(index);
        }

        let mut numbers: HashMap<String, usize> = HashMap::new();
        let mut references = Vec::new();
        for (start, _) in text.match_indices("[^") {
            if marker_starts.contains(&start) || in_ranges(&skipped, start) {
                continue;
            }
            let Some((label, end)) = reference_at_start(text, start) else {
                continue;
            };
            let key = normalized(label);
            if !first_definition.contains_key(&key) {
                continue;
            }
            let next_number = numbers.len() + 1;
            let number = *numbers.entry(key).or_insert(next_number);
            references.push(Reference {
                label: label.to_string(),
                range: start..end,
                number,
            });
        }

        for (key, index) in &first_definition {
            if let (Some(number), Some(definition)) =
                (numbers.get(key), definitions.get_mut(*index))
            {
                definition.number = Some(*number);
            }
        }

        Self {
            references,
            definitions,
        }
    }

    /// The reference that covers `offset`.
    pub fn reference_at(&self, offset: usize) -> Option<&Reference> {
        let index = self
            .references
            .partition_point(|reference| reference.range.end <= offset);
        self.references
            .get(index)
            .filter(|reference| reference.range.start <= offset)
    }

    /// The definition of `label`, which is the first one when it has several.
    pub fn definition_of(&self, label: &str) -> Option<&Definition> {
        let key = normalized(label);
        self.definitions
            .iter()
            .find(|definition| normalized(&definition.label) == key)
    }
}

/// Labels match without regard to case, as the labels of links do.
fn normalized(label: &str) -> String {
    label.to_lowercase()
}

fn is_label(label: &str) -> bool {
    !label.is_empty()
        && !label
            .chars()
            .any(|character| character.is_whitespace() || character == '[' || character == ']')
}

/// The label and end of the `[^label]` that starts at `start`, when it is a
/// reference and not a link, an escaped bracket or part of a wikilink.
fn reference_at_start(text: &str, start: usize) -> Option<(&str, usize)> {
    let after = text.get(start + 2..)?;
    let line = after.split('\n').next()?;
    let close = line.find(']')?;
    let label = line.get(..close)?;
    if !is_label(label) {
        return None;
    }
    let end = start + 2 + close + 1;
    let before = text.get(..start)?.chars().next_back();
    if matches!(before, Some('\\' | '[')) || text.get(end..)?.starts_with('(') {
        return None;
    }
    Some((label, end))
}

fn lines_of(text: &str) -> Vec<Line<'_>> {
    let mut start = 0;
    text.split_inclusive('\n')
        .map(|raw| {
            let line = Line {
                start,
                content: raw.trim_end_matches(['\n', '\r']),
            };
            start += raw.len();
            line
        })
        .collect()
}

/// The definition that starts on line `index`, and the index of the first line
/// after it.
fn definition_at(
    lines: &[Line],
    index: usize,
    skipped: &[Range<usize>],
) -> Option<(Definition, usize)> {
    let line = lines.get(index)?;
    let indent = line.content.len() - line.content.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let inner = line.content.get(indent..)?.strip_prefix("[^")?;
    let close = inner.find(']')?;
    let label = inner.get(..close)?;
    if !is_label(label) {
        return None;
    }
    let after_marker = inner.get(close + 1..)?.strip_prefix(':')?;
    let marker_start = line.start + indent;
    if in_ranges(skipped, marker_start) {
        return None;
    }
    let marker_end = marker_start + 2 + close + 2;

    let first = after_marker.trim_start_matches([' ', '\t']);
    let text_start = marker_end + (after_marker.len() - first.len());
    let mut parts = vec![first.to_string()];
    let mut next = index + 1;
    let mut blank_lines = 0;
    while let Some(candidate) = lines.get(next) {
        if candidate.content.trim().is_empty() {
            blank_lines += 1;
            next += 1;
            continue;
        }
        let indented = candidate.content.starts_with('\t') || candidate.content.starts_with("    ");
        // A line that is not indented continues the paragraph only when no
        // blank line came before it and it does not begin a block of its own.
        if !indented && (blank_lines > 0 || starts_block(candidate.content)) {
            break;
        }
        parts.extend(std::iter::repeat_n(String::new(), blank_lines));
        blank_lines = 0;
        parts.push(dedented(candidate.content).to_string());
        next += 1;
    }

    let definition = Definition {
        label: label.to_string(),
        marker: marker_start..marker_end,
        text_start,
        text: parts.join("\n").trim().to_string(),
        number: None,
    };
    Some((definition, next))
}

fn dedented(line: &str) -> &str {
    if let Some(rest) = line.strip_prefix('\t') {
        return rest;
    }
    let spaces = line.len() - line.trim_start_matches(' ').len();
    line.get(spaces.min(4)..).unwrap_or(line)
}

/// Whether a line that is not indented begins something other than the
/// continuation of a paragraph.
fn starts_block(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with(['#', '>']) || trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        return true;
    }
    if is_list_marker(trimmed) || is_thematic_break(trimmed) {
        return true;
    }
    trimmed.strip_prefix("[^").is_some_and(|rest| {
        rest.find(']').is_some_and(|close| {
            rest.get(close + 1..)
                .is_some_and(|after| after.starts_with(':'))
        })
    })
}

fn is_list_marker(line: &str) -> bool {
    let mut characters = line.chars();
    match characters.next() {
        Some('-' | '*' | '+') => matches!(characters.next(), None | Some(' ' | '\t')),
        Some(digit) if digit.is_ascii_digit() => {
            let rest = line.trim_start_matches(|character: char| character.is_ascii_digit());
            let mut rest = rest.chars();
            matches!(rest.next(), Some('.' | ')')) && matches!(rest.next(), None | Some(' ' | '\t'))
        }
        _ => false,
    }
}

fn is_thematic_break(line: &str) -> bool {
    let mut marks = line.chars().filter(|character| !character.is_whitespace());
    let Some(first) = marks
        .next()
        .filter(|first| matches!(first, '-' | '*' | '_'))
    else {
        return false;
    };
    let mut count = 1;
    for mark in marks {
        if mark != first {
            return false;
        }
        count += 1;
    }
    count >= 3
}

/// `ranges` as disjoint ranges in order.
fn merged(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|range| range.start);
    let mut result: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        match result.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => result.push(range),
        }
    }
    result
}

/// Whether `offset` is in one of `ranges`, which are disjoint and in order.
fn in_ranges(ranges: &[Range<usize>], offset: usize) -> bool {
    let index = ranges.partition_point(|range| range.end <= offset);
    ranges.get(index).is_some_and(|range| range.start <= offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::parse_blocks;

    fn scan(text: &str) -> Footnotes {
        Footnotes::scan(text, parse_blocks(text).as_ref())
    }

    fn numbered(footnotes: &Footnotes, text: &str) -> Vec<(String, usize)> {
        footnotes
            .references
            .iter()
            .map(|reference| {
                (
                    text.get(reference.range.clone())
                        .unwrap_or_default()
                        .to_string(),
                    reference.number,
                )
            })
            .collect()
    }

    #[test]
    fn test_numbers_follow_the_order_of_first_reference() {
        let text = "one[^b] two[^a] again[^b]\n\n[^a]: Alpha.\n[^b]: Beta.\n";
        let footnotes = scan(text);

        assert_eq!(
            numbered(&footnotes, text),
            vec![
                ("[^b]".to_string(), 1),
                ("[^a]".to_string(), 2),
                ("[^b]".to_string(), 1)
            ]
        );
        let number_of = |label: &str| {
            footnotes
                .definition_of(label)
                .and_then(|definition| definition.number)
        };
        assert_eq!(number_of("a"), Some(2));
        assert_eq!(number_of("b"), Some(1));
    }

    #[test]
    fn test_a_definition_has_its_marker_and_text() {
        let text = "a[^1]\n\n[^1]:   The note.\n";
        let footnotes = scan(text);
        let definition = footnotes.definition_of("1").expect("a definition");

        assert_eq!(
            text.get(definition.marker.clone()).unwrap_or_default(),
            "[^1]:"
        );
        assert_eq!(
            text.get(definition.text_start..).unwrap_or_default(),
            "The note.\n"
        );
        assert_eq!(definition.text, "The note.");
    }

    #[test]
    fn test_a_reference_with_no_definition_is_text() {
        let text = "a[^1] and b[^2]\n\n[^2]: Two.\n";
        let footnotes = scan(text);

        assert_eq!(
            numbered(&footnotes, text),
            vec![("[^2]".to_string(), 1)],
            "the first label is undefined, so it takes no number"
        );
    }

    #[test]
    fn test_a_definition_nothing_refers_to_has_no_number() {
        let footnotes = scan("text\n\n[^1]: Lonely.\n");

        assert!(footnotes.references.is_empty());
        assert_eq!(footnotes.definition_of("1").and_then(|d| d.number), None);
    }

    #[test]
    fn test_the_first_of_two_definitions_of_a_label_counts() {
        let text = "a[^1]\n\n[^1]: First.\n\n[^1]: Second.\n";
        let footnotes = scan(text);

        assert_eq!(footnotes.definitions.len(), 2);
        assert_eq!(
            footnotes
                .definitions
                .iter()
                .map(|definition| definition.number)
                .collect::<Vec<_>>(),
            vec![Some(1), None]
        );
        assert_eq!(
            footnotes.definition_of("1").map(|d| d.text.as_str()),
            Some("First.")
        );
    }

    #[test]
    fn test_labels_match_without_regard_to_case() {
        let text = "a[^Note]\n\n[^note]: Text.\n";
        let footnotes = scan(text);

        assert_eq!(numbered(&footnotes, text), vec![("[^Note]".to_string(), 1)]);
    }

    #[test]
    fn test_code_and_front_matter_hold_no_footnotes() {
        let text = "---\nref: [^1]\n---\n\nIn `code[^1]` and\n\n```\nfenced[^1]\n[^1]: nope\n```\n\n    indented[^1]\n\nreal[^1]\n\n[^1]: Yes.\n";
        let footnotes = scan(text);

        let real = text.rfind("real[^1]").map(|at| at + 4).unwrap_or(0);
        assert_eq!(
            footnotes
                .references
                .iter()
                .map(|reference| reference.range.start)
                .collect::<Vec<_>>(),
            vec![real]
        );
        assert_eq!(footnotes.definitions.len(), 1);
        assert_eq!(
            footnotes.definition_of("1").map(|d| d.text.as_str()),
            Some("Yes.")
        );
    }

    #[test]
    fn test_brackets_that_are_something_else_are_not_references() {
        let text = "esc \\[^1] wiki [[^1]] link [^1](http://x.org) space [^a b] empty [^] real[^1]\n\n[^1]: One.\n";
        let footnotes = scan(text);

        let real = text.find("real[^1]").map(|at| at + 4).unwrap_or(0);
        assert_eq!(
            footnotes
                .references
                .iter()
                .map(|reference| reference.range.start)
                .collect::<Vec<_>>(),
            vec![real]
        );
    }

    #[test]
    fn test_an_indented_marker_is_not_a_definition() {
        for text in [
            "a[^1]\n\n    [^1]: code\n",
            "a[^1]\n\n- item\n\n    [^1]: inside the item\n",
        ] {
            assert!(scan(text).definitions.is_empty(), "{text:?}");
        }
    }

    #[test]
    fn test_a_definition_continues_on_indented_and_lazy_lines() {
        let text = "a[^1]\n\n[^1]: First line\ncontinues here\n    and here.\n\n    Second paragraph.\n\nAfter.\n";
        let footnotes = scan(text);

        assert_eq!(
            footnotes.definition_of("1").map(|d| d.text.as_str()),
            Some("First line\ncontinues here\nand here.\n\nSecond paragraph.")
        );
    }

    #[test]
    fn test_a_definition_ends_where_another_block_begins() {
        for next in [
            "# Heading",
            "- item",
            "1. item",
            "> quote",
            "```",
            "---",
            "[^2]: Two.",
        ] {
            let text = format!("a[^1] b[^2]\n\n[^1]: One\n{next}\n");
            let footnotes = scan(&text);

            assert_eq!(
                footnotes.definition_of("1").map(|d| d.text.as_str()),
                Some("One"),
                "before {next:?}"
            );
        }
    }

    #[test]
    fn test_a_definition_whose_text_starts_on_the_next_line() {
        let text = "a[^1]\n\n[^1]:\n    Indented text.\n";
        let footnotes = scan(text);
        let definition = footnotes.definition_of("1").expect("a definition");

        assert_eq!(definition.text, "Indented text.");
        assert_eq!(definition.text_start, definition.marker.end);
    }

    #[test]
    fn test_offsets_are_bytes_after_multibyte_text() {
        let text = "é ü ñ[^1] — ok\n\n[^1]: Ünï.\r\nnext\r\n";
        let footnotes = scan(text);
        let reference = footnotes.references.first().expect("a reference");

        assert_eq!(
            text.get(reference.range.clone()).unwrap_or_default(),
            "[^1]"
        );
        assert_eq!(
            footnotes.definition_of("1").map(|d| d.text.as_str()),
            Some("Ünï.\nnext")
        );
    }

    #[test]
    fn test_a_reference_is_found_by_the_offsets_it_covers() {
        let text = "ab[^1]cd\n\n[^1]: x\n";
        let footnotes = scan(text);

        assert!(footnotes.reference_at(1).is_none());
        for offset in 2..6 {
            assert!(footnotes.reference_at(offset).is_some(), "at {offset}");
        }
        assert!(footnotes.reference_at(6).is_none());
    }

    #[test]
    fn test_text_without_footnotes_has_none() {
        assert_eq!(scan("plain [link](x) and [[wiki]]\n"), Footnotes::default());
        assert_eq!(scan(""), Footnotes::default());
    }
}
