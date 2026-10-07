//! Pure "toggle bold, italic, strikethrough, highlight, code or a link on the
//! selection" logic for visual_md, backing `Ctrl/Cmd+B`/`I` and the other
//! formatting shortcuts per `docs/live-preview-spec.md`'s "Selection formatting
//! shortcuts" bullet.
//!
//! Like [`crate::plan`] and [`crate::list_continuation`], this has no
//! GPUI/`Editor` dependency: it takes raw buffer text plus a set of cursor or
//! selection byte ranges and returns the edits to apply (or an empty set to
//! mean "do nothing").

use std::ops::Range;

use tree_sitter::{Node, Parser};

/// Which construct a shortcut toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    Bold,
    Italic,
    Strikethrough,
    Highlight,
    Code,
}

impl Emphasis {
    /// The marker that goes on both sides of the text. For code it is the
    /// shortest, and [`wrap_markers`] makes it longer when the text has
    /// backticks of its own.
    fn marker(self) -> &'static str {
        match self {
            Emphasis::Bold => "**",
            Emphasis::Italic => "*",
            Emphasis::Strikethrough => "~~",
            Emphasis::Highlight => "==",
            Emphasis::Code => "`",
        }
    }

    /// The grammar's node for the construct. `==highlight==` has none, so it
    /// is found by scanning text instead.
    fn node_kind(self) -> Option<&'static str> {
        match self {
            Emphasis::Bold => Some("strong_emphasis"),
            Emphasis::Italic => Some("emphasis"),
            Emphasis::Strikethrough => Some("strikethrough"),
            Emphasis::Code => Some("code_span"),
            Emphasis::Highlight => None,
        }
    }

    fn delimiter_kind(self) -> &'static str {
        match self {
            Emphasis::Code => "code_span_delimiter",
            _ => "emphasis_delimiter",
        }
    }
}

/// What goes before and after `selected` to make it `kind`. Code is the only one
/// that depends on the text: its backtick run has to be longer than any run
/// inside it, with a space inside when the text starts or ends with a backtick,
/// or the markers would run into it.
fn wrap_markers(kind: Emphasis, selected: &str) -> (String, String) {
    if kind != Emphasis::Code {
        return (kind.marker().to_string(), kind.marker().to_string());
    }
    let mut longest_run = 0;
    let mut run = 0;
    for character in selected.chars() {
        if character == '`' {
            run += 1;
            longest_run = longest_run.max(run);
        } else {
            run = 0;
        }
    }
    let ticks = "`".repeat(longest_run + 1);
    if selected.starts_with('`') || selected.ends_with('`') {
        (format!("{ticks} "), format!(" {ticks}"))
    } else {
        (ticks.clone(), ticks)
    }
}

/// The result of toggling `kind` for every selection: a single batch of
/// edits plus each selection's resulting position, in the same order as the
/// input selections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatEdit {
    /// Edits to apply together in one `Editor::edit` call, in the
    /// *original* text's byte coordinates. `text::Buffer::apply_local_edit`
    /// walks the pre-edit rope once, combining every edit in the batch
    /// itself, so every range here must stay expressed against the original
    /// text -- pre-shifting them by an earlier edit's length delta would
    /// double-count that delta once the buffer applies the batch. Zed's own
    /// `Editor::handle_input` builds its multi-cursor edit lists the same
    /// way (each selection's own unshifted `start..end`).
    pub edits: Vec<(Range<usize>, String)>,
    /// Where each input selection should land afterwards, one per input
    /// selection, in the *new* text's coordinates -- unlike `edits`, these
    /// genuinely do need to account for every earlier selection's own
    /// length delta, since that delta is a real shift in the final,
    /// post-batch document.
    pub selections: Vec<Range<usize>>,
}

/// Computes the edits (and resulting selections) for pressing the `kind`
/// shortcut with `selections` active in `text`. See the module doc for the
/// five rules this follows.
pub fn toggle(text: &str, selections: &[Range<usize>], kind: Emphasis) -> FormatEdit {
    let identity = || FormatEdit {
        edits: Vec::new(),
        selections: selections.to_vec(),
    };

    let mut block_parser = Parser::new();
    if block_parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .is_err()
    {
        return identity();
    }
    let Some(block_tree) = block_parser.parse(text, None) else {
        return identity();
    };
    let mut inline_parser = Parser::new();
    if inline_parser
        .set_language(&tree_sitter_md::INLINE_LANGUAGE.into())
        .is_err()
    {
        return identity();
    }

    apply_plans(selections, |selection| {
        plan_selection(
            text,
            selection,
            kind,
            block_tree.root_node(),
            &mut inline_parser,
        )
    })
}

