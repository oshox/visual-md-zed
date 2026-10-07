//! Pure list editing for visual_md, beyond what `Enter` does in
//! [`crate::list_continuation`]: nesting an item under the one before it,
//! taking it out a level, `Backspace` at the start of an item, moving an item
//! past its sibling, and keeping ordered lists numbered in the source.
//!
//! Like the modules it builds on, this has no GPUI/`Editor` dependency: it takes
//! the buffer text and a selection and returns one edit with the selection to
//! leave behind, or `None` when it does not apply and the keystroke should do
//! what it always did.
//!
//! Every operation works on whole lines of the item, children included: a
//! `list_item` node's range contains the lists nested in it, so what moves,
//! indents or outdents is exactly the item and its subtree.

use std::collections::BTreeMap;
use std::ops::Range;

use tree_sitter::{Node, Parser, Tree};

use crate::list_continuation::{
    ItemMarker, content_start_after, enclosing_list_item, line_end, line_start, newline_edit,
    own_marker, parent_list_item,
};

/// One edit to the text, and where the selection is afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEdit {
    /// The range of the original text to replace.
    pub replace: Range<usize>,
    pub insert: String,
    /// The selection in the text after the edit.
    pub selection: Range<usize>,
}

/// Which way [`move_item`] moves an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

type Edits = Vec<(Range<usize>, String)>;

fn parse(text: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_md::LANGUAGE.into()).ok()?;
    parser.parse(text, None)
}

/// Applies `edits`, which are in the coordinates of `text`, ascending and apart
/// from each other.
fn apply_edits(text: &str, edits: &Edits) -> String {
    let mut result = String::with_capacity(text.len());
    let mut position = 0;
    for (range, insert) in edits {
        result.push_str(&text[position..range.start]);
        result.push_str(insert);
        position = range.end;
    }
    result.push_str(&text[position..]);
    result
}

/// Where `offset` of the text lands once `edits` are applied. Text inside a
/// replaced range lands after what replaced it, and a cursor at the very spot
/// something is inserted stays with the text that follows.
fn map_offset(offset: usize, edits: &Edits) -> usize {
    let mut shift: isize = 0;
    for (range, insert) in edits {
        let length_change = insert.len() as isize - range.len() as isize;
        let before_it = offset < range.start || (offset == range.start && !range.is_empty());
        if before_it {
            break;
        }
        if offset < range.end {
            return (range.start as isize + shift) as usize + insert.len();
        }
        shift += length_change;
    }
    (offset as isize + shift) as usize
}

fn map_range(range: &Range<usize>, edits: &Edits) -> Range<usize> {
    map_offset(range.start, edits)..map_offset(range.end, edits)
}

/// The one edit that turns `old` into `new`: everything between the text they
/// start and end alike with.
fn minimal_edit(old: &str, new: &str) -> Option<(Range<usize>, String)> {
    if old == new {
        return None;
    }
    let mut prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(old_byte, new_byte)| old_byte == new_byte)
        .count();
    while prefix > 0 && !(old.is_char_boundary(prefix) && new.is_char_boundary(prefix)) {
        prefix -= 1;
    }
    let mut suffix = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take_while(|(old_byte, new_byte)| old_byte == new_byte)
        .count()
        .min(old.len() - prefix)
        .min(new.len() - prefix);
    while suffix > 0
        && !(old.is_char_boundary(old.len() - suffix) && new.is_char_boundary(new.len() - suffix))
    {
        suffix -= 1;
    }
    Some((
        prefix..old.len() - suffix,
        new[prefix..new.len() - suffix].to_string(),
    ))
}

/// Turns the edits an operation made, and the selection it wants afterwards (in
/// the text after them), into the one [`ListEdit`], after numbering the ordered
/// lists around the selection again.
fn finish(text: &str, edits: Edits, selection: Range<usize>) -> Option<ListEdit> {
    let edited = apply_edits(text, &edits);
    let renumbering = renumber_edits(&edited, selection.start);
    let final_text = apply_edits(&edited, &renumbering);
    let selection = map_range(&selection, &renumbering);
    let (replace, insert) = minimal_edit(text, &final_text)?;
    Some(ListEdit {
        replace,
        insert,
        selection,
    })
}

