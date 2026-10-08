//! Pure decoration planner for visual_md's Markdown live preview.
//!
//! Parses raw buffer text with tree-sitter-md and, given the current cursor /
//! selection byte ranges, decides which markup delimiters should be hidden,
//! which should be revealed-but-dimmed, and which content spans need a
//! persistent style (bold, italic, strikethrough, code, heading level).
//!
//! Deliberately has no GPUI or `Editor` dependency, so the spec's "Core
//! mechanic" (docs/visual-md-spec.md) is unit-testable without a window,
//! and so the same logic can later be scoped to a viewport without touching
//! the rendering side at all.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use extension::VisualMdSpanStyle;
use tree_sitter::{Node, Parser, Tree};

use crate::inline_scan;
use crate::rules::{self, DynamicKey, DynamicResult, RuleHit, RuleSet};

/// A persistent style to apply to a content span (never to its markers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanStyle {
    Heading(u8),
    Bold,
    Italic,
    Strikethrough,
    InlineCode,
    Highlight,
    /// Background tint for a callout, covering its whole `block_quote`
    /// range (title row included, not just the body) -- the `>` bar and
    /// `[!type]` marker both render as non-opaque widgets, so the tint
    /// shows through underneath them.
    Callout(CalloutKind),
    /// A markdown link's visible text (`[text](url)`) or an autolink's URL
    /// text (`<https://...>`) — never the hidden brackets/parens/angle
    /// brackets around it. See `visual_md.rs`'s `link_style` for why this is
    /// the one span style still allowed a distinct color.
    Link,
    /// A `#tag`, including the `#`.
    Tag,
}

/// The callout types the spec calls out by name; anything else still renders
/// as a callout (title capitalized, generic tint) via `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalloutKind {
    Note,
    Tip,
    Warning,
    Danger,
    Other,
}

impl CalloutKind {
    /// The name settings and theme tokens use for this kind, which also
    /// applies to its aliases (`info` is a `Note`).
    pub fn canonical_name(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Tip => "tip",
            Self::Warning => "warning",
            Self::Danger => "danger",
            Self::Other => "other",
        }
    }

    /// The kind a manifest names, by its canonical name or an alias, or `None`
    /// for a name that is neither.
    pub fn from_declared_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "other" => Some(Self::Other),
            known => match Self::from_type_name(known) {
                Self::Other => None,
                kind => Some(kind),
            },
        }
    }

    fn from_type_name(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "note" | "info" => Self::Note,
            "tip" | "success" | "hint" => Self::Tip,
            "warning" | "caution" => Self::Warning,
            "danger" | "error" | "bug" | "failure" => Self::Danger,
            _ => Self::Other,
        }
    }
}

/// A callout's `+`/`-` fold-state suffix, right after `[!type]`. `None` and
/// `Expanded` render identically (both open) -- kept distinct so the
/// fold-toggle button in visual_md.rs never writes a redundant `+` back to a
/// buffer that never had one: collapsing then re-expanding a bare `[!note]`
/// leaves it bare, not `[!note]+`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalloutFold {
    None,
    Expanded,
    Collapsed,
}

impl CalloutFold {
    pub fn is_collapsed(self) -> bool {
        matches!(self, CalloutFold::Collapsed)
    }
}

/// A parsed `> [!type]` callout, everything visual_md.rs needs to render its
/// title widget and body collapse without re-deriving anything from raw
/// text. See `detect_callout`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalloutInfo {
    /// `[!type]` plus any `+`/`-` suffix, as one range.
    pub marker_range: Range<usize>,
    pub kind: CalloutKind,
    /// The exact typed type name (e.g. `"todo"`), kept even when `kind`
    /// resolves to `Other` -- an unrecognized type still gets a real,
    /// specific title label, just generic styling.
    pub raw_type_name: String,
    pub fold: CalloutFold,
    /// Where a `+`/`-` character does (or, if `fold` is `None`, would) go --
    /// a zero-width range positioned right after `]` when absent. Editing
    /// this one range (an insert, a replace, or a delete to `""`) covers all
    /// three fold-toggle transitions with the same single-edit shape the
    /// checkbox toggle already uses.
    pub suffix_range: Range<usize>,
    /// The whole `block_quote` node's range, used for the background tint so
    /// it covers the title row too, not just the body.
    pub node_range: Range<usize>,
    /// From the end of `marker_range` to the end of `node_range` -- what
    /// gets collapsed into an ellipsis crease when `fold` is `Collapsed`.
    pub body_range: Range<usize>,
    /// Whether a selection touches `marker_range`. When `true`,
    /// `plan_block_quote` reveals it as raw, dimmed text (like every other
    /// hideable construct) instead of visual_md.rs rendering its title
    /// widget for it -- the callout's raw-text editing affordance, standing
    /// in for a right-click "change type" menu this crate doesn't build.
    pub touched: bool,
}

/// What kind of typographic marker a `glyph_markers` entry replaces. Kept as
/// a real enum rather than a raw glyph string so the applying side
/// (`visual_md.rs`) can render a bullet/blockquote-bar as an actual drawn
/// shape and a checkbox-adjacent ordinal as text, without string-sniffing
/// glyph content to figure out which is which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlyphKind {
    /// An unordered list item's `-`/`*`/`+` marker.
    Bullet,
    /// An ordered list item's marker, renumbered and formatted (e.g. `"3. "`
    /// or `"2) "`) — the only glyph kind that's still genuinely text, since
    /// digits have no font-coverage risk the way symbol glyphs do.
    Ordinal(String),
    /// A blockquote or callout's `>` marker, one per nesting level and one
    /// per continuation line.
    BlockquoteBar,
    /// A GFM pipe table's `|` column separator, in a header or data row
    /// (never the delimiter row, which is handled entirely differently --
    /// see `TableInfo`'s own doc comment). Always folded regardless of
    /// selection, the same as `BlockquoteBar`.
    TablePipe,
}

/// A `[[wikilink]]` as the plan sees it: what it shows, and the note it names.
/// Whether that note exists is for the applying side to decide, since this
/// module knows nothing of projects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikilinkSpan {
    /// The text the link shows, which the brackets around it were hidden for.
    pub range: Range<usize>,
    /// The note part of the target, without a `#heading` or `#^block`. Empty for
    /// a link to a heading of the same note, `[[#Heading]]`.
    pub note: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Marker byte ranges to fold away because no selection touches their
    /// containing construct.
    pub hidden_markers: Vec<Range<usize>>,
    /// Marker byte ranges to render, but dimmed: either a selection touches
    /// their construct, or (inline code) they are always shown this way.
    pub dimmed_markers: Vec<Range<usize>>,
    /// Content byte ranges (markers excluded) that get a persistent style.
    pub styled_spans: Vec<(Range<usize>, SpanStyle)>,
    /// The `[[wikilinks]]` (not embeds) in view, whose brackets are in
    /// `hidden_markers` or `dimmed_markers` like a link's.
    pub wikilinks: Vec<WikilinkSpan>,
    /// Marker byte ranges replaced with a specific glyph (list bullets and
    /// renumbered ordinals, blockquote/callout left bars) — unlike
    /// `hidden_markers`, these are always folded regardless of selection,
    /// since a typographic marker isn't "raw source syntax" the way
    /// `**`/`#`/`>` are; there's nothing to reveal by touching them. A
    /// callout's own `[!type]` title is deliberately *not* here (see
    /// `callouts` below) — it needs to reveal as raw text on touch, so a
    /// user can retype the type name or its fold suffix directly.
    pub glyph_markers: Vec<(Range<usize>, GlyphKind)>,
    /// Byte ranges of `[ ]`/`[x]` task markers, with their current checked
    /// state (from which grammar node matched, `task_list_marker_checked`
    /// vs. `_unchecked`). These get a real interactive checkbox widget
    /// rather than a plain glyph substitution, since the spec requires them
    /// to stay clickable in both raw and rendered modes, unconditionally.
    /// The checked bool travels with the range (rather than the widget
    /// re-reading the live buffer at render time) deliberately: render runs
    /// *during* the editor's own paint pass, so re-entrantly reading that
    /// same editor entity from inside it is a real borrow conflict, not a
    /// hypothetical one — confirmed the hard way (see `checkbox_placeholder`
    /// in visual_md.rs for the fix this drives on the applying side).
    pub checkboxes: Vec<(Range<usize>, bool)>,
    /// Byte ranges of task marks other than `[ ]` and `[x]`, such as `[/]`, with
    /// the character between the brackets. Only marks in
    /// `PlanExtensions::task_marks` are here, since the item's own bullet is
    /// hidden for a checkbox that is going to be drawn in its place.
    pub task_marks: Vec<(Range<usize>, char)>,
    /// Byte ranges of `thematic_break` nodes (`---`, `***`, `___`) that
    /// aren't touched by a selection. Unlike every other category above,
    /// these don't become a fold at all — a full-width `<hr>` needs the
    /// editor's block-decoration API (`insert_blocks`), not an inline
    /// `FoldPlaceholder`, since a fold can only size itself to its own
    /// content, never to the line's actual available width. See
    /// `apply_horizontal_rules` in visual_md.rs. A touched thematic_break is
    /// simply left out of this list, so its raw `---` shows through exactly
    /// like an untouched-vs-touched heading marker.
    pub horizontal_rules: Vec<Range<usize>>,
    /// One entry per fenced-code-block *line* (the opening ` ``` `+info
    /// string, or the closing ` ``` `) not touched by a selection; the
    /// `Option<String>` is the trimmed language name, `Some` only for the
    /// opening line. Rendered the same way as `horizontal_rules` (a
    /// full-width `insert_blocks` border, plus a language chip on the
    /// opening one) and for the same reason — a fold can't stretch to the
    /// real editor width. A touched line is simply left out, so its raw
    /// ` ``` ` shows through.
    pub code_fence_borders: Vec<(Range<usize>, Option<String>)>,
    /// One entry per fenced code block's `code_fence_content`, with its
    /// trimmed language name if any — *always* present regardless of
    /// selection, unlike every other category here: per spec, a code
    /// block's content highlighting never toggles off, only the fence lines
    /// do. Byte ranges only; the actual `Language`/syntax highlighting is
    /// necessarily computed in visual_md.rs (this module has no GPUI/
    /// `Language` dependency by design), see `apply_code_syntax_highlights`.
    pub code_fence_content: Vec<(Range<usize>, Option<String>)>,
    /// GFM pipe tables. Unlike every other category here, table cells are
    /// never hidden or block-replaced -- every cell stays normal, always-
    /// editable inline text (see `TableInfo`'s own doc comment for why).
    pub tables: Vec<TableInfo>,
    /// Parsed `> [!type]` callouts (see `CalloutInfo`), one per callout
    /// regardless of touch state -- visual_md.rs still needs an untouched-but-
    /// collapsed callout's `body_range` even while its title happens to be
    /// showing raw (touched) text.
    pub callouts: Vec<CalloutInfo>,
    /// Lines that hold nothing but one embed (an image `![alt](path)`, or the
    /// Obsidian-style `![[name]]` of a note, image, audio, video or PDF file)
    /// and aren't touched by a selection. Rendered as a block that replaces the
    /// whole line, for the same reason `horizontal_rules` are: an image or a
    /// note needs the editor's real width, which a fold can't stretch to. See
    /// `embeds.rs`.
    pub embeds: Vec<EmbedInfo>,
    /// Fenced code blocks an extension renders in place of the block, which a
    /// selection does not touch. Such a block gets neither borders nor
    /// `code_fence_content`: the extension's output stands in for all of it.
    pub rendered_fences: Vec<RenderedFence>,
    /// Styles that extensions' syntax rules give to ranges of text.
    pub extension_styled: Vec<(Range<usize>, VisualMdSpanStyle)>,
    /// Ranges that extensions' syntax rules replace with other text while no
    /// selection touches their match. Each is on one line.
    pub extension_replacements: Vec<(Range<usize>, String)>,
    /// Matches of dynamic rules that no extension has answered for yet, once
    /// each, for the caller to ask about.
    pub missing_rule_inputs: Vec<MissingRuleInput>,
    /// Scratch for the three lists below: what extensions' rules want hidden,
    /// dimmed and replaced, filled in while walking and resolved against Zed
    /// MD's own decorations before the plan is returned, which leaves them empty.
    candidate_hidden: Vec<Range<usize>>,
    candidate_dimmed: Vec<Range<usize>>,
    candidate_replacements: Vec<(Range<usize>, String)>,
}

/// A match of a dynamic rule waiting for an extension's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingRuleInput {
    pub key: DynamicKey,
    /// The capture groups as they were where the match was first found. An
    /// answer is filed under the match's text alone, so the first one wins.
    pub captures: Vec<Option<Range<usize>>>,
}

/// A fenced code block that an extension renders, see `Plan::rendered_fences`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFence {
    /// From the start of the opening fence to the end of the closing fence,
    /// without the closing line's newline, so a cursor at the start of the next
    /// line does not touch it.
    pub range: Range<usize>,
    /// The lowercased language tag.
    pub language: String,
    /// The info string after the opening fence, trimmed.
    pub info: String,
    pub content_range: Range<usize>,
    /// A hash of the content, so a change to it that leaves the ranges alone
    /// (an undo, a replace elsewhere) still makes the plan differ.
    pub content_hash: u64,
}

/// What a standalone embed line points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmbedKind {
    Image,
    /// A note, to be shown in place. A `![[name]]` whose name has an extension
    /// that is not one of the kinds below is also planned as a note, since a
    /// dot is as likely to be part of a note's name (`Notes 1.2`); what the
    /// name resolves to decides.
    Note,
    Audio,
    Video,
    Pdf,
    /// A file of another kind, named by a Markdown link.
    Other,
}

impl EmbedKind {
    /// The kind of file with this extension, for the extensions that have one.
    pub fn for_extension(extension: &str) -> Option<Self> {
        let extension = extension.to_ascii_lowercase();
        let kind = match extension.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "avif" => Self::Image,
            "md" | "markdown" => Self::Note,
            "mp3" | "wav" | "m4a" | "ogg" | "oga" | "flac" | "opus" | "3gp" => Self::Audio,
            "mp4" | "webm" | "ogv" | "mov" | "mkv" => Self::Video,
            "pdf" => Self::Pdf,
            _ => return None,
        };
        Some(kind)
    }
}

/// The part of a note that an embed or a link names after its `#`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Subpath {
    /// A heading, as written. With several `#` it is everything after the first.
    Heading(String),
    /// A block id, without the `^`.
    Block(String),
}

/// The size an embed asks for with `|300` or `|300x200`, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EmbedSize {
    pub width: u32,
    pub height: Option<u32>,
}

/// A standalone embed line, see `Plan::embeds`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedInfo {
    /// The line's byte range with leading indentation and the newline
    /// excluded.
    pub range: Range<usize>,
    /// The path, URL or note name as written, without any `|size` and, for a
    /// `![[name]]`, without any `#heading` or `#^block`. Empty for `![[#Heading]]`,
    /// which names a part of the note being edited.
    pub target: String,
    /// Whether this came from a `![[name]]` embed, which Obsidian resolves by
    /// name anywhere in the vault rather than strictly relative to the note.
    pub is_wikilink: bool,
    pub kind: EmbedKind,
    pub subpath: Option<Subpath>,
    pub size: Option<EmbedSize>,
}

/// A column's alignment, from its `pipe_table_delimiter_cell`
/// (`pipe_table_align_left`/`_right`, both present = center, neither =
/// default/unspecified).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAlignment {
    Default,
    Left,
    Center,
    Right,
}

/// One `pipe_table_cell`'s byte ranges. `content` is the cell's own node
/// range with its captured trailing whitespace trimmed back off; `leading_gap`
/// (between the previous `|` and this cell's first byte) and `trailing_gap`
/// (the whitespace this trimming just removed, or empty if there was none)
/// are the two places visual_md.rs can widen into a computed-width spacer to
/// align this column -- either may be empty, meaning there's no existing
/// whitespace there to widen (that side just doesn't get padding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCell {
    pub leading_gap: Range<usize>,
    pub content: Range<usize>,
    pub trailing_gap: Range<usize>,
}

/// A `pipe_table`. Every `|` in a header/data row always folds to a bar
/// glyph regardless of selection -- pushed straight into `Plan::glyph_markers`
/// as `GlyphKind::TablePipe`, the same unconditional treatment
/// `plan_block_quote` already gives `>` (see `GlyphKind::BlockquoteBar`'s own
/// doc comment: "there's nothing to reveal by touching them"), rather than
/// duplicated here. Cell text itself is never in `hidden_markers`/
/// `dimmed_markers` at all: it was never hidden, so there's nothing to
/// toggle when the cursor enters a cell -- that's what gives this design
/// genuine per-cell editing without a rendered/raw mode switch. Column width
/// alignment and the header/body divider are computed by visual_md.rs
/// (`apply_table_alignment`/`apply_table_dividers`), which needs real text
/// measurement and the block-decoration API this module deliberately has no
/// dependency on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    /// One entry per column, from the delimiter row.
    pub alignments: Vec<TableAlignment>,
    /// The delimiter row's own line range, only if it isn't touched by a
    /// selection (same convention as `horizontal_rules`) -- a touched
    /// delimiter line is left out entirely so its raw `|---|---:|` shows
    /// through for editing (e.g. to add a column or change alignment).
    pub delimiter_line: Option<Range<usize>>,
    /// One entry per row (header first, then each data row in order), each
    /// with one `TableCell` per column.
    pub rows: Vec<Vec<TableCell>>,
}

/// Computes the live-preview decoration plan for `text`, given the current
/// selections as byte ranges (collapsed cursors are zero-width ranges).
///
/// Equivalent to [`plan_viewport`] with a visible range spanning the whole
/// document — see that function's doc comment for why a caller with an
/// actual viewport should prefer it instead. `visual_md.rs` never calls this
/// (it always has a real viewport), so it's only exercised by this module's
/// own tests below -- kept `pub` as a convenience non-viewport-scoped entry
/// point for exactly that, rather than making every one of those dozens of
/// `plan(text, &[])` calls spell out `plan_viewport(text, selections,
/// 0..text.len())` instead.
#[allow(dead_code)]
pub fn plan(text: &str, selections: &[Range<usize>]) -> Plan {
    plan_viewport(text, selections, 0..text.len())
}

/// Computes the live-preview decoration plan for `text`, restricted to
/// constructs that intersect `visible_range` (plus whatever selections add,
/// see below).
///
/// The whole document is still parsed once — tree-sitter is fast enough that
/// re-parsing on every keystroke is not the actual cost problem for large
/// files. The real cost is downstream: every decoration this planner emits
/// becomes a crease or a `highlight_text` range in the real editor, and
/// creating/diffing thousands of those for a multi-MB file on every
/// keystroke is what makes typing feel laggy. So the walk itself prunes any
/// block-level node (heading, paragraph, list, blockquote, and anything
/// nested inside them) whose byte range doesn't overlap `visible_range` at
/// all, skipping its entire subtree — no decorations are emitted for it, and
/// no inline reparse happens for its content either. A node that does
/// overlap, even partially, is processed in full (so e.g. an ordered list
/// that's mostly offscreen still renumbers correctly for the part that
/// isn't).
///
/// `visible_range` is unioned with every selection's own range before
/// pruning, not used as-is: a selection should always reveal its construct's
/// raw markers regardless of scroll position (this matters if the caller's
/// notion of "visible" is ever stale relative to where the cursor actually
/// is; cheap to guard against unconditionally).
pub fn plan_viewport(text: &str, selections: &[Range<usize>], visible_range: Range<usize>) -> Plan {
    let Some(block_tree) = parse_blocks(text) else {
        return Plan::default();
    };
    plan_viewport_with_tree(text, &block_tree, selections, visible_range)
}

/// Parses `text`'s Markdown block structure. Split out of [`plan_viewport`] so
/// a caller planning repeatedly against unchanged text (scrolling, cursor
/// moves) can parse once and reuse the tree.
pub fn parse_blocks(text: &str) -> Option<Tree> {
    let mut block_parser = Parser::new();
    block_parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .ok()?;
    block_parser.parse(text, None)
}