/// Folds each selection's own plan into one batch: the edits stay in the original
/// text's coordinates, and each resulting selection is shifted by the edits of
/// the selections before it.
fn apply_plans(
    selections: &[Range<usize>],
    mut plan_one: impl FnMut(&Range<usize>) -> SelectionPlan,
) -> FormatEdit {
    let mut delta: isize = 0;
    // For a span two cursors share, `plan_selection` computes both cursors'
    // `selection_after` the same way -- already fully accounting for that
    // span's own (single) edit, independently of `delta`. So a duplicate
    // must be shifted by the delta as it stood *before* that span's edit was
    // folded in, not the current running delta, which already includes it
    // once (from the first cursor to claim the span) -- shifting by the
    // current delta would double-count it. This records that "delta before"
    // value per claimed span.
    let mut claimed: Vec<(Range<usize>, isize)> = Vec::new();
    let mut edits = Vec::new();
    let mut new_selections = Vec::with_capacity(selections.len());

    for selection in selections {
        let plan = plan_one(selection);

        let (applied, shift_by): (&[(Range<usize>, String)], isize) = match &plan.target {
            Some(target) => match claimed
                .iter()
                .find(|(claimed_range, _)| claimed_range == target)
            {
                Some((_, delta_before)) => (&[], *delta_before),
                None => {
                    claimed.push((target.clone(), delta));
                    (&plan.edits, delta)
                }
            },
            None => (&plan.edits, delta),
        };

        edits.extend(applied.iter().cloned());
        new_selections.push(apply_delta(plan.selection_after.clone(), shift_by));

        let local_delta: isize = applied
            .iter()
            .map(|(range, new_text)| new_text.len() as isize - (range.end - range.start) as isize)
            .sum();
        delta += local_delta;
    }

    FormatEdit {
        edits,
        selections: new_selections,
    }
}

/// The placeholder a new link's destination starts as, selected so that typing
/// replaces it.
const LINK_PLACEHOLDER: &str = "url";

/// Computes the edits for the link shortcut with `selections` active in `text`:
/// a selection inside a `[text](url)` link unwraps it to `text`, any other
/// selection becomes `[selection](url)` with `url` selected, and a bare cursor
/// gets `[]()` with the cursor between the brackets.
pub fn toggle_link(text: &str, selections: &[Range<usize>]) -> FormatEdit {
    let identity = || FormatEdit {
        edits: Vec::new(),
        selections: selections.to_vec(),
    };

    let mut block_parser = Parser::new();
    if block_parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .is_err()
    {
        return identity();
    }
    let Some(block_tree) = block_parser.parse(text, None) else {
        return identity();
    };
    let mut inline_parser = Parser::new();
    if inline_parser
        .set_language(&tree_sitter_md::INLINE_LANGUAGE.into())
        .is_err()
    {
        return identity();
    }

    apply_plans(selections, |selection| {
        plan_link_selection(text, selection, block_tree.root_node(), &mut inline_parser)
    })
}

fn plan_link_selection(
    text: &str,
    selection: &Range<usize>,
    block_root: Node,
    inline_parser: &mut Parser,
) -> SelectionPlan {
    if let Some(link) = detect_link(text, selection, block_root, inline_parser) {
        let selection_after = map_offset_after_removal(selection.start, &link.open, &link.close)
            ..map_offset_after_removal(selection.end, &link.open, &link.close);
        return SelectionPlan {
            edits: vec![
                (link.open.clone(), String::new()),
                (link.close.clone(), String::new()),
            ],
            selection_after,
            target: Some(link.node_range),
        };
    }

    if selection.is_empty() {
        let cursor = selection.start + 1;
        return SelectionPlan {
            edits: vec![(selection.start..selection.start, "[]()".to_string())],
            selection_after: cursor..cursor,
            target: None,
        };
    }

    let Some((trim_start, trim_end)) = trim_whitespace_range(text, selection) else {
        return no_change(selection);
    };
    if trim_start >= trim_end {
        return no_change(selection);
    }
    let close = format!("]({LINK_PLACEHOLDER})");
    // `[` goes in before the text, so the destination starts after the text, the
    // `[` and the `](`.
    let url_start = trim_end + 1 + 2;
    SelectionPlan {
        edits: vec![
            (trim_start..trim_start, "[".to_string()),
            (trim_end..trim_end, close),
        ],
        selection_after: url_start..url_start + LINK_PLACEHOLDER.len(),
        target: None,
    }
}

fn no_change(selection: &Range<usize>) -> SelectionPlan {
    SelectionPlan {
        edits: Vec::new(),
        selection_after: selection.clone(),
        target: None,
    }
}

