//! What can be folded in a Markdown document: a heading folds its section and
//! a list item folds what is nested under it.
//!
//! Like the rest of the planning code this works on text and the block parse,
//! not on an editor. A range starts where the heading's or item's first line
//! ends, so that line stays on screen with a "⋯" after it, and stops after the
//! last character of the section, so the newline after it and the blank lines
//! before whatever follows stay too.

use std::ops::Range;

use tree_sitter::{Node, Tree};

/// The foldable ranges of `text`, whose block structure is `block_tree`, in
/// document order. A range inside another one comes after it.
pub fn foldable_ranges(text: &str, block_tree: &Tree) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut pending = vec![block_tree.root_node()];
    while let Some(node) = pending.pop() {
        let range = match node.kind() {
            "section" => section_range(text, node),
            "list_item" => item_range(text, node),
            _ => None,
        };
        ranges.extend(range);

        let mut cursor = node.walk();
        // Pushed in reverse, so that they are taken in document order.
        let children: Vec<Node> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    ranges.sort_by_key(|range| (range.start, std::cmp::Reverse(range.end)));
    ranges
}

fn section_range(text: &str, section: Node) -> Option<Range<usize>> {
    let heading = section
        .child(0)
        .filter(|child| child.kind() == "atx_heading")?;
    let start = first_line_end(text, heading.start_byte())?;
    let end = trimmed_end(text, section.start_byte(), section.end_byte());
    (end > start).then_some(start..end)
}

fn item_range(text: &str, item: Node) -> Option<Range<usize>> {
    let mut cursor = item.walk();
    let mut paragraphs = 0;
    let has_nested_content = item.children(&mut cursor).any(|child| match child.kind() {
        "paragraph" => {
            paragraphs += 1;
            paragraphs > 1
        }
        "list"
        | "fenced_code_block"
        | "indented_code_block"
        | "block_quote"
        | "html_block"
        | "pipe_table" => true,
        _ => false,
    });
    if !has_nested_content {
        return None;
    }

    let start = first_line_end(text, item.start_byte())?;
    let end = trimmed_end(text, item.start_byte(), item.end_byte());
    (end > start).then_some(start..end)
}

/// Where the line `from` is on ends, trailing spaces included, so that typing
/// them at the end of a heading does not move where its fold starts. `None` for
/// the last line, which has nothing after it to fold.
fn first_line_end(text: &str, from: usize) -> Option<usize> {
    text.get(from..)?.find('\n').map(|newline| from + newline)
}