/// The item whose own first line holds the selection, with its marker: nothing
/// for a selection that is anywhere else or runs over lines.
fn item_on_first_line<'a>(
    text: &str,
    root: Node<'a>,
    selection: &Range<usize>,
) -> Option<(Node<'a>, ItemMarker)> {
    if selection.end > line_end(text, selection.start) {
        return None;
    }
    let item = item_at(root, selection.start)?;
    let marker = own_marker(item)?;
    (line_start(text, marker.marker.start) == line_start(text, selection.start))
        .then_some((item, marker))
}

/// The deepest item at `offset`. A cursor at the very end of a document with no
/// final newline is past the last character, so the character before it is
/// looked at too.
fn item_at<'a>(root: Node<'a>, offset: usize) -> Option<Node<'a>> {
    enclosing_list_item(root, offset).or_else(|| {
        offset
            .checked_sub(1)
            .and_then(|before| enclosing_list_item(root, before))
    })
}

/// The lines of `item` from the start of its first to the end of its last, with
/// that line's newline, leaving out the blank lines after it, which belong
/// between it and the next item and stay put when it moves. The grammar can
/// also end an item in the indentation of the line after it, which is not part
/// of it either.
fn item_lines(text: &str, item: Node) -> Range<usize> {
    let start = line_start(text, item.start_byte());
    let mut end = item.end_byte().max(start);
    let end_line_start = line_start(text, end);
    if end > end_line_start
        && text[end_line_start..end]
            .chars()
            .all(|character| character == ' ' || character == '\t')
    {
        end = end_line_start;
    }
    if end > start {
        let last = end - 1;
        end = (line_end(text, last) + 1).min(text.len());
    }
    // Trailing blank lines, but never the first line itself.
    let content = text[start..end].trim_end().len();
    if content > 0 {
        end = (line_end(text, start + content) + 1).min(text.len());
    }
    start..end
}

/// Whether only spaces and tabs come before the marker, which is when
/// re-indenting an item's lines is just a matter of their first bytes. A list
/// in a quote has `>` in front.
fn has_plain_indent(text: &str, marker: &ItemMarker) -> bool {
    let start = line_start(text, marker.marker.start);
    text.get(start..marker.marker.start)
        .is_some_and(|prefix| prefix.chars().all(|character| character == ' '))
}

fn column(text: &str, offset: usize) -> usize {
    offset - line_start(text, offset)
}

fn sibling_item<'a>(item: Node<'a>, direction: Direction) -> Option<Node<'a>> {
    let mut sibling = item;
    loop {
        sibling = match direction {
            Direction::Up => sibling.prev_sibling()?,
            Direction::Down => sibling.next_sibling()?,
        };
        if sibling.kind() == "list_item" {
            return Some(sibling);
        }
    }
}

/// Each line of `lines` that has anything on it, as the offset it starts at.
fn non_blank_line_starts(text: &str, lines: Range<usize>) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut start = lines.start;
    for line in text[lines].split_inclusive('\n') {
        if !line.trim().is_empty() {
            starts.push(start);
        }
        start += line.len();
    }
    starts
}

/// Nests the item the selection is on under the item before it, with its
/// children: every line of it moves in by the width of the previous item's
/// marker, which puts it exactly under that item's text. `None` when it is not on
/// an item's first line, has no previous item, or is in a quote.
pub fn nest(text: &str, selection: &Range<usize>) -> Option<ListEdit> {
    let tree = parse(text)?;
    let (item, marker) = item_on_first_line(text, tree.root_node(), selection)?;
    let previous = sibling_item(item, Direction::Up)?;
    let previous_marker = own_marker(previous)?;
    if !has_plain_indent(text, &marker) || !has_plain_indent(text, &previous_marker) {
        return None;
    }
    let indent =
        column(text, previous_marker.marker.end).checked_sub(column(text, marker.marker.start))?;
    if indent == 0 {
        return None;
    }

    // An ordered item that starts a new nested list starts it from 1, however it
    // was numbered where it was. A number with another number of digits moves the
    // item's other lines by the difference, as renumbering does.
    let starts_a_list = !has_nested_list(previous);
    let digits = digit_width(text, marker.marker.start);
    let restart = (marker.ordered
        && starts_a_list
        && text.get(marker.marker.start..marker.marker.start + digits) != Some("1"))
    .then(|| marker.marker.start..marker.marker.start + digits);
    let digit_shift = restart.as_ref().map_or(0, |_| 1 - digits as isize);

    let lines = item_lines(text, item);
    let mut edits: Edits = Vec::new();
    for start in non_blank_line_starts(text, lines) {
        let is_first_line = start == line_start(text, marker.marker.start);
        let width = if is_first_line {
            indent as isize
        } else {
            indent as isize + digit_shift
        };
        if width > 0 {
            edits.push((start..start, " ".repeat(width as usize)));
        }
    }
    if let Some(range) = restart {
        edits.push((range, "1".to_string()));
    }
    edits.sort_by_key(|(range, _)| (range.start, range.end));
    let selection_after = map_range(selection, &edits);
    finish(text, edits, selection_after)
}