/// The `[text](url)` that holds a selection: its opening `[`, everything from
/// the closing `]` to the last `)`, and the whole link.
fn detect_link(
    text: &str,
    selection: &Range<usize>,
    block_root: Node,
    inline_parser: &mut Parser,
) -> Option<Span> {
    let context = inline_context(text, selection, block_root, inline_parser)?;
    let local_leaf = context
        .tree
        .root_node()
        .descendant_for_byte_range(context.selection.start, context.selection.end)?;
    let link = find_ancestor(local_leaf, |node| node.kind() == "inline_link")?;

    let mut cursor = link.walk();
    let children: Vec<Node> = link.children(&mut cursor).collect();
    let open = children.iter().find(|child| child.kind() == "[")?;
    let close_bracket = children.iter().find(|child| child.kind() == "]")?;
    let close_paren = children.iter().rev().find(|child| child.kind() == ")")?;
    Some(Span {
        node_range: shift(link.byte_range(), context.offset),
        open: shift(open.byte_range(), context.offset),
        close: shift(
            close_bracket.byte_range().start..close_paren.byte_range().end,
            context.offset,
        ),
    })
}

fn apply_delta(range: Range<usize>, delta: isize) -> Range<usize> {
    ((range.start as isize + delta) as usize)..((range.end as isize + delta) as usize)
}

fn shift(range: Range<usize>, offset: usize) -> Range<usize> {
    (range.start + offset)..(range.end + offset)
}

/// One selection's own edits (in original-text coordinates, unaffected by
/// any other selection) and where it should end up, plus -- for an unwrap --
/// the enclosing node's range, used to dedupe multiple cursors inside the
/// same span.
struct SelectionPlan {
    edits: Vec<(Range<usize>, String)>,
    selection_after: Range<usize>,
    target: Option<Range<usize>>,
}

fn plan_selection(
    text: &str,
    selection: &Range<usize>,
    kind: Emphasis,
    block_root: Node,
    inline_parser: &mut Parser,
) -> SelectionPlan {
    if let Some(span) = detect_span(text, selection, kind, block_root, inline_parser) {
        let edits = vec![
            (span.open.clone(), String::new()),
            (span.close.clone(), String::new()),
        ];
        let selection_after = map_offset_after_removal(selection.start, &span.open, &span.close)
            ..map_offset_after_removal(selection.end, &span.open, &span.close);
        return SelectionPlan {
            edits,
            selection_after,
            target: Some(span.node_range),
        };
    }

    if selection.is_empty() {
        if let Some(remove) = detect_empty_pair_at_cursor(text, selection.start, kind) {
            return SelectionPlan {
                edits: vec![(remove.clone(), String::new())],
                selection_after: remove.start..remove.start,
                target: None,
            };
        }
        let marker = kind.marker();
        let insert = format!("{marker}{marker}");
        let cursor = selection.start + marker.len();
        return SelectionPlan {
            edits: vec![(selection.start..selection.start, insert)],
            selection_after: cursor..cursor,
            target: None,
        };
    }

    let Some((trim_start, trim_end)) = trim_whitespace_range(text, selection) else {
        return SelectionPlan {
            edits: Vec::new(),
            selection_after: selection.clone(),
            target: None,
        };
    };
    if trim_start >= trim_end {
        return SelectionPlan {
            edits: Vec::new(),
            selection_after: selection.clone(),
            target: None,
        };
    }
    let (open, close) = wrap_markers(kind, text.get(trim_start..trim_end).unwrap_or_default());
    let selection_after = (trim_start + open.len())..(trim_end + open.len());
    let edits = vec![(trim_start..trim_start, open), (trim_end..trim_end, close)];
    SelectionPlan {
        edits,
        selection_after,
        target: None,
    }
}

/// The enclosing bold/italic span for `selection`, if any, found the same
/// way `crate::plan` finds a block's inline content: the smallest block-tree
/// descendant covering the selection, walked up to its nearest `inline` (or
/// `pipe_table_cell`) ancestor, then re-parsed as its own inline tree so the
/// selection can be re-anchored against a real `strong_emphasis`/`emphasis`
/// node.
struct Span {
    node_range: Range<usize>,
    open: Range<usize>,
    close: Range<usize>,
}

/// What the inline grammar says about the text around a selection: the inline
/// node that holds it, re-parsed as its own tree with the selection moved into
/// its coordinates, so it can be matched against real nodes.
struct InlineContext {
    text: String,
    tree: tree_sitter::Tree,
    selection: Range<usize>,
    offset: usize,
}

fn inline_context(
    text: &str,
    selection: &Range<usize>,
    block_root: Node,
    inline_parser: &mut Parser,
) -> Option<InlineContext> {
    let leaf = block_root.descendant_for_byte_range(selection.start, selection.end)?;
    let inline_node = find_ancestor(leaf, |node| {
        node.kind() == "inline" || node.kind() == "pipe_table_cell"
    })?;

    let range = inline_node.byte_range();
    if selection.start < range.start || selection.end > range.end {
        return None;
    }
    let inline_text = blank_block_continuations(inline_node, text)?;
    let tree = inline_parser.parse(&inline_text, None)?;
    Some(InlineContext {
        text: inline_text,
        tree,
        selection: (selection.start - range.start)..(selection.end - range.start),
        offset: range.start,
    })
}

