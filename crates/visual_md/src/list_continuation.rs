//! Pure "smart list continuation" logic for visual_md: what a plain `Enter`
//! keypress should actually do when the cursor sits on a list item's own
//! marker line, per `docs/visual-md-spec.md`'s "Smart list continuation"
//! bullet.
//!
//! Like [`crate::plan`], this has no GPUI/`Editor` dependency: it takes raw
//! buffer text plus a cursor byte offset and returns an edit description (or
//! `None` to mean "do nothing special, let the caller fall back to a plain
//! newline").

use std::ops::Range;

use tree_sitter::{Node, Parser};

use crate::plan::{ORDERED_MARKERS, UNORDERED_MARKERS};

const TASK_MARKERS: [&str; 2] = ["task_list_marker_checked", "task_list_marker_unchecked"];

/// The edit to apply in place of a plain newline insertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListNewline {
    /// The byte range in the original text to replace.
    pub replace: Range<usize>,
    /// The text to replace it with.
    pub insert: String,
    /// Where the cursor should land afterwards, as a byte offset into the
    /// *new* text (i.e. already accounting for `replace`/`insert` length
    /// changes relative to the original document).
    pub cursor_after: usize,
}

/// What pressing Enter does with several cursors at once: every cursor's
/// edit, and where every cursor ends up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewlineBatch {
    /// The edits to apply together in one buffer edit. Every range is in the
    /// coordinates of the original text, not shifted by the edits before it:
    /// the buffer walks the original text once and combines the batch itself,
    /// so shifting a range here would count that edit's length change twice.
    pub edits: Vec<(Range<usize>, String)>,
    /// Where each cursor lands, as a byte offset into the text *after* the
    /// whole batch. Unlike the edit ranges, these do need every earlier edit's
    /// length change, since that is a real shift in the final text.
    pub cursors: Vec<usize>,
}

/// Computes the Enter-key edits for collapsed cursors at the byte offsets
/// `cursors`, which must be in document order. Returns `None` if any cursor is
/// not on a list item's own marker line (so the caller should insert a plain
/// newline for all of them), or if two edits would overlap, which no single
/// buffer edit can express.
pub fn newline_batch(text: &str, cursors: &[usize]) -> Option<NewlineBatch> {
    let mut batch = NewlineBatch {
        edits: Vec::with_capacity(cursors.len()),
        cursors: Vec::with_capacity(cursors.len()),
    };
    let mut delta: isize = 0;
    let mut previous_end = 0;
    for &cursor in cursors {
        let edit = newline_edit(text, cursor)?;
        if edit.replace.start < previous_end {
            return None;
        }
        previous_end = edit.replace.end;

        let cursor_after =
            usize::try_from(isize::try_from(edit.cursor_after).ok()? + delta).ok()?;
        delta +=
            isize::try_from(edit.insert.len()).ok()? - isize::try_from(edit.replace.len()).ok()?;
        batch.edits.push((edit.replace, edit.insert));
        batch.cursors.push(cursor_after);
    }
    Some(batch)
}

/// A list item's own marker, as found among its *direct* children (so a
/// nested list's markers, being children of a nested `list_item` several
/// levels down, are never picked up here).
struct ItemMarker {
    /// Byte range of the `list_marker_*` node (bullet or ordinal; already
    /// includes its trailing whitespace per the grammar).
    marker: Range<usize>,
    ordered: bool,
    /// Byte range of a `task_list_marker_*` sibling, if this item is a task
    /// item (`- [ ] ...`).
    task: Option<Range<usize>>,
}