/// [`plan_viewport`] against an already-parsed `block_tree` of `text`.
pub fn plan_viewport_with_tree(
    text: &str,
    block_tree: &Tree,
    selections: &[Range<usize>],
    visible_range: Range<usize>,
) -> Plan {
    plan_viewport_with_extensions(
        text,
        block_tree,
        selections,
        visible_range,
        &PlanExtensions::default(),
    )
}

/// What extensions claim, as far as planning is concerned. Kept as plain data
/// so this module still knows nothing about extensions or GPUI.
#[derive(Debug, Clone, Default)]
pub struct PlanExtensions {
    /// The lowercased language tags of fenced code blocks that an extension
    /// renders in place of the block.
    pub rendered_fence_languages: HashSet<String>,
    /// The syntax rules in force, and what extensions have answered for them.
    pub rules: RuleSet,
    /// The characters of the task marks, besides `[ ]` and `[x]`, that get a
    /// checkbox of their own.
    pub task_marks: HashSet<char>,
    /// The `[label]: destination` definitions of the document, as
    /// [`link_definitions`] finds them, when the caller already has them. They
    /// are looked for in the block parse when it does not.
    pub link_definitions: Option<Arc<HashMap<String, String>>>,
}

/// [`plan_viewport_with_tree`], also planning what `extensions` claim.
pub fn plan_viewport_with_extensions(
    text: &str,
    block_tree: &Tree,
    selections: &[Range<usize>],
    visible_range: Range<usize>,
    extensions: &PlanExtensions,
) -> Plan {
    let mut inline_parser = Parser::new();
    let Ok(()) = inline_parser.set_language(&tree_sitter_md::INLINE_LANGUAGE.into()) else {
        return Plan::default();
    };

    let mut visible_range = visible_range;
    for selection in selections {
        visible_range.start = visible_range.start.min(selection.start);
        visible_range.end = visible_range.end.max(selection.end);
    }

    let with_definitions;
    let extensions = if extensions.link_definitions.is_some() {
        extensions
    } else {
        with_definitions = PlanExtensions {
            link_definitions: Some(Arc::new(link_definitions(text, block_tree))),
            ..extensions.clone()
        };
        &with_definitions
    };

    let mut plan = Plan::default();
    walk_block(
        block_tree.root_node(),
        text,
        selections,
        &mut inline_parser,
        &visible_range,
        extensions,
        &mut plan,
    );

    // Standalone images are planned before the extension candidates are
    // resolved, since their rows are among the ranges extensions must stay off.
    plan_embeds(text, selections, &visible_range, &mut plan);
    resolve_extension_candidates(&mut plan);

    // Nested constructs (`***bold italic***`, and the grammar's own
    // self-nested `~~strikethrough~~` representation) each contribute their
    // own marker ranges independently, so an outer and inner construct can
    // produce genuinely overlapping ranges (e.g. an outer emphasis's merged
    // "***" prefix fully contains an inner strong_emphasis's "**" prefix). A
    // fold can't be created over an already-folded sub-range — Zed's own
    // display-map code assumes disjoint fold regions and panics otherwise —
    // so this must be resolved to a disjoint partition before it's usable.
    // A dimmed (cursor-touched) determination always wins over hidden for
    // the same bytes, since it's the more conservative/correct choice when
    // constructs disagree about whether the cursor is "in" them.
    plan.hidden_markers = merge_ranges(plan.hidden_markers);
    plan.dimmed_markers = merge_ranges(plan.dimmed_markers);
    plan.hidden_markers = subtract_ranges(&plan.hidden_markers, &plan.dimmed_markers);

    let mut seen_inputs = HashSet::new();
    plan.missing_rule_inputs
        .retain(|input| seen_inputs.insert(input.key.clone()));

    plan
}

/// Finds lines consisting solely of an embed. Done as a line scan rather than
/// through the tree: `![[embed]]` isn't markdown grammar at all, and a
/// standalone embed line is the only shape that can sensibly be swapped for a
/// block. Fenced code is skipped by tracking the fence markers directly.
fn plan_embeds(
    text: &str,
    selections: &[Range<usize>],
    visible_range: &Range<usize>,
    plan: &mut Plan,
) {
    let mut offset = 0;
    let mut open_fence: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let content = line.trim_end_matches(['\n', '\r']);
        let trimmed = content.trim_start_matches(' ');
        let indentation = content.len() - trimmed.len();
        if indentation >= 4 {
            continue;
        }

        let fence_character = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'));
        if let Some(fence_character) = fence_character {
            let run = trimmed
                .chars()
                .take_while(|c| *c == fence_character)
                .count();
            if run >= 3 {
                match open_fence {
                    None => open_fence = Some((fence_character, run)),
                    Some((open_character, open_run))
                        if open_character == fence_character
                            && run >= open_run
                            && trimmed[run..].trim().is_empty() =>
                    {
                        open_fence = None;
                    }
                    Some(_) => {}
                }
                continue;
            }
        }
        if open_fence.is_some() {
            continue;
        }

        let range = (line_start + indentation)..(line_start + content.trim_end().len());
        if range.start >= range.end
            || !overlaps(&range, visible_range) && !visible_range.is_empty()
            || touches_selection(&range, selections)
        {
            continue;
        }
        if let Some(embed) = parse_embed_line(&text[range.clone()]) {
            plan.embeds.push(EmbedInfo {
                range,
                target: embed.target,
                is_wikilink: embed.is_wikilink,
                kind: embed.kind,
                subpath: embed.subpath,
                size: embed.size,
            });
        }
    }
}

/// What `parse_embed_line` found on a line, before it has a range.
struct ParsedEmbed {
    target: String,
    is_wikilink: bool,
    kind: EmbedKind,
    subpath: Option<Subpath>,
    size: Option<EmbedSize>,
}

/// `line` must already be trimmed.
fn parse_embed_line(line: &str) -> Option<ParsedEmbed> {
    if line.starts_with("![[") {
        let link = inline_scan::find_wikilinks(line, 0, &[])
            .into_iter()
            .next()
            .filter(|link| link.is_embed && link.range == (0..line.len()))?;
        let (name, subpath) = split_subpath(&link.target);
        if name.is_empty() && subpath.is_none() {
            return None;
        }
        // A name with an extension that is no kind of its own is still taken for a
        // note, since a dot is as likely to be part of its name.
        let kind = file_extension(name)
            .and_then(|extension| EmbedKind::for_extension(&extension))
            .unwrap_or(EmbedKind::Note);
        let size = link.alias.as_deref().and_then(parse_size);
        return Some(ParsedEmbed {
            target: name.to_string(),
            is_wikilink: true,
            kind,
            subpath,
            size,
        });
    }

    let rest = line.strip_prefix("![")?;
    let alt_end = rest.find("](")?;
    let alt = &rest[..alt_end];
    if alt.contains(['[', ']']) {
        return None;
    }
    let destination = rest[alt_end + 2..].strip_suffix(')')?;
    if destination.contains(['(', ')']) && !destination.starts_with('<') {
        return None;
    }
    let destination = destination.trim();
    let destination = match destination.strip_prefix('<') {
        Some(bracketed) => bracketed.split('>').next()?,
        None => destination.split_whitespace().next()?,
    };
    if destination.is_empty() {
        return None;
    }
    let is_url = destination.starts_with("http://") || destination.starts_with("https://");
    let kind = match file_extension(destination)
        .and_then(|extension| EmbedKind::for_extension(&extension))
    {
        Some(kind) => kind,
        // An address with no known extension may still serve an image.
        None if is_url => EmbedKind::Image,
        None if file_extension(destination).is_none() => EmbedKind::Image,
        None => EmbedKind::Other,
    };
    let size = alt.rsplit_once('|').and_then(|(_, size)| parse_size(size));
    Some(ParsedEmbed {
        target: destination.to_string(),
        is_wikilink: false,
        kind,
        subpath: None,
        size,
    })
}

/// A wikilink target split into the note's name and the part of it named
/// after the first `#`.
pub(crate) fn split_subpath(target: &str) -> (&str, Option<Subpath>) {
    let Some((name, rest)) = target.split_once('#') else {
        return (target.trim(), None);
    };
    let rest = rest.trim();
    let subpath = match rest.strip_prefix('^') {
        Some(id) if !id.is_empty() => Some(Subpath::Block(id.trim().to_string())),
        _ if rest.is_empty() => None,
        _ => Some(Subpath::Heading(rest.to_string())),
    };
    (name.trim(), subpath)
}

/// The extension of the file a path or address names, lowercased: letters and
/// digits after the last dot of its last component, without a query or fragment.
pub(crate) fn file_extension(target: &str) -> Option<String> {
    let path = target.split(['?', '#']).next().unwrap_or(target);
    let name = path.rsplit(['/', '\\']).next()?;
    let (stem, extension) = name.rsplit_once('.')?;
    let is_extension = !stem.is_empty()
        && !extension.is_empty()
        && extension.len() <= 5
        && extension
            .chars()
            .all(|character| character.is_ascii_alphanumeric());
    is_extension.then(|| extension.to_ascii_lowercase())
}

/// `300` or `300x200`, or the last part after a `|` of text that has one.
fn parse_size(text: &str) -> Option<EmbedSize> {
    let text = text.rsplit('|').next()?.trim();
    let (width, height) = match text.split_once(['x', 'X']) {
        Some((width, height)) => (width, Some(height)),
        None => (text, None),
    };
    let valid = |number: &str| {
        number
            .parse::<u32>()
            .ok()
            .filter(|number| (1..=10_000).contains(number))
    };
    Some(EmbedSize {
        width: valid(width)?,
        height: match height {
            Some(height) => Some(valid(height)?),
            None => None,
        },
    })
}

/// Sorts and merges overlapping/touching ranges into a minimal disjoint set.
fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.retain(|range| !range.is_empty());
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// Removes from `ranges` any portion overlapping `subtract`. Both inputs must
/// already be disjoint and sorted by start (as [`merge_ranges`] produces).
fn subtract_ranges(ranges: &[Range<usize>], subtract: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    for range in ranges {
        let mut cursor = range.start;
        for sub in subtract {
            if sub.end <= cursor || sub.start >= range.end {
                continue;
            }
            if sub.start > cursor {
                result.push(cursor..sub.start.min(range.end));
            }
            cursor = cursor.max(sub.end);
            if cursor >= range.end {
                break;
            }
        }
        if cursor < range.end {
            result.push(cursor..range.end);
        }
    }
    result
}

/// Turns one match of an extension's syntax rule into what the plan holds: its
/// style, the capture groups it hides, and whatever the extension has answered
/// for it, which is asked about if it has not been yet. While a selection
/// touches the match, what would be hidden is dimmed instead and nothing is
/// replaced, so the source can be edited.
fn plan_rule_hit(hit: RuleHit, selections: &[Range<usize>], rule_set: &RuleSet, plan: &mut Plan) {
    let visible_length = hit.text.trim_end_matches(['\n', '\r']).len();
    let touched = touches_selection(
        &(hit.range.start..hit.range.start + visible_length),
        selections,
    );
    let shift_to_document =
        |range: &Range<usize>| (range.start + hit.range.start)..(range.end + hit.range.start);
    let hide = |plan: &mut Plan, range: Range<usize>| {
        if touched {
            plan.candidate_dimmed.push(range);
        } else {
            plan.candidate_hidden.push(range);
        }
    };

    if let Some(style) = &hit.rule.style {
        plan.extension_styled
            .push((hit.range.clone(), style.clone()));
    }
    for group in &hit.rule.hide {
        if let Some(Some(range)) = hit.captures.get(*group)
            && !range.is_empty()
        {
            hide(plan, shift_to_document(range));
        }
    }

    if !hit.rule.dynamic {
        return;
    }
    let key = DynamicKey {
        extension_id: hit.rule.extension_id.clone(),
        generation: hit.rule.generation,
        rule: hit.rule.id.clone(),
        text: hit.text.clone(),
    };
    match rule_set.results.get(&key) {
        Some(DynamicResult::Ready(effects)) => {
            for (range, style) in &effects.styled {
                plan.extension_styled
                    .push((shift_to_document(range), style.clone()));
            }
            for range in &effects.hidden {
                hide(plan, shift_to_document(range));
            }
            if !touched {
                for (range, replacement) in &effects.replacements {
                    plan.candidate_replacements
                        .push((shift_to_document(range), replacement.clone()));
                }
            }
        }
        Some(DynamicResult::Failed) => {}
        None => plan.missing_rule_inputs.push(MissingRuleInput {
            key,
            captures: hit.captures.clone(),
        }),
    }
}

/// Sorts `ranges` and drops the empty ones and any that overlap an earlier one.
fn without_overlaps<T>(mut items: Vec<(Range<usize>, T)>) -> Vec<(Range<usize>, T)> {
    items.sort_by_key(|(range, _)| (range.start, range.end));
    let mut covered_until = 0;
    items.retain(|(range, _)| {
        let keep = !range.is_empty() && range.start >= covered_until;
        if keep {
            covered_until = range.end;
        }
        keep
    });
    items
}

fn overlaps_any(sorted_disjoint: &[Range<usize>], range: &Range<usize>) -> bool {
    let first = sorted_disjoint.partition_point(|candidate| candidate.end <= range.start);
    sorted_disjoint
        .get(first)
        .is_some_and(|candidate| candidate.start < range.end)
}

/// Settles what extensions' rules wanted hidden, dimmed and replaced against
/// everything Zed MD's own decorations occupy. Zed MD's win: a range of an
/// extension that overlaps one of them is dropped, because two folds over the
/// same text would panic the editor and a hidden marker must not be revealed by
/// someone else's styling. Among extensions' own ranges, the leftmost wins.
fn resolve_extension_candidates(plan: &mut Plan) {
    let hidden = std::mem::take(&mut plan.candidate_hidden);
    let dimmed = std::mem::take(&mut plan.candidate_dimmed);
    let replacements = std::mem::take(&mut plan.candidate_replacements);
    if hidden.is_empty() && dimmed.is_empty() && replacements.is_empty() {
        return;
    }

    let mut occupied: Vec<Range<usize>> = plan
        .hidden_markers
        .iter()
        .chain(&plan.dimmed_markers)
        .chain(&plan.horizontal_rules)
        .cloned()
        .chain(plan.glyph_markers.iter().map(|(range, _)| range.clone()))
        .chain(plan.checkboxes.iter().map(|(range, _)| range.clone()))
        .chain(plan.task_marks.iter().map(|(range, _)| range.clone()))
        .chain(
            plan.code_fence_borders
                .iter()
                .map(|(range, _)| range.clone()),
        )
        .chain(plan.embeds.iter().map(|embed| embed.range.clone()))
        .chain(plan.rendered_fences.iter().map(|fence| fence.range.clone()))
        .collect();
    for callout in &plan.callouts {
        occupied.push(callout.marker_range.clone());
        if callout.fold.is_collapsed() {
            occupied.push(callout.body_range.clone());
        }
    }
    for table in &plan.tables {
        occupied.extend(table.delimiter_line.iter().cloned());
        for cell in table.rows.iter().flatten() {
            occupied.push(cell.leading_gap.clone());
            occupied.push(cell.trailing_gap.clone());
        }
    }
    let occupied = merge_ranges(occupied);
    let is_clear = |range: &Range<usize>| !overlaps_any(&occupied, range);

    let hidden = without_overlaps(
        hidden
            .into_iter()
            .filter(is_clear)
            .map(|range| (range, ()))
            .collect(),
    );
    let dimmed = without_overlaps(
        dimmed
            .into_iter()
            .filter(is_clear)
            .map(|range| (range, ()))
            .collect(),
    );
    let hidden: Vec<Range<usize>> = hidden.into_iter().map(|(range, ())| range).collect();
    let dimmed: Vec<Range<usize>> = dimmed.into_iter().map(|(range, ())| range).collect();
    let touches_extension_range =
        |range: &Range<usize>| overlaps_any(&hidden, range) || overlaps_any(&dimmed, range);

    plan.extension_replacements = without_overlaps(
        replacements
            .into_iter()
            .filter(|(range, _)| is_clear(range) && !touches_extension_range(range))
            .collect(),
    );
    plan.hidden_markers.extend(hidden);
    plan.dimmed_markers.extend(dimmed);
}

fn hash_text(text: &str) -> u64 {
    use std::hash::{DefaultHasher, Hasher};

    let mut hasher = DefaultHasher::new();
    hasher.write(text.as_bytes());
    hasher.finish()
}

/// `end`, or the offset of the newline just before it when the range it ends
/// includes its line's newline.
fn end_of_line_content(text: &str, end: usize) -> usize {
    match end.checked_sub(1) {
        Some(newline) if text.as_bytes().get(newline) == Some(&b'\n') => newline,
        _ => end,
    }
}

fn touches_selection(range: &Range<usize>, selections: &[Range<usize>]) -> bool {
    selections
        .iter()
        .any(|selection| selection.start <= range.end && selection.end >= range.start)
}

/// Inclusive-ish overlap test: touching/zero-width ranges count as
/// overlapping, so pruning only ever risks decorating a few extra bytes at a
/// viewport boundary, never dropping a decoration that should be there.
fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start <= b.end && a.end >= b.start
}

fn walk_block(
    node: Node,
    text: &str,
    selections: &[Range<usize>],
    inline_parser: &mut Parser,
    visible_range: &Range<usize>,
    extensions: &PlanExtensions,
    plan: &mut Plan,
) {
    if !overlaps(&node.byte_range(), visible_range) {
        return;
    }
    for rule in extensions.rules.node_rules(node.kind()) {
        if let Some(hit) = rules::node_hit(rule, node.byte_range(), text) {
            plan_rule_hit(hit, selections, &extensions.rules, plan);
        }
    }
    match node.kind() {
        "atx_heading" => {
            plan_heading(node, text, selections, inline_parser, extensions, plan);
            return;
        }
        "inline" | "pipe_table_cell" => {
            plan_inline(node, text, selections, inline_parser, extensions, plan);
            return;
        }
        "list" => {
            plan_list(
                node,
                text,
                selections,
                inline_parser,
                visible_range,
                extensions,
                plan,
            );
            return;
        }
        "block_quote" => {
            plan_block_quote(
                node,
                text,
                selections,
                inline_parser,
                visible_range,
                extensions,
                plan,
            );
            return;
        }
        "thematic_break" => {
            // Without the line's newline: the rule is a one-row block, and a
            // range that ends after the newline ends on the next row, which
            // the block would then swallow too.
            let range = node.start_byte()..end_of_line_content(text, node.end_byte());
            if !touches_selection(&range, selections) {
                plan.horizontal_rules.push(range);
            }
            return;
        }
        "fenced_code_block" => {
            plan_fenced_code_block(node, text, selections, extensions, plan);
            return;
        }
        "pipe_table" => {
            plan_pipe_table(
                node,
                text,
                selections,
                inline_parser,
                visible_range,
                extensions,
                plan,
            );
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_block(
            child,
            text,
            selections,
            inline_parser,
            visible_range,
            extensions,
            plan,
        );
    }
}

fn heading_level(node: Node) -> Option<u8> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "atx_h1_marker" => return Some(1),
            "atx_h2_marker" => return Some(2),
            "atx_h3_marker" => return Some(3),
            "atx_h4_marker" => return Some(4),
            "atx_h5_marker" => return Some(5),
            "atx_h6_marker" => return Some(6),
            _ => {}
        }
    }
    None
}

/// `range` cut off at its first newline. A block node's byte range includes its
/// trailing `\n`, and `touches_selection` is inclusive at both ends, so testing
/// the whole node would also count a cursor at column 0 of the next line.
fn first_line(range: &Range<usize>, text: &str) -> Range<usize> {
    let end = text
        .get(range.clone())
        .and_then(|node_text| node_text.find('\n'))
        .map_or(range.end, |offset| range.start + offset);
    range.start..end
}