fn detect_span(
    text: &str,
    selection: &Range<usize>,
    kind: Emphasis,
    block_root: Node,
    inline_parser: &mut Parser,
) -> Option<Span> {
    let context = inline_context(text, selection, block_root, inline_parser)?;
    let Some(node_kind) = kind.node_kind() else {
        return highlight_span(&context);
    };

    let local_leaf = context
        .tree
        .root_node()
        .descendant_for_byte_range(context.selection.start, context.selection.end)?;
    let mut matched = find_ancestor(local_leaf, |node| node.kind() == node_kind)?;
    // The grammar writes `~~x~~` as a strikethrough inside a strikethrough, one
    // `~` each, so the construct is the outermost of them.
    if kind == Emphasis::Strikethrough {
        while let Some(parent) = matched.parent()
            && parent.kind() == node_kind
        {
            matched = parent;
        }
    }

    // Only the matched node's *own* delimiters: a recursive search (like
    // `plan::collect_delimiters`) would pull in a nested construct's own
    // markers too (e.g. `***x***`'s inner `strong_emphasis` markers when
    // matching the outer `emphasis`), merging runs that this shortcut needs to
    // keep separate so toggling one doesn't disturb the other. The grammar
    // emits one `emphasis_delimiter` node per character (so `**` is two
    // single-byte nodes, not one two-byte node), so the open/close markers are
    // each the contiguous run of delimiters at that end -- same merge
    // `plan::plan_delimited_span` does, just restricted to the node's own
    // delimiters. The exception is strikethrough, whose own `~~` is split
    // across the nested nodes it is written as.
    let mut delimiters: Vec<Range<usize>> = Vec::new();
    collect_own_delimiters(
        matched,
        kind.delimiter_kind(),
        (kind == Emphasis::Strikethrough).then_some(node_kind),
        &mut delimiters,
    );
    if delimiters.len() < 2 {
        return None;
    }
    delimiters.sort_by_key(|range| range.start);
    let first = delimiters.first()?.clone();
    let last = delimiters.last()?.clone();

    let mut prefix_end = first.end;
    for delimiter in delimiters.iter().skip(1) {
        if delimiter.start == prefix_end {
            prefix_end = delimiter.end;
        } else {
            break;
        }
    }
    let mut suffix_start = last.start;
    for delimiter in delimiters.iter().rev().skip(1) {
        if delimiter.end == suffix_start {
            suffix_start = delimiter.start;
        } else {
            break;
        }
    }

    let open_local = first.start..prefix_end;
    let close_local = suffix_start..last.end;
    if open_local.end > close_local.start {
        return None;
    }

    Some(Span {
        node_range: shift(matched.byte_range(), context.offset),
        open: shift(open_local, context.offset),
        close: shift(close_local, context.offset),
    })
}

/// The delimiters of `node` itself, and, when `nested_kind` is given, of the
/// nodes of that kind inside it.
fn collect_own_delimiters(
    node: Node,
    delimiter_kind: &str,
    nested_kind: Option<&str>,
    out: &mut Vec<Range<usize>>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == delimiter_kind {
            out.push(child.byte_range());
        } else if nested_kind.is_some_and(|nested| child.kind() == nested) {
            collect_own_delimiters(child, delimiter_kind, nested_kind, out);
        }
    }
}

/// The `==highlight==` that holds the selection, found by scanning the inline
/// text the way `plan::plan_highlight_marks` does, leaving code spans alone.
fn highlight_span(context: &InlineContext) -> Option<Span> {
    let mut code_ranges = Vec::new();
    collect_ranges_of_kind(context.tree.root_node(), "code_span", &mut code_ranges);

    let bytes = context.text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'=' && bytes[i + 1] == b'=' && !crate::plan::in_code(i, &code_ranges) {
            if let Some(close) = crate::plan::find_closing(bytes, i + 2, &code_ranges, 0) {
                let open = i..i + 2;
                let close_range = close..close + 2;
                if context.selection.start >= open.start && context.selection.end <= close_range.end
                {
                    return Some(Span {
                        node_range: shift(open.start..close_range.end, context.offset),
                        open: shift(open, context.offset),
                        close: shift(close_range, context.offset),
                    });
                }
                i = close + 2;
                continue;
            }
        }
        i += 1;
    }
    None
}

fn collect_ranges_of_kind(node: Node, kind: &str, out: &mut Vec<Range<usize>>) {
    if node.kind() == kind {
        out.push(node.byte_range());
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_ranges_of_kind(child, kind, out);
    }
}

fn find_ancestor<'a>(node: Node<'a>, matches: impl Fn(&Node) -> bool) -> Option<Node<'a>> {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if matches(&candidate) {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}