fn has_nested_list(item: Node) -> bool {
    let mut cursor = item.walk();
    item.children(&mut cursor)
        .any(|child| child.kind() == "list")
}

/// Takes the item the selection is on out of its parent, to the parent's own
/// level, with its children. Items after it in the same list stay where they
/// are, which makes them its children. `None` for a top-level item.
pub fn outdent(text: &str, selection: &Range<usize>) -> Option<ListEdit> {
    let tree = parse(text)?;
    let (item, marker) = item_on_first_line(text, tree.root_node(), selection)?;
    outdent_item(text, item, &marker, selection)
}

fn outdent_item(
    text: &str,
    item: Node,
    marker: &ItemMarker,
    selection: &Range<usize>,
) -> Option<ListEdit> {
    let parent = parent_list_item(item)?;
    let parent_marker = own_marker(parent)?;
    if !has_plain_indent(text, marker) || !has_plain_indent(text, &parent_marker) {
        return None;
    }
    let remove =
        column(text, marker.marker.start).checked_sub(column(text, parent_marker.marker.start))?;
    if remove == 0 {
        return None;
    }

    let edits: Edits = non_blank_line_starts(text, item_lines(text, item))
        .into_iter()
        .filter_map(|start| {
            let spaces = text[start..]
                .bytes()
                .take_while(|byte| *byte == b' ')
                .count()
                .min(remove);
            (spaces > 0).then(|| (start..start + spaces, String::new()))
        })
        .collect();
    let selection_after = map_range(selection, &edits);
    finish(text, edits, selection_after)
}

/// What `Backspace` does with the cursor at the start of an item's text: take
/// off its checkbox, else take the item out a level, else remove its marker and
/// leave the text. `None` anywhere else.
pub fn backspace(text: &str, cursor: usize) -> Option<ListEdit> {
    let tree = parse(text)?;
    let selection = cursor..cursor;
    let (item, marker) = item_on_first_line(text, tree.root_node(), &selection)?;
    if content_start_after(&marker, text) != cursor {
        return None;
    }

    if let Some(task) = &marker.task {
        let edits: Edits = vec![(task.start..cursor, String::new())];
        let selection_after = task.start..task.start;
        return finish(text, edits, selection_after);
    }
    if parent_list_item(item).is_some() {
        return outdent_item(text, item, &marker, &selection);
    }
    let edits: Edits = vec![(marker.marker.clone(), String::new())];
    let start = marker.marker.start;
    finish(text, edits, start..start)
}

/// Moves the item the selection is in past its previous or next sibling, with
/// its children. `None` when there is no such sibling.
pub fn move_item(text: &str, selection: &Range<usize>, direction: Direction) -> Option<ListEdit> {
    let tree = parse(text)?;
    let item = item_at(tree.root_node(), selection.start)?;
    let moving = item_lines(text, item);
    if selection.end > moving.end {
        return None;
    }
    let sibling = item_lines(text, sibling_item(item, direction)?);

    let (first, second) = match direction {
        Direction::Up => (sibling, moving.clone()),
        Direction::Down => (moving.clone(), sibling),
    };
    if first.end > second.start {
        return None;
    }
    let gap = &text[first.end..second.start];
    let mut first_text = text[first.clone()].to_string();
    let mut second_text = text[second.clone()].to_string();
    // The last item of a document may have no newline, which has to go with
    // whichever item ends up last.
    if !second_text.ends_with('\n') {
        second_text.push('\n');
        if gap.is_empty() {
            first_text = first_text.trim_end_matches('\n').to_string();
        }
    }

    let region = first.start..second.end;
    let (swapped_before, swapped_after) = (second_text, first_text);
    let moved_start = match direction {
        Direction::Up => region.start,
        Direction::Down => region.start + swapped_before.len() + gap.len(),
    };
    let new_region = format!("{swapped_before}{gap}{swapped_after}");
    let selection_after = (moved_start + selection.start - moving.start)
        ..(moved_start + selection.end - moving.start);
    finish(text, vec![(region, new_region)], selection_after)
}