fn plan_heading(
    node: Node,
    text: &str,
    selections: &[Range<usize>],
    inline_parser: &mut Parser,
    extensions: &PlanExtensions,
    plan: &mut Plan,
) {
    let Some(level) = heading_level(node) else {
        return;
    };
    let node_range = node.byte_range();
    let content = node.child_by_field_name("heading_content");
    let content_start = content
        .map(|n| n.byte_range().start)
        .unwrap_or(node_range.end);
    let marker_range = node_range.start..content_start;

    if touches_selection(&first_line(&node_range, text), selections) {
        plan.dimmed_markers.push(marker_range);
    } else {
        plan.hidden_markers.push(marker_range);
    }

    if let Some(content) = content {
        let content_range = content.byte_range();
        if !content_range.is_empty() {
            plan.styled_spans
                .push((content_range, SpanStyle::Heading(level)));
        }
        plan_inline(content, text, selections, inline_parser, extensions, plan);
    }
}

pub(crate) const UNORDERED_MARKERS: [&str; 3] =
    ["list_marker_minus", "list_marker_plus", "list_marker_star"];
pub(crate) const ORDERED_MARKERS: [&str; 2] = ["list_marker_dot", "list_marker_parenthesis"];

/// Renders bullets as "• " and renumbers ordered lists visually (1, 2, 3...
/// regardless of the source's own digits, per spec), recursing into each
/// item's other content (paragraph text, nested lists/quotes) normally.
fn plan_list(
    node: Node,
    text: &str,
    selections: &[Range<usize>],
    inline_parser: &mut Parser,
    visible_range: &Range<usize>,
    extensions: &PlanExtensions,
    plan: &mut Plan,
) {
    let mut ordinal = first_ordinal(node, text);
    let mut cursor = node.walk();
    for item in node.children(&mut cursor) {
        if item.kind() != "list_item" {
            continue;
        }
        let mut item_cursor = item.walk();
        let children: Vec<Node> = item.children(&mut item_cursor).collect();
        let has_standard_task_marker = children.iter().any(|child| {
            matches!(
                child.kind(),
                "task_list_marker_checked" | "task_list_marker_unchecked"
            )
        });
        let custom_task_mark = (!has_standard_task_marker)
            .then(|| custom_task_mark(&children, text, &extensions.task_marks))
            .flatten();
        let has_task_marker = has_standard_task_marker || custom_task_mark.is_some();
        if let Some(mark) = custom_task_mark {
            plan.task_marks.push(mark);
        }

        for child in &children {
            match child.kind() {
                // A task item's bullet/ordinal is redundant once the
                // checkbox glyph takes over as the visual marker, so it's
                // just hidden rather than replaced with its usual glyph.
                kind if UNORDERED_MARKERS.contains(&kind) => {
                    if has_task_marker {
                        plan.hidden_markers.push(child.byte_range());
                    } else {
                        plan.glyph_markers
                            .push((child.byte_range(), GlyphKind::Bullet));
                    }
                }
                kind if ORDERED_MARKERS.contains(&kind) => {
                    let separator = if kind == "list_marker_parenthesis" {
                        ")"
                    } else {
                        "."
                    };
                    let n = ordinal.unwrap_or(1);
                    ordinal = Some(n + 1);
                    if has_task_marker {
                        plan.hidden_markers.push(child.byte_range());
                    } else {
                        plan.glyph_markers.push((
                            child.byte_range(),
                            GlyphKind::Ordinal(format!("{n}{separator} ")),
                        ));
                    }
                }
                "task_list_marker_checked" => {
                    plan.checkboxes.push((child.byte_range(), true));
                }
                "task_list_marker_unchecked" => {
                    plan.checkboxes.push((child.byte_range(), false));
                }
                _ => walk_block(
                    *child,
                    text,
                    selections,
                    inline_parser,
                    visible_range,
                    extensions,
                    plan,
                ),
            }
        }
    }
}

/// The `[c]` that starts a list item whose text begins with one, when `c` is one
/// of `known`. The grammar only knows `[ ]` and `[x]` as task markers, so any
/// other mark is plain text at the start of the item's paragraph.
fn custom_task_mark(
    children: &[Node],
    text: &str,
    known: &HashSet<char>,
) -> Option<(Range<usize>, char)> {
    if known.is_empty() {
        return None;
    }
    let paragraph = children.iter().find(|child| child.kind() == "paragraph")?;
    let start = paragraph.start_byte();
    let rest = text.get(start..)?;
    let mut characters = rest.chars();
    let (Some('['), Some(mark), Some(']')) =
        (characters.next(), characters.next(), characters.next())
    else {
        return None;
    };
    let after = characters.next();
    if !known.contains(&mark) || !after.is_none_or(|after| after == ' ' || after == '\n') {
        return None;
    }
    Some((start..start + mark.len_utf8() + 2, mark))
}

/// The display number an ordered list should start counting from, taken from
/// its first item's own literal digits (so `5. foo` starts a list at 5); `None`
/// for an unordered list.
fn first_ordinal(list_node: Node, text: &str) -> Option<u32> {
    let mut cursor = list_node.walk();
    let first_item = list_node
        .children(&mut cursor)
        .find(|n| n.kind() == "list_item")?;
    let mut item_cursor = first_item.walk();
    let marker = first_item
        .children(&mut item_cursor)
        .find(|child| ORDERED_MARKERS.contains(&child.kind()))?;
    let marker_text = text.get(marker.byte_range())?;
    marker_text.trim_end_matches([' ', '.', ')']).parse().ok()
}

/// Replaces each `>` (or, for a multi-line paragraph's continuation lines,
/// the `block_continuation` markers found within its inline tree — see
/// `walk_inline`) with a left-bar glyph. Detects a callout (`> [!type]`) on
/// the block's first line and tints its body accordingly; nested blockquotes
/// recurse naturally since each nesting level is its own `block_quote` node
/// with its own marker, stacking bars visually without extra bookkeeping.
fn plan_block_quote(
    node: Node,
    text: &str,
    selections: &[Range<usize>],
    inline_parser: &mut Parser,
    visible_range: &Range<usize>,
    extensions: &PlanExtensions,
    plan: &mut Plan,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "block_quote_marker" {
            plan.glyph_markers
                .push((child.byte_range(), GlyphKind::BlockquoteBar));
        }
    }

    if let Some(callout) = detect_callout(node, text, selections) {
        if callout.touched {
            plan.dimmed_markers.push(callout.marker_range.clone());
        }
        if !callout.node_range.is_empty() {
            plan.styled_spans
                .push((callout.node_range.clone(), SpanStyle::Callout(callout.kind)));
        }
        plan.callouts.push(callout);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "block_quote_marker" {
            walk_block(
                child,
                text,
                selections,
                inline_parser,
                visible_range,
                extensions,
                plan,
            );
        }
    }
}

/// Recognizes a ` ``` `/`~~~` fenced code block's structure: which lines are
/// its opening/closing fence (each independently hidden behind a full-width
/// border+chip via `apply_code_fence_borders` in visual_md.rs, unless the
/// cursor is touching that specific line) and its content range (always
/// exposed via `code_fence_content`, regardless of the cursor, since content
/// highlighting never toggles off per spec).
///
/// Bails out (the whole block is left completely unhandled, i.e. fully raw)
/// if the grammar didn't produce a `code_fence_content` child at all -- an
/// unterminated fence at end-of-file is the one realistic way that happens,
/// and it's not worth guessing at where the "content" would have ended.
fn plan_fenced_code_block(
    node: Node,
    text: &str,
    selections: &[Range<usize>],
    extensions: &PlanExtensions,
    plan: &mut Plan,
) {
    let mut opening_delimiter: Option<Range<usize>> = None;
    let mut closing_delimiter: Option<Range<usize>> = None;
    let mut info_string: Option<Range<usize>> = None;
    let mut content: Option<Range<usize>> = None;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "fenced_code_block_delimiter" => {
                if opening_delimiter.is_none() {
                    opening_delimiter = Some(child.byte_range());
                } else {
                    closing_delimiter = Some(child.byte_range());
                }
            }
            "info_string" => info_string = Some(child.byte_range()),
            "code_fence_content" => content = Some(child.byte_range()),
            _ => {}
        }
    }

    let Some(opening_delimiter) = opening_delimiter else {
        return;
    };
    let Some(content) = content else {
        return;
    };

    // The language name is the info string's first whitespace-delimited
    // word: CommonMark allows arbitrary trailing content after it (e.g.
    // `` ```rust {.line-numbers} ``), and only the language itself matters
    // here.
    let language = info_string
        .as_ref()
        .and_then(|range| text.get(range.clone()))
        .and_then(|s| s.split_whitespace().next())
        .map(str::to_string);

    // An unclosed fence is left alone: without a closing line there is no
    // block to stand a rendering in for.
    if let (Some(closing_delimiter), Some(language)) = (&closing_delimiter, &language)
        && extensions
            .rendered_fence_languages
            .contains(&language.to_lowercase())
    {
        let range = opening_delimiter.start..closing_delimiter.end;
        if !touches_selection(&range, selections) {
            plan.rendered_fences.push(RenderedFence {
                range,
                language: language.to_lowercase(),
                info: info_string
                    .as_ref()
                    .and_then(|range| text.get(range.clone()))
                    .map(|info| info.trim().to_string())
                    .unwrap_or_default(),
                content_hash: hash_text(text.get(content.clone()).unwrap_or_default()),
                content_range: content,
            });
            return;
        }
    }

    // Each fence line becomes a one-row block, so its range must end on that
    // row: a range that includes the newline ends on the next row, and the
    // block would replace that row as well, hiding the first line of the code
    // after the opening fence and whatever follows the closing one. The
    // opening line is everything up to where `code_fence_content` begins,
    // minus the newline that content starts after.
    let opening_line = opening_delimiter.start..end_of_line_content(text, content.start);
    if !touches_selection(&opening_line, selections) {
        plan.code_fence_borders
            .push((opening_line, language.clone()));
    }

    if let Some(closing_delimiter) = closing_delimiter
        && !touches_selection(&closing_delimiter, selections)
    {
        plan.code_fence_borders.push((closing_delimiter, None));
    }

    plan.code_fence_content.push((content, language));
}

/// Recognizes a `pipe_table`'s structure -- see `TableInfo`'s own doc
/// comment for why this only ever produces structural byte ranges (pipes,
/// per-cell content/gap ranges, alignments), never hides or block-replaces
/// any cell content itself.
///
/// Bails out (the whole table left unhandled, i.e. fully raw) if either the
/// header or delimiter row is missing -- shouldn't happen for a real
/// `pipe_table` node, but not worth guessing at if the grammar ever produces
/// one some other way.
fn plan_pipe_table(
    node: Node,
    text: &str,
    selections: &[Range<usize>],
    inline_parser: &mut Parser,
    visible_range: &Range<usize>,
    extensions: &PlanExtensions,
    plan: &mut Plan,
) {
    let mut cursor = node.walk();
    let mut header = None;
    let mut delimiter_row = None;
    let mut data_rows = Vec::new();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "pipe_table_header" => header = Some(child),
            "pipe_table_delimiter_row" => delimiter_row = Some(child),
            "pipe_table_row" => data_rows.push(child),
            _ => {}
        }
    }
    let Some(header) = header else {
        return;
    };
    let Some(delimiter_row) = delimiter_row else {
        return;
    };

    let alignments = table_alignments(delimiter_row);

    let mut rows = vec![table_row_cells(header, text, plan)];
    for row in &data_rows {
        rows.push(table_row_cells(*row, text, plan));
    }

    // Recurse into every cell's own content for inline formatting (bold,
    // links, etc. inside a cell) -- this is the *only* place viewport
    // pruning applies for a table: `walk_block`'s own entry already skips a
    // cell whose range doesn't overlap `visible_range`. The structural data
    // above is collected for every row unconditionally, regardless of
    // visibility, since visual_md.rs needs every cell in a column measured to
    // keep that column's width (and therefore every row's spacer) visually
    // consistent as the table scrolls in and out of view.
    for row_node in std::iter::once(header).chain(data_rows.iter().copied()) {
        let mut cell_cursor = row_node.walk();
        for cell in row_node.children(&mut cell_cursor) {
            if cell.kind() == "pipe_table_cell" {
                walk_block(
                    cell,
                    text,
                    selections,
                    inline_parser,
                    visible_range,
                    extensions,
                    plan,
                );
            }
        }
    }

    let delimiter_range = delimiter_row.byte_range();
    let delimiter_line =
        (!touches_selection(&delimiter_range, selections)).then_some(delimiter_range);

    plan.tables.push(TableInfo {
        alignments,
        delimiter_line,
        rows,
    });
}

/// One [`TableAlignment`] per `pipe_table_delimiter_cell` in a
/// `pipe_table_delimiter_row`.
fn table_alignments(delimiter_row: Node) -> Vec<TableAlignment> {
    let mut cursor = delimiter_row.walk();
    delimiter_row
        .children(&mut cursor)
        .filter(|child| child.kind() == "pipe_table_delimiter_cell")
        .map(|cell| {
            let mut has_left = false;
            let mut has_right = false;
            let mut inner_cursor = cell.walk();
            for part in cell.children(&mut inner_cursor) {
                match part.kind() {
                    "pipe_table_align_left" => has_left = true,
                    "pipe_table_align_right" => has_right = true,
                    _ => {}
                }
            }
            match (has_left, has_right) {
                (true, true) => TableAlignment::Center,
                (true, false) => TableAlignment::Left,
                (false, true) => TableAlignment::Right,
                (false, false) => TableAlignment::Default,
            }
        })
        .collect()
}

/// Walks one `pipe_table_header`/`pipe_table_row`'s direct children (`|`
/// tokens alternating with `pipe_table_cell`s), pushing every `|`'s range
/// into `plan.glyph_markers` as `GlyphKind::TablePipe` and returning one
/// [`TableCell`] per cell in column order.
fn table_row_cells(row: Node, text: &str, plan: &mut Plan) -> Vec<TableCell> {
    let mut cells = Vec::new();
    let mut prev_pipe_end = None;
    let mut cursor = row.walk();
    for child in row.children(&mut cursor) {
        match child.kind() {
            "|" => {
                let range = child.byte_range();
                prev_pipe_end = Some(range.end);
                plan.glyph_markers.push((range, GlyphKind::TablePipe));
            }
            "pipe_table_cell" => {
                let cell_range = child.byte_range();
                let leading_gap = match prev_pipe_end {
                    Some(end) => end..cell_range.start,
                    None => cell_range.start..cell_range.start,
                };
                // The cell's own node range already includes any trailing
                // whitespace before the next `|` (confirmed by inspection);
                // trimming it back off here is what leaves it available as
                // `trailing_gap` for visual_md.rs to widen into a spacer.
                let cell_text = text.get(cell_range.clone()).unwrap_or("");
                let trimmed_len = cell_text.trim_end().len();
                let content = cell_range.start..cell_range.start + trimmed_len;
                let trailing_gap = content.end..cell_range.end;
                cells.push(TableCell {
                    leading_gap,
                    content,
                    trailing_gap,
                });
            }
            _ => {}
        }
    }
    cells
}

/// Looks for `[!type]` immediately after the marker on a blockquote's first
/// line. Returns the `[!type]` marker's own byte range (to hide), the
/// recognized callout kind, and the byte range of the rest of the
/// blockquote's content (to tint).
fn detect_callout(
    block_quote: Node,
    text: &str,
    selections: &[Range<usize>],
) -> Option<CalloutInfo> {
    let node_range = block_quote.byte_range();
    let first_marker_end = {
        let mut cursor = block_quote.walk();
        block_quote
            .children(&mut cursor)
            .find(|child| child.kind() == "block_quote_marker")?
            .byte_range()
            .end
    };
    let after_marker = text.get(first_marker_end..node_range.end)?;
    if !after_marker.starts_with("[!") {
        return None;
    }
    let close = after_marker.find(']')?;
    let type_name = &after_marker[2..close];
    if type_name.is_empty() || !type_name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let bracket_end = first_marker_end + close + 1;
    let (fold, suffix_range) = match text.as_bytes().get(bracket_end) {
        Some(b'+') => (CalloutFold::Expanded, bracket_end..bracket_end + 1),
        Some(b'-') => (CalloutFold::Collapsed, bracket_end..bracket_end + 1),
        _ => (CalloutFold::None, bracket_end..bracket_end),
    };
    let marker_end = suffix_range.end;
    let marker_range = first_marker_end..marker_end;
    // Matches `plan_heading`'s own "touched" scope, not a bare overlap with
    // `marker_range`: a heading reveals its marker when the cursor is
    // anywhere on that (single-line) construct, so a callout reveals its
    // title when the cursor is anywhere on *its* title line -- including
    // past the marker, in same-line text like "> [!warning] Be careful" --
    // but deliberately not for a cursor anywhere in the body, which stays
    // independently live-previewed (per spec) rather than coupled to the
    // title's own raw/rendered state.
    let touched = touches_selection(&first_line(&node_range, text), selections);
    Some(CalloutInfo {
        kind: CalloutKind::from_type_name(type_name),
        raw_type_name: type_name.to_string(),
        fold,
        suffix_range,
        body_range: marker_end..node_range.end,
        node_range,
        marker_range,
        touched,
    })
}

/// The real `tree-sitter-md` crate's `MarkdownParser` (see `plan()`'s doc

/// The real `tree-sitter-md` crate's `MarkdownParser` (see `plan()`'s doc
/// comment for why this crate hand-rolls a simpler two-pass parse instead of
/// using it) builds an inline node's parsed range by explicitly excluding
/// any block-level `block_continuation` children first, via tree-sitter's
/// multi-range parsing — a multi-line blockquote paragraph's repeated "> "
/// prefixes on continuation lines are exactly this. This crate's simpler
/// single-contiguous-substring reparse doesn't get that for free: fed
/// straight through, the inline grammar sees a lone ">" as meaningless plain
/// text (confirmed empirically, not assumed) rather than as markup. Fixed
/// here by reading the block tree's own `block_continuation` children (which
/// *do* show up correctly there) directly for their bar-glyph ranges, then
/// blanking their bytes to spaces before handing the text to the inline
/// parser, so it never sees the stray `>` at all. Blanking preserves length
/// and every other node's byte offsets exactly, so the rest of this module's
/// offset math is untouched by it.
fn plan_inline(
    inline_node: Node,
    text: &str,
    selections: &[Range<usize>],
    inline_parser: &mut Parser,
    extensions: &PlanExtensions,
    plan: &mut Plan,
) {
    let range = inline_node.byte_range();
    if range.is_empty() {
        return;
    }
    let Some(original) = text.get(range.clone()) else {
        return;
    };

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
        if text
            .get(child_range.clone())
            .is_some_and(|s| s.trim_start().starts_with('>'))
        {
            plan.glyph_markers
                .push((child_range.clone(), GlyphKind::BlockquoteBar));
        }
        let local_start = child_range.start - range.start;
        let local_end = child_range.end - range.start;
        if let Some(slice) = bytes.get_mut(local_start..local_end) {
            slice.fill(b' ');
        }
    }
    let Ok(inline_text) = String::from_utf8(bytes) else {
        return;
    };

    let Some(tree) = inline_parser.parse(&inline_text, None) else {
        return;
    };

    let mut code_ranges = Vec::new();
    let mut link_ranges = Vec::new();
    let no_definitions = HashMap::new();
    walk_inline(
        tree.root_node(),
        range.start,
        selections,
        plan,
        &mut code_ranges,
        &mut link_ranges,
        &extensions.rules,
        extensions
            .link_definitions
            .as_deref()
            .unwrap_or(&no_definitions),
        &inline_text,
    );
    plan_highlight_marks(&inline_text, range.start, &code_ranges, selections, plan);
    plan_note_syntax(
        &inline_text,
        range.start,
        &code_ranges,
        &link_ranges,
        selections,
        plan,
    );

    for hit in rules::find_pattern_hits(&extensions.rules, &inline_text, range.start, &code_ranges)
    {
        plan_rule_hit(hit, selections, &extensions.rules, plan);
    }
}