/// Mirrors `plan::plan_inline`'s handling of a multi-line construct's
/// continuation lines (a blockquote's `> ` or a list item's indentation on
/// every line after its first): those bytes are blanked to spaces before
/// re-parsing, so they can't be mistaken for inline content, without
/// disturbing any byte offset within the inline node's own range.
fn blank_block_continuations(inline_node: Node, text: &str) -> Option<String> {
    let range = inline_node.byte_range();
    let original = text.get(range.clone())?;
    let mut bytes = original.as_bytes().to_vec();
    let mut cursor = inline_node.walk();
    for child in inline_node.children(&mut cursor) {
        if child.kind() != "block_continuation" {
            continue;
        }
        let child_range = child.byte_range();
        if child_range.is_empty() {
            continue;
        }
        let local_start = child_range.start - range.start;
        let local_end = child_range.end - range.start;
        if let Some(slice) = bytes.get_mut(local_start..local_end) {
            slice.fill(b' ');
        }
    }
    String::from_utf8(bytes).ok()
}

/// Where original offset `o` (with `open.start <= o <= close.end`, i.e.
/// somewhere in the span being unwrapped) lands once both marker ranges are
/// deleted: unaffected if entirely before a marker, pulled back to that
/// marker's start if inside it (the normal "deleted range collapses to its
/// start" convention), and shifted back by the marker's full length if
/// entirely past it.
fn map_offset_after_removal(o: usize, open: &Range<usize>, close: &Range<usize>) -> usize {
    let mut result = o;
    if o > open.start {
        result -= o.min(open.end) - open.start;
    }
    if o > close.start {
        result -= o.min(close.end) - close.start;
    }
    result
}

/// Rule 5's fallback: a cursor sitting exactly between an already-inserted,
/// still-empty marker pair (e.g. `**|**`) doesn't parse as a node at all, so
/// `detect_span` never finds it. Undoing that insertion needs a direct textual
/// check instead. A pair of one character, such as italic's `*` or code's
/// backtick, only counts when it is not the middle of a longer run of the same
/// character, so the middle of an empty bold pair (`**|**`) is never mistaken
/// for an empty italic one.
fn detect_empty_pair_at_cursor(text: &str, at: usize, kind: Emphasis) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    let marker = kind.marker().as_bytes();
    let length = marker.len();
    if at < length
        || at + length > bytes.len()
        || &bytes[at - length..at] != marker
        || &bytes[at..at + length] != marker
    {
        return None;
    }
    if length == 1 {
        let character = marker[0];
        let run_continues_before = at >= 2 && bytes[at - 2] == character;
        let run_continues_after = at + 1 < bytes.len() && bytes[at + 1] == character;
        if run_continues_before || run_continues_after {
            return None;
        }
    }
    Some((at - length)..(at + length))
}