/// What `Enter` does at `cursor` on an item, as [`newline_edit`] says, with the
/// ordered list the new item is in numbered again.
pub fn newline(text: &str, cursor: usize) -> Option<ListEdit> {
    let newline = newline_edit(text, cursor)?;
    let selection = newline.cursor_after..newline.cursor_after;
    finish(text, vec![(newline.replace, newline.insert)], selection)
}

/// The edits that number every ordered list in the outermost list around
/// `around` as `first, first + 1, ...`, in the coordinates of `text`. An item
/// whose number gets a digit longer or shorter moves its other lines the same
/// amount, or its children would no longer be under its text.
fn renumber_edits(text: &str, around: usize) -> Edits {
    let Some(tree) = parse(text) else {
        return Vec::new();
    };
    let Some(mut node) = tree.root_node().descendant_for_byte_range(around, around) else {
        return Vec::new();
    };
    let mut outermost = None;
    loop {
        if node.kind() == "list" {
            outermost = Some(node);
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => break,
        }
    }
    let Some(list) = outermost else {
        return Vec::new();
    };

    let mut number_edits: Edits = Vec::new();
    let mut line_shifts: BTreeMap<usize, isize> = BTreeMap::new();
    collect_renumbering(text, list, &mut number_edits, &mut line_shifts);

    let mut edits = number_edits;
    for (start, shift) in line_shifts {
        if shift > 0 {
            edits.push((start..start, " ".repeat(shift as usize)));
        } else if shift < 0 {
            let spaces = text[start..]
                .bytes()
                .take_while(|byte| *byte == b' ')
                .count()
                .min(shift.unsigned_abs());
            if spaces > 0 {
                edits.push((start..start + spaces, String::new()));
            }
        }
    }
    edits.sort_by_key(|(range, _)| (range.start, range.end));
    edits
}

fn collect_renumbering(
    text: &str,
    list: Node,
    number_edits: &mut Edits,
    line_shifts: &mut BTreeMap<usize, isize>,
) {
    let mut cursor = list.walk();
    let items: Vec<Node> = list
        .children(&mut cursor)
        .filter(|child| child.kind() == "list_item")
        .collect();

    let first_number = items
        .first()
        .and_then(|item| own_marker(*item))
        .filter(|marker| marker.ordered)
        .and_then(|marker| leading_number(text, marker.marker.start));

    for (index, item) in items.iter().enumerate() {
        if let (Some(first_number), Some(marker)) = (first_number, own_marker(*item))
            && marker.ordered
            && let Some(current) = leading_number(text, marker.marker.start)
        {
            let expected = first_number + index as u64;
            if current != expected {
                let old_width = digit_width(text, marker.marker.start);
                let new_digits = expected.to_string();
                let shift = new_digits.len() as isize - old_width as isize;
                number_edits.push((
                    marker.marker.start..marker.marker.start + old_width,
                    new_digits,
                ));
                if shift != 0 {
                    let lines = item_lines(text, *item);
                    let first_line_end = line_end(text, lines.start) + 1;
                    if first_line_end < lines.end {
                        for start in non_blank_line_starts(text, first_line_end..lines.end) {
                            *line_shifts.entry(start).or_default() += shift;
                        }
                    }
                }
            }
        }

        let mut item_cursor = item.walk();
        for child in item.children(&mut item_cursor) {
            if child.kind() == "list" {
                collect_renumbering(text, child, number_edits, line_shifts);
            }
        }
    }
}

fn digit_width(text: &str, start: usize) -> usize {
    text[start..]
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count()
}