/// Computes the Enter-key edit for `text` with a single collapsed cursor at
/// byte offset `cursor`, or `None` if the cursor isn't on a list item's own
/// marker line (so the caller should just insert a plain newline).
pub fn newline_edit(text: &str, cursor: usize) -> Option<ListNewline> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_md::LANGUAGE.into()).ok()?;
    let tree = parser.parse(text, None)?;

    let item = enclosing_list_item(tree.root_node(), cursor)?;
    let item_marker = own_marker(item)?;

    let cursor_line_start = line_start(text, cursor);
    let line_end = line_end(text, cursor);

    // Only continue when the cursor's current line is literally the line the
    // item's own marker sits on -- a wrapped continuation line, or a later
    // blank line inside a multi-paragraph item, should just get a plain
    // newline instead of re-triggering continuation partway through content.
    if line_start(text, item_marker.marker.start) != cursor_line_start {
        return None;
    }
    let line_start = cursor_line_start;

    let content_start_min = content_start_after(&item_marker, text);
    if cursor < content_start_min {
        return None;
    }

    let prefix = text.get(line_start..item_marker.marker.start)?;
    if !prefix.chars().all(|c| c == ' ' || c == '\t' || c == '>') {
        return None;
    }

    let mut content_start = content_start_min;
    let mut is_empty = text.get(content_start..line_end)?.trim().is_empty();

    // tree-sitter-md doesn't parse an empty checkbox (`- [ ]` with nothing
    // after it) as a real `task_list_marker_*` node -- it falls back to
    // treating `[ ]`/`[x]` as ordinary paragraph text, since the grammar's
    // task-marker rule only fires when there's following content on the
    // same line. Detect that shape textually instead: if nothing but a
    // bracket pair follows the bullet/ordinal marker on this line, treat it
    // exactly like an empty (non-task) item -- clearing/outdenting removes
    // the whole `- [ ] ` (or `- [x] `) prefix either way.
    if !is_empty && item_marker.task.is_none() {
        let rest = text.get(item_marker.marker.end..line_end)?.trim();
        if matches!(rest, "[ ]" | "[x]" | "[X]") {
            content_start = line_end;
            is_empty = true;
        }
    }

    if !is_empty {
        let mut insert = String::from("\n");
        insert.push_str(prefix);
        insert.push_str(&next_marker_text(&item_marker, text));
        let cursor_after = cursor + insert.len();
        return Some(ListNewline {
            replace: cursor..cursor,
            insert,
            cursor_after,
        });
    }

    // Empty item: outdent to the parent list level if there is one, or clear
    // the marker entirely (exiting the list) if this is a top-level item.
    match parent_list_item(item) {
        Some(parent) => {
            let Some(parent_marker) = own_marker(parent) else {
                return Some(clear_marker(line_start, content_start));
            };
            let parent_line_start = self::line_start(text, parent_marker.marker.start);
            let Some(parent_prefix) = text.get(parent_line_start..parent_marker.marker.start)
            else {
                return Some(clear_marker(line_start, content_start));
            };
            let mut insert = parent_prefix.to_string();
            insert.push_str(&next_marker_text(&parent_marker, text));
            let cursor_after = line_start + insert.len();
            Some(ListNewline {
                replace: line_start..content_start,
                insert,
                cursor_after,
            })
        }
        None => Some(clear_marker(line_start, content_start)),
    }
}

fn clear_marker(line_start: usize, content_start: usize) -> ListNewline {
    ListNewline {
        replace: line_start..content_start,
        insert: String::new(),
        cursor_after: line_start,
    }
}