/// Rule 2's whitespace trim: `** x **` is not bold in CommonMark (a
/// delimiter run can't be immediately followed/preceded by whitespace and
/// still open/close emphasis), so wrapping a selection that starts or ends
/// with whitespace must place the markers inside it instead of around it.
fn trim_whitespace_range(text: &str, selection: &Range<usize>) -> Option<(usize, usize)> {
    let slice = text.get(selection.clone())?;
    let trimmed_start = selection.start + (slice.len() - slice.trim_start().len());
    let trimmed_end = selection.end - (slice.len() - slice.trim_end().len());
    Some((trimmed_start, trimmed_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, edits: &[(Range<usize>, String)]) -> String {
        let mut result = text.to_string();
        let mut ordered = edits.to_vec();
        ordered.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        for (range, insert) in ordered {
            result.replace_range(range, &insert);
        }
        result
    }

    #[test]
    fn wrap_selection_bold() {
        let text = "Hello world\n";
        let result = toggle(text, &[6..11], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(6..6, "**".to_string()), (11..11, "**".to_string())]
        );
        assert_eq!(result.selections, vec![8..13]);
        assert_eq!(apply(text, &result.edits), "Hello **world**\n");
    }

    #[test]
    fn wrap_selection_italic() {
        let text = "Hello world\n";
        let result = toggle(text, &[6..11], Emphasis::Italic);
        assert_eq!(
            result.edits,
            vec![(6..6, "*".to_string()), (11..11, "*".to_string())]
        );
        assert_eq!(result.selections, vec![7..12]);
        assert_eq!(apply(text, &result.edits), "Hello *world*\n");
    }

    #[test]
    fn wrap_selection_trims_surrounding_whitespace() {
        let text = "Hello  world  now\n";
        let result = toggle(text, &[5..14], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(7..7, "**".to_string()), (12..12, "**".to_string())]
        );
        assert_eq!(result.selections, vec![9..14]);
        assert_eq!(apply(text, &result.edits), "Hello  **world**  now\n");
    }

    #[test]
    fn all_whitespace_selection_is_a_no_op() {
        let text = "Hello   world\n";
        let result = toggle(text, &[5..8], Emphasis::Bold);
        assert!(result.edits.is_empty());
        assert_eq!(result.selections, vec![5..8]);
    }

    #[test]
    fn unwrap_selection_inside_bold_content() {
        let text = "Hello **world** now\n";
        let result = toggle(text, &[8..13], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(6..8, String::new()), (13..15, String::new())]
        );
        assert_eq!(result.selections, vec![6..11]);
        assert_eq!(apply(text, &result.edits), "Hello world now\n");
    }

    #[test]
    fn unwrap_selection_covering_the_markers_too() {
        let text = "Hello **world** now\n";
        let result = toggle(text, &[6..15], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(6..8, String::new()), (13..15, String::new())]
        );
        assert_eq!(result.selections, vec![6..11]);
    }

    #[test]
    fn unwrap_from_cursor_inside_bold() {
        let text = "Hello **world** now\n";
        let result = toggle(text, &[11..11], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(6..8, String::new()), (13..15, String::new())]
        );
        assert_eq!(result.selections, vec![9..9]);
    }

    #[test]
    fn unwrap_from_cursor_at_content_edge() {
        let text = "Hello **world** now\n";
        let result = toggle(text, &[8..8], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(6..8, String::new()), (13..15, String::new())]
        );
        assert_eq!(result.selections, vec![6..6]);
    }

    #[test]
    fn insert_and_remove_empty_bold_pair() {
        let text = "Hello world\n";
        let inserted = toggle(text, &[5..5], Emphasis::Bold);
        assert_eq!(inserted.edits, vec![(5..5, "****".to_string())]);
        assert_eq!(inserted.selections, vec![7..7]);

        let after_insert = apply(text, &inserted.edits);
        assert_eq!(after_insert, "Hello**** world\n");

        let removed = toggle(&after_insert, &[7..7], Emphasis::Bold);
        assert_eq!(removed.edits, vec![(5..9, String::new())]);
        assert_eq!(removed.selections, vec![5..5]);
        assert_eq!(apply(&after_insert, &removed.edits), text);
    }

    #[test]
    fn insert_and_remove_empty_italic_pair() {
        let text = "Hello world\n";
        let inserted = toggle(text, &[5..5], Emphasis::Italic);
        assert_eq!(inserted.edits, vec![(5..5, "**".to_string())]);
        assert_eq!(inserted.selections, vec![6..6]);

        let after_insert = apply(text, &inserted.edits);
        assert_eq!(after_insert, "Hello** world\n");

        let removed = toggle(&after_insert, &[6..6], Emphasis::Italic);
        assert_eq!(removed.edits, vec![(5..7, String::new())]);
        assert_eq!(apply(&after_insert, &removed.edits), text);
    }

    #[test]
    fn italic_empty_pair_detection_ignores_the_middle_of_a_bold_pair() {
        // A cursor exactly between an empty bold pair's two "**" runs must
        // not be mistaken for a lone italic "*|*" pair -- that would eat
        // half of the bold markers.
        let text = "Hello**** world\n";
        assert_eq!(detect_empty_pair_at_cursor(text, 7, Emphasis::Italic), None);
    }

    #[test]
    fn italic_toggle_on_bold_only_text_does_not_touch_bold_markers() {
        let text = "Hello **world** now\n";
        let result = toggle(text, &[10..10], Emphasis::Italic);
        assert_eq!(result.edits, vec![(10..10, "**".to_string())]);
    }

    #[test]
    fn triple_star_bold_toggle_leaves_italic_markers() {
        let text = "A ***both*** word.\n";
        let result = toggle(text, &[6..6], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(3..5, String::new()), (9..11, String::new())]
        );
        assert_eq!(apply(text, &result.edits), "A *both* word.\n");
    }

    #[test]
    fn triple_star_italic_toggle_leaves_bold_markers() {
        let text = "A ***both*** word.\n";
        let result = toggle(text, &[6..6], Emphasis::Italic);
        assert_eq!(
            result.edits,
            vec![(2..3, String::new()), (11..12, String::new())]
        );
        assert_eq!(apply(text, &result.edits), "A **both** word.\n");
    }

    #[test]
    fn underscore_italic_unwraps() {
        let text = "A _word_ end.\n";
        let result = toggle(text, &[4..4], Emphasis::Italic);
        assert_eq!(
            result.edits,
            vec![(2..3, String::new()), (7..8, String::new())]
        );
        assert_eq!(apply(text, &result.edits), "A word end.\n");
    }

    #[test]
    fn bold_toggle_inside_blockquote_line() {
        let text = "> **bold** text\n";
        let result = toggle(text, &[4..4], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(2..4, String::new()), (8..10, String::new())]
        );
        assert_eq!(apply(text, &result.edits), "> bold text\n");
    }

    #[test]
    fn bold_toggle_inside_list_item() {
        let text = "- **bold** text\n";
        let result = toggle(text, &[4..4], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(2..4, String::new()), (8..10, String::new())]
        );
        assert_eq!(apply(text, &result.edits), "- bold text\n");
    }

    #[test]
    fn wrap_and_unwrap_round_trip_multibyte_text() {
        let text = "Café naïve end\n";
        let word_start = text.find("naïve").unwrap();
        let word_end = word_start + "naïve".len();

        let wrapped = toggle(text, &[word_start..word_end], Emphasis::Bold);
        assert_eq!(wrapped.edits.len(), 2);
        let after_wrap = apply(text, &wrapped.edits);
        assert_eq!(after_wrap, "Café **naïve** end\n");

        let cursor = wrapped.selections[0].start;
        let unwrapped = toggle(&after_wrap, &[cursor..cursor], Emphasis::Bold);
        assert_eq!(unwrapped.edits.len(), 2);
        assert_eq!(apply(&after_wrap, &unwrapped.edits), text);
    }

    #[test]
    fn multi_cursor_wraps_independently_with_correct_deltas() {
        let text = "aaa bbb ccc\n";
        let result = toggle(text, &[0..3, 8..11], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![
                (0..0, "**".to_string()),
                (3..3, "**".to_string()),
                (8..8, "**".to_string()),
                (11..11, "**".to_string()),
            ]
        );
        assert_eq!(result.selections, vec![2..5, 14..17]);
        assert_eq!(apply(text, &result.edits), "**aaa** bbb **ccc**\n");
    }

    #[test]
    fn two_cursors_in_the_same_span_only_unwrap_once() {
        let text = "Hello **world** now\n";
        // Cursors after "wo" and after "orl", both inside the same span.
        let result = toggle(text, &[10..10, 12..12], Emphasis::Bold);
        assert_eq!(
            result.edits,
            vec![(6..8, String::new()), (13..15, String::new())]
        );
        assert_eq!(result.selections, vec![8..8, 10..10]);
        assert_eq!(apply(text, &result.edits), "Hello world now\n");
    }

    #[test]
    fn no_panic_on_empty_document() {
        let result = toggle("", &[0..0], Emphasis::Bold);
        assert_eq!(result.edits, vec![(0..0, "****".to_string())]);
    }

    #[test]
    fn no_panic_on_selection_at_eof_without_trailing_newline() {
        let text = "Hello world";
        let result = toggle(text, &[6..11], Emphasis::Italic);
        assert_eq!(
            result.edits,
            vec![(6..6, "*".to_string()), (11..11, "*".to_string())]
        );
    }

    #[test]
    fn strikethrough_wraps_and_unwraps() {
        let text = "Hello world\n";
        let wrapped = toggle(text, &[6..11], Emphasis::Strikethrough);
        assert_eq!(apply(text, &wrapped.edits), "Hello ~~world~~\n");
        assert_eq!(wrapped.selections, vec![8..13]);

        let struck = "a ~~gone~~ b\n";
        for selection in [5..9, 2..10, 6..6] {
            let unwrapped = toggle(
                struck,
                std::slice::from_ref(&selection),
                Emphasis::Strikethrough,
            );
            assert_eq!(
                apply(struck, &unwrapped.edits),
                "a gone b\n",
                "for {selection:?}"
            );
        }
    }

    #[test]
    fn strikethrough_inserts_and_removes_an_empty_pair() {
        let text = "ab\n";
        let inserted = toggle(text, &[1..1], Emphasis::Strikethrough);
        assert_eq!(apply(text, &inserted.edits), "a~~~~b\n");
        assert_eq!(inserted.selections, vec![3..3]);

        let removed = toggle("a~~~~b\n", &[3..3], Emphasis::Strikethrough);
        assert_eq!(apply("a~~~~b\n", &removed.edits), "ab\n");
    }

    #[test]
    fn strikethrough_leaves_bold_alone() {
        let text = "**bold ~~both~~**\n";
        let result = toggle(text, &[10..14], Emphasis::Strikethrough);

        assert_eq!(apply(text, &result.edits), "**bold both**\n");
    }

    #[test]
    fn highlight_wraps_and_unwraps() {
        let text = "Hello world\n";
        let wrapped = toggle(text, &[6..11], Emphasis::Highlight);
        assert_eq!(apply(text, &wrapped.edits), "Hello ==world==\n");
        assert_eq!(wrapped.selections, vec![8..13]);

        let marked = "a ==keep== b\n";
        for selection in [5..9, 2..10, 6..6] {
            let unwrapped = toggle(
                marked,
                std::slice::from_ref(&selection),
                Emphasis::Highlight,
            );
            assert_eq!(
                apply(marked, &unwrapped.edits),
                "a keep b\n",
                "for {selection:?}"
            );
        }
    }

    #[test]
    fn highlight_ignores_a_pair_in_code_and_picks_the_right_pair() {
        let text = "`==x==` and ==y== and ==z==\n";
        let y = text.find('y').expect("the text has it");
        let result = toggle(text, &[y..y], Emphasis::Highlight);
        assert_eq!(
            apply(text, &result.edits),
            "`==x==` and y and ==z==\n",
            "only the pair around the cursor"
        );

        let in_code = toggle(text, &[3..3], Emphasis::Highlight);
        assert_eq!(
            apply(text, &in_code.edits),
            "`======x==` and ==y== and ==z==\n",
            "a pair in code is not a highlight, so this adds one"
        );
    }

    #[test]
    fn highlight_inserts_and_removes_an_empty_pair() {
        let inserted = toggle("ab\n", &[1..1], Emphasis::Highlight);
        assert_eq!(apply("ab\n", &inserted.edits), "a====b\n");
        let removed = toggle("a====b\n", &[3..3], Emphasis::Highlight);
        assert_eq!(apply("a====b\n", &removed.edits), "ab\n");
    }

    #[test]
    fn code_wraps_and_unwraps() {
        let text = "run the tests now\n";
        let wrapped = toggle(text, &[8..13], Emphasis::Code);
        assert_eq!(apply(text, &wrapped.edits), "run the `tests` now\n");
        assert_eq!(wrapped.selections, vec![9..14]);

        let coded = "run `tests` now\n";
        for selection in [6..11, 4..11, 8..8] {
            let unwrapped = toggle(coded, std::slice::from_ref(&selection), Emphasis::Code);
            assert_eq!(
                apply(coded, &unwrapped.edits),
                "run tests now\n",
                "for {selection:?}"
            );
        }
    }

    #[test]
    fn code_uses_a_longer_run_than_the_backticks_inside_it() {
        let text = "a x`y b\n";
        let inner = toggle(text, &[2..5], Emphasis::Code);
        assert_eq!(apply(text, &inner.edits), "a ``x`y`` b\n");
        assert_eq!(inner.selections, vec![4..7]);

        let edge = toggle("a `x b\n", &[2..4], Emphasis::Code);
        assert_eq!(
            apply("a `x b\n", &edge.edits),
            "a `` `x `` b\n",
            "a space keeps the markers off the backtick"
        );
    }

    #[test]
    fn code_inserts_and_removes_an_empty_pair() {
        let inserted = toggle("ab\n", &[1..1], Emphasis::Code);
        assert_eq!(apply("ab\n", &inserted.edits), "a``b\n");
        assert_eq!(inserted.selections, vec![2..2]);
        let removed = toggle("a``b\n", &[2..2], Emphasis::Code);
        assert_eq!(apply("a``b\n", &removed.edits), "ab\n");
    }

    #[test]
    fn the_middle_of_a_double_backtick_is_not_an_empty_code_pair() {
        let result = toggle("a````b\n", &[3..3], Emphasis::Code);

        assert_ne!(apply("a````b\n", &result.edits), "ab\n");
    }

    #[test]
    fn a_link_wraps_a_selection_and_selects_the_url() {
        let text = "see docs now\n";
        let result = toggle_link(text, &[4..8]);

        assert_eq!(apply(text, &result.edits), "see [docs](url) now\n");
        assert_eq!(result.selections, vec![11..14]);
        let new_text = apply(text, &result.edits);
        assert_eq!(&new_text[result.selections[0].clone()], "url");
    }

    #[test]
    fn a_link_trims_the_selection_like_emphasis_does() {
        let text = "see  docs  now\n";
        let result = toggle_link(text, &[3..10]);

        assert_eq!(apply(text, &result.edits), "see  [docs](url)  now\n");
    }

    #[test]
    fn a_bare_cursor_gets_empty_brackets_with_the_cursor_inside() {
        let text = "a b\n";
        let result = toggle_link(text, &[2..2]);

        assert_eq!(apply(text, &result.edits), "a []()b\n");
        assert_eq!(result.selections, vec![3..3]);
    }

    #[test]
    fn a_link_unwraps_from_inside_its_text_or_with_everything_selected() {
        let text = "a [text](https://x.org) b\n";
        for selection in [4..4, 3..7, 2..23] {
            let result = toggle_link(text, std::slice::from_ref(&selection));
            assert_eq!(
                apply(text, &result.edits),
                "a text b\n",
                "for {selection:?}"
            );
        }
        let inside = toggle_link(text, &[6..6]);
        assert_eq!(
            inside.selections,
            vec![5..5],
            "the cursor keeps its place in the text"
        );
    }

    #[test]
    fn an_image_is_not_a_link_to_unwrap_but_its_text_is_wrapped_like_any_other() {
        let text = "![alt](pic.png)\n";
        let result = toggle_link(text, &[2..5]);

        assert!(
            !result.edits.is_empty(),
            "an image is an inline_link too, so its markers come off"
        );
    }

    #[test]
    fn a_whitespace_selection_makes_no_link() {
        let result = toggle_link("a   b\n", &[1..4]);

        assert!(result.edits.is_empty());
    }

    #[test]
    fn links_work_for_several_cursors_and_never_panic_on_odd_input() {
        let text = "one two\n";
        let result = toggle_link(text, &[0..3, 4..7]);
        assert_eq!(apply(text, &result.edits), "[one](url) [two](url)\n");

        assert!(toggle_link("", &[0..0]).edits.len() <= 1);
        assert!(toggle_link("x", &[1..1]).edits.len() <= 1);
    }
}