fn walk_inline(
    node: Node,
    offset: usize,
    selections: &[Range<usize>],
    plan: &mut Plan,
    code_ranges: &mut Vec<Range<usize>>,
    link_ranges: &mut Vec<Range<usize>>,
    rule_set: &RuleSet,
    definitions: &HashMap<String, String>,
    inline_text: &str,
) {
    for rule in rule_set.node_rules(node.kind()) {
        let range = shift(node.byte_range(), offset);
        let node_text = inline_text.get(node.byte_range());
        if let Some(hit) = node_text.and_then(|node_text| {
            rules::node_hit(rule, 0..node_text.len(), node_text).map(|mut hit| {
                hit.range = range;
                hit
            })
        }) {
            plan_rule_hit(hit, selections, rule_set, plan);
        }
    }

    match node.kind() {
        "inline_link" => {
            link_ranges.push(shift(node.byte_range(), offset));
            plan_link(node, offset, selections, plan);
            // A link's children are structural tokens (brackets/parens) plus
            // `link_text`/`link_destination`, none of which are themselves
            // emphasis/link/code_span nodes in practice — like `code_span`,
            // there's no nested markup worth recursing into here.
            return;
        }
        "uri_autolink" | "email_autolink" => {
            link_ranges.push(shift(node.byte_range(), offset));
            plan_autolink(node, offset, selections, plan);
            return;
        }
        "full_reference_link" | "collapsed_reference_link" | "shortcut_link"
            if reference_label(node, inline_text)
                .is_some_and(|label| definitions.contains_key(&normalize_label(label))) =>
        {
            link_ranges.push(shift(node.byte_range(), offset));
            plan_reference_link(node, offset, selections, plan);
            return;
        }
        _ => {}
    }

    let style = match node.kind() {
        "strong_emphasis" => Some((SpanStyle::Bold, "emphasis_delimiter")),
        "emphasis" => Some((SpanStyle::Italic, "emphasis_delimiter")),
        "strikethrough" => Some((SpanStyle::Strikethrough, "emphasis_delimiter")),
        "code_span" => Some((SpanStyle::InlineCode, "code_span_delimiter")),
        _ => None,
    };

    if let Some((span_style, delimiter_kind)) = style {
        plan_delimited_span(node, offset, delimiter_kind, span_style, selections, plan);
        if span_style == SpanStyle::InlineCode {
            code_ranges.push(shift(node.byte_range(), offset));
        }
        // Don't recurse into a code span's contents (raw text, no nested
        // markup); do recurse into emphasis/strong/strikethrough since they
        // can nest (e.g. `***bold italic***`, or the grammar's own
        // self-nested `~~strikethrough~~` representation).
        if node.kind() != "code_span" {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk_inline(
                    child,
                    offset,
                    selections,
                    plan,
                    code_ranges,
                    link_ranges,
                    rule_set,
                    definitions,
                    inline_text,
                );
            }
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_inline(
            child,
            offset,
            selections,
            plan,
            code_ranges,
            link_ranges,
            rule_set,
            definitions,
            inline_text,
        );
    }
}

fn shift(range: Range<usize>, offset: usize) -> Range<usize> {
    (range.start + offset)..(range.end + offset)
}

/// The first direct child of `node` with the given grammar kind, if any.
fn find_child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == kind)
}

/// Hides an `inline_link`'s (`[text](url)`) brackets/parens/destination,
/// leaving only `text` visible and styled as a link; reveals them (dimmed)
/// instead if the cursor is anywhere on the link. Bails out (leaves the node
/// entirely unhandled, i.e. fully raw) if any expected child is missing --
/// malformed/unusual grammar output isn't worth guessing at.
///
/// Only the direct `[text](url)` shape is handled here. Reference-style links
/// (`[text][1]`, `[text][]`, `[shortcut]`) are other node kinds, planned by
/// `plan_reference_link` when the document defines the label they name.
fn plan_link(node: Node, offset: usize, selections: &[Range<usize>], plan: &mut Plan) {
    let Some(open_bracket) = find_child(node, "[") else {
        return;
    };
    let Some(link_text) = find_child(node, "link_text") else {
        return;
    };
    let Some(close_bracket) = find_child(node, "]") else {
        return;
    };
    let Some(close_paren) = find_child(node, ")") else {
        return;
    };

    let node_range = shift(node.byte_range(), offset);
    let prefix = shift(open_bracket.byte_range(), offset);
    // `]`, `(`, `link_destination`, `)` sit back-to-back with no gaps, so
    // this is a single contiguous span, not several -- same "merge the
    // contiguous run" idea as `plan_delimited_span`'s prefix/suffix, just
    // computed directly since a link's trailing cluster isn't a repeated
    // delimiter of one kind.
    let suffix = shift(
        close_bracket.byte_range().start..close_paren.byte_range().end,
        offset,
    );
    let link_text = shift(link_text.byte_range(), offset);

    if touches_selection(&node_range, selections) {
        plan.dimmed_markers.push(prefix);
        plan.dimmed_markers.push(suffix);
    } else {
        plan.hidden_markers.push(prefix);
        plan.hidden_markers.push(suffix);
    }
    plan.styled_spans.push((link_text, SpanStyle::Link));
}

/// The label a reference link (`[text][label]`, `[text][]` or `[label]`) names,
/// as written.
pub(crate) fn reference_label<'a>(node: Node, text: &'a str) -> Option<&'a str> {
    let label_node = match node.kind() {
        "full_reference_link" => find_child(node, "link_label")?,
        "collapsed_reference_link" | "shortcut_link" => find_child(node, "link_text")?,
        _ => return None,
    };
    let written = text.get(label_node.byte_range())?;
    if node.kind() == "full_reference_link" {
        return written.strip_prefix('[')?.strip_suffix(']');
    }
    // `[[name]]` is a wikilink, not a link to the label `name` inside brackets.
    let inside_double_brackets = text
        .get(..node.start_byte())
        .is_some_and(|before| before.ends_with('['))
        && text
            .get(node.end_byte()..)
            .is_some_and(|after| after.starts_with(']'));
    (!inside_double_brackets).then_some(written)
}