/// The end of `text[start..end]` without the whitespace after its last
/// character, as an offset into `text`.
fn trimmed_end(text: &str, start: usize, end: usize) -> usize {
    let end = end.min(text.len());
    text.get(start..end)
        .map_or(end, |content| start + content.trim_end().len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::parse_blocks;

    fn folded_text(text: &str) -> Vec<(String, String)> {
        let tree = parse_blocks(text).expect("the text parses");
        foldable_ranges(text, &tree)
            .into_iter()
            .map(|range| {
                let line_start = text[..range.start].rfind('\n').map_or(0, |index| index + 1);
                (
                    text[line_start..range.start].to_string(),
                    text[range].to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn test_a_heading_folds_up_to_the_next_heading_of_its_level() {
        let text = "# One\nbody\n\n# Two\nmore\n";
        assert_eq!(
            folded_text(text),
            [
                ("# One".to_string(), "\nbody".to_string()),
                ("# Two".to_string(), "\nmore".to_string()),
            ]
        );
    }

    #[test]
    fn test_a_heading_folds_its_subsections_and_stops_at_a_heading_of_a_higher_level() {
        let text = "# One\n## Sub\ntext\n### Deep\ndeeper\n## Next\n# Two\nx\n";
        let ranges = folded_text(text);

        assert_eq!(
            ranges,
            [
                (
                    "# One".to_string(),
                    "\n## Sub\ntext\n### Deep\ndeeper\n## Next".to_string()
                ),
                ("## Sub".to_string(), "\ntext\n### Deep\ndeeper".to_string()),
                ("### Deep".to_string(), "\ndeeper".to_string()),
                ("# Two".to_string(), "\nx".to_string()),
            ]
        );
    }

    #[test]
    fn test_a_heading_with_nothing_under_it_does_not_fold() {
        assert_eq!(folded_text("# One\n# Two\n"), []);
        assert_eq!(folded_text("# Only\n"), []);
        assert_eq!(folded_text("# Only"), []);
    }

    #[test]
    fn test_trailing_spaces_on_a_heading_line_stay_before_the_fold() {
        let text = "# One  \nbody\n";
        let tree = parse_blocks(text).expect("parses");
        let range = foldable_ranges(text, &tree).remove(0);
        assert_eq!(&text[..range.start], "# One  ");
    }

    #[test]
    fn test_blank_lines_before_the_next_heading_stay_out_of_the_fold() {
        let text = "# One\ntext\n\n\n# Two\ntext\n";
        let one = &foldable_ranges(text, &parse_blocks(text).expect("parses"))[0];
        assert_eq!(&text[one.clone()], "\ntext");
    }

    #[test]
    fn test_a_hash_in_a_code_block_is_not_a_heading() {
        let text = "# One\n```\n# not a heading\n```\n";
        assert_eq!(
            folded_text(text),
            [(
                "# One".to_string(),
                "\n```\n# not a heading\n```".to_string()
            )]
        );
    }

    #[test]
    fn test_a_list_item_folds_what_is_nested_under_it() {
        let text = "- one\n  - two\n    - three\n- four\n";
        assert_eq!(
            folded_text(text),
            [
                ("- one".to_string(), "\n  - two\n    - three".to_string()),
                ("  - two".to_string(), "\n    - three".to_string()),
            ]
        );
    }

    #[test]
    fn test_an_item_with_a_wrapped_line_and_nothing_nested_does_not_fold() {
        assert_eq!(folded_text("- one\n  continued\n- two\n"), []);
    }

    #[test]
    fn test_an_item_with_a_second_paragraph_or_a_code_block_folds() {
        let text = "- one\n\n  second paragraph\n- two\n  ```\n  code\n  ```\n";
        assert_eq!(
            folded_text(text),
            [
                ("- one".to_string(), "\n\n  second paragraph".to_string()),
                ("- two".to_string(), "\n  ```\n  code\n  ```".to_string()),
            ]
        );
    }

    #[test]
    fn test_ordered_and_task_items_fold_from_the_end_of_their_first_line() {
        let text = "1. one\n   - [ ] task\n2. two\n";
        assert_eq!(
            folded_text(text),
            [("1. one".to_string(), "\n   - [ ] task".to_string())]
        );
    }

    #[test]
    fn test_a_list_under_a_heading_is_foldable_inside_the_heading() {
        let text = "# Title\n- one\n  - two\n";
        assert_eq!(
            folded_text(text),
            [
                ("# Title".to_string(), "\n- one\n  - two".to_string()),
                ("- one".to_string(), "\n  - two".to_string()),
            ]
        );
    }

    #[test]
    fn test_multibyte_text_gives_ranges_on_character_boundaries() {
        let text = "# Überschrift ✓\nkörper 😀\n- ä\n  - ö\n";
        let tree = parse_blocks(text).expect("parses");
        for range in foldable_ranges(text, &tree) {
            assert!(text.is_char_boundary(range.start));
            assert!(text.is_char_boundary(range.end));
        }
        assert_eq!(foldable_ranges(text, &tree).len(), 2);
    }

    #[test]
    fn test_an_empty_document_has_nothing_to_fold() {
        assert_eq!(folded_text(""), []);
        assert_eq!(folded_text("just text\n"), []);
    }
}