fn leading_number(text: &str, start: usize) -> Option<u64> {
    text.get(start..start + digit_width(text, start))?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, edit: &ListEdit) -> String {
        let mut result = text.to_string();
        result.replace_range(edit.replace.clone(), &edit.insert);
        result
    }

    /// The text after the edit, with the selection marked by `ˇ` for a cursor and
    /// `«»` for a range.
    fn show(text: &str, edit: &ListEdit) -> String {
        let new_text = apply(text, edit);
        let selection = &edit.selection;
        if selection.is_empty() {
            format!(
                "{}ˇ{}",
                &new_text[..selection.start],
                &new_text[selection.start..]
            )
        } else {
            format!(
                "{}«{}»{}",
                &new_text[..selection.start],
                &new_text[selection.clone()],
                &new_text[selection.end..]
            )
        }
    }

    fn at(text: &str, needle: &str) -> usize {
        text.find(needle).expect("the text has the needle")
    }

    #[test]
    fn test_tab_nests_an_item_under_the_one_before_it() {
        let text = "- one\n- two\n- three\n";
        let cursor = at(text, "two") + 1;
        let edit = nest(text, &(cursor..cursor)).expect("it nests");

        assert_eq!(show(text, &edit), "- one\n  - tˇwo\n- three\n");
    }

    #[test]
    fn test_tab_takes_the_children_along() {
        let text = "- one\n- two\n  - child\n    more\n- three\n";
        let cursor = at(text, "two");
        let edit = nest(text, &(cursor..cursor)).expect("it nests");

        assert_eq!(
            apply(text, &edit),
            "- one\n  - two\n    - child\n      more\n- three\n"
        );
    }

    #[test]
    fn test_tab_indents_by_the_width_of_an_ordered_marker() {
        let text = "1. one\n2. two\n";
        let cursor = at(text, "two");
        let edit = nest(text, &(cursor..cursor)).expect("it nests");

        assert_eq!(apply(text, &edit), "1. one\n   1. two\n");
    }

    #[test]
    fn test_tab_does_nothing_for_a_first_item_or_off_the_first_line_or_in_a_quote() {
        let text = "- one\n- two\n  continued\n";
        assert!(nest(text, &(2..2)).is_none(), "no item before it");
        let continued = at(text, "continued");
        assert!(nest(text, &(continued..continued)).is_none());
        let quoted = "> - one\n> - two\n";
        let cursor = at(quoted, "two");
        assert!(nest(quoted, &(cursor..cursor)).is_none());
        assert!(nest("plain text\n", &(3..3)).is_none());
    }

    #[test]
    fn test_tab_with_a_selection_over_lines_is_left_to_the_editor() {
        let text = "- one\n- two\n- three\n";
        let start = at(text, "two");
        assert!(nest(text, &(start..text.len())).is_none());
    }

    #[test]
    fn test_nesting_an_item_renumbers_the_ordered_list_it_left() {
        let text = "1. one\n2. two\n3. three\n4. four\n";
        let cursor = at(text, "three");
        let edit = nest(text, &(cursor..cursor)).expect("it nests");

        assert_eq!(apply(text, &edit), "1. one\n2. two\n   1. three\n3. four\n");
    }

    #[test]
    fn test_shift_tab_takes_an_item_out_to_its_parents_level() {
        let text = "- one\n  - two\n  - three\n";
        let cursor = at(text, "two") + 1;
        let edit = outdent(text, &(cursor..cursor)).expect("it outdents");

        assert_eq!(show(text, &edit), "- one\n- tˇwo\n  - three\n");
    }

    #[test]
    fn test_shift_tab_takes_children_along_and_leaves_later_siblings_as_children() {
        let text = "- one\n  - two\n    - deep\n  - three\n";
        let cursor = at(text, "two");
        let edit = outdent(text, &(cursor..cursor)).expect("it outdents");

        assert_eq!(
            apply(text, &edit),
            "- one\n- two\n  - deep\n  - three\n",
            "three is now under two"
        );
    }

    #[test]
    fn test_shift_tab_does_nothing_at_the_top_level() {
        assert!(outdent("- one\n- two\n", &(8..8)).is_none());
    }

    #[test]
    fn test_outdenting_renumbers_an_ordered_list() {
        let text = "1. one\n   1. a\n   2. b\n2. two\n";
        let cursor = at(text, "b");
        let edit = outdent(text, &(cursor..cursor)).expect("it outdents");

        assert_eq!(apply(text, &edit), "1. one\n   1. a\n2. b\n3. two\n");
    }

    #[test]
    fn test_backspace_at_the_start_of_a_top_level_item_removes_its_marker() {
        let text = "- one\n- two\n";
        let cursor = at(text, "two");
        let edit = backspace(text, cursor).expect("it removes the marker");

        assert_eq!(show(text, &edit), "- one\nˇtwo\n");
    }

    #[test]
    fn test_backspace_at_the_start_of_a_nested_item_takes_it_out_a_level() {
        let text = "- one\n  - two\n";
        let cursor = at(text, "two");
        let edit = backspace(text, cursor).expect("it outdents");

        assert_eq!(show(text, &edit), "- one\n- ˇtwo\n");
    }

    #[test]
    fn test_backspace_at_the_start_of_a_task_removes_its_checkbox_first() {
        let text = "- [ ] task\n";
        let cursor = at(text, "task");
        let edit = backspace(text, cursor).expect("it removes the checkbox");

        assert_eq!(show(text, &edit), "- ˇtask\n");

        let again = backspace("- task\n", 2).expect("then the marker");
        assert_eq!(show("- task\n", &again), "ˇtask\n");
    }

    #[test]
    fn test_backspace_elsewhere_is_left_to_the_editor() {
        let text = "- one\n- two\n";
        assert!(
            backspace(text, at(text, "two") + 1).is_none(),
            "inside the text"
        );
        assert!(backspace(text, 0).is_none(), "before the marker");
        assert!(backspace(text, 1).is_none(), "inside the marker");
        assert!(backspace("plain\n", 0).is_none());
    }

    #[test]
    fn test_backspace_on_an_empty_item_removes_the_marker() {
        let text = "- one\n- \n";
        let edit = backspace(text, 8).expect("it removes the marker");

        assert_eq!(show(text, &edit), "- one\nˇ\n");
    }

    #[test]
    fn test_an_item_moves_up_past_its_sibling_with_its_children() {
        let text = "- a\n- b\n  - b1\n- c\n";
        let cursor = at(text, "b\n") + 1;
        let edit = move_item(text, &(cursor..cursor), Direction::Up).expect("it moves");

        assert_eq!(apply(text, &edit), "- b\n  - b1\n- a\n- c\n");
        assert_eq!(show(text, &edit), "- bˇ\n  - b1\n- a\n- c\n");
    }

    #[test]
    fn test_an_item_moves_down_past_its_sibling() {
        let text = "- a\n- b\n- c\n";
        let cursor = at(text, "a") + 1;
        let edit = move_item(text, &(cursor..cursor), Direction::Down).expect("it moves");

        assert_eq!(apply(text, &edit), "- b\n- a\n- c\n");
        assert_eq!(edit.selection, 7..7);
    }

    #[test]
    fn test_the_last_item_of_a_document_moves_up_without_a_final_newline() {
        let text = "- a\n- b";
        let cursor = text.len();
        let edit = move_item(text, &(cursor..cursor), Direction::Up).expect("it moves");

        assert_eq!(apply(text, &edit), "- b\n- a");
    }

    #[test]
    fn test_the_first_item_cannot_move_up_nor_the_last_down() {
        let text = "- a\n- b\n";
        assert!(move_item(text, &(1..1), Direction::Up).is_none());
        assert!(move_item(text, &(text.len() - 2..text.len() - 2), Direction::Down).is_none());
    }

    #[test]
    fn test_moving_an_item_keeps_blank_lines_between_items_where_they_were() {
        let text = "- a\n\n- b\n";
        let cursor = at(text, "b");
        let edit = move_item(text, &(cursor..cursor), Direction::Up).expect("it moves");

        assert_eq!(apply(text, &edit), "- b\n\n- a\n");
    }

    #[test]
    fn test_moving_ordered_items_renumbers_them() {
        let text = "1. a\n2. b\n3. c\n";
        let cursor = at(text, "c");
        let edit = move_item(text, &(cursor..cursor), Direction::Up).expect("it moves");

        assert_eq!(apply(text, &edit), "1. a\n2. c\n3. b\n");
        assert_eq!(&apply(text, &edit)[edit.selection.start..], "c\n3. b\n");
    }

    #[test]
    fn test_moving_only_applies_inside_a_list() {
        assert!(move_item("plain\ntext\n", &(1..1), Direction::Up).is_none());
    }

    #[test]
    fn test_enter_in_an_ordered_list_renumbers_the_items_after_the_new_one() {
        let text = "1. one\n2. two\n3. three\n";
        let cursor = at(text, "one") + 3;
        let edit = newline(text, cursor).expect("it continues the list");

        assert_eq!(apply(text, &edit), "1. one\n2. \n3. two\n4. three\n");
        assert_eq!(edit.selection, 10..10);
    }

    #[test]
    fn test_enter_on_an_empty_item_that_ends_a_list_renumbers_nothing_else() {
        let text = "1. one\n2. \n";
        let edit = newline(text, 10).expect("it ends the list");

        assert_eq!(apply(text, &edit), "1. one\n\n");
    }

    #[test]
    fn test_a_number_that_gains_a_digit_moves_its_children_too() {
        let text = "1. a\n2. b\n3. c\n4. d\n5. e\n6. f\n7. g\n8. h\n9. i\n   under nine\n";
        let cursor = at(text, "a") + 1;
        let edit = newline(text, cursor).expect("it continues the list");

        assert_eq!(
            apply(text, &edit),
            "1. a\n2. \n3. b\n4. c\n5. d\n6. e\n7. f\n8. g\n9. h\n10. i\n    under nine\n"
        );
    }

    #[test]
    fn test_a_number_that_loses_a_digit_pulls_its_children_back() {
        let text = "8. a\n9. b\n10. c\n    under c\n";
        let cursor = at(text, "c");
        let edit = move_item(text, &(cursor..cursor), Direction::Up).expect("it moves");

        assert_eq!(apply(text, &edit), "8. a\n9. c\n   under c\n10. b\n");
    }

    #[test]
    fn test_nested_ordered_lists_are_renumbered_inside_their_parent() {
        let text = "1. a\n   1. x\n   1. y\n1. b\n";
        let cursor = at(text, "a") + 1;
        let edit = newline(text, cursor).expect("it continues the list");

        assert_eq!(
            apply(text, &edit),
            "1. a\n2. \n   1. x\n   2. y\n3. b\n",
            "the nested items now belong to the new item"
        );
    }

    #[test]
    fn test_unordered_lists_and_other_text_are_never_renumbered() {
        let text = "3. three\n5. five\n\n- a\n- b\n";
        let cursor = at(text, "a") + 1;
        let edit = newline(text, cursor).expect("it continues the list");

        assert_eq!(
            apply(text, &edit),
            "3. three\n5. five\n\n- a\n- \n- b\n",
            "the ordered list is not near the edit, so it keeps its odd numbers"
        );
    }

    #[test]
    fn test_a_list_that_starts_at_another_number_keeps_it() {
        let text = "5. a\n7. b\n";
        let cursor = at(text, "a") + 1;
        let edit = newline(text, cursor).expect("it continues the list");

        assert_eq!(apply(text, &edit), "5. a\n6. \n7. b\n");
    }

    #[test]
    fn test_multibyte_text_survives_every_operation() {
        let text = "- héllo\n- wörld ✓\n";
        let cursor = at(text, "wörld");
        let nested = nest(text, &(cursor..cursor)).expect("it nests");
        assert_eq!(apply(text, &nested), "- héllo\n  - wörld ✓\n");
        let moved = move_item(text, &(cursor..cursor), Direction::Up).expect("it moves");
        assert_eq!(apply(text, &moved), "- wörld ✓\n- héllo\n");
    }

    #[test]
    fn test_minimal_edit_replaces_only_what_differs() {
        assert_eq!(minimal_edit("abc", "abc"), None);
        assert_eq!(minimal_edit("abcd", "abXd"), Some((2..3, "X".to_string())));
        assert_eq!(minimal_edit("ab", "abcd"), Some((2..2, "cd".to_string())));
        assert_eq!(minimal_edit("abcd", "ab"), Some((2..4, String::new())));
        assert_eq!(minimal_edit("aéa", "aéa"), None, "equal text is no edit");
        let (range, insert) = minimal_edit("é", "è").expect("they differ");
        assert_eq!((range, insert.as_str()), (0..2, "è"));
    }

    #[test]
    fn test_offsets_map_through_edits() {
        let edits: Edits = vec![(2..2, "  ".to_string()), (5..7, String::new())];
        assert_eq!(map_offset(1, &edits), 1);
        assert_eq!(
            map_offset(2, &edits),
            4,
            "a cursor stays with the text after an insertion"
        );
        assert_eq!(map_offset(3, &edits), 5);
        assert_eq!(
            map_offset(6, &edits),
            7,
            "inside a deleted range, at its start"
        );
        assert_eq!(map_offset(9, &edits), 9);
    }
}