/// A label as CommonMark matches it: case folded, with every run of whitespace
/// a single space.
pub(crate) fn normalize_label(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The `[label]: destination` definitions of a document, by normalized label.
/// When a label is defined twice the first definition counts, as in CommonMark.
pub(crate) fn link_definitions(text: &str, block_tree: &Tree) -> HashMap<String, String> {
    let mut definitions = HashMap::new();
    // Nearly every document has none, and finding that out should not take a
    // walk of its block tree.
    if !text.contains("]:") {
        return definitions;
    }

    let mut pending = vec![block_tree.root_node()];
    while let Some(node) = pending.pop() {
        if node.kind() == "link_reference_definition" {
            let label = find_child(node, "link_label")
                .and_then(|label| text.get(label.byte_range()))
                .and_then(|written| written.strip_prefix('[')?.strip_suffix(']'));
            let destination = find_child(node, "link_destination")
                .and_then(|destination| text.get(destination.byte_range()));
            if let (Some(label), Some(destination)) = (label, destination) {
                let destination = destination
                    .strip_prefix('<')
                    .and_then(|inner| inner.strip_suffix('>'))
                    .unwrap_or(destination);
                definitions
                    .entry(normalize_label(label))
                    .or_insert_with(|| destination.to_string());
            }
            continue;
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        // Taken in document order, which decides which of two definitions of a
        // label counts.
        pending.extend(children.into_iter().rev());
    }
    definitions
}

/// Hides a reference link's brackets and label, leaving its text, styled as a
/// link, and reveals them (dimmed) while the cursor is on the link. Only called
/// for a link whose label the document defines: any other `[text]` is text.
fn plan_reference_link(node: Node, offset: usize, selections: &[Range<usize>], plan: &mut Plan) {
    let Some(open_bracket) = find_child(node, "[") else {
        return;
    };
    let Some(link_text) = find_child(node, "link_text") else {
        return;
    };
    let Some(close_bracket) = find_child(node, "]") else {
        return;
    };

    let node_range = shift(node.byte_range(), offset);
    let prefix = shift(open_bracket.byte_range(), offset);
    // From the `]` that ends the text to the end of the link: nothing, a `[]`,
    // or the `[label]`, which sit back to back.
    let suffix = shift(
        close_bracket.byte_range().start..node.byte_range().end,
        offset,
    );
    let link_text = shift(link_text.byte_range(), offset);

    if touches_selection(&node_range, selections) {
        plan.dimmed_markers.push(prefix);
        plan.dimmed_markers.push(suffix);
    } else {
        plan.hidden_markers.push(prefix);
        plan.hidden_markers.push(suffix);
    }
    plan.styled_spans.push((link_text, SpanStyle::Link));
}

/// Hides a `uri_autolink`/`email_autolink`'s (`<https://...>`) angle
/// brackets, styling the URL text between them as a link -- reveals them
/// (dimmed) instead if the cursor is anywhere on it. Unlike `plan_link` this
/// is a leaf node (no children at all per the grammar), so the brackets are
/// just its first and last byte.
fn plan_autolink(node: Node, offset: usize, selections: &[Range<usize>], plan: &mut Plan) {
    let node_range = shift(node.byte_range(), offset);
    if node_range.len() < 2 {
        return;
    }
    let prefix = node_range.start..node_range.start + 1;
    let suffix = node_range.end - 1..node_range.end;
    let inner = prefix.end..suffix.start;

    if touches_selection(&node_range, selections) {
        plan.dimmed_markers.push(prefix);
        plan.dimmed_markers.push(suffix);
    } else {
        plan.hidden_markers.push(prefix);
        plan.hidden_markers.push(suffix);
    }
    if !inner.is_empty() {
        plan.styled_spans.push((inner, SpanStyle::Link));
    }
}

/// Finds every descendant delimiter node of `kind`, merges the contiguous run
/// touching the node's start into a "prefix" marker and the contiguous run
/// touching its end into a "suffix" marker, and styles the gap between them
/// as content. Handles the grammar's own nested representation of runs like
/// `~~strikethrough~~` (a `strikethrough` node containing another
/// `strikethrough` node) uniformly, since it walks all descendants rather
/// than only direct children.
fn plan_delimited_span(
    node: Node,
    offset: usize,
    delimiter_kind: &str,
    style: SpanStyle,
    selections: &[Range<usize>],
    plan: &mut Plan,
) {
    let node_range = shift(node.byte_range(), offset);

    let mut delimiters: Vec<Range<usize>> = Vec::new();
    collect_delimiters(node, delimiter_kind, offset, &mut delimiters);
    delimiters.sort_by_key(|range| range.start);

    let Some(first) = delimiters.first().cloned() else {
        return;
    };
    let Some(last) = delimiters.last().cloned() else {
        return;
    };

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

    let prefix = node_range.start..prefix_end;
    let suffix = suffix_start..node_range.end;

    let is_code = style == SpanStyle::InlineCode;
    let is_raw = touches_selection(&node_range, selections);
    if is_code || is_raw {
        plan.dimmed_markers.push(prefix.clone());
        plan.dimmed_markers.push(suffix.clone());
    } else {
        plan.hidden_markers.push(prefix.clone());
        plan.hidden_markers.push(suffix.clone());
    }

    if prefix.end < suffix.start {
        plan.styled_spans.push((prefix.end..suffix.start, style));
    }
}

fn collect_delimiters(node: Node, kind: &str, offset: usize, out: &mut Vec<Range<usize>>) {
    if node.kind() == kind {
        out.push(shift(node.byte_range(), offset));
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_delimiters(child, kind, offset, out);
    }
}

/// `==highlight==` has no tree-sitter-md node at all (this grammar doesn't
/// support it), so it is found with a manual scan of the inline node's raw
/// text instead, skipping any byte already claimed by a code span so that
/// `` `==literal==` `` inside code is left alone.
fn plan_highlight_marks(
    inline_text: &str,
    offset: usize,
    code_ranges: &[Range<usize>],
    selections: &[Range<usize>],
    plan: &mut Plan,
) {
    let bytes = inline_text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'=' && bytes[i + 1] == b'=' && !in_code(offset + i, code_ranges) {
            if let Some(close) = find_closing(bytes, i + 2, code_ranges, offset) {
                let open = shift(i..i + 2, offset);
                let close_range = shift(close..close + 2, offset);
                let outer = open.start..close_range.end;
                if touches_selection(&outer, selections) {
                    plan.dimmed_markers.push(open.clone());
                    plan.dimmed_markers.push(close_range.clone());
                } else {
                    plan.hidden_markers.push(open.clone());
                    plan.hidden_markers.push(close_range.clone());
                }
                if open.end < close_range.start {
                    plan.styled_spans
                        .push((open.end..close_range.start, SpanStyle::Highlight));
                }
                i = close + 2;
                continue;
            }
        }
        i += 1;
    }
}

/// `%%comments%%`, `^block-ids`, `[[wikilinks]]` and `#tags`, which no node of
/// the Markdown grammar covers, so they are found by scanning the inline node's
/// text like `==highlight==`. Code spans and links are left alone, and so is an
/// embed `![[...]]`, which is an image or a note drawn in place rather than a
/// link.
///
/// A comment is hidden until a selection touches it, then shown dimmed. One that
/// runs over several lines of a paragraph is only ever dimmed, since a fold
/// cannot span lines. Nothing inside a comment is anything else. A block id is
/// always dimmed.
///
/// A wikilink behaves like a Markdown link: its brackets, and the target in front
/// of an alias, are hidden until a selection touches the link, then shown dimmed.
fn plan_note_syntax(
    inline_text: &str,
    offset: usize,
    code_ranges: &[Range<usize>],
    link_ranges: &[Range<usize>],
    selections: &[Range<usize>],
    plan: &mut Plan,
) {
    let mut excluded: Vec<Range<usize>> = code_ranges.iter().chain(link_ranges).cloned().collect();

    for comment in inline_scan::find_comments(inline_text, offset, &excluded) {
        excluded.push(comment.range.clone());
        if comment.is_multiline || touches_selection(&comment.range, selections) {
            plan.dimmed_markers.push(comment.range);
        } else {
            plan.hidden_markers.push(comment.range);
        }
    }

    for id in inline_scan::find_block_ids(inline_text, offset, &excluded) {
        plan.dimmed_markers.push(id);
    }

    for link in inline_scan::find_wikilinks(inline_text, offset, &excluded) {
        excluded.push(link.range.clone());
        if link.is_embed {
            continue;
        }
        if touches_selection(&link.range, selections) {
            plan.dimmed_markers.push(link.prefix);
            plan.dimmed_markers.push(link.suffix);
        } else {
            plan.hidden_markers.push(link.prefix);
            plan.hidden_markers.push(link.suffix);
        }
        let note = link
            .target
            .split('#')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        plan.wikilinks.push(WikilinkSpan {
            range: link.visible,
            note,
        });
    }

    for tag in inline_scan::find_tags(inline_text, offset, &excluded) {
        plan.styled_spans.push((tag.range, SpanStyle::Tag));
    }
}

pub(crate) fn in_code(byte_offset: usize, code_ranges: &[Range<usize>]) -> bool {
    code_ranges.iter().any(|range| range.contains(&byte_offset))
}

pub(crate) fn find_closing(
    bytes: &[u8],
    start: usize,
    code_ranges: &[Range<usize>],
    offset: usize,
) -> Option<usize> {
    let mut i = start;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\n' {
            return None;
        }
        if bytes[i] == b'=' && bytes[i + 1] == b'=' && !in_code(offset + i, code_ranges) {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(text: &str, style: SpanStyle) -> Vec<Range<usize>> {
        plan(text, &[])
            .styled_spans
            .into_iter()
            .filter(|(_, s)| *s == style)
            .map(|(range, _)| range)
            .collect()
    }

    #[test]
    fn heading_marker_hidden_when_not_touched() {
        let result = plan("# Heading\n", &[]);
        assert_eq!(result.hidden_markers, vec![0..2]);
        assert!(result.dimmed_markers.is_empty());
        assert_eq!(result.styled_spans, vec![(2..9, SpanStyle::Heading(1))]);
    }

    #[test]
    fn heading_marker_dimmed_when_cursor_on_line() {
        let result = plan("# Heading\n", &[4..4]);
        assert!(result.hidden_markers.is_empty());
        assert_eq!(result.dimmed_markers, vec![0..2]);
    }

    #[test]
    fn heading_marker_hidden_when_cursor_on_next_line() {
        for cursor in [10, 11] {
            let result = plan("# Heading\n\ntext", &[cursor..cursor]);
            assert_eq!(result.hidden_markers, vec![0..2], "cursor at {cursor}");
            assert!(result.dimmed_markers.is_empty(), "cursor at {cursor}");
        }
        let result = plan("# Heading\ntext", &[10..10]);
        assert_eq!(result.hidden_markers, vec![0..2]);
    }

    #[test]
    fn heading_marker_dimmed_when_cursor_at_end_of_heading_line() {
        let result = plan("# Heading\n\ntext", &[9..9]);
        assert!(result.hidden_markers.is_empty());
        assert_eq!(result.dimmed_markers, vec![0..2]);
    }

    #[test]
    fn heading_levels() {
        for (marker, level) in [
            ("#", 1),
            ("##", 2),
            ("###", 3),
            ("####", 4),
            ("#####", 5),
            ("######", 6),
        ] {
            let text = format!("{marker} Heading\n");
            let result = plan(&text, &[]);
            assert_eq!(
                result.styled_spans[0].1,
                SpanStyle::Heading(level),
                "for {marker}"
            );
        }
    }

    #[test]
    fn bold_hidden_by_default_and_revealed_by_cursor() {
        let text = "Some **bold** text.\n";
        let hidden = plan(text, &[]);
        assert_eq!(hidden.hidden_markers, vec![5..7, 11..13]);
        assert!(hidden.dimmed_markers.is_empty());
        assert_eq!(spans(text, SpanStyle::Bold), vec![7..11]);

        // cursor inside "bold"
        let revealed = plan(text, &[8..8]);
        assert!(revealed.hidden_markers.is_empty());
        assert_eq!(revealed.dimmed_markers, vec![5..7, 11..13]);
    }

    #[test]
    fn italic_single_star() {
        let text = "An *italic* word.\n";
        let result = plan(text, &[]);
        assert_eq!(result.hidden_markers, vec![3..4, 10..11]);
        assert_eq!(spans(text, SpanStyle::Italic), vec![4..10]);
    }

    #[test]
    fn bold_italic_nested() {
        let text = "A ***both*** word.\n";
        let result = plan(text, &[]);
        // The outer `emphasis` node's own delimiter-collection is recursive
        // (see `plan_delimited_span`'s doc comment), so its merged "*" run
        // absorbs the immediately-adjacent inner "**" run too: outer's
        // prefix/suffix end up as the full "***" on each side. The inner
        // `strong_emphasis` node separately contributes its own narrower
        // "**" prefix/suffix, which is a strict subset of the outer's and so
        // disappears once overlapping hidden ranges are merged (folds can't
        // overlap) — the merged result is just the outer's "***" on each
        // side.
        assert_eq!(result.hidden_markers, vec![2..5, 9..12]);
        // Outer (italic) and inner (bold) content both resolve to the same
        // "both" byte range, so it gets both styles applied together, which
        // is exactly the desired combined bold+italic rendering.
        assert_eq!(spans(text, SpanStyle::Bold), vec![5..9]);
        assert_eq!(spans(text, SpanStyle::Italic), vec![5..9]);
    }

    #[test]
    fn strikethrough_self_nested_grammar_quirk() {
        let text = "A ~~strike~~ word.\n";
        let result = plan(text, &[]);
        // This grammar represents `~~x~~` as a `strikethrough` node wrapping
        // another `strikethrough` node; the inner node's narrower marker
        // ranges are subsets of the outer's and merge away, same as above.
        assert_eq!(result.hidden_markers, vec![2..4, 10..12]);
        assert!(
            spans(text, SpanStyle::Strikethrough)
                .iter()
                .all(|range| *range == (4..10))
        );
    }

    #[test]
    fn inline_code_always_dimmed_never_hidden() {
        let text = "Some `code` here.\n";
        let touching = plan(text, &[]);
        assert!(touching.hidden_markers.is_empty());
        assert_eq!(touching.dimmed_markers, vec![5..6, 10..11]);
        assert_eq!(spans(text, SpanStyle::InlineCode), vec![6..10]);

        let with_cursor = plan(text, &[8..8]);
        assert!(with_cursor.hidden_markers.is_empty());
        assert_eq!(with_cursor.dimmed_markers, vec![5..6, 10..11]);
    }

    #[test]
    fn highlight_mark() {
        let text = "Some ==highlighted== text.\n";
        let result = plan(text, &[]);
        assert_eq!(result.hidden_markers, vec![5..7, 18..20]);
        assert_eq!(spans(text, SpanStyle::Highlight), vec![7..18]);
    }

    #[test]
    fn highlight_mark_ignored_inside_code_span() {
        let text = "Some `==literal==` code.\n";
        assert!(spans(text, SpanStyle::Highlight).is_empty());
    }

    #[test]
    fn multiple_selections_each_reveal_their_own_span() {
        let text = "**a** and **b**\n";
        // second selection sits inside the second bold span
        let result = plan(text, &[12..12]);
        assert_eq!(result.hidden_markers, vec![0..2, 3..5]);
        assert!(result.dimmed_markers.contains(&(10..12)));
        assert!(result.dimmed_markers.contains(&(13..15)));
    }

    #[test]
    fn no_hidden_range_ever_overlaps_another() {
        // A fold can't be created over an already-folded sub-range (Zed's
        // display-map panics on overlapping fold input), so this is the
        // actual regression this milestone shipped with: nested constructs
        // producing overlapping ranges crashed the real editor.
        for text in [
            "A ***both*** word.\n",
            "A ~~strike~~ word.\n",
            "# H\n\n***nested at start of line***\n",
            "Some **a *b* c** text.\n",
            "# Heading One\n\nSome regular paragraph text that should render completely unstyled by visual_md.\n\nSome **bold**, *italic*, ***both***, ~~strike~~, ==highlight==, and `code`.\n\n## Heading Two\n\nNot a heading: this line starts with a hash but no space:\n#nope\n",
        ] {
            let result = plan(text, &[]);
            for window in result.hidden_markers.windows(2) {
                assert!(
                    window[0].end <= window[1].start,
                    "overlapping hidden ranges {:?} and {:?} for {text:?}",
                    window[0],
                    window[1]
                );
            }
        }
    }

    #[test]
    fn merge_ranges_merges_overlapping_and_touching() {
        assert_eq!(merge_ranges(vec![2..5, 3..5]), vec![2..5]);
        assert_eq!(merge_ranges(vec![0..2, 2..4]), vec![0..4]);
        assert_eq!(merge_ranges(vec![5..7, 0..2]), vec![0..2, 5..7]);
        assert_eq!(merge_ranges(vec![0..0, 1..3]), vec![1..3]);
    }

    #[test]
    fn subtract_ranges_removes_overlap() {
        assert_eq!(subtract_ranges(&[0..10], &[3..5]), vec![0..3, 5..10]);
        assert_eq!(
            subtract_ranges(&[0..10], &[0..10]),
            Vec::<Range<usize>>::new()
        );
        assert_eq!(subtract_ranges(&[0..10], &[]), vec![0..10]);
        assert_eq!(subtract_ranges(&[0..5, 8..10], &[4..9]), vec![0..4, 9..10]);
    }

    #[test]
    fn unordered_list_bullets() {
        let text = "- one\n- two\n- three\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.glyph_markers,
            vec![
                (0..2, GlyphKind::Bullet),
                (6..8, GlyphKind::Bullet),
                (12..14, GlyphKind::Bullet),
            ]
        );
    }

    #[test]
    fn ordered_list_renumbers_regardless_of_source_digits() {
        let text = "1. a\n1. b\n1. c\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.glyph_markers,
            vec![
                (0..3, GlyphKind::Ordinal("1. ".to_string())),
                (5..8, GlyphKind::Ordinal("2. ".to_string())),
                (10..13, GlyphKind::Ordinal("3. ".to_string())),
            ]
        );
    }

    #[test]
    fn ordered_list_honors_custom_start_number() {
        let text = "5. a\n5. b\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.glyph_markers,
            vec![
                (0..3, GlyphKind::Ordinal("5. ".to_string())),
                (5..8, GlyphKind::Ordinal("6. ".to_string())),
            ]
        );
    }

    #[test]
    fn task_checkbox_suppresses_bullet_and_registers_widget() {
        let text = "- [ ] a\n- [x] b\n";
        let result = plan(text, &[]);
        assert!(
            result.glyph_markers.is_empty(),
            "bullets should be suppressed for task items"
        );
        assert_eq!(result.hidden_markers, vec![0..2, 8..10]);
        assert_eq!(result.checkboxes, vec![(2..5, false), (10..13, true)]);
    }

    #[test]
    fn blockquote_marker_becomes_bar_glyph() {
        let text = "> quoted text\n";
        let result = plan(text, &[]);
        assert!(
            result
                .glyph_markers
                .contains(&(0..2, GlyphKind::BlockquoteBar))
        );
    }

    #[test]
    fn multiline_blockquote_bars_every_line() {
        let text = "> line one\n> line two\n";
        let result = plan(text, &[]);
        let bar_count = result
            .glyph_markers
            .iter()
            .filter(|(_, kind)| *kind == GlyphKind::BlockquoteBar)
            .count();
        assert_eq!(
            bar_count, 2,
            "both the first and continuation line should get a bar"
        );
    }

    #[test]
    fn nested_blockquote_stacks_bars() {
        let text = "> > nested\n";
        let result = plan(text, &[]);
        let bar_count = result
            .glyph_markers
            .iter()
            .filter(|(_, kind)| *kind == GlyphKind::BlockquoteBar)
            .count();
        assert_eq!(bar_count, 2, "each nesting level contributes its own bar");
    }

    #[test]
    fn callout_hides_bracket_syntax_and_tints_the_whole_box() {
        let text = "> [!warning] Be careful\n";
        let result = plan(text, &[]);
        // The marker never lands in `hidden_markers` (that would trigger the
        // generic space-fold): visual_md.rs renders its own title widget for
        // an untouched callout, driven by `Plan::callouts` instead.
        assert!(!result.hidden_markers.contains(&(2..12)));
        assert!(!result.dimmed_markers.contains(&(2..12)));
        let callout = result.callouts.first().expect("expected one callout");
        assert_eq!(callout.marker_range, 2..12);
        assert_eq!(callout.kind, CalloutKind::Warning);
        assert_eq!(callout.raw_type_name, "warning");
        assert_eq!(callout.fold, CalloutFold::None);
        assert!(!callout.touched);
        // The tint covers the whole block_quote node (title row included),
        // not just the body.
        assert_eq!(callout.node_range, 0..text.len());
        assert!(
            result
                .styled_spans
                .iter()
                .any(|(range, style)| *range == callout.node_range
                    && *style == SpanStyle::Callout(CalloutKind::Warning))
        );
    }

    #[test]
    fn touched_callout_marker_reveals_as_raw_dimmed_text() {
        let text = "> [!warning] Be careful\n";
        // Cursor placed inside the `[!warning]` marker range (2..12).
        let result = plan(text, &[5..5]);
        assert!(result.dimmed_markers.contains(&(2..12)));
        assert!(!result.hidden_markers.contains(&(2..12)));
        let callout = result.callouts.first().expect("expected one callout");
        assert!(callout.touched);
        // Still tinted and still tracked, even while its title shows raw.
        assert!(
            result
                .styled_spans
                .iter()
                .any(|(range, style)| *range == callout.node_range
                    && *style == SpanStyle::Callout(CalloutKind::Warning))
        );
    }

    #[test]
    fn cursor_anywhere_on_the_title_line_touches_it_not_just_the_marker_bytes() {
        let text = "> [!warning] Be careful\n";
        // Cursor inside "careful", well past the `[!warning]` marker itself,
        // but still on the same title line -- matches `plan_heading`'s own
        // "touched" scope (the whole construct's line, not a bare overlap
        // with the marker's own bytes).
        let result = plan(text, &[20..20]);
        let callout = result.callouts.first().expect("expected one callout");
        assert!(callout.touched);
        assert!(result.dimmed_markers.contains(&(2..12)));
    }

    #[test]
    fn cursor_in_a_multiline_callouts_body_does_not_touch_the_title() {
        let text = "> [!note] Title\n> Body line one\n> Body line two\n";
        // Cursor inside "Body line one" (second line) -- the body stays
        // independently live-previewed; it shouldn't couple back to
        // revealing the title's raw `[!note]` text.
        let cursor = text.find("line one").unwrap();
        let result = plan(text, &[cursor..cursor]);
        let callout = result.callouts.first().expect("expected one callout");
        assert!(!callout.touched);
        assert!(
            result.hidden_markers.is_empty()
                || !result.hidden_markers.contains(&callout.marker_range)
        );
        assert!(!result.dimmed_markers.contains(&callout.marker_range));
    }

    #[test]
    fn callout_fold_suffix_states_parse_correctly() {
        let none = plan("> [!note] a\n", &[]);
        assert_eq!(none.callouts.first().unwrap().fold, CalloutFold::None);

        let expanded = plan("> [!note]+ a\n", &[]);
        assert_eq!(
            expanded.callouts.first().unwrap().fold,
            CalloutFold::Expanded
        );

        let collapsed = plan("> [!note]- a\n", &[]);
        let collapsed_callout = collapsed.callouts.first().unwrap();
        assert_eq!(collapsed_callout.fold, CalloutFold::Collapsed);
        assert!(collapsed_callout.fold.is_collapsed());
        assert!(!CalloutFold::None.is_collapsed());
    }

    #[test]
    fn callout_suffix_range_supports_insert_and_replace() {
        // No suffix: a zero-width insertion point right after `]`.
        let none = plan("> [!note] a\n", &[]);
        let callout = none.callouts.first().unwrap();
        assert_eq!(
            callout.suffix_range,
            callout.marker_range.end..callout.marker_range.end
        );

        // An existing suffix: a one-byte range to replace or delete.
        let collapsed = plan("> [!note]- a\n", &[]);
        let callout = collapsed.callouts.first().unwrap();
        assert_eq!(callout.suffix_range.end - callout.suffix_range.start, 1);
        assert_eq!(callout.suffix_range.end, callout.marker_range.end);
    }

    #[test]
    fn unrecognized_callout_type_keeps_its_raw_name() {
        let result = plan("> [!todo] buy milk\n", &[]);
        let callout = result.callouts.first().expect("expected one callout");
        assert_eq!(callout.kind, CalloutKind::Other);
        assert_eq!(callout.raw_type_name, "todo");
    }

    #[test]
    fn nested_callouts_do_not_panic_and_each_gets_its_own_info() {
        let text = "> [!note] outer\n> > [!warning] inner\n";
        let result = plan(text, &[]);
        assert_eq!(result.callouts.len(), 2);
        assert!(result.callouts.iter().any(|c| c.kind == CalloutKind::Note));
        assert!(
            result
                .callouts
                .iter()
                .any(|c| c.kind == CalloutKind::Warning)
        );
    }

    #[test]
    fn plain_blockquote_is_not_a_callout() {
        let text = "> just a quote\n";
        let result = plan(text, &[]);
        assert!(
            !result
                .styled_spans
                .iter()
                .any(|(_, style)| matches!(style, SpanStyle::Callout(_)))
        );
    }

    #[test]
    fn callout_kind_recognizes_common_aliases() {
        assert_eq!(CalloutKind::from_type_name("NOTE"), CalloutKind::Note);
        assert_eq!(CalloutKind::from_type_name("info"), CalloutKind::Note);
        assert_eq!(CalloutKind::from_type_name("tip"), CalloutKind::Tip);
        assert_eq!(CalloutKind::from_type_name("caution"), CalloutKind::Warning);
        assert_eq!(CalloutKind::from_type_name("bug"), CalloutKind::Danger);
        assert_eq!(
            CalloutKind::from_type_name("something-else"),
            CalloutKind::Other
        );
    }

    #[test]
    fn no_fold_inducing_range_ever_overlaps_another_m2() {
        // hidden_markers, glyph_markers, and checkboxes all become creases in
        // the real editor and share the same fold space, so all three
        // together must be globally disjoint or folding panics (same class
        // of bug as `no_hidden_range_ever_overlaps_another`).
        for text in [
            "- [ ] a\n- [x] b\n",
            "1. one\n2. two\n   - nested\n   - list\n",
            "> [!note] title\n> body **bold** text\n",
            "- item with **bold** and a [!note] look-alike\n",
        ] {
            let result = plan(text, &[]);
            let mut all: Vec<Range<usize>> = result
                .hidden_markers
                .iter()
                .cloned()
                .chain(result.checkboxes.iter().map(|(range, _)| range.clone()))
                .chain(result.glyph_markers.iter().map(|(range, _)| range.clone()))
                .collect();
            all.sort_by_key(|range| range.start);
            for window in all.windows(2) {
                assert!(
                    window[0].end <= window[1].start,
                    "overlapping fold ranges {:?} and {:?} for {text:?}",
                    window[0],
                    window[1]
                );
            }
        }
    }

    /// Collects every range the real editor turns into a fold, for the
    /// overlap check shared by several tests below.
    fn fold_inducing_ranges(result: &Plan) -> Vec<Range<usize>> {
        let mut all: Vec<Range<usize>> = result
            .hidden_markers
            .iter()
            .cloned()
            .chain(result.checkboxes.iter().map(|(range, _)| range.clone()))
            .chain(result.task_marks.iter().map(|(range, _)| range.clone()))
            .chain(result.glyph_markers.iter().map(|(range, _)| range.clone()))
            .chain(
                result
                    .extension_replacements
                    .iter()
                    .map(|(range, _)| range.clone()),
            )
            .collect();
        all.sort_by_key(|range| range.start);
        all
    }

    fn assert_no_overlaps(ranges: &[Range<usize>], text: &str) {
        for window in ranges.windows(2) {
            assert!(
                window[0].end <= window[1].start,
                "overlapping fold ranges {:?} and {:?} for {text:?}",
                window[0],
                window[1]
            );
        }
    }

    #[test]
    fn plan_viewport_matches_full_plan_when_range_covers_everything() {
        let text = "# H\n\n- [ ] a\n- [x] b\n\n> [!note] hi **bold** text\n";
        assert_eq!(plan_viewport(text, &[], 0..text.len()), plan(text, &[]));
    }

    /// The whole point of `plan_viewport`: a construct entirely outside the
    /// requested range contributes no decorations at all, while one that
    /// does overlap is decorated exactly as `plan` would decorate it.
    #[test]
    fn plan_viewport_prunes_constructs_outside_the_visible_range() {
        let text = "# First heading\n\nSecond paragraph with **bold** text.\n";
        let second_paragraph_start = text.find("Second").unwrap();

        let scoped = plan_viewport(text, &[], second_paragraph_start..text.len());
        assert!(
            scoped
                .hidden_markers
                .iter()
                .all(|range| range.start >= second_paragraph_start),
            "heading marker should have been pruned: {:?}",
            scoped.hidden_markers
        );
        assert!(
            scoped
                .styled_spans
                .iter()
                .any(|(range, style)| *style == SpanStyle::Bold
                    && range.start >= second_paragraph_start),
            "the in-range bold span should still be planned: {:?}",
            scoped.styled_spans
        );
        assert!(
            !scoped
                .styled_spans
                .iter()
                .any(|(_, style)| matches!(style, SpanStyle::Heading(_))),
            "the offscreen heading should not have been planned at all: {:?}",
            scoped.styled_spans
        );
    }

    /// A selection must always reveal its own construct's raw markers, even
    /// if the caller's notion of "visible" is stale relative to where the
    /// cursor actually is (see `plan_viewport`'s doc comment).
    #[test]
    fn plan_viewport_still_reveals_a_selection_outside_the_given_range() {
        let text = "Some **bold** text far from the viewport.\n";
        let cursor_inside_bold = 8..8;
        // An empty visible range elsewhere in the document; the selection
        // must still win.
        let scoped = plan_viewport(text, &[cursor_inside_bold], 0..0);
        assert_eq!(scoped.dimmed_markers, vec![5..7, 11..13]);
    }

    /// Deterministic xorshift PRNG so this test suite stays dependency-free.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, bound: usize) -> usize {
            (self.next() as usize) % bound.max(1)
        }
    }

    /// Assembles a pseudo-random document out of every construct this
    /// planner understands, so property tests below exercise combinations no
    /// hand-written fixture would think to try.
    fn random_document(rng: &mut Rng, lines: usize) -> String {
        let fragments = [
            "# Heading text\n",
            "## Nested heading\n",
            "Plain paragraph text with nothing special.\n",
            "Some **bold**, *italic*, ***both***, ~~strike~~, ==mark==, and `code`.\n",
            "- bullet one\n",
            "- [ ] unchecked task\n",
            "- [x] checked task\n",
            "1. ordered one\n",
            "2. ordered two\n",
            "> plain quote\n",
            "> [!warning] callout title\n",
            "> continuation of a quote\n",
            "[a link](https://example.com)\n",
            "<https://example.com>\n",
            "See [[Note]] and [[Other Note|an alias]] near #tag/sub.\n",
            "- [[Note#Heading]] and [[#Here]] with #tag\n",
            "**[[bold link]]** and ==[[marked #tag]]== and [x [[inside]]](https://example.com)\n",
            "Code `[[not a link]]` and `#not-a-tag` and ![[embed.png]] inline\n",
            "Aside %%a comment with [[a link]] and #tag%% and a block id ^blk-1\n",
            "- [/] in progress with [[Note]]\n",
            "1. [-] cancelled #tag ^id\n",
            "- [?] a question %%aside%%\n",
            "%% a comment that\nruns over lines %% and `%%code%%` text\n",
            "---\n",
            "```rust\nfn main() {}\n```\n",
            "| A | B |\n|--|--:|\n| 1 | 2 |\n",
            "\n",
        ];
        let mut text = String::new();
        for _ in 0..lines {
            text.push_str(fragments[rng.below(fragments.len())]);
        }
        text
    }

    #[test]
    fn property_random_documents_never_produce_overlapping_folds_or_panic() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..200 {
            let lines = 1 + rng.below(40);
            let text = random_document(&mut rng, lines);
            let selections = if rng.below(2) == 0 {
                vec![]
            } else {
                let start = rng.below(text.len() + 1);
                let end = start + rng.below(text.len() + 1 - start);
                vec![start..end]
            };
            let result = plan(&text, &selections);
            assert_no_overlaps(&fold_inducing_ranges(&result), &text);
            for range in fold_inducing_ranges(&result) {
                assert!(
                    range.end <= text.len(),
                    "out-of-bounds fold range {range:?} for {text:?}"
                );
            }
            for (range, _) in &result.styled_spans {
                assert!(
                    range.end <= text.len(),
                    "out-of-bounds styled span {range:?} for {text:?}"
                );
            }
        }
    }

    /// Same random corpus, but checking `plan_viewport`'s pruning specifically:
    /// restricting to a sub-range must never produce a fold-inducing range
    /// that plan() (the unrestricted equivalent) didn't already produce, and
    /// must still never overlap.
    #[test]
    fn property_random_viewports_are_a_subset_of_the_full_plan_and_never_overlap() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..200 {
            let lines = 1 + rng.below(40);
            let text = random_document(&mut rng, lines);
            if text.is_empty() {
                continue;
            }
            let start = rng.below(text.len());
            let end = start + rng.below(text.len() - start + 1);

            let full = plan(&text, &[]);
            let scoped = plan_viewport(&text, &[], start..end);

            assert_no_overlaps(&fold_inducing_ranges(&scoped), &text);

            let full_hidden: std::collections::HashSet<_> =
                full.hidden_markers.iter().cloned().collect();
            for range in &scoped.hidden_markers {
                assert!(
                    full_hidden.contains(range),
                    "scoped plan invented a hidden range {range:?} the full plan didn't have, for {text:?}"
                );
            }
        }
    }

    /// Regression tripwire, not a strict benchmark: on a document with tens
    /// of thousands of decoration-inducing lines, `plan_viewport` restricted
    /// to a small window must (a) finish in a generous but bounded time, and
    /// (b) actually emit far fewer decorations than an unrestricted `plan`
    /// over the same text — proving the pruning in `walk_block` is doing
    /// real work, not a no-op.
    #[test]
    fn plan_viewport_scopes_work_on_a_large_document() {
        let mut rng = Rng(0xc0ff_ee15_dead_beef);
        let text = random_document(&mut rng, 20_000);

        let window_start = text.len() / 2;
        let window_end = window_start + 200;

        let started = std::time::Instant::now();
        let scoped = plan_viewport(&text, &[], window_start..window_end);
        let scoped_elapsed = started.elapsed();
        assert!(
            scoped_elapsed < std::time::Duration::from_secs(5),
            "scoped plan over a large document took {scoped_elapsed:?}, which is suspiciously slow"
        );

        let full = plan(&text, &[]);
        let scoped_decorations = fold_inducing_ranges(&scoped).len() + scoped.styled_spans.len();
        let full_decorations = fold_inducing_ranges(&full).len() + full.styled_spans.len();
        assert!(
            scoped_decorations * 20 < full_decorations,
            "scoped plan ({scoped_decorations} decorations) should be far smaller than the full \
             plan ({full_decorations} decorations) for a large document"
        );
    }

    /// A fold-inducing range that silently swallowed a `\n` would merge two
    /// buffer rows into what the display layer sees as one row (the
    /// per-row chunk-counting in `element.rs`'s `from_chunks` only advances
    /// on a literal `\n` in the chunk stream) — every row after it would
    /// then be off by one, which is exactly the kind of thing that would
    /// make a mouse-driven selection or copy silently skip a line break.
    /// Checked directly here (byte-level, on the planner's own output)
    /// rather than only indirectly via the editor-integration tests, since
    /// this is the one invariant that must hold for every construct
    /// category, not just the ones with dedicated fixtures.
    #[test]
    fn no_fold_inducing_range_contains_a_newline_byte() {
        for text in [
            "> line one\n> line two\n",
            "> > nested\n> > second\n",
            "> [!note] title\n> body line two\n> body line three\n",
            "- [ ] a\n- [x] b\n- plain\n",
            "1. one\n2. two\n   - nested\n",
            "# Heading One\n\nSome regular paragraph text that should render completely unstyled by visual_md.\n\nSome **bold**, *italic*, ***both***, ~~strike~~, ==highlight==, and `code`.\n\n## Heading Two\n",
        ] {
            let result = plan(text, &[]);
            let all: Vec<(&str, Range<usize>)> = result
                .hidden_markers
                .iter()
                .cloned()
                .map(|r| ("hidden", r))
                .chain(
                    result
                        .glyph_markers
                        .iter()
                        .map(|(r, _)| ("glyph", r.clone())),
                )
                .chain(
                    result
                        .checkboxes
                        .iter()
                        .map(|(r, _)| ("checkbox", r.clone())),
                )
                .chain(result.dimmed_markers.iter().cloned().map(|r| ("dimmed", r)))
                .collect();
            for (label, range) in &all {
                let slice = &text[range.clone()];
                assert!(
                    !slice.contains('\n'),
                    "{label} range {range:?} ({slice:?}) contains a newline byte for {text:?}"
                );
            }
        }
    }

    #[test]
    fn link_hidden_when_not_touched() {
        let text = "[Zed](https://zed.dev)\n";
        let result = plan(text, &[]);
        assert_eq!(result.hidden_markers, vec![0..1, 4..22]);
        assert!(result.dimmed_markers.is_empty());
        assert_eq!(result.styled_spans, vec![(1..4, SpanStyle::Link)]);
    }

    #[test]
    fn link_dimmed_when_cursor_touches() {
        let text = "[Zed](https://zed.dev)\n";
        let result = plan(text, &[2..2]); // cursor inside "Zed"
        assert!(result.hidden_markers.is_empty());
        assert_eq!(result.dimmed_markers, vec![0..1, 4..22]);
        // The link text stays styled regardless of raw/rendered state, same
        // as bold/italic.
        assert_eq!(result.styled_spans, vec![(1..4, SpanStyle::Link)]);
    }

    #[test]
    fn autolink_hidden_when_not_touched() {
        let text = "<https://zed.dev>\n";
        let result = plan(text, &[]);
        assert_eq!(result.hidden_markers, vec![0..1, 16..17]);
        assert!(result.dimmed_markers.is_empty());
        assert_eq!(result.styled_spans, vec![(1..16, SpanStyle::Link)]);
    }

    #[test]
    fn autolink_dimmed_when_cursor_touches() {
        let text = "<https://zed.dev>\n";
        let result = plan(text, &[5..5]);
        assert!(result.hidden_markers.is_empty());
        assert_eq!(result.dimmed_markers, vec![0..1, 16..17]);
    }

    #[test]
    fn reference_links_to_a_defined_label_show_only_their_text() {
        let definition = "\n[ref]: https://example.com\n[text]: /t\n";
        for (written, hidden_before, hidden_after) in [
            ("a [text][ref] b", 2..3, 7..13),
            ("a [text][] b", 2..3, 7..10),
            ("a [ref] b", 2..3, 6..7),
        ] {
            let text = format!("{written}\n{definition}");
            let result = plan(&text, &[]);
            assert_eq!(
                result.hidden_markers,
                vec![hidden_before, hidden_after],
                "for {written:?}"
            );
            let link = spans(&text, SpanStyle::Link);
            assert_eq!(link.len(), 1, "for {written:?}");
            assert_eq!(
                text.get(link[0].clone()),
                Some(if written.contains("text") {
                    "text"
                } else {
                    "ref"
                }),
                "for {written:?}"
            );
        }
    }

    #[test]
    fn reference_links_match_their_label_without_regard_to_case_or_spacing() {
        let text = "see [A  Thing][a thing] and [Other Thing]\n\n[A THING]: <https://a.example>\n[other thing]: /b\n";
        assert_eq!(spans(text, SpanStyle::Link).len(), 2);
    }

    #[test]
    fn a_wikilink_is_not_a_reference_link_to_a_label_of_the_same_name() {
        let text = "see [[ref]] and ![[ref]]\n\n[ref]: https://example.com\n";
        assert!(spans(text, SpanStyle::Link).is_empty());
    }

    #[test]
    fn a_reference_link_whose_label_is_not_defined_stays_text() {
        let text = "[text][nope] and [nope]\n\n[other]: https://example.com\n";
        assert!(spans(text, SpanStyle::Link).is_empty());
        assert!(plan(text, &[]).hidden_markers.is_empty());
    }

    #[test]
    fn a_reference_link_is_dimmed_while_the_cursor_is_on_it() {
        let text = "a [text][ref] b\n\n[ref]: https://example.com\n";
        let result = plan(text, &[5..5]);
        assert!(result.hidden_markers.is_empty());
        assert_eq!(result.dimmed_markers, vec![2..3, 7..13]);
        assert_eq!(spans(text, SpanStyle::Link).len(), 1);
    }

    #[test]
    fn a_definition_anywhere_in_the_document_counts_even_out_of_view() {
        let mut text = String::from("a [text][ref] b\n\n");
        for line in 0..200 {
            text.push_str(&format!("filler {line}\n\n"));
        }
        text.push_str("[ref]: https://example.com\n");
        let tree = parse_blocks(&text).expect("parses");

        let result = plan_viewport_with_tree(&text, &tree, &[], 0..20);

        assert_eq!(result.hidden_markers, vec![2..3, 7..13]);
    }

    #[test]
    fn the_first_definition_of_a_label_is_the_one_that_counts() {
        let text = "[ref]: https://first.example\n[REF]: https://second.example\n";
        let tree = parse_blocks(text).expect("parses");
        let definitions = link_definitions(text, &tree);
        assert_eq!(
            definitions.get("ref").map(String::as_str),
            Some("https://first.example")
        );
        assert_eq!(definitions.len(), 1);
    }

    #[test]
    fn definitions_are_found_inside_lists_and_quotes_and_without_brackets_in_text() {
        let text = "> [quoted]: /q\n\n- item\n\n  [nested]: </n>\n\nno definitions]: here\n";
        let tree = parse_blocks(text).expect("parses");
        let definitions = link_definitions(text, &tree);
        assert_eq!(definitions.get("quoted").map(String::as_str), Some("/q"));
        assert!(definitions.contains_key("nested"));
        assert!(!definitions.contains_key("no definitions"));
    }

    #[test]
    fn a_document_without_definitions_is_not_walked_for_them() {
        let text = "# Title\n\n[text][ref]\n";
        let tree = parse_blocks(text).expect("parses");
        assert!(link_definitions(text, &tree).is_empty());
    }

    #[test]
    fn reference_style_links_are_left_completely_raw() {
        // No definition backs any of these, so they are text in brackets and
        // nothing about the line should be touched.
        for text in ["[shortcut]\n", "[ref link][1]\n", "[collapsed][]\n"] {
            let result = plan(text, &[]);
            assert!(
                spans(text, SpanStyle::Link).is_empty(),
                "unexpected link styling for {text:?}"
            );
            assert!(
                result.hidden_markers.is_empty(),
                "unexpected hidden markers for {text:?}"
            );
            assert!(
                result.dimmed_markers.is_empty(),
                "unexpected dimmed markers for {text:?}"
            );
        }
    }

    #[test]
    fn link_nested_inside_bold_still_gets_styled() {
        let text = "**[Zed](https://zed.dev)**\n";
        let link_spans = spans(text, SpanStyle::Link);
        let bold_spans = spans(text, SpanStyle::Bold);
        assert_eq!(link_spans.len(), 1);
        assert_eq!(bold_spans.len(), 1);
        assert!(
            bold_spans[0].start <= link_spans[0].start && link_spans[0].end <= bold_spans[0].end,
            "link span {:?} should sit inside bold span {:?}",
            link_spans[0],
            bold_spans[0]
        );
    }

    #[test]
    fn image_and_email_are_not_mistaken_for_a_markdown_link() {
        // `image` is a distinct node kind from `inline_link` (embeds are a
        // separate, not-yet-implemented milestone) -- confirm it never picks
        // up link styling by accident.
        let text = "![alt text](image.png)\n";
        assert!(spans(text, SpanStyle::Link).is_empty());
    }

    #[test]
    fn email_autolink_is_styled_like_a_uri_autolink() {
        let text = "<user@example.com>\n";
        let result = plan(text, &[]);
        assert_eq!(result.hidden_markers, vec![0..1, 17..18]);
        assert_eq!(result.styled_spans, vec![(1..17, SpanStyle::Link)]);
    }

    #[test]
    fn thematic_break_variants_populate_horizontal_rules() {
        for text in ["---\n", "***\n", "___\n", "- - -\n"] {
            let result = plan(text, &[]);
            assert_eq!(
                result.horizontal_rules,
                vec![0..text.len() - 1],
                "for {text:?}: the rule is one row, so it stops before the newline"
            );
            assert!(result.hidden_markers.is_empty());
            assert!(
                result.glyph_markers.is_empty(),
                "a `- - -` rule must not be mistaken for a list, for {text:?}"
            );
        }
    }

    #[test]
    fn touched_thematic_break_is_left_alone() {
        let text = "---\n";
        let result = plan(text, &[1..1]);
        assert!(result.horizontal_rules.is_empty());
        assert!(result.hidden_markers.is_empty());
        assert!(result.dimmed_markers.is_empty());
    }

    #[test]
    fn fenced_code_block_captures_language_and_borders() {
        let text = "```rust\nfn main() {}\n```\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.code_fence_borders,
            vec![(0..7, Some("rust".to_string())), (21..24, None)]
        );
        assert_eq!(
            result.code_fence_content,
            vec![(8..21, Some("rust".to_string()))]
        );
    }

    #[test]
    fn fenced_code_block_with_tilde_fence() {
        let text = "~~~python\nprint(1)\n~~~\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.code_fence_borders,
            vec![(0..9, Some("python".to_string())), (19..22, None)]
        );
        assert_eq!(
            result.code_fence_content,
            vec![(10..19, Some("python".to_string()))]
        );
    }

    #[test]
    fn fenced_code_block_indentation_is_kept_in_border_range() {
        let text = "  ```js\n  indented\n  ```\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.code_fence_borders,
            vec![(0..7, Some("js".to_string())), (19..24, None)]
        );
    }

    #[test]
    fn fenced_code_block_with_no_language_still_produces_content_entry() {
        let text = "```\nno language\n```\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.code_fence_borders,
            vec![(0..3, None), (16..19, None)]
        );
        assert_eq!(result.code_fence_content, vec![(4..16, None)]);
    }

    #[test]
    fn touched_opening_fence_line_is_excluded_but_content_stays() {
        let text = "```rust\nfn main() {}\n```\n";
        let result = plan(text, &[1..1]); // cursor on the opening fence line
        assert_eq!(
            result.code_fence_borders,
            vec![(21..24, None)],
            "closing line still not touched"
        );
        assert_eq!(
            result.code_fence_content,
            vec![(8..21, Some("rust".to_string()))],
            "content stays present regardless of cursor position, per spec"
        );
    }

    #[test]
    fn touched_closing_fence_line_is_excluded_but_content_stays() {
        let text = "```rust\nfn main() {}\n```\n";
        let result = plan(text, &[22..22]); // cursor on the closing fence line
        assert_eq!(
            result.code_fence_borders,
            vec![(0..7, Some("rust".to_string()))]
        );
        assert_eq!(
            result.code_fence_content,
            vec![(8..21, Some("rust".to_string()))]
        );
    }

    #[test]
    fn touched_content_line_leaves_both_fence_borders_alone() {
        let text = "```rust\nfn main() {}\n```\n";
        let result = plan(text, &[10..10]); // cursor inside the code content
        assert_eq!(
            result.code_fence_borders,
            vec![(0..7, Some("rust".to_string())), (21..24, None)],
            "cursor being in the content shouldn't reveal either fence line"
        );
    }

    fn plan_with_task_marks(text: &str, selections: &[Range<usize>], marks: &str) -> Plan {
        let tree = parse_blocks(text).expect("the document should parse");
        let extensions = PlanExtensions {
            task_marks: marks.chars().collect(),
            ..Default::default()
        };
        plan_viewport_with_extensions(text, &tree, selections, 0..text.len(), &extensions)
    }

    #[test]
    fn a_known_task_mark_gets_a_checkbox_and_its_bullet_goes() {
        let text = "- [/] in progress\n";
        let result = plan_with_task_marks(text, &[], "/-");

        assert_eq!(result.task_marks, vec![(2..5, '/')]);
        assert_eq!(
            result.hidden_markers,
            vec![0..2],
            "the bullet and its space"
        );
        assert!(result.glyph_markers.is_empty());
        assert!(result.checkboxes.is_empty());
    }

    #[test]
    fn an_unknown_task_mark_is_left_as_text() {
        let text = "- [~] unknown\n";
        let result = plan_with_task_marks(text, &[], "/-");

        assert!(result.task_marks.is_empty());
        assert_eq!(
            result.glyph_markers,
            vec![(0..2, GlyphKind::Bullet)],
            "an ordinary item"
        );
        assert!(
            plan(text, &[]).task_marks.is_empty(),
            "nothing is known by default"
        );
    }

    #[test]
    fn the_standard_marks_are_still_checkboxes() {
        let text = "- [ ] open\n- [x] done\n";
        let result = plan_with_task_marks(text, &[], "/ x");

        assert_eq!(result.checkboxes, vec![(2..5, false), (13..16, true)]);
        assert!(result.task_marks.is_empty());
    }

    #[test]
    fn a_task_mark_needs_a_space_or_the_end_of_the_line_after_it() {
        assert!(
            plan_with_task_marks("- [/]text\n", &[], "/")
                .task_marks
                .is_empty()
        );
        assert_eq!(
            plan_with_task_marks("- [/]\n", &[], "/").task_marks.len(),
            1
        );
        assert!(
            plan_with_task_marks("- text [/] more\n", &[], "/")
                .task_marks
                .is_empty(),
            "only at the start of the item"
        );
    }

    #[test]
    fn task_marks_work_in_ordered_lists_and_with_wide_characters() {
        let ordered = plan_with_task_marks("1. [-] cancelled\n", &[], "-");
        assert_eq!(ordered.task_marks, vec![(3..6, '-')]);
        assert_eq!(ordered.hidden_markers, vec![0..3], "the number goes too");

        let wide = plan_with_task_marks("- [★] starred\n", &[], "★");
        assert_eq!(wide.task_marks, vec![(2..7, '★')]);
    }

    #[test]
    fn property_task_marks_never_make_overlapping_folds() {
        let mut rng = Rng(0x7a5c_0dd5_1234_5678);
        for _ in 0..200 {
            let lines = 1 + rng.below(40);
            let text = random_document(&mut rng, lines);
            let selections = if rng.below(2) == 0 {
                vec![]
            } else {
                let start = rng.below(text.len() + 1);
                vec![start..start]
            };
            let result = plan_with_task_marks(&text, &selections, "/-?");

            assert_no_overlaps(&fold_inducing_ranges(&result), &text);
        }
    }

    fn wikilink_texts<'a>(result: &'a Plan, text: &'a str) -> Vec<(&'a str, &'a str)> {
        result
            .wikilinks
            .iter()
            .map(|link| (&text[link.range.clone()], link.note.as_str()))
            .collect()
    }

    #[test]
    fn a_wikilink_hides_its_brackets_and_shows_its_target() {
        let text = "see [[Note]] now\n";
        let result = plan(text, &[]);

        assert_eq!(result.hidden_markers, vec![4..6, 10..12]);
        assert!(result.dimmed_markers.is_empty());
        assert_eq!(wikilink_texts(&result, text), vec![("Note", "Note")]);
    }

    #[test]
    fn an_alias_is_shown_and_the_target_before_it_is_hidden() {
        let text = "[[Some Note|the alias]]\n";
        let result = plan(text, &[]);

        assert_eq!(result.hidden_markers, vec![0..12, 21..23]);
        assert_eq!(
            wikilink_texts(&result, text),
            vec![("the alias", "Some Note")]
        );
    }

    #[test]
    fn the_note_of_a_link_leaves_out_its_heading_or_block() {
        let text = "[[Note#Heading]] [[Note#^abc]] [[#Here]]\n";
        let result = plan(text, &[]);

        assert_eq!(
            wikilink_texts(&result, text),
            vec![
                ("Note#Heading", "Note"),
                ("Note#^abc", "Note"),
                ("#Here", "")
            ]
        );
    }

    #[test]
    fn a_touched_wikilink_shows_its_brackets_dimmed_and_still_counts() {
        let text = "see [[Note|alias]] now\n";
        let result = plan(text, &[12..12]);

        assert!(result.hidden_markers.is_empty());
        assert_eq!(result.dimmed_markers, vec![4..11, 16..18]);
        assert_eq!(wikilink_texts(&result, text), vec![("alias", "Note")]);

        let at_its_edge = plan(text, &[4..4]);
        assert!(at_its_edge.hidden_markers.is_empty(), "the edge touches it");
        let before_it = plan(text, &[3..3]);
        assert_eq!(before_it.hidden_markers.len(), 2, "one byte off does not");
        let away = plan(text, &[20..20]);
        assert_eq!(away.hidden_markers.len(), 2);
    }

    #[test]
    fn a_wikilink_in_code_or_an_autolink_is_not_one() {
        for text in ["`[[a]]`\n", "<https://example.com/[[c]]>\n"] {
            let result = plan(text, &[]);
            assert!(
                result.wikilinks.is_empty(),
                "{text:?} gave {:?}",
                result.wikilinks
            );
        }
    }

    #[test]
    fn an_embed_inside_a_line_is_left_alone() {
        let text = "a ![[pic.png]] b\n";
        let result = plan(text, &[]);

        assert!(result.wikilinks.is_empty());
        assert!(result.hidden_markers.is_empty());
    }

    #[test]
    fn wikilinks_work_in_headings_lists_and_quotes() {
        let text = "# About [[Heading Link]]\n\n- item [[List Link]]\n\n> quoted [[Quote Link]]\n";
        let result = plan(text, &[]);

        let notes: Vec<&str> = result
            .wikilinks
            .iter()
            .map(|link| link.note.as_str())
            .collect();
        assert_eq!(notes, vec!["Heading Link", "List Link", "Quote Link"]);
    }

    #[test]
    fn a_comment_is_hidden_until_the_cursor_is_in_it() {
        let text = "a %%hidden%% b\n";

        let away = plan(text, &[0..0]);
        assert_eq!(away.hidden_markers, vec![2..12]);
        assert!(away.dimmed_markers.is_empty());

        let inside = plan(text, &[6..6]);
        assert!(inside.hidden_markers.is_empty());
        assert_eq!(inside.dimmed_markers, vec![2..12]);
    }

    #[test]
    fn a_comment_over_several_lines_is_dimmed_and_never_hidden() {
        let text = "a %%one\ntwo%% b\n";
        let result = plan(text, &[]);

        assert!(result.hidden_markers.is_empty(), "a fold cannot span lines");
        assert_eq!(result.dimmed_markers, vec![2..13]);
    }

    #[test]
    fn nothing_inside_a_comment_is_anything_else() {
        let text = "%% [[Note]] #tag ^id %% after\n";
        let result = plan(text, &[]);

        assert_eq!(result.hidden_markers, vec![0..23]);
        assert!(result.wikilinks.is_empty());
        assert!(result.styled_spans.is_empty());
        assert!(result.dimmed_markers.is_empty());
    }

    #[test]
    fn a_comment_in_code_is_not_one() {
        let result = plan("`%%x%%` and 100% and 5% more\n", &[]);

        assert!(result.hidden_markers.is_empty());
    }

    #[test]
    fn a_block_id_is_dimmed_with_or_without_the_cursor() {
        let text = "a paragraph ^abc\n";

        assert_eq!(plan(text, &[]).dimmed_markers, vec![12..16]);
        assert_eq!(plan(text, &[3..3]).dimmed_markers, vec![12..16]);
        assert_eq!(plan("- item ^id\n", &[]).dimmed_markers, vec![7..10]);
    }

    #[test]
    fn something_that_only_looks_like_a_block_id_is_not_one() {
        assert!(plan("x^2 and 2 ^n more\n", &[]).dimmed_markers.is_empty());

        // The backticks of inline code are dimmed, but the id inside is not.
        let text = "`code ^id`\n";
        let result = plan(text, &[]);
        assert!(
            result
                .dimmed_markers
                .iter()
                .all(|range| !text[range.clone()].contains("^id"))
        );
    }

    fn tag_texts<'a>(result: &Plan, text: &'a str) -> Vec<&'a str> {
        result
            .styled_spans
            .iter()
            .filter(|(_, style)| *style == SpanStyle::Tag)
            .map(|(range, _)| &text[range.clone()])
            .collect()
    }

    #[test]
    fn tags_are_styled_with_their_hash() {
        let text = "a #tag and #nested/tag, not #12 or x#no\n";
        let result = plan(text, &[]);

        assert_eq!(tag_texts(&result, text), vec!["#tag", "#nested/tag"]);
        assert!(result.hidden_markers.is_empty(), "a tag hides nothing");
    }

    #[test]
    fn a_tag_in_code_a_link_or_a_wikilink_is_not_one() {
        let text = "`#code` [x](#anchor) [[Note #spaced]] #real\n";
        let result = plan(text, &[]);

        assert_eq!(tag_texts(&result, text), vec!["#real"]);
    }

    #[test]
    fn tags_in_headings_count() {
        let text = "## Plans #later\n";
        let result = plan(text, &[]);

        assert_eq!(tag_texts(&result, text), vec!["#later"]);
    }

    #[test]
    fn a_cursor_on_the_first_code_line_does_not_reveal_the_opening_fence() {
        let text = "```rust\nfn main() {}\n```\n";

        // Offset 7 is the end of the fence line, offset 8 the start of the
        // next one: only the first is on the fence line.
        let on_the_fence = plan(text, &[7..7]);
        assert_eq!(on_the_fence.code_fence_borders, vec![(21..24, None)]);

        let on_the_code = plan(text, &[8..8]);
        assert_eq!(on_the_code.code_fence_borders.len(), 2);
    }

    #[test]
    fn a_cursor_after_the_closing_fence_does_not_reveal_it() {
        let text = "```rust\nfn main() {}\n```\nafter\n";

        let on_the_fence = plan(text, &[24..24]);
        assert_eq!(on_the_fence.code_fence_borders.len(), 1);

        let on_the_next_line = plan(text, &[25..25]);
        assert_eq!(on_the_next_line.code_fence_borders.len(), 2);
    }

    #[test]
    fn a_cursor_on_the_line_after_a_rule_does_not_reveal_it() {
        let text = "---\nafter\n";

        assert!(plan(text, &[3..3]).horizontal_rules.is_empty());
        assert_eq!(plan(text, &[4..4]).horizontal_rules, vec![0..3]);
    }

    #[test]
    fn the_end_of_a_line_stops_before_its_newline_when_it_has_one() {
        assert_eq!(end_of_line_content("a\n", 2), 1);
        assert_eq!(end_of_line_content("ab\ncd\n", 3), 2);
        assert_eq!(end_of_line_content("\n", 1), 0);
        assert_eq!(
            end_of_line_content("a", 1),
            1,
            "no newline, nothing to trim"
        );
        assert_eq!(
            end_of_line_content("ab\ncd", 2),
            2,
            "an end before the newline stays"
        );
        assert_eq!(end_of_line_content("", 0), 0);
    }

    #[test]
    fn property_a_fence_line_or_rule_never_spans_a_newline() {
        let mut rng = Rng(0x0bad_cafe_f00d_1234);
        for _ in 0..300 {
            let lines = 1 + rng.below(40);
            let text = random_document(&mut rng, lines);
            let selections = if rng.below(2) == 0 {
                vec![]
            } else {
                let start = rng.below(text.len() + 1);
                vec![start..start]
            };
            let result = plan(&text, &selections);

            let single_row_blocks = result
                .code_fence_borders
                .iter()
                .map(|(range, _)| range)
                .chain(&result.horizontal_rules);
            for range in single_row_blocks {
                assert!(
                    !text[range.clone()].contains('\n'),
                    "{range:?} would make a one-row block replace two rows in {text:?}"
                );
            }
        }
    }

    fn plan_with_claimed_languages(
        text: &str,
        selections: &[Range<usize>],
        languages: &[&str],
    ) -> Plan {
        let tree = parse_blocks(text).expect("the document should parse");
        let extensions = PlanExtensions {
            rendered_fence_languages: languages
                .iter()
                .map(|language| language.to_string())
                .collect(),
            ..Default::default()
        };
        plan_viewport_with_extensions(text, &tree, selections, 0..text.len(), &extensions)
    }

    #[test]
    fn claimed_fenced_code_block_is_rendered_without_borders_or_content() {
        let text = "```flow\na -> b\n```\n";
        let result = plan_with_claimed_languages(text, &[], &["flow"]);

        assert_eq!(
            result.rendered_fences,
            vec![RenderedFence {
                range: 0..18,
                language: "flow".to_string(),
                info: "flow".to_string(),
                content_range: 8..15,
                content_hash: hash_text("a -> b\n"),
            }]
        );
        assert!(result.code_fence_borders.is_empty());
        assert!(result.code_fence_content.is_empty());
    }

    #[test]
    fn unclaimed_fenced_code_block_is_planned_as_before() {
        let text = "```rust\nfn main() {}\n```\n";
        let result = plan_with_claimed_languages(text, &[], &["flow"]);

        assert!(result.rendered_fences.is_empty());
        assert_eq!(result.code_fence_borders.len(), 2);
        assert_eq!(result.code_fence_content.len(), 1);
        assert_eq!(result, plan(text, &[]));
    }

    #[test]
    fn claim_matches_the_language_ignoring_case_and_keeps_the_whole_info_string() {
        let text = "```Flow {width=3}\na -> b\n```\n";
        let result = plan_with_claimed_languages(text, &[], &["flow"]);

        let fence = &result.rendered_fences[0];
        assert_eq!(fence.language, "flow");
        assert_eq!(fence.info, "Flow {width=3}");
    }

    #[test]
    fn an_indented_claimed_fence_is_rendered_from_the_start_of_its_line() {
        let text = "  ```flow\n  a -> b\n  ```\n";
        let result = plan_with_claimed_languages(text, &[], &["flow"]);

        assert_eq!(result.rendered_fences.len(), 1, "got {result:?}");
        assert_eq!(result.rendered_fences[0].range, 0..text.len() - 1);
    }

    #[test]
    fn a_selection_touching_any_part_of_a_claimed_fence_reveals_it() {
        let text = "```flow\na -> b\n```\n";
        // Opening line, content, closing line, and the end of the closing line.
        for cursor in [0, 3, 10, 15, 16, 18] {
            let result = plan_with_claimed_languages(text, &[cursor..cursor], &["flow"]);
            assert!(
                result.rendered_fences.is_empty(),
                "cursor at {cursor} should reveal the source"
            );
            assert_eq!(
                result.code_fence_content.len(),
                1,
                "a revealed fence is planned like any other, cursor at {cursor}"
            );
        }
    }

    #[test]
    fn a_selection_on_the_next_line_leaves_a_claimed_fence_rendered() {
        let text = "```flow\na -> b\n```\nafter\n";
        let result = plan_with_claimed_languages(text, &[19..19], &["flow"]);

        assert_eq!(result.rendered_fences.len(), 1);
    }

    #[test]
    fn an_unclosed_claimed_fence_is_not_rendered() {
        let text = "```flow\na -> b\n";
        let result = plan_with_claimed_languages(text, &[], &["flow"]);

        assert!(result.rendered_fences.is_empty(), "got {result:?}");
    }

    #[test]
    fn a_fence_with_no_language_is_never_claimed() {
        let text = "```\na -> b\n```\n";
        let result = plan_with_claimed_languages(text, &[], &["flow", ""]);

        assert!(result.rendered_fences.is_empty());
    }

    #[test]
    fn a_change_to_the_content_changes_the_plan_even_when_the_ranges_do_not_move() {
        let before = plan_with_claimed_languages("```flow\na -> b\n```\n", &[], &["flow"]);
        let after = plan_with_claimed_languages("```flow\na -> c\n```\n", &[], &["flow"]);

        assert_eq!(
            before.rendered_fences[0].range,
            after.rendered_fences[0].range
        );
        assert_ne!(before, after);
    }

    #[test]
    fn claimed_fences_in_random_documents_never_overlap_anything_else_or_panic() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        for _ in 0..200 {
            let lines = 1 + rng.below(40);
            let text = random_document(&mut rng, lines);
            let selections = if rng.below(2) == 0 {
                vec![]
            } else {
                let start = rng.below(text.len() + 1);
                let end = start + rng.below(text.len() + 1 - start);
                vec![start..end]
            };
            let result = plan_with_claimed_languages(&text, &selections, &["rust"]);

            assert_no_overlaps(&fold_inducing_ranges(&result), &text);
            let mut previous_end = 0;
            for fence in &result.rendered_fences {
                assert!(
                    fence.range.start >= previous_end && fence.range.end <= text.len(),
                    "rendered fences overlap or run out of bounds: {:?} for {text:?}",
                    result.rendered_fences
                );
                previous_end = fence.range.end;
                assert!(text.is_char_boundary(fence.range.start));
                assert!(text.is_char_boundary(fence.range.end));
                assert!(
                    !text[fence.range.clone()].ends_with('\n'),
                    "a rendered fence must not include the closing line's newline"
                );
                for folded in fold_inducing_ranges(&result) {
                    assert!(
                        folded.end <= fence.range.start || folded.start >= fence.range.end,
                        "fold {folded:?} inside rendered fence {:?} for {text:?}",
                        fence.range
                    );
                }
                for (border, _) in &result.code_fence_borders {
                    assert!(border.end <= fence.range.start || border.start >= fence.range.end);
                }
                for embed in &result.embeds {
                    assert!(
                        embed.range.end <= fence.range.start
                            || embed.range.start >= fence.range.end
                    );
                }
            }
        }
    }

    #[test]
    fn table_structure_and_alignment_evenly_spaced() {
        let text = "| Name | Role |\n|------|-----:|\n| Ada  | Dev  |\n";
        let result = plan(text, &[]);
        assert_eq!(result.tables.len(), 1);
        let table = &result.tables[0];
        assert_eq!(
            table.alignments,
            vec![TableAlignment::Default, TableAlignment::Right]
        );
        assert_eq!(table.delimiter_line, Some(16..31));
        let pipes: Vec<Range<usize>> = result
            .glyph_markers
            .iter()
            .filter(|(_, kind)| *kind == GlyphKind::TablePipe)
            .map(|(range, _)| range.clone())
            .collect();
        assert_eq!(pipes, vec![0..1, 7..8, 14..15, 32..33, 39..40, 46..47]);
        assert_eq!(table.rows.len(), 2, "one header row + one data row");
        assert_eq!(
            table.rows[0],
            vec![
                TableCell {
                    leading_gap: 1..2,
                    content: 2..6,
                    trailing_gap: 6..7
                },
                TableCell {
                    leading_gap: 8..9,
                    content: 9..13,
                    trailing_gap: 13..14
                },
            ]
        );
        assert_eq!(
            table.rows[1],
            vec![
                TableCell {
                    leading_gap: 33..34,
                    content: 34..37,
                    trailing_gap: 37..39
                },
                TableCell {
                    leading_gap: 40..41,
                    content: 41..44,
                    trailing_gap: 44..46
                },
            ]
        );
    }

    #[test]
    fn table_all_four_alignments() {
        let text = "| A | B | C | D |\n|--|:--|--:|:-:|\n| 1 | 2 | 3 | 4 |\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.tables[0].alignments,
            vec![
                TableAlignment::Default,
                TableAlignment::Left,
                TableAlignment::Right,
                TableAlignment::Center,
            ]
        );
    }

    #[test]
    fn table_with_no_spacing_has_empty_gaps() {
        let text = "|A|B|\n|-|-|\n|1|2|\n";
        let result = plan(text, &[]);
        let table = &result.tables[0];
        for row in &table.rows {
            for cell in row {
                assert!(
                    cell.leading_gap.is_empty(),
                    "no source whitespace to widen: {cell:?}"
                );
                assert!(
                    cell.trailing_gap.is_empty(),
                    "no source whitespace to widen: {cell:?}"
                );
            }
        }
        // Content itself is still captured correctly despite no padding.
        assert_eq!(table.rows[0][0].content, 1..2);
        assert_eq!(table.rows[1][1].content, 15..16);
    }

    #[test]
    fn table_multi_row_groups_cells_by_column() {
        let text = "| A | B |\n|--|--|\n| 1 | 2 |\n| 3 | 4 |\n";
        let result = plan(text, &[]);
        let table = &result.tables[0];
        assert_eq!(table.rows.len(), 3, "header + two data rows");
        assert_eq!(table.rows[1].len(), 2);
        assert_eq!(table.rows[2].len(), 2);
    }

    #[test]
    fn touched_delimiter_line_is_excluded() {
        let text = "| A | B |\n|---|---|\n| 1 | 2 |\n";
        let result = plan(text, &[11..11]); // cursor on the delimiter row
        assert_eq!(result.tables[0].delimiter_line, None);
    }

    #[test]
    fn untouched_delimiter_line_is_present() {
        let text = "| A | B |\n|---|---|\n| 1 | 2 |\n";
        let result = plan(text, &[0..0]); // cursor elsewhere in the table
        assert!(result.tables[0].delimiter_line.is_some());
    }

    #[test]
    fn table_cell_with_bold_gets_both_decorations() {
        let text = "| A |\n|--|\n| **bold** |\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.tables.len(),
            1,
            "table structure is still recognized"
        );
        assert!(
            result
                .styled_spans
                .iter()
                .any(|(_, style)| *style == SpanStyle::Bold),
            "bold inside a cell should still be styled"
        );
    }

    #[test]
    fn standalone_markdown_image_line_is_planned() {
        let text = "before\n![alt](pics/a.png)\nafter\n";
        let result = plan(text, &[]);
        assert_eq!(
            result.embeds,
            vec![EmbedInfo {
                range: 7..25,
                target: "pics/a.png".to_string(),
                is_wikilink: false,
                kind: EmbedKind::Image,
                subpath: None,
                size: None,
            }]
        );
        assert_eq!(&text[7..25], "![alt](pics/a.png)");
    }

    #[test]
    fn image_title_and_angle_brackets_are_stripped_from_the_target() {
        let titled = plan("![a](x.png \"a title\")\n", &[]);
        assert_eq!(titled.embeds[0].target, "x.png");
        let bracketed = plan("![a](<my pic.png>)\n", &[]);
        assert_eq!(bracketed.embeds[0].target, "my pic.png");
    }

    #[test]
    fn an_embed_keeps_its_name_and_reads_its_size_and_subpath() {
        let sized = plan("![[cat.png|300]]\n", &[]);
        assert_eq!(sized.embeds[0].target, "cat.png");
        assert!(sized.embeds[0].is_wikilink);
        assert_eq!(
            sized.embeds[0].size,
            Some(EmbedSize {
                width: 300,
                height: None
            })
        );

        let both = plan("![[cat.png|alt words|300x200]]\n", &[]);
        assert_eq!(
            both.embeds[0].size,
            Some(EmbedSize {
                width: 300,
                height: Some(200)
            })
        );

        let heading = plan("![[Note#Some heading]]\n", &[]);
        assert_eq!(heading.embeds[0].target, "Note");
        assert_eq!(
            heading.embeds[0].subpath,
            Some(Subpath::Heading("Some heading".to_string()))
        );

        let block = plan("![[Note#^abc-1]]\n", &[]);
        assert_eq!(
            block.embeds[0].subpath,
            Some(Subpath::Block("abc-1".to_string()))
        );

        let same_note = plan("![[#Part]]\n", &[]);
        assert_eq!(same_note.embeds[0].target, "");
        assert_eq!(same_note.embeds[0].kind, EmbedKind::Note);
    }

    #[test]
    fn a_size_in_the_alt_text_of_a_markdown_image_is_read() {
        let sized = plan("![a picture|250](x.png)\n", &[]);
        assert_eq!(
            sized.embeds[0].size,
            Some(EmbedSize {
                width: 250,
                height: None
            })
        );
        assert_eq!(plan("![a|wide](x.png)\n", &[]).embeds[0].size, None);
        assert_eq!(plan("![a|0](x.png)\n", &[]).embeds[0].size, None);
        assert_eq!(plan("![a|99999](x.png)\n", &[]).embeds[0].size, None);
    }

    #[test]
    fn an_embed_is_a_note_an_image_or_a_file_by_its_extension() {
        let kind = |text: &str| plan(text, &[]).embeds[0].kind;
        assert_eq!(kind("![[Note]]\n"), EmbedKind::Note);
        assert_eq!(kind("![[Note.md]]\n"), EmbedKind::Note);
        assert_eq!(kind("![[Notes 1.2]]\n"), EmbedKind::Note);
        assert_eq!(kind("![[cat.PNG]]\n"), EmbedKind::Image);
        assert_eq!(kind("![[song.mp3]]\n"), EmbedKind::Audio);
        assert_eq!(kind("![[clip.mp4]]\n"), EmbedKind::Video);
        assert_eq!(kind("![[paper.pdf]]\n"), EmbedKind::Pdf);
        assert_eq!(kind("![](x.png)\n"), EmbedKind::Image);
        assert_eq!(kind("![](x.pdf)\n"), EmbedKind::Pdf);
        assert_eq!(kind("![](other.md)\n"), EmbedKind::Note);
        assert_eq!(kind("![](data.csv)\n"), EmbedKind::Other);
        assert_eq!(
            kind("![](https://example.com/photo?id=7)\n"),
            EmbedKind::Image
        );
        assert_eq!(
            kind("![](https://example.com/photo.php)\n"),
            EmbedKind::Image
        );
        assert_eq!(kind("![](no-extension)\n"), EmbedKind::Image);
    }

    #[test]
    fn file_extensions_ignore_queries_and_odd_names() {
        assert_eq!(file_extension("a/b/c.PNG").as_deref(), Some("png"));
        assert_eq!(
            file_extension("https://x.org/a.jpg?w=3#top").as_deref(),
            Some("jpg")
        );
        assert_eq!(file_extension(".hidden"), None);
        assert_eq!(file_extension("name."), None);
        assert_eq!(file_extension("dir.d/file"), None);
        assert_eq!(file_extension("v1.2 notes"), None);
        assert_eq!(file_extension("archive.tar.gz").as_deref(), Some("gz"));
    }

    #[test]
    fn image_mixed_with_text_is_left_raw() {
        assert!(plan("see ![a](x.png) here\n", &[]).embeds.is_empty());
        assert!(plan("![a](x.png) trailing\n", &[]).embeds.is_empty());
        assert!(plan("![a]()\n", &[]).embeds.is_empty());
        assert!(plan("![[]]\n", &[]).embeds.is_empty());
        assert!(plan("see ![[Note]] here\n", &[]).embeds.is_empty());
        assert!(plan("![[A]] ![[B]]\n", &[]).embeds.is_empty());
    }

    #[test]
    fn touched_image_line_is_excluded() {
        let text = "![a](x.png)\n";
        assert!(plan(text, &[3..3]).embeds.is_empty());
    }

    #[test]
    fn images_inside_fenced_or_indented_code_are_ignored() {
        let fenced = "```\n![a](x.png)\n```\n![b](y.png)\n";
        let result = plan(fenced, &[]);
        assert_eq!(result.embeds.len(), 1);
        assert_eq!(result.embeds[0].target, "y.png");
        assert!(plan("    ![a](x.png)\n", &[]).embeds.is_empty());
    }

    #[test]
    fn images_outside_the_viewport_are_pruned() {
        let text = "![a](x.png)\n\n\n![b](y.png)\n";
        let result = plan_viewport(text, &[], 0..12);
        assert_eq!(result.embeds.len(), 1);
        assert_eq!(result.embeds[0].target, "x.png");
    }

    mod extension_rules {
        use std::sync::Arc;

        use extension::{VisualMdSpanStyle, VisualMdSyntaxRuleManifestEntry};

        use super::*;
        use crate::rules::{CompiledRule, RuleEffects};

        fn italic() -> VisualMdSpanStyle {
            VisualMdSpanStyle {
                italic: Some(true),
                ..Default::default()
            }
        }

        fn pattern_rule(id: &str, pattern: &str) -> VisualMdSyntaxRuleManifestEntry {
            VisualMdSyntaxRuleManifestEntry {
                id: id.into(),
                pattern: Some(pattern.to_string()),
                style: Some(italic()),
                ..Default::default()
            }
        }

        fn hiding_rule(id: &str, pattern: &str, hide: &[usize]) -> VisualMdSyntaxRuleManifestEntry {
            VisualMdSyntaxRuleManifestEntry {
                style: None,
                hide: hide.to_vec(),
                ..pattern_rule(id, pattern)
            }
        }

        fn node_rule(id: &str, node: &str) -> VisualMdSyntaxRuleManifestEntry {
            VisualMdSyntaxRuleManifestEntry {
                pattern: None,
                node: Some(node.to_string()),
                ..pattern_rule(id, "")
            }
        }

        fn dynamic_rule(id: &str, pattern: &str) -> VisualMdSyntaxRuleManifestEntry {
            VisualMdSyntaxRuleManifestEntry {
                style: None,
                dynamic: true,
                ..pattern_rule(id, pattern)
            }
        }

        fn rule_set(entries: &[VisualMdSyntaxRuleManifestEntry]) -> RuleSet {
            RuleSet {
                rules: entries
                    .iter()
                    .map(|entry| {
                        Arc::new(
                            CompiledRule::compile("notes".into(), 1, entry)
                                .expect("the rule compiles"),
                        )
                    })
                    .collect(),
                results: Arc::default(),
            }
        }

        fn plan_with(text: &str, selections: &[Range<usize>], rules: RuleSet) -> Plan {
            plan_visible(text, selections, 0..text.len(), rules)
        }

        fn plan_visible(
            text: &str,
            selections: &[Range<usize>],
            visible: Range<usize>,
            rules: RuleSet,
        ) -> Plan {
            let tree = parse_blocks(text).expect("the document should parse");
            let extensions = PlanExtensions {
                rules,
                ..Default::default()
            };
            plan_viewport_with_extensions(text, &tree, selections, visible, &extensions)
        }

        fn styled_ranges(plan: &Plan) -> Vec<Range<usize>> {
            plan.extension_styled
                .iter()
                .map(|(range, _)| range.clone())
                .collect()
        }

        fn missing_keys(plan: &Plan) -> Vec<DynamicKey> {
            plan.missing_rule_inputs
                .iter()
                .map(|input| input.key.clone())
                .collect()
        }

        fn key(rule: &str, text: &str) -> DynamicKey {
            DynamicKey {
                extension_id: "notes".into(),
                generation: 1,
                rule: rule.into(),
                text: text.to_string(),
            }
        }

        fn with_result(mut rules: RuleSet, key: DynamicKey, result: DynamicResult) -> RuleSet {
            let mut results = (*rules.results).clone();
            results.insert(key, result);
            rules.results = Arc::new(results);
            rules
        }

        fn ready(effects: RuleEffects) -> DynamicResult {
            DynamicResult::Ready(Arc::new(effects))
        }

        #[test]
        fn a_rule_styles_every_match() {
            let result = plan_with(
                "hi @ada and @bob\n",
                &[],
                rule_set(&[pattern_rule("mention", r"@\w+")]),
            );

            assert_eq!(styled_ranges(&result), vec![3..7, 12..16]);
            assert_eq!(result.extension_styled[0].1, italic());
        }

        #[test]
        fn without_rules_the_plan_is_what_it_always_was() {
            let text = "# H\n\nhi @ada, **bold** and `code`\n\n- [ ] task\n";

            assert_eq!(plan_with(text, &[], RuleSet::default()), plan(text, &[]));
        }

        #[test]
        fn rules_apply_in_headings_lists_quotes_and_table_cells() {
            let text = "# @head\n\n- @item\n\n> @quote\n\n| a | b |\n|---|---|\n| @cell | x |\n";

            let result = plan_with(text, &[], rule_set(&[pattern_rule("m", r"@\w+")]));

            let matched: Vec<&str> = styled_ranges(&result)
                .into_iter()
                .filter_map(|range| text.get(range))
                .collect();
            assert_eq!(matched, vec!["@head", "@item", "@quote", "@cell"]);
        }

        #[test]
        fn rules_leave_inline_code_alone() {
            let text = "`@ada` and @bob and `x @cy z`\n";

            let result = plan_with(text, &[], rule_set(&[pattern_rule("m", r"@\w+")]));

            assert_eq!(styled_ranges(&result), vec![11..15]);
        }

        #[test]
        fn a_match_never_spans_a_line_break() {
            let text = "one\ntwo\n";

            let result = plan_with(text, &[], rule_set(&[pattern_rule("m", r"one\s+two")]));

            assert!(result.extension_styled.is_empty());
        }

        #[test]
        fn hidden_groups_are_hidden_until_a_selection_touches_the_match() {
            let text = "say :smile: now\n";
            let rules = rule_set(&[hiding_rule("emoji", r"(:)(\w+)(:)", &[1, 3])]);

            let untouched = plan_with(text, &[], rules.clone());
            assert_eq!(untouched.hidden_markers, vec![4..5, 10..11]);
            assert!(untouched.dimmed_markers.is_empty());

            let touched = plan_with(text, &[7..7], rules.clone());
            assert!(touched.hidden_markers.is_empty());
            assert_eq!(touched.dimmed_markers, vec![4..5, 10..11]);

            let at_the_edge = plan_with(text, &[11..11], rules.clone());
            assert_eq!(at_the_edge.dimmed_markers, vec![4..5, 10..11]);
            let past_it = plan_with(text, &[12..12], rules);
            assert_eq!(past_it.hidden_markers, vec![4..5, 10..11]);
        }

        #[test]
        fn a_rule_may_hide_the_whole_match() {
            let result = plan_with(
                "a %% b\n",
                &[],
                rule_set(&[hiding_rule("gone", "%%", &[0])]),
            );

            assert_eq!(result.hidden_markers, vec![2..4]);
        }

        #[test]
        fn extensions_never_override_zed_mds_own_hidden_markers() {
            let text = "some **bold** text\n";
            let rules = rule_set(&[hiding_rule("stars", r"\*\*bold\*\*", &[0])]);

            let with_rule = plan_with(text, &[], rules);

            assert_eq!(with_rule.hidden_markers, plan(text, &[]).hidden_markers);
        }

        #[test]
        fn extensions_stay_off_bullets_checkboxes_and_callout_titles() {
            for (text, pattern) in [
                ("- item\n", "- item"),
                ("1. item\n", r"1\. item"),
                ("- [ ] task\n", r"\[ \]"),
                ("> quote\n", "> quote"),
                ("> [!note] title\n> body\n", r"\[!note\]"),
            ] {
                let with_rule = plan_with(text, &[], rule_set(&[hiding_rule("r", pattern, &[0])]));

                assert_eq!(
                    with_rule.hidden_markers,
                    plan(text, &[]).hidden_markers,
                    "{pattern} on {text:?}"
                );
            }
        }

        #[test]
        fn extensions_stay_off_table_gaps_and_the_delimiter_row() {
            let text = "| a | b |\n|---|---|\n| c | d |\n";
            let rules = rule_set(&[hiding_rule("r", r"[ -]+", &[0])]);

            let with_rule = plan_with(text, &[], rules);

            assert_eq!(with_rule.hidden_markers, plan(text, &[]).hidden_markers);
        }

        #[test]
        fn overlapping_hides_from_rules_leave_the_leftmost() {
            let rules = rule_set(&[
                hiding_rule("wide", "abcd", &[0]),
                hiding_rule("narrow", "cdef", &[0]),
            ]);

            let result = plan_with("abcdef\n", &[], rules);

            assert_eq!(result.hidden_markers, vec![0..4]);
        }

        #[test]
        fn a_missing_answer_is_asked_for_once_per_distinct_text() {
            let rules = rule_set(&[dynamic_rule("emoji", r":\w+:")]);

            let result = plan_with(":a: :b: :a:\n", &[], rules);

            assert_eq!(
                missing_keys(&result),
                vec![key("emoji", ":a:"), key("emoji", ":b:")]
            );
        }

        #[test]
        fn an_answer_is_placed_at_each_match_and_not_asked_for_again() {
            let effects = RuleEffects {
                styled: vec![(1..3, italic())],
                hidden: vec![0..1],
                replacements: vec![(1..6, "🙂".to_string())],
            };
            let rules = with_result(
                rule_set(&[dynamic_rule("emoji", r":\w+:")]),
                key("emoji", ":smile:"),
                ready(effects),
            );

            let result = plan_with("a :smile: b :smile:\n", &[], rules);

            assert!(result.missing_rule_inputs.is_empty());
            assert_eq!(styled_ranges(&result), vec![3..5, 13..15]);
            assert_eq!(result.hidden_markers, vec![2..3, 12..13]);
            assert_eq!(
                result.extension_replacements,
                vec![(3..8, "🙂".to_string()), (13..18, "🙂".to_string())]
            );
        }

        #[test]
        fn touching_a_match_dims_its_hidden_ranges_and_skips_its_replacements() {
            let effects = RuleEffects {
                styled: vec![(1..3, italic())],
                hidden: vec![0..1],
                replacements: vec![(1..6, "🙂".to_string())],
            };
            let rules = with_result(
                rule_set(&[dynamic_rule("emoji", r":\w+:")]),
                key("emoji", ":smile:"),
                ready(effects),
            );

            let result = plan_with("a :smile: b\n", &[5..5], rules);

            assert_eq!(styled_ranges(&result), vec![3..5], "the style stays");
            assert!(result.hidden_markers.is_empty());
            assert_eq!(result.dimmed_markers, vec![2..3]);
            assert!(result.extension_replacements.is_empty());
        }

        #[test]
        fn a_failed_answer_is_neither_applied_nor_asked_for_again() {
            let rules = with_result(
                rule_set(&[dynamic_rule("emoji", r":\w+:")]),
                key("emoji", ":smile:"),
                DynamicResult::Failed,
            );

            let result = plan_with("a :smile: b\n", &[], rules);

            assert!(result.missing_rule_inputs.is_empty());
            assert!(result.extension_styled.is_empty());
            assert!(result.hidden_markers.is_empty());
        }

        #[test]
        fn an_answer_for_another_build_of_the_extension_is_not_used() {
            // The rule is from build 2 of the extension, the answer from build 1.
            let rule = CompiledRule::compile("notes".into(), 2, &dynamic_rule("emoji", r":\w+:"))
                .expect("the rule compiles");
            let rules = with_result(
                RuleSet {
                    rules: Arc::from([Arc::new(rule)]),
                    results: Arc::default(),
                },
                key("emoji", ":smile:"),
                ready(RuleEffects {
                    hidden: vec![0..1],
                    ..Default::default()
                }),
            );

            let result = plan_with(":smile:\n", &[], rules);

            assert!(result.hidden_markers.is_empty());
            assert_eq!(result.missing_rule_inputs.len(), 1);
            assert_eq!(result.missing_rule_inputs[0].key.generation, 2);
        }

        #[test]
        fn an_answer_cannot_replace_text_that_zed_md_has_folded() {
            let effects = RuleEffects {
                replacements: vec![(0..1, "x".to_string())],
                ..Default::default()
            };
            let rules = with_result(
                rule_set(&[dynamic_rule("task", r"\[ \] task")]),
                key("task", "[ ] task"),
                ready(effects),
            );

            let result = plan_with("- [ ] task\n", &[], rules);

            assert!(result.extension_replacements.is_empty());
        }

        #[test]
        fn rules_only_match_what_is_in_view() {
            let text = "@first\n\n@second\n";
            let second = text.find("@second").expect("the text has it");

            let result = plan_visible(
                text,
                &[],
                second..text.len(),
                rule_set(&[pattern_rule("m", r"@\w+")]),
            );

            assert_eq!(styled_ranges(&result), vec![second..second + 7]);
        }

        #[test]
        fn a_node_rule_styles_each_node_of_its_kind() {
            let text = "see <b>bold</b> now\n";

            let result = plan_with(text, &[], rule_set(&[node_rule("tags", "html_tag")]));

            let tags: Vec<&str> = styled_ranges(&result)
                .into_iter()
                .filter_map(|range| text.get(range))
                .collect();
            assert_eq!(tags, vec!["<b>", "</b>"]);
        }

        /// The kinds a rule may name are the ones tree-sitter-md really emits for
        /// the constructs below, checked against the grammar itself.
        #[test]
        fn every_allowed_node_kind_is_one_the_grammar_produces() {
            let documents = [
                ("html_tag", "see <b>bold</b>\n"),
                (
                    "full_reference_link",
                    "a [text][1] b\n\n[1]: https://example.com\n",
                ),
                (
                    "collapsed_reference_link",
                    "a [text][] b\n\n[text]: https://example.com\n",
                ),
                (
                    "shortcut_link",
                    "a [text] b\n\n[text]: https://example.com\n",
                ),
                ("html_block", "<div>\nhello\n</div>\n"),
                ("link_reference_definition", "[1]: https://example.com\n"),
                ("minus_metadata", "---\ntitle: x\n---\n\nbody\n"),
                ("plus_metadata", "+++\na = 1\n+++\n\nbody\n"),
            ];
            assert_eq!(
                documents.len(),
                extension::VISUAL_MD_RULE_NODE_KINDS.len(),
                "a document for every allowed kind"
            );
            for (kind, text) in documents {
                assert!(extension::VISUAL_MD_RULE_NODE_KINDS.contains(&kind));

                let result = plan_with(text, &[], rule_set(&[node_rule("r", kind)]));

                assert!(
                    !result.extension_styled.is_empty(),
                    "the grammar produced no `{kind}` for {text:?}"
                );
            }
        }

        #[test]
        fn a_node_rule_touched_by_the_selection_still_styles_but_dims_its_effects() {
            let text = "see <b>bold</b> now\n";
            let effects = RuleEffects {
                hidden: vec![0..1],
                ..Default::default()
            };
            let rules = with_result(
                rule_set(&[VisualMdSyntaxRuleManifestEntry {
                    dynamic: true,
                    ..node_rule("tags", "html_tag")
                }]),
                key("tags", "<b>"),
                ready(effects),
            );

            let untouched = plan_with(text, &[], rules.clone());
            assert_eq!(untouched.hidden_markers, vec![4..5]);

            let touched = plan_with(text, &[5..5], rules);
            assert_eq!(touched.dimmed_markers, vec![4..5]);
        }

        #[test]
        fn extension_ranges_in_random_documents_never_overlap_anything_or_panic() {
            let mut rng = Rng(0x7a3b_91c4_5d2e_8f01);
            let entries = [
                pattern_rule("word", r"[a-z]{3,}"),
                hiding_rule("pair", r"(\*)(\w+)(\*)", &[1, 3]),
                hiding_rule("mark", r"[=\-|>]+", &[0]),
                hiding_rule("bracket", r"\[[ x]\]", &[0]),
                dynamic_rule("any", r"\w+"),
                node_rule("tag", "html_tag"),
            ];
            let text_pool = ["bold", "tag", "other"];
            for _ in 0..200 {
                let lines = 1 + rng.below(30);
                let text = random_document(&mut rng, lines);
                let mut rules = rule_set(&entries);
                // Answer some dynamic matches so their hides and replacements get exercised.
                for word in text_pool {
                    let effects = RuleEffects {
                        styled: vec![(0..1, italic())],
                        hidden: vec![0..1],
                        replacements: vec![(1..word.len().clamp(2, 3), "x".to_string())],
                    };
                    rules = with_result(rules, key("any", word), ready(effects));
                }
                let selections = if rng.below(2) == 0 {
                    vec![]
                } else {
                    let start = rng.below(text.len() + 1);
                    let end = start + rng.below(text.len() + 1 - start);
                    vec![start..end]
                };

                let result = plan_with(&text, &selections, rules);

                assert_no_overlaps(&fold_inducing_ranges(&result), &text);
                for range in fold_inducing_ranges(&result) {
                    assert!(range.end <= text.len(), "{range:?} for {text:?}");
                    assert!(
                        !text[range.clone()].contains('\n'),
                        "a fold over a line break at {range:?} for {text:?}"
                    );
                }
                for (range, _) in &result.extension_styled {
                    assert!(range.end <= text.len() && text.is_char_boundary(range.start));
                }
                assert!(
                    result
                        .extension_replacements
                        .iter()
                        .all(|(range, replacement)| {
                            !replacement.contains('\n') && !range.is_empty()
                        })
                );
            }
        }
    }
}