/// Walks down to the deepest `list_item` node whose byte range contains
/// `cursor`.
fn enclosing_list_item(root: Node, cursor: usize) -> Option<Node> {
    let mut node = root.descendant_for_byte_range(cursor, cursor)?;
    loop {
        if node.kind() == "list_item" {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// The `list_item` one level up from `item`, if any (i.e. `item`'s `list`
/// parent is itself inside another `list_item`, not a top-level list).
fn parent_list_item(item: Node) -> Option<Node> {
    let list = item.parent()?;
    debug_assert_eq!(list.kind(), "list");
    let maybe_item = list.parent()?;
    (maybe_item.kind() == "list_item").then_some(maybe_item)
}

/// Finds `item`'s own bullet/ordinal marker (and task marker, if any) among
/// its direct children only -- deliberately not a descendant search, so a
/// nested sub-list's markers (children of a nested `list_item`) are never
/// mistaken for this item's own.
fn own_marker(item: Node) -> Option<ItemMarker> {
    let mut cursor = item.walk();
    let mut marker = None;
    let mut task = None;
    for child in item.children(&mut cursor) {
        if UNORDERED_MARKERS.contains(&child.kind()) {
            marker = Some((child.byte_range(), false));
        } else if ORDERED_MARKERS.contains(&child.kind()) {
            marker = Some((child.byte_range(), true));
        } else if TASK_MARKERS.contains(&child.kind()) {
            task = Some(child.byte_range());
        }
    }
    let (marker, ordered) = marker?;
    Some(ItemMarker {
        marker,
        ordered,
        task,
    })
}

/// The byte offset where this item's actual content begins: right after the
/// marker, and after the task checkbox (plus its single separating space)
/// when present.
fn content_start_after(item_marker: &ItemMarker, text: &str) -> usize {
    match &item_marker.task {
        Some(task) => {
            let after_task = task.end;
            if text.as_bytes().get(after_task) == Some(&b' ') {
                after_task + 1
            } else {
                after_task
            }
        }
        None => item_marker.marker.end,
    }
}

/// The marker text to use for a new sibling item continuing `item_marker`:
/// the same bullet character, or the ordinal incremented by one -- always
/// followed by an unchecked `[ ] ` when `item_marker` was a task item (a
/// continued/outdented item should never inherit a checked state).
fn next_marker_text(item_marker: &ItemMarker, text: &str) -> String {
    let marker_text = &text[item_marker.marker.clone()];
    let mut result = if item_marker.ordered {
        let sep_index = marker_text
            .find(['.', ')'])
            .expect("ordered list marker always contains its separator");
        let (digits, rest) = marker_text.split_at(sep_index);
        let separator = &rest[..1];
        let trailing = &rest[1..];
        let n: u64 = digits.trim().parse().unwrap_or(1);
        format!("{}{separator}{trailing}", n + 1)
    } else {
        marker_text.to_string()
    };
    if item_marker.task.is_some() {
        result.push_str("[ ] ");
    }
    result
}

fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

fn line_end(text: &str, offset: usize) -> usize {
    text[offset..]
        .find('\n')
        .map(|i| offset + i)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies a batch the way the buffer does: every range is in the
    /// coordinates of `text`, so working back to front keeps them valid.
    fn apply_batch(text: &str, batch: &NewlineBatch) -> String {
        let mut new_text = text.to_string();
        for (range, insert) in batch.edits.iter().rev() {
            new_text.replace_range(range.clone(), insert);
        }
        new_text
    }

    fn apply(text: &str, cursor: usize) -> Option<(String, usize)> {
        let edit = newline_edit(text, cursor)?;
        let mut new_text = String::with_capacity(text.len() + edit.insert.len());
        new_text.push_str(&text[..edit.replace.start]);
        new_text.push_str(&edit.insert);
        new_text.push_str(&text[edit.replace.end..]);
        Some((new_text, edit.cursor_after))
    }

    #[test]
    fn continues_dash_bullet() {
        let text = "- one\n";
        let cursor = 5; // end of "one"
        let (new_text, cursor_after) = apply(text, cursor).unwrap();
        assert_eq!(new_text, "- one\n- \n");
        assert_eq!(&new_text[cursor_after..], "\n");
        assert_eq!(&new_text[..cursor_after], "- one\n- ");
    }

    #[test]
    fn continues_star_and_plus_bullets() {
        for marker in ["*", "+"] {
            let text = format!("{marker} one\n");
            let cursor = 5;
            let (new_text, _) = apply(&text, cursor).unwrap();
            assert_eq!(new_text, format!("{marker} one\n{marker} \n"));
        }
    }

    #[test]
    fn continues_ordered_dot_marker_incrementing() {
        let text = "1. one\n";
        let cursor = 6; // end of "one"
        let (new_text, _) = apply(text, cursor).unwrap();
        assert_eq!(new_text, "1. one\n2. \n");
    }

    #[test]
    fn continues_ordered_paren_marker_preserving_delimiter() {
        let text = "9) one\n";
        let cursor = 6;
        let (new_text, _) = apply(text, cursor).unwrap();
        assert_eq!(new_text, "9) one\n10) \n");
    }

    #[test]
    fn continues_unchecked_task_item() {
        let text = "- [ ] one\n";
        let cursor = 9; // end of "one"
        let (new_text, _) = apply(text, cursor).unwrap();
        assert_eq!(new_text, "- [ ] one\n- [ ] \n");
    }

    #[test]
    fn continuing_checked_task_item_produces_unchecked_next() {
        let text = "- [x] one\n";
        let cursor = 9;
        let (new_text, _) = apply(text, cursor).unwrap();
        assert_eq!(new_text, "- [x] one\n- [ ] \n");
    }

    #[test]
    fn continues_ordered_task_item() {
        let text = "1. [ ] one\n";
        let cursor = 10;
        let (new_text, _) = apply(text, cursor).unwrap();
        assert_eq!(new_text, "1. [ ] one\n2. [ ] \n");
    }

    #[test]
    fn splits_line_at_cursor_mid_content() {
        let text = "- one two\n";
        let cursor = 6; // right after "one "
        let (new_text, cursor_after) = apply(text, cursor).unwrap();
        assert_eq!(new_text, "- one \n- two\n");
        assert_eq!(&new_text[..cursor_after], "- one \n- ");
    }

    #[test]
    fn continues_nested_item_with_its_own_indent() {
        let text = "- one\n  - nested\n";
        let cursor = text.find("nested").unwrap() + "nested".len();
        let (new_text, _) = apply(text, cursor).unwrap();
        assert_eq!(new_text, "- one\n  - nested\n  - \n");
    }

    #[test]
    fn cursor_inside_marker_returns_none() {
        let text = "- one\n";
        assert!(newline_edit(text, 1).is_none());
    }

    #[test]
    fn non_list_line_returns_none() {
        let text = "just a paragraph\n";
        assert!(newline_edit(text, 5).is_none());
    }

    #[test]
    fn wrapped_continuation_line_returns_none() {
        // Cursor on the second physical line of a single multi-line list
        // item's paragraph (a soft-wrapped continuation, not the item's own
        // marker line).
        let text = "- top\n  more text\n";
        let cursor = text.find("more").unwrap() + 4;
        assert!(newline_edit(text, cursor).is_none());
    }

    #[test]
    fn blank_line_inside_item_returns_none() {
        let text = "- top\n\n  more text\n";
        let cursor = 6; // the blank line
        assert!(newline_edit(text, cursor).is_none());
    }

    #[test]
    fn inside_fenced_code_block_returns_none() {
        let text = "```\n- foo\n```\n";
        let cursor = text.find("foo").unwrap() + 3;
        assert!(newline_edit(text, cursor).is_none());
    }

    #[test]
    fn blockquoted_list_keeps_quote_prefix() {
        let text = "> - quoted item\n";
        let cursor = text.find("item").unwrap() + 4;
        let (new_text, _) = apply(text, cursor).unwrap();
        assert_eq!(new_text, "> - quoted item\n> - \n");
    }

    #[test]
    fn empty_top_level_item_clears_marker() {
        let text = "- one\n- \n";
        let cursor = text.rfind("- ").unwrap() + 2; // end of the empty item's marker
        let (new_text, cursor_after) = apply(text, cursor).unwrap();
        assert_eq!(new_text, "- one\n\n");
        assert_eq!(cursor_after, "- one\n".len());
    }

    #[test]
    fn empty_top_level_task_item_clears_marker() {
        let text = "- [ ] \n";
        let cursor = text.len() - 1;
        let (new_text, cursor_after) = apply(text, cursor).unwrap();
        assert_eq!(new_text, "\n");
        assert_eq!(cursor_after, 0);
    }

    #[test]
    fn empty_nested_item_outdents_to_parent_marker() {
        // A lone `  - ` right after the parent's own text line is ambiguous
        // with CommonMark's setext-heading-underline syntax and parses as
        // one instead of a nested list item (a real grammar quirk, verified
        // against tree-sitter-md directly) -- so this uses the realistic
        // buffer shape smart continuation actually produces: a real nested
        // sibling already exists before the empty item being outdented.
        let text = "- one\n  - nested\n  - \n";
        let cursor = text.len() - 1; // end of the empty nested item
        let (new_text, cursor_after) = apply(text, cursor).unwrap();
        assert_eq!(new_text, "- one\n  - nested\n- \n");
        assert_eq!(cursor_after, "- one\n  - nested\n- ".len());
    }

    #[test]
    fn empty_nested_item_under_ordered_parent_increments_parent_number() {
        let text = "5. one\n   1. nested\n   1. \n";
        let cursor = text.len() - 1;
        let (new_text, _) = apply(text, cursor).unwrap();
        assert_eq!(new_text, "5. one\n   1. nested\n6. \n");
    }

    #[test]
    fn empty_nested_task_item_outdents_clearing_checkbox() {
        // Same grammar quirk as the empty-task tests above (an empty `[ ]`
        // doesn't parse as a real task marker), now combined with nesting.
        let text = "- one\n  - [ ] nested\n  - [ ] \n";
        let cursor = text.len() - 1;
        let (new_text, _) = apply(text, cursor).unwrap();
        assert_eq!(new_text, "- one\n  - [ ] nested\n- \n");
    }

    #[test]
    fn a_batch_keeps_edits_in_original_coordinates_and_shifts_cursors() {
        let text = "- one\n- two\n";
        let batch = newline_batch(text, &[5, 11]).expect("both cursors are on list items");

        assert_eq!(
            batch.edits,
            vec![(5..5, "\n- ".to_string()), (11..11, "\n- ".to_string())],
            "the second edit is not moved by the first one's length"
        );
        assert_eq!(batch.cursors, vec![8, 17]);
        let new_text = apply_batch(text, &batch);
        assert_eq!(new_text, "- one\n- \n- two\n- \n");
        assert_eq!(&new_text[..batch.cursors[0]], "- one\n- ");
        assert_eq!(&new_text[..batch.cursors[1]], "- one\n- \n- two\n- ");
    }

    #[test]
    fn a_batch_mixes_edits_that_grow_and_shrink_the_text() {
        let text = "- one\n- \n- three\n";
        let batch = newline_batch(text, &[5, 8, 16]).expect("every cursor is on a list item");

        assert_eq!(
            batch.edits,
            vec![
                (5..5, "\n- ".to_string()),
                (6..8, String::new()),
                (16..16, "\n- ".to_string()),
            ]
        );
        let new_text = apply_batch(text, &batch);
        assert_eq!(new_text, "- one\n- \n\n- three\n- \n");
        assert_eq!(batch.cursors, vec![8, 9, 20]);
        assert_eq!(&new_text[..batch.cursors[0]], "- one\n- ");
        assert_eq!(&new_text[..batch.cursors[1]], "- one\n- \n");
        assert_eq!(&new_text[..batch.cursors[2]], "- one\n- \n\n- three\n- ");
    }

    #[test]
    fn a_batch_of_one_matches_the_single_cursor_edit() {
        let text = "1. one\n";
        let batch = newline_batch(text, &[6]).expect("the cursor is on a list item");
        let edit = newline_edit(text, 6).expect("the cursor is on a list item");

        assert_eq!(batch.edits, vec![(edit.replace, edit.insert)]);
        assert_eq!(batch.cursors, vec![edit.cursor_after]);
    }

    #[test]
    fn a_batch_is_nothing_when_any_cursor_is_off_a_list_item() {
        assert_eq!(newline_batch("- one\n\nplain\n", &[5, 13]), None);
    }

    #[test]
    fn a_batch_for_no_cursors_has_no_edits() {
        assert_eq!(
            newline_batch("- one\n", &[]),
            Some(NewlineBatch {
                edits: Vec::new(),
                cursors: Vec::new(),
            })
        );
    }

    #[test]
    fn a_batch_refuses_edits_that_overlap() {
        // Both cursors would clear the same empty item, and out-of-order
        // cursors would give edits in the wrong order. The editor never
        // reports either, but one buffer edit could not express them.
        let text = "- a\n-   \n";
        assert!(newline_batch(text, &[8]).is_some());
        assert_eq!(newline_batch(text, &[8, 8]), None);
        assert_eq!(newline_batch(text, &[8, 2]), None);
    }
}
