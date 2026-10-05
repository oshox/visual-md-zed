//! visual_md: an Obsidian-style live-preview Markdown editor for Zed.
//!
//! This crate hooks every full-mode editor via [`editor::Addon`] and decorates
//! Markdown buffers without ever mutating the underlying buffer text —
//! decorations are view-only, per the spec's core mechanic. See
//! `docs/visual-md-spec.md` for the full behavior spec this crate targets.
//!
//! ## Status
//!
//! **M12**: a line holding only an image (`![alt](path)` or `![[name]]`)
//! becomes a fixed-height image block while the cursor is off it; see
//! `apply_images`. Blocks are `IMAGE_BLOCK_ROWS` rows tall because block
//! heights can't follow an image's real size.
//!
//! **M4** (current): M1's inline engine (headings, bold, italic, bold-italic,
//! strikethrough, `==highlight==`, inline code) plus M2's lists (bullets and
//! renumbered ordinals, nesting inherited for free from source indentation),
//! interactive task checkboxes (real click-to-toggle, unconditionally
//! rendered per spec — never hidden, never gated on the cursor), blockquotes
//! (marker replaced with a left-bar glyph, nesting stacks naturally), and
//! callouts (`> [!type]`: bracket syntax hidden, body tinted per
//! recognized type). A full callout box (its own accent-bar element, icon,
//! foldable title row) needs block-level rendering this pass doesn't add;
//! see `callout_style`'s doc comment for the precise scope line. Links,
//! tables, and frontmatter remain out of scope until later milestones.
//!
//! M3 hardened the above rather than adding new constructs: decoration
//! generation is scoped to the visible viewport (see [`plan::plan_viewport`]
//! and `visible_byte_range`) so a large document doesn't replan/rediff its
//! entire contents on every keystroke, the fold-diffing algorithm is O(n)
//! instead of O(n²), and the planner has property-based tests (random
//! documents/selections never produce overlapping or out-of-bounds
//! decorations) alongside the hand-written fixtures.
//!
//! M4 gives headings genuine variable row height instead of style-only
//! (bold + color at uniform size): each level now carries a
//! `HighlightStyle::font_size_scale` (a new field added to `gpui`, see its
//! doc comment) that `crates/editor/src/element.rs` reads per row to shape
//! that row's text at a taller size and lay out every row below it
//! accordingly — see `LineWithInvisibles::row_y_offset`/`row_for_y`, the
//! canonical replacement for the uniform `line_height * (row -
//! scroll_position.y)` formula used throughout that file. The core-editor
//! surface this touches is deliberately bounded: text painting, cursor
//! positioning, hit-testing, the current-line/search-highlight backgrounds,
//! and the gutter (line numbers, fold/crease icons) all stay pixel-aligned
//! with a resized heading row. Diff-hunk markers, the full git-blame gutter,
//! and the excerpt-expand icon are known, disclosed gaps — those still
//! assume uniform row height, a rare/secondary-feature cosmetic
//! misalignment rather than a functional one.
//!
//! **M5** adds the one editing behavior this crate has beyond pure
//! decoration: "smart list continuation" (see [`list_continuation`]).
//! Pressing Enter on a bullet/ordinal/checkbox item's own marker line
//! continues that marker onto the new line instead of inserting a plain
//! newline; Enter on an empty item outdents it (or exits the list at the top
//! level) instead. Wired in ahead of `Editor::newline` via
//! `editor.register_action`, so it only ever changes behavior for the exact
//! cases the spec calls out and falls through to the normal handler (via
//! `cx.propagate()`) for everything else.
//!
//! **M6** adds markdown links (`[text](url)`), autolinks (`<https://...>` /
//! `<user@example.com>`), and horizontal rules (`---`/`***`/`___`).
//! Reference-style links (`[text][1]`, `[shortcut]`) are deliberately not
//! handled — see `plan::plan_link`'s doc comment for why the per-paragraph
//! inline grammar can't tell those apart from ordinary bracketed prose
//! without a document-wide reference-definition lookup this crate doesn't
//! do. Two more scope lines, both disclosed the same way the callout-box gap
//! above already is:
//! - Link text gets a real color (`theme::colors().link_text_hover`) even
//!   though every other construct in this crate deliberately renders in the
//!   same color as prose — color is a link's only non-structural cue, so
//!   this is a narrow, intentional exception (see `apply_style_highlights`).
//! - Only autolinks are actually clickable to navigate: Zed's generic
//!   cmd+click URL detection (`find_url` in
//!   `crates/editor/src/hover_links.rs`) scans the *raw buffer text* around
//!   the click for a URL-shaped substring, which still works once only the
//!   `<`/`>` chars are hidden (the visible text remains the literal URL at
//!   its real offset). A `[text](url)` link's visible glyph is the link
//!   *text*, not the URL, so that same generic mechanism can't find the URL
//!   from a click there — making it clickable would need a custom widget
//!   (like the checkbox's) or extending `hover_links`, deferred here.
//!
//! The horizontal rule uses a genuinely different mechanism from every other
//! decoration in this file: `insert_blocks`/`BlockProperties` (see
//! `apply_horizontal_rules`) instead of a `FoldPlaceholder`, since a fold can
//! only size itself to its own content and there's no way to make one
//! stretch to the editor's actual visible width for a full-width `<hr>`.
//!
//! **M7** gives markdown prose its own proportional font
//! (`theme_settings::ThemeSettings::ui_font`, reusing Zed's own UI chrome font rather
//! than inventing a new setting) while keeping code (inline spans and, as of
//! M8, fenced-block content) on the normal `buffer_font` — i.e. exactly the
//! font every markdown buffer already rendered in before this. This needed a
//! real `gpui` change: `HighlightStyle` had no font-family field at all
//! (only color/weight/italic/underline/strikethrough/background/
//! `font_size_scale`), so `crates/gpui/src/style.rs` gained one, following
//! the same precedent `font_size_scale` set. See `KEY_PROSE_FONT`/
//! `KEY_CODE_FONT`'s own doc comment for the highlight-key ordering this
//! relies on.
//!
//! **M8** adds fenced code blocks: the ` ``` `/`~~~` fence lines collapse to
//! a full-width border (plus a language chip on the opening line) via the
//! same `insert_blocks` mechanism the horizontal rule uses (see
//! `apply_code_fence_borders`), and the content gets real per-language
//! syntax coloring. That coloring needs a genuine `Arc<Language>` — a
//! tree-sitter-grammar-backed object [`plan`] deliberately can't depend on
//! (see its own doc comment) — resolved asynchronously via
//! `LanguageRegistry::language_for_name_or_extension` (grammars load/compile
//! lazily) and cached per-editor on `VisualMdAddon`; a `refresh` runs again
//! once a language resolves so its highlighting appears without waiting on
//! the next edit. See `ensure_code_languages_loaded`/
//! `apply_code_syntax_highlights`. With no project or buffer-level language
//! registry available at all, or an unrecognized language name, a fence's
//! content simply stays plain (monospace, uncolored) rather than erroring.
//!
//! **M9** adds GFM pipe tables, with a genuinely different editing model
//! from every other block-level construct here: table cells are never
//! hidden, folded, or block-replaced -- every cell stays normal,
//! always-visible, always-editable inline text, so there's no rendered/raw
//! mode switch to toggle when the cursor enters one (true per-cell editing,
//! per explicit product direction). The table *look* (vertical column
//! borders, column-width alignment) is still built entirely out of the
//! `Crease`/`FoldPlaceholder` mechanism every other decoration here uses,
//! just applied to bytes that already exist in the raw table syntax rather
//! than to the cell content itself:
//! - Every `|` in a header/data row folds to a bar glyph unconditionally
//!   (`GlyphKind::TablePipe`), the same treatment blockquote's `>` already
//!   gets — see its own doc comment for why there's nothing to reveal by
//!   touching a typographic marker like this.
//! - Column alignment widens a cell's own *existing* whitespace (its
//!   trailing padding, or the ambient gap after the previous `|` — both real
//!   source bytes, confirmed by inspecting the grammar's own output) into a
//!   spacer sized by real text measurement (`window.text_system().shape_line`,
//!   the first use of runtime text shaping to size a decoration in this
//!   crate) — see `table_alignment_spacer_folds`. A cell with no existing
//!   whitespace on a given side (e.g. a minimally-spaced `|A|B|` table) just
//!   doesn't get padding there, rather than erroring.
//! - The delimiter row (`|------|-----:|`) has no displayable content of its
//!   own, so it's the one part of a table that *does* use the M6/M8
//!   whole-line block-toggle pattern (`apply_table_dividers`, structurally
//!   identical to `apply_horizontal_rules`).
//! - Disclosed scope trim: only a header/body divider and per-column
//!   vertical bars are rendered, not a horizontal line between every data
//!   row.
//!
//! **M10** adds the spec's "Selection formatting shortcuts" bullet for bold
//! and italic: `ToggleBold`/`ToggleItalic` (bound to `ctrl/cmd-b`/`i` in the
//! keymap's `Editor && visual_md` context), handled by
//! `intercept_toggle_bold`/`intercept_toggle_italic`, which both delegate
//! their actual detection/wrap/unwrap logic to the pure
//! [`format_toggle::toggle`] — same shape as M5's `list_continuation`. The
//! other shortcut the spec calls out, pasting a URL over a selection to make
//! `[text](url)`, needs no work here: it already exists in Zed core
//! (`Editor::paste`, see `test_paste_url_from_other_app_creates_markdown_link_over_selected_text`
//! in `crates/editor/src/editor_tests.rs`).
//!
//! `ctrl/cmd-b`/`i` are bound elsewhere too (`workspace::ToggleLeftDock` and
//! `editor::ShowSignatureHelp`, respectively) — the new bindings
//! *deliberately* shadow those, but only inside a Markdown buffer visual_md
//! is actively decorating: the `visual_md` key context is added by
//! `VisualMdAddon::extend_key_context`, gated on the same `active` flag
//! `refresh` already computes from `is_markdown_editor` + the setting. If
//! `intercept_toggle` ever finds nothing to do (empty selections, a
//! selection covering no bold/italic-eligible text, or the setting just got
//! disabled mid-keystroke), it calls `cx.propagate()`, which lets gpui's key
//! dispatch continue down the context stack to the shadowed binding — so
//! `ctrl-b` still opens the dock outside a live-preview buffer, and even
//! falls back to it inside one if the shortcut genuinely has nothing to
//! toggle. Vim mode is a known, narrow gap: `vim.jsonc`'s own `ctrl-b`/`ctrl-i`
//! bindings (page-up, jump-forward) take precedence in normal/visual mode,
//! so these shortcuts currently only fire in insert mode or with vim off.
//!
//! **M11** upgrades callouts (`> [!type]`) from M2's minimal handling (the
//! `[!type]` bracket syntax permanently hidden, no icon, no color) into real
//! boxes: an icon + capitalized title chip, a `+`/`-` fold toggle that
//! actually collapses the body, and a background tint per kind
//! (`callout_look`, reusing Zed's existing semantic status colors —
//! `cx.theme().status()` — rather than inventing new theme tokens). Two
//! fixes rode along with the new construct, not just additions:
//! - The `[!type]` marker used to be unconditionally hidden, with no way to
//!   reveal it no matter where the cursor was — there was literally no way
//!   to edit a callout's type. It now reveals as raw, dimmed text exactly
//!   like every other hideable construct here, scoped to the *title line*
//!   specifically (matching `plan_heading`'s own "touched" scope, not a
//!   bare overlap with the marker's own bytes) so editing the type doesn't
//!   couple to the body, which stays independently live-previewed per spec.
//!   This is also the callout's whole "change type" affordance — there's no
//!   right-click menu (see below).
//! - Callout body coloring had been explicitly removed in an earlier
//!   session (grouped with inline-code/highlight, "per product direction,
//!   only structural styling survives") — a real conflict with the spec
//!   file's own "colored, rounded box" wording. Resolved by asking the user:
//!   color comes back for callouts specifically, as a second deliberate
//!   exception alongside links (`KEY_LINK`) — a callout's whole purpose is
//!   standing out, unlike the tints that were removed.
//!
//! The title widget (`callout_title_placeholder`) is modeled directly on
//! `checkbox_placeholder`: its chevron is a real click target that writes
//! the fold suffix back to the buffer with the same single-edit shape the
//! checkbox uses for `[ ]`/`[x]`, not a read-only glyph. The collapsed body
//! (`callout_collapsed_body_placeholder`) is a genuine multi-line `Crease`
//! spanning every body line — the same mechanism Zed's own code folding
//! uses to collapse a function body, not a new capability — fed into the
//! same unified `folds` list every other decoration in `refresh` already
//! uses, so its diffing/removal falls out of `apply_folds` for free.
//!
//! Disclosed scope trims:
//! - No right-click "change callout type" menu — direct raw-text editing of
//!   the now-revealable marker covers the same need.
//! - The left accent bar stays the same neutral color as a plain
//!   blockquote's, not tinted per kind — `GlyphKind::BlockquoteBar` is also
//!   emitted from a second, unrelated code path (`plan_inline`'s
//!   `block_continuation` handling, for a wrapped paragraph's continuation
//!   lines inside a blockquote) that has no easy access to the enclosing
//!   callout's kind; the background tint and title icon already do the
//!   identifying work, so threading kind context into that second path
//!   wasn't worth it this pass.
//! - Nested callouts of *different* kinds get no special overlap
//!   resolution — whichever kind's `KEY_CALLOUT_*` happens to be numbered
//!   higher wins the background-color conflict for the overlapping bytes,
//!   not necessarily the innermost one. Real-world callouts essentially
//!   never nest mismatched kinds, so this is a disclosed, low-cost trim
//!   rather than a general regression (nesting itself works fine — see
//!   `nested_callouts_do_not_panic_and_each_gets_its_own_info`).
//! - No genuine multi-line CSS box (border, corner radius): the "box" is
//!   entirely a per-line background tint + icon + left bar, the same
//!   honest, real-decorations-only approach M9 used for tables, rather than
//!   faking a DOM structure this crate's decoration primitives don't have.
//!
//! The parsing and decision logic lives in [`plan`], a pure function with no
//! GPUI or `Editor` dependency. This module's job is only to drive it off
//! editor events and translate its plain byte ranges into creases and
//! `highlight_text` calls, diffing against what was previously applied so a
//! single keystroke or cursor move touches only what changed.

mod format_toggle;
mod list_continuation;
mod plan;
mod style;

use std::any::Any;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use arc_swap::ArcSwap;
use editor::actions::Newline;
use editor::display_map::{
    BlockContext, BlockPlacement, BlockProperties, BlockStyle, Crease, CreaseId, CustomBlockId,
    DisplayPoint, DisplayRow, DisplaySnapshot,
};
use editor::{
    Addon, Anchor, Bias, Editor, EditorEvent, HighlightKey, MultiBufferOffset, MultiBufferSnapshot,
    SelectionEffects,
};
use format_toggle::Emphasis;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FontWeight, HighlightStyle, Hsla, ImageSource,
    InteractiveElement, IntoElement, KeyContext, ObjectFit, ParentElement, Pixels, SharedString,
    StatefulInteractiveElement, Styled, StyledImage, Subscription, Task, TextRun,
    TextStyleRefinement, WeakEntity, Window, actions, black, div, img, px, svg,
};
use language::{Language, Rope};
use plan::{CalloutFold, CalloutKind, GlyphKind, ImageInfo, Plan, SpanStyle, TableAlignment};
use settings::Settings;
use style::ResolvedStyle;
use util::ResultExt;

actions!(
    visual_md,
    [
        /// Toggles `**bold**` on the selection (or unwraps it, or edits an
        /// empty pair at the cursor) — see `intercept_toggle_bold`.
        ToggleBold,
        /// Toggles `*italic*` on the selection, the same way `ToggleBold`
        /// does for bold — see `intercept_toggle_italic`.
        ToggleItalic,
        /// Toggles Markdown live preview in this editor only, without
        /// changing any settings file — see `toggle_live_preview`.
        ToggleLivePreview,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(register_editor).detach();
}

/// Registers visual_md on every full-mode editor, unconditionally.
///
/// Registration cannot be gated on "is this a Markdown buffer" here: a buffer's
/// language is assigned asynchronously (by `LanguageRegistry`, sometimes after
/// loading a grammar), so it is frequently still unknown at the moment the
/// `Editor` entity is constructed. Gating here raced against that and silently
/// dropped the decorations on newly opened files. Instead every full-mode editor
/// gets the addon and `refresh` itself checks the language on each call, so a
/// later `LanguageChanged` event (subscribed to below) picks it up correctly.
fn register_editor(editor: &mut Editor, window: Option<&mut Window>, cx: &mut Context<Editor>) {
    let Some(window) = window else {
        return;
    };
    if !editor.mode().is_full() {
        return;
    }

    let buffer = editor.buffer().clone();
    let state = VisualMdState::new(buffer, window, cx);
    let newline_action = editor.register_action(cx.listener(intercept_newline));
    let toggle_bold_action = editor.register_action(cx.listener(intercept_toggle_bold));
    let toggle_italic_action = editor.register_action(cx.listener(intercept_toggle_italic));
    let toggle_live_preview_action = editor.register_action(cx.listener(toggle_live_preview));
    editor.register_addon(VisualMdAddon {
        _state: state,
        _newline_action: newline_action,
        _toggle_bold_action: toggle_bold_action,
        _toggle_italic_action: toggle_italic_action,
        _toggle_live_preview_action: toggle_live_preview_action,
        enabled_override: None,
        active: false,
        folded_markers: Vec::new(),
        hr_blocks: Vec::new(),
        image_blocks: Vec::new(),
        code_fence_borders: Vec::new(),
        code_languages: HashMap::new(),
        pending_language_tasks: HashMap::new(),
        active_syntax_ids: HashSet::new(),
        table_dividers: Vec::new(),
        parsed: None,
        planned: None,
        nonempty_highlight_keys: HashSet::new(),
        last_applied: None,
        selection_refresh_queued: false,
        style: Arc::new(ArcSwap::from_pointee(ResolvedStyle::resolve(
            &settings::VisualMdSettingsContent::default(),
            cx,
        ))),
        saved_text_style_refinement: None,
        callout_key_count: 0,
    });
    refresh(editor, window, cx);

    // A newly-registered editor's very first refresh can have its first
    // fold-inducing crease (whichever one happens to come first in the
    // document — a heading marker, a `**` marker, ...) render as Zed's own
    // default "⋯" fold ellipsis instead of visual_md's real placeholder.
    // Confirmed via direct user testing to be purely a first-paint timing
    // issue: it self-heals the instant *any* later refresh fires, e.g. just
    // clicking that line, and the planner/placeholder content itself is
    // correct the whole time (this crate's own tests, which read the
    // planner's output and each placeholder's `collapsed_text` directly,
    // never reproduce it). Scheduling one more refresh right after the
    // window's first real paint settles it without waiting on the user to
    // interact with anything first.
    let editor_entity = cx.entity();
    window.on_next_frame(move |window, cx| {
        editor_entity.update(cx, |editor, cx| refresh(editor, window, cx));
    });
}

/// Intercepts a plain `Enter` keypress on a visual_md-managed Markdown editor
/// and, when every cursor sits on a list item's own marker line, replaces it
/// with "smart list continuation" (see `docs/visual-md-spec.md`) instead
/// of a plain newline: the same bullet/ordinal/checkbox is carried onto the
/// new line, and Enter on an empty item outdents (or exits the list) rather
/// than adding another empty bullet.
///
/// Registered on the editor before `Editor::newline` itself (see
/// `register_editor`), so it runs first on every `Newline` dispatch;
/// `cx.propagate()` falls through to the normal handler for every case this
/// doesn't apply to -- a disabled/non-Markdown buffer, a non-empty selection
/// anywhere, or any cursor not on a list marker line.
fn intercept_newline(
    editor: &mut Editor,
    _: &Newline,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    if !live_preview_enabled(editor, cx) {
        cx.propagate();
        return;
    }

    let display_snapshot = editor.display_snapshot(cx);
    let selections = editor
        .selections
        .all::<MultiBufferOffset>(&display_snapshot);
    if selections.is_empty() || selections.iter().any(|selection| !selection.is_empty()) {
        cx.propagate();
        return;
    }

    let text = editor.buffer().read(cx).snapshot(cx).text();
    let Some(edits) = selections
        .iter()
        .map(|selection| list_continuation::newline_edit(&text, selection.head().0))
        .collect::<Option<Vec<_>>>()
    else {
        cx.propagate();
        return;
    };

    // Multiple cursors can each produce their own edit; apply them together
    // in one buffer edit (in ascending order, since selections are already
    // reported in document order) and track the running length delta so
    // each cursor's `cursor_after` -- computed independently against the
    // *original* text -- lands at the right offset once every earlier
    // edit's own length change has shifted things.
    let mut buffer_edits = Vec::with_capacity(edits.len());
    let mut new_cursors = Vec::with_capacity(edits.len());
    let mut delta: isize = 0;
    for edit in &edits {
        let start = (edit.replace.start as isize + delta) as usize;
        let end = (edit.replace.end as isize + delta) as usize;
        buffer_edits.push((
            MultiBufferOffset(start)..MultiBufferOffset(end),
            edit.insert.clone(),
        ));
        let cursor_after = (edit.cursor_after as isize + delta) as usize;
        new_cursors.push(MultiBufferOffset(cursor_after)..MultiBufferOffset(cursor_after));
        delta += edit.insert.len() as isize - edit.replace.len() as isize;
    }

    editor.transact(window, cx, |editor, window, cx| {
        editor.edit(buffer_edits, cx);
        editor.change_selections(SelectionEffects::default(), window, cx, |s| {
            s.select_ranges(new_cursors);
        });
    });
}

/// Handles `ToggleBold`, bound to `ctrl/cmd-b` in a visual_md-managed
/// Markdown editor (see the keymap's `Editor && visual_md` context and this
/// module's M10 doc section). Delegates all the actual detection/wrap/unwrap
/// logic to [`format_toggle::toggle`]; this function's only job is dispatch
/// (the enabled/language guard, matching `intercept_newline`'s) and applying
/// the returned edits/selections as one transaction.
fn intercept_toggle_bold(
    editor: &mut Editor,
    _: &ToggleBold,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    intercept_toggle(editor, Emphasis::Bold, window, cx);
}

/// Handles `ToggleItalic` — see `intercept_toggle_bold`.
fn intercept_toggle_italic(
    editor: &mut Editor,
    _: &ToggleItalic,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    intercept_toggle(editor, Emphasis::Italic, window, cx);
}

fn intercept_toggle(
    editor: &mut Editor,
    kind: Emphasis,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    if !live_preview_enabled(editor, cx) {
        cx.propagate();
        return;
    }

    let display_snapshot = editor.display_snapshot(cx);
    let selections = editor
        .selections
        .all::<MultiBufferOffset>(&display_snapshot)
        .into_iter()
        .map(|selection| {
            let range = selection.range();
            range.start.0..range.end.0
        })
        .collect::<Vec<_>>();
    if selections.is_empty() {
        cx.propagate();
        return;
    }

    let text = editor.buffer().read(cx).snapshot(cx).text();
    let result = format_toggle::toggle(&text, &selections, kind);
    if result.edits.is_empty() {
        cx.propagate();
        return;
    }

    let buffer_edits = result.edits.into_iter().map(|(range, insert)| {
        (
            MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
            insert,
        )
    });
    let new_cursors = result
        .selections
        .into_iter()
        .map(|range| MultiBufferOffset(range.start)..MultiBufferOffset(range.end));

    editor.transact(window, cx, |editor, window, cx| {
        editor.edit(buffer_edits, cx);
        editor.change_selections(SelectionEffects::default(), window, cx, |s| {
            s.select_ranges(new_cursors);
        });
    });
}

fn is_markdown_editor(editor: &Editor, cx: &App) -> bool {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    snapshot
        .language_at(MultiBufferOffset(0))
        .is_some_and(|language| language.name().as_ref() == "Markdown")
}

/// The `visual_md.enabled` language setting for this editor's buffer, which
/// resolves default, user, project and per-language settings in that order.
/// Read at offset 0, the same place `is_markdown_editor` checks the language.
fn live_preview_setting(editor: &Editor, cx: &App) -> bool {
    editor
        .buffer()
        .read(cx)
        .language_settings_at(MultiBufferOffset(0), cx)
        .visual_md
        .is_enabled()
}

/// Whether live preview should currently decorate this editor: it must be a
/// Markdown buffer, and this editor's own `ToggleLivePreview` override, if it
/// has one, takes priority over the settings.
fn live_preview_enabled(editor: &Editor, cx: &App) -> bool {
    if !is_markdown_editor(editor, cx) {
        return false;
    }
    editor
        .addon::<VisualMdAddon>()
        .and_then(|addon| addon.enabled_override)
        .unwrap_or_else(|| live_preview_setting(editor, cx))
}

/// Drops `enabled_override` once the settings agree with it, so a later
/// settings change is not masked by an override that no longer overrides
/// anything.
fn drop_redundant_override(editor: &mut Editor, cx: &mut Context<Editor>) {
    let setting = live_preview_setting(editor, cx);
    if let Some(addon) = editor.addon_mut::<VisualMdAddon>()
        && addon.enabled_override == Some(setting)
    {
        addon.enabled_override = None;
    }
}

/// Handles `ToggleLivePreview`: flips what this editor currently shows without
/// writing any settings. The override is dropped as soon as it matches the
/// setting again, so a later settings change is not masked by a stale override.
fn toggle_live_preview(
    editor: &mut Editor,
    _: &ToggleLivePreview,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    if !is_markdown_editor(editor, cx) {
        cx.propagate();
        return;
    }

    let enabled = live_preview_enabled(editor, cx);
    let setting = live_preview_setting(editor, cx);
    let Some(addon) = editor.addon_mut::<VisualMdAddon>() else {
        return;
    };
    addon.enabled_override = (!enabled != setting).then_some(!enabled);
    refresh(editor, window, cx);
}

/// Addon registered on every visual_md-managed editor. Besides keeping
/// [`VisualMdState`] (and the editor-event subscriptions it owns) alive, this
/// is where the currently-folded marker creases are tracked so `refresh` can
/// diff against them instead of re-folding everything from scratch.
struct VisualMdAddon {
    _state: Entity<VisualMdState>,
    /// Keeps the `Newline` interceptor (see [`intercept_newline`]) alive for
    /// as long as this editor is visual_md-managed; dropping it would let
    /// `Editor::newline` handle every Enter keypress unconditionally again.
    _newline_action: Subscription,
    /// Keep the `ToggleBold`/`ToggleItalic` interceptors (see
    /// `intercept_toggle_bold`/`intercept_toggle_italic`) alive the same way
    /// `_newline_action` does.
    _toggle_bold_action: Subscription,
    _toggle_italic_action: Subscription,
    _toggle_live_preview_action: Subscription,
    /// What `ToggleLivePreview` last forced for this editor, taking priority
    /// over the settings. `None` once the toggle lands back on the setting's
    /// own value.
    enabled_override: Option<bool>,
    /// Whether `refresh` last found this editor markdown-and-enabled.
    /// `extend_key_context` reads this to add the `visual_md` context key the
    /// keymap's `Editor && visual_md` bindings (`ToggleBold`/`ToggleItalic`)
    /// match against, so they only shadow the dock/signature-help shortcuts
    /// while visual_md is actually decorating this buffer.
    active: bool,
    folded_markers: Vec<(Range<usize>, String, CreaseId)>,
    /// Horizontal-rule block decorations currently inserted (see
    /// `apply_horizontal_rules`). Unlike `folded_markers`, no content-key
    /// string travels alongside the range: a horizontal rule's rendering
    /// never varies, so range equality alone is enough to diff old vs. new.
    hr_blocks: Vec<(Range<usize>, CustomBlockId)>,
    /// Standalone-image blocks currently inserted (see `apply_images`),
    /// diffed on `(range, resolved source)`: retyping the path changes the
    /// source without moving the range's start, and the block must be rebuilt
    /// to show the new image.
    image_blocks: Vec<(Range<usize>, String, CustomBlockId)>,
    /// Fenced-code-block fence-line border/chip blocks currently inserted
    /// (see `apply_code_fence_borders`). Diffed on `(range, language)`
    /// together, not range alone, the same reasoning `folded_markers`'
    /// content key has: a fence's language name can change (the info string
    /// gets edited) without its byte range moving, and the chip needs to be
    /// recreated with the new text rather than mistaken for "unchanged".
    code_fence_borders: Vec<(Range<usize>, Option<String>, CustomBlockId)>,
    /// Resolved-language cache for fenced code blocks, keyed by the fence's
    /// info-string name. `None` means resolution was attempted and the name
    /// didn't match any known language/extension, so it isn't retried every
    /// refresh. See `ensure_code_languages_loaded`.
    code_languages: HashMap<String, Option<Arc<Language>>>,
    /// In-flight language-resolution tasks, keyed the same way as
    /// `code_languages`. Kept alive here (a dropped `Task` is cancelled);
    /// each task removes its own entry on completion.
    pending_language_tasks: HashMap<String, Task<()>>,
    /// Which `VisualMdCodeSyntax` highlight keys (one per distinct
    /// highlight id actually present in view) the previous refresh left
    /// active, so `apply_code_syntax_highlights` only has to clear the ones
    /// that are no longer wanted rather than iterating every highlight
    /// category the current theme happens to define.
    active_syntax_ids: HashSet<usize>,
    /// Table delimiter-row divider blocks currently inserted (see
    /// `apply_table_dividers`) -- structurally identical to `hr_blocks`,
    /// since a divider line's rendering never varies either.
    table_dividers: Vec<(Range<usize>, CustomBlockId)>,
    /// The buffer text and its block-level parse as of `edit_count`, reused
    /// by every refresh (scroll, cursor move) that doesn't follow an edit.
    parsed: Option<ParsedDocument>,
    /// The buffer's `edit_count` and the byte range (viewport plus overscan)
    /// the last refresh planned decorations for. `refresh_after_scroll` skips
    /// a scroll that stays inside it.
    planned: Option<(usize, Range<usize>)>,
    /// `VisualMd` highlight sub-keys currently holding non-empty ranges, so
    /// `set_visual_md_highlight` can skip re-clearing a key that is already
    /// empty.
    nonempty_highlight_keys: HashSet<usize>,
    /// The buffer's `edit_count` and the plan the last refresh applied. A
    /// refresh that plans the same thing again (a drag-select that touches the
    /// same constructs, a scroll that reveals nothing new) has nothing to
    /// apply and skips it.
    last_applied: Option<(usize, Plan)>,
    /// Whether a selection-triggered refresh is already queued for the next
    /// frame, so a burst of selection events costs one refresh.
    selection_refresh_queued: bool,
    /// The fonts and colors the last refresh resolved. Fold and block render
    /// closures hold a clone of this handle and read it when they paint, so a
    /// style change reaches them without recreating any of them.
    style: Arc<ArcSwap<ResolvedStyle>>,
    /// The editor's own text style refinement from before visual_md applied
    /// the prose size and line height, restored when it stops applying them.
    /// The outer `Option` is whether visual_md has applied anything.
    saved_text_style_refinement: Option<Option<TextStyleRefinement>>,
    /// How many `KEY_CALLOUT_FIRST` keys the last refresh used, so the ones a
    /// later refresh no longer needs can be cleared.
    callout_key_count: usize,
}

struct ParsedDocument {
    edit_count: usize,
    text: Arc<str>,
    block_tree: tree_sitter::Tree,
}

impl Addon for VisualMdAddon {
    fn extend_key_context(&self, key_context: &mut KeyContext, _: &App) {
        if self.active {
            key_context.add("visual_md");
        }
    }

    fn to_any(&self) -> &dyn Any {
        self
    }

    // `Addon::to_any_mut` defaults to returning `None` — easy to miss since
    // nothing enforces overriding it, and the failure mode is silent:
    // `Editor::addon_mut::<T>()` just always returns `None` too, rather than
    // panicking or failing to compile. Without this override, `apply_folds`
    // below (which relies on `addon_mut` both to read back the previous
    // refresh's creases and to persist the new ones) always treated "no
    // previous state" as true, so every single refresh recreated every
    // decoration's crease from scratch and never removed the previous set —
    // an unbounded accumulation of duplicate creases on every keystroke or
    // cursor move, not just a missed optimization. Found via an M3 test that
    // actually inspects `VisualMdAddon::folded_markers` after a real refresh
    // through the `cx.observe_new` registration path, rather than only
    // calling `refresh` directly the way earlier tests did.
    fn to_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}

struct VisualMdState {
    editor: WeakEntity<Editor>,
    _subscriptions: [Subscription; 5],
}

impl VisualMdState {
    fn new(
        buffer: Entity<editor::MultiBuffer>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Entity<Self> {
        let editor_entity = cx.entity();
        cx.new(|cx| VisualMdState {
            editor: editor_entity.downgrade(),
            _subscriptions: [
                // `WeakEntity::update_in` only works for a window's own root
                // view; `Editor` is a child view within the workspace/pane
                // tree, not a window root, so it always fails with "entity
                // has no current window" there. `subscribe_in` already hands
                // this callback its own `window`, so capture that directly
                // into a plain `update` instead.
                cx.subscribe_in(
                    &editor_entity,
                    window,
                    |_state, editor, event, window, cx| {
                        // Scrolling is included, not just edits/selection moves:
                        // decoration generation is scoped to the visible
                        // viewport (see `visible_byte_range`), so newly
                        // scrolled-into-view content needs its own refresh to
                        // get decorated rather than staying stale/blank until
                        // the next edit or cursor move.
                        match event {
                            EditorEvent::BufferEdited => {
                                editor.update(cx, |editor, cx| refresh(editor, window, cx));
                            }
                            EditorEvent::SelectionsChanged { .. } => {
                                editor.update(cx, |editor, cx| {
                                    queue_selection_refresh(editor, window, cx)
                                });
                            }
                            EditorEvent::ScrollPositionChanged { .. } => {
                                editor.update(cx, |editor, cx| {
                                    refresh_after_scroll(editor, window, cx)
                                });
                            }
                            _ => {}
                        }
                    },
                ),
                cx.subscribe_in(
                    &buffer,
                    window,
                    |state: &mut VisualMdState, _buffer, event, window, cx| {
                        match event {
                            multi_buffer::Event::LanguageChanged(..) => {
                                state
                                    .editor
                                    .update(cx, |editor, cx| force_refresh(editor, window, cx))
                                    .log_err();
                            }
                            // Emitted only when a buffer's own resolved
                            // language settings change, so this covers the
                            // user file, a project's `.zed/settings.json` and
                            // a `languages` entry, for just the buffers they
                            // affect.
                            multi_buffer::Event::SettingsChanged => {
                                state
                                    .editor
                                    .update(cx, |editor, cx| {
                                        drop_redundant_override(editor, cx);
                                        refresh(editor, window, cx)
                                    })
                                    .log_err();
                            }
                            _ => {}
                        }
                    },
                ),
                // A theme switch changes the colors baked into highlights, and
                // the language registry re-resolves each grammar's highlight
                // ids against the new theme, which the plan does not capture.
                cx.observe_global_in::<theme::GlobalTheme>(window, |state, window, cx| {
                    state
                        .editor
                        .update(cx, |editor, cx| {
                            if is_decorating(editor) {
                                force_refresh(editor, window, cx)
                            }
                        })
                        .log_err();
                }),
                // Font settings live in `ThemeSettings`, so a change to them
                // does not alter the buffer's own language settings.
                cx.observe_global_in::<settings::SettingsStore>(window, |state, window, cx| {
                    state
                        .editor
                        .update(cx, |editor, cx| {
                            if is_decorating(editor) {
                                refresh(editor, window, cx)
                            }
                        })
                        .log_err();
                }),
                theme_settings::observe_buffer_font_size_adjustment_in(
                    window,
                    cx,
                    |state, window, cx| {
                        state
                            .editor
                            .update(cx, |editor, cx| {
                                if is_decorating(editor) {
                                    refresh(editor, window, cx)
                                }
                            })
                            .log_err();
                    },
                ),
            ],
        })
    }
}

/// Whether live preview is currently decorating `editor`. Global observers
/// fire for every editor, so they use this to leave the others alone.
fn is_decorating(editor: &Editor) -> bool {
    editor
        .addon::<VisualMdAddon>()
        .is_some_and(|addon| addon.active)
}

/// Which `HighlightKey::VisualMd` sub-key each decoration category uses. The
/// variant namespaces visual_md's own highlights from every other highlight
/// source, and the sub-key picks the decoration family (heading, emphasis,
/// etc.) so unrelated categories can be replaced independently.
///
/// `CustomHighlightsChunks::next`
/// (crates/editor/src/display_map/custom_highlights.rs) folds every active
/// key into one `HighlightStyle` in ascending key order, each later style's
/// set fields overriding earlier ones, so the numbering is the precedence:
/// prose sits below everything it applies to, and the layers a user can
/// configure for a specific construct (inline code, code blocks) sit above
/// the layers that apply to whole categories of text.
const KEY_PROSE_FONT: usize = 0;
/// Restores the buffer font's weight on code when prose has its own weight,
/// which would otherwise carry over to it.
const KEY_CODE_WEIGHT: usize = 1;
const KEY_DIMMED_MARKER: usize = 2;
// Headings get one key per level because each level carries its own
// `HighlightStyle::font_size_scale`: keeping them disjoint means changing
// one heading's level cleanly removes its old size/color highlight instead of
// leaving a stale one from a different level composited underneath.
const KEY_HEADING_1: usize = 3;
const KEY_HEADING_2: usize = 4;
const KEY_HEADING_3: usize = 5;
const KEY_HEADING_4: usize = 6;
const KEY_HEADING_5: usize = 7;
const KEY_HEADING_6: usize = 8;
const KEY_BOLD: usize = 9;
const KEY_ITALIC: usize = 10;
const KEY_STRIKETHROUGH: usize = 11;
const KEY_LINK: usize = 12;
/// The first of the callout background keys: one per distinct callout type
/// name in view, each setting a different `background_color`. Keeping them
/// disjoint means a callout that changes type (edited from `[!note]` to
/// `[!warning]`) cleanly drops its old tint instead of compositing two
/// backgrounds together.
const KEY_CALLOUT_FIRST: usize = 100;
const KEY_HIGHLIGHT: usize = 1000;
const KEY_INLINE_CODE: usize = 1001;
const KEY_CODE_BLOCK: usize = 1002;

fn heading_key(level: u8) -> usize {
    match level {
        1 => KEY_HEADING_1,
        2 => KEY_HEADING_2,
        3 => KEY_HEADING_3,
        4 => KEY_HEADING_4,
        5 => KEY_HEADING_5,
        _ => KEY_HEADING_6,
    }
}

/// How many extra display rows above/below the actual visible viewport
/// `refresh` still decorates. Purely a smoothness margin for scrolling (so a
/// decoration doesn't visibly pop in a frame after it scrolls into view) —
/// not a correctness requirement, since `plan_viewport` also always covers
/// the current selection(s) regardless of this margin.
const VIEWPORT_OVERSCAN_ROWS: u32 = 200;

/// The buffer byte range `refresh` should bother planning decorations for.
///
/// Returns the whole buffer when the editor hasn't been laid out yet
/// (`visible_line_count` is `None` until the first paint, which is exactly
/// the situation at `register_editor`'s own initial `refresh` call) — there
/// is no meaningful viewport to scope to yet, and decorating everything is
/// the safe default rather than risking an under-decorated freshly opened
/// file.
fn visible_byte_range(
    editor: &Editor,
    display_snapshot: &DisplaySnapshot,
    text_len: usize,
    overscan_rows: u32,
    cx: &App,
) -> Range<usize> {
    let Some(visible_lines) = editor.visible_line_count() else {
        return 0..text_len;
    };

    let scroll_top = editor
        .scroll_manager
        .scroll_top_display_point(display_snapshot, cx);
    let top_row = scroll_top.row().0.saturating_sub(overscan_rows);
    let bottom_row = scroll_top
        .row()
        .0
        .saturating_add(visible_lines.ceil() as u32)
        .saturating_add(overscan_rows);

    // A column of `u32::MAX` is not a safe "clamp to end of line" sentinel:
    // `clip_point` clips the *row* first and the column arithmetic further
    // downstream can overflow before the column itself is ever clamped.
    // Using column 0 of the row just past `bottom_row` instead gives a point
    // that is always valid pre-clip (row is clamped by `clip_point`, column
    // 0 never needs clamping) and still lands at or beyond the true end of
    // `bottom_row`.
    let max_point = display_snapshot.max_point();
    let top_point =
        display_snapshot.clip_point(DisplayPoint::new(DisplayRow(top_row), 0), Bias::Left);
    let bottom_point = if bottom_row.saturating_add(1) >= max_point.row().0 {
        max_point
    } else {
        display_snapshot.clip_point(DisplayPoint::new(DisplayRow(bottom_row + 1), 0), Bias::Left)
    };

    let start = top_point.to_offset(display_snapshot, Bias::Left).0;
    let end = bottom_point.to_offset(display_snapshot, Bias::Right).0;
    start..end
}

/// Recomputes and reapplies visual_md's decorations for `editor`'s buffer:
/// parses the current text, checks which constructs the current selection(s)
/// touch, and diffs the result against what is already folded/highlighted.
///
/// Decoration generation itself is scoped to the visible viewport (plus a
/// margin, see [`VIEWPORT_OVERSCAN_ROWS`]) via [`plan::plan_viewport`], not
/// the whole buffer: for a multi-MB file, re-planning (and re-diffing) every
/// decoration in the entire document on every keystroke is what actually
/// makes typing feel laggy, not the tree-sitter parse itself. See
/// `plan_viewport`'s own doc comment for the full reasoning.
fn refresh(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let enabled = live_preview_enabled(editor, cx);

    let was_active = editor
        .addon_mut::<VisualMdAddon>()
        .map(|addon| std::mem::replace(&mut addon.active, enabled))
        .unwrap_or(false);

    // Every full-mode editor gets this addon (see `register_editor`), and
    // `refresh` runs on every scroll tick and cursor move, so a buffer that
    // isn't Markdown must cost nothing here rather than re-applying empty
    // highlights and copying the whole buffer each time.
    if !enabled && !was_active {
        return;
    }

    let (style_handle, style) = current_style(editor, cx);
    apply_text_style_refinement(editor, enabled, &style, cx);

    if enabled != was_active {
        // Line numbers don't fit the live-preview reading experience, and
        // "foldable" isn't meaningful there either (including the
        // indentation-heuristic fold affordance, which fires independently
        // of any crease visual_md made), so both are hidden only while
        // visual_md is decorating this buffer and restored to the user's
        // global preference otherwise.
        let gutter = editor::EditorSettings::get_global(cx).gutter;
        editor.set_show_line_numbers(if enabled { false } else { gutter.line_numbers }, cx);
        editor.set_show_fold_indicators(if enabled { false } else { gutter.folds }, cx);
    }

    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let document = enabled.then(|| parsed_document(editor, &snapshot));
    let text: Arc<str> = document
        .as_ref()
        .map_or_else(|| Arc::from(""), |(text, _)| text.clone());
    let computed = match &document {
        Some((text, Some(block_tree))) => {
            let display_snapshot = editor.display_snapshot(cx);
            let selections = editor
                .selections
                .all::<MultiBufferOffset>(&display_snapshot)
                .into_iter()
                .map(|selection| {
                    let range = selection.range();
                    range.start.0..range.end.0
                })
                .collect::<Vec<_>>();
            let visible_range = visible_byte_range(
                editor,
                &display_snapshot,
                text.len(),
                VIEWPORT_OVERSCAN_ROWS,
                cx,
            );
            let plan =
                plan::plan_viewport_with_tree(text, block_tree, &selections, visible_range.clone());
            if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
                addon.planned = Some((snapshot.edit_count(), visible_range));
            }
            plan
        }
        _ => {
            if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
                addon.planned = None;
            }
            Plan::default()
        }
    };

    let edit_count = snapshot.edit_count();
    let unchanged = enabled == was_active
        && editor
            .addon::<VisualMdAddon>()
            .and_then(|addon| addon.last_applied.as_ref())
            .is_some_and(|(applied_edit_count, applied)| {
                *applied_edit_count == edit_count && *applied == computed
            });
    if unchanged {
        return;
    }

    let editor_handle = cx.weak_entity();
    // The `String` alongside each range is a content key, not just an
    // identifier: `apply_folds` diffs on `(range, key)` together, not range
    // alone, specifically so a checkbox toggle — which edits `[ ]` to `[x]`
    // in place, leaving its byte range unchanged — still gets its crease
    // recreated with the flipped glyph instead of being treated as
    // "unchanged, nothing to do".
    let mut folds: Vec<(Range<usize>, String, editor::FoldPlaceholder)> = computed
        .hidden_markers
        .iter()
        .map(|range| (range.clone(), " ".to_string(), space_placeholder()))
        .chain(
            computed
                .glyph_markers
                .iter()
                .map(|(range, kind)| match kind {
                    GlyphKind::Bullet => {
                        (range.clone(), "bullet".to_string(), bullet_placeholder())
                    }
                    GlyphKind::Ordinal(text) => (
                        range.clone(),
                        format!("ordinal:{text}"),
                        ordinal_placeholder(text.clone()),
                    ),
                    GlyphKind::BlockquoteBar => (
                        range.clone(),
                        "blockquote_bar".to_string(),
                        blockquote_bar_placeholder(style_handle.clone()),
                    ),
                    GlyphKind::TablePipe => (
                        range.clone(),
                        "table_pipe".to_string(),
                        table_pipe_placeholder(style_handle.clone()),
                    ),
                }),
        )
        .collect();
    folds.extend(computed.checkboxes.iter().map(|(range, checked)| {
        (
            range.clone(),
            format!("checkbox:{checked}"),
            checkbox_placeholder(editor_handle.clone(), *checked, style_handle.clone()),
        )
    }));
    // An untouched callout's title (see `callout_title_placeholder`'s own
    // doc comment for why a touched one is excluded here -- it reveals as
    // raw, dimmed text via `dimmed_markers` instead, handled entirely by
    // `apply_style_highlights`, not this fold list).
    folds.extend(
        computed
            .callouts
            .iter()
            .filter(|callout| !callout.touched)
            .map(|callout| {
                (
                    callout.marker_range.clone(),
                    format!(
                        "callout-title:{:?}:{:?}:{}",
                        callout.kind, callout.fold, callout.raw_type_name
                    ),
                    callout_title_placeholder(
                        editor_handle.clone(),
                        callout.kind,
                        callout.raw_type_name.clone(),
                        callout.fold,
                        callout.suffix_range.clone(),
                        style_handle.clone(),
                    ),
                )
            }),
    );
    // A collapsed callout's body folds regardless of whether its title is
    // currently touched -- collapsing is a deliberate choice the user made
    // by clicking the chevron, not a cursor-driven reveal, so editing the
    // type name on the title line doesn't spuriously re-expand the body.
    folds.extend(
        computed
            .callouts
            .iter()
            .filter(|callout| callout.fold.is_collapsed() && !callout.body_range.is_empty())
            .map(|callout| {
                (
                    callout.body_range.clone(),
                    "callout-collapsed".to_string(),
                    callout_collapsed_body_placeholder(),
                )
            }),
    );
    folds.extend(table_alignment_spacer_folds(
        &computed, &text, &style, window, cx,
    ));

    apply_folds(editor, &snapshot, folds, window, cx);
    apply_style_highlights(editor, &snapshot, &computed, enabled, &style, cx);
    apply_horizontal_rules(editor, &snapshot, &computed, style_handle.clone(), cx);
    apply_images(editor, &snapshot, &computed, cx);
    apply_table_dividers(editor, &snapshot, &computed, style_handle.clone(), cx);
    apply_code_fence_borders(editor, &snapshot, &computed, style_handle, cx);
    let code_languages: HashSet<String> = computed
        .code_fence_content
        .iter()
        .filter_map(|(_, name)| name.clone())
        .collect();
    ensure_code_languages_loaded(editor, window, cx, code_languages);
    apply_code_syntax_highlights(editor, &snapshot, &text, &computed, cx);

    if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
        addon.last_applied = Some((edit_count, computed));
    }
}

/// Resolves the style for `editor`'s buffer and returns it with the shared
/// handle that paint-time closures read. A style that differs from the last
/// one is published to that handle and invalidates `last_applied`, because the
/// plan does not capture fonts and colors.
fn current_style(
    editor: &mut Editor,
    cx: &App,
) -> (Arc<ArcSwap<ResolvedStyle>>, Arc<ResolvedStyle>) {
    let settings = editor
        .buffer()
        .read(cx)
        .language_settings_at(MultiBufferOffset(0), cx);
    let resolved = ResolvedStyle::resolve(&settings.visual_md, cx);
    let Some(addon) = editor.addon_mut::<VisualMdAddon>() else {
        let resolved = Arc::new(resolved);
        return (Arc::new(ArcSwap::new(resolved.clone())), resolved);
    };
    if **addon.style.load() != resolved {
        addon.style.store(Arc::new(resolved));
        addon.last_applied = None;
    }
    (addon.style.clone(), addon.style.load_full())
}

/// Gives the editor the prose size and line height, which only a refinement of
/// its base text style can do (it also makes soft wrap measure prose at the
/// right size). The editor's own refinement is saved first and restored once
/// live preview stops applying these, and nothing is touched while neither is
/// configured.
fn apply_text_style_refinement(
    editor: &mut Editor,
    enabled: bool,
    style: &ResolvedStyle,
    cx: &mut Context<Editor>,
) {
    let wanted = style.text_style_refinement().filter(|_| enabled);
    let Some(ours) = wanted else {
        let saved = editor
            .addon_mut::<VisualMdAddon>()
            .and_then(|addon| addon.saved_text_style_refinement.take());
        if let Some(saved) = saved {
            match saved {
                Some(refinement) => editor.set_text_style_refinement(refinement),
                None => editor.clear_text_style_refinement(),
            }
            cx.notify();
        }
        return;
    };

    let existing = editor.text_style_refinement().cloned();
    let base = match editor.addon_mut::<VisualMdAddon>() {
        Some(addon) => match &addon.saved_text_style_refinement {
            Some(saved) => saved.clone(),
            None => {
                addon.saved_text_style_refinement = Some(existing);
                addon.saved_text_style_refinement.clone().flatten()
            }
        },
        None => existing,
    };
    let mut refinement = base.unwrap_or_default();
    if ours.font_size.is_some() {
        refinement.font_size = ours.font_size;
    }
    if ours.line_height.is_some() {
        refinement.line_height = ours.line_height;
    }
    if editor.text_style_refinement() != Some(&refinement) {
        editor.set_text_style_refinement(refinement);
        cx.notify();
    }
}

/// `refresh`, but re-applying even if the plan is unchanged: for when
/// something the plan doesn't capture (a language finishing loading) changed
/// what applying it produces.
fn force_refresh(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
        addon.last_applied = None;
    }
    refresh(editor, window, cx);
}

/// Defers a selection-triggered refresh to the next frame. A mouse drag can
/// report selection changes far faster than frames are drawn, and only the
/// final selection of each frame is ever visible.
fn queue_selection_refresh(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let Some(addon) = editor.addon_mut::<VisualMdAddon>() else {
        return;
    };
    if std::mem::replace(&mut addon.selection_refresh_queued, true) {
        return;
    }
    let editor_entity = cx.entity();
    window.on_next_frame(move |window, cx| {
        editor_entity.update(cx, |editor, cx| {
            if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
                addon.selection_refresh_queued = false;
            }
            refresh(editor, window, cx);
        });
    });
}

/// Returns the buffer text and its block parse for `snapshot`, reusing the
/// previous refresh's when no edit has happened since. The tree is `None` only
/// if tree-sitter failed to parse.
fn parsed_document(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
) -> (Arc<str>, Option<tree_sitter::Tree>) {
    let edit_count = snapshot.edit_count();
    if let Some(cached) = editor
        .addon::<VisualMdAddon>()
        .and_then(|addon| addon.parsed.as_ref())
        .filter(|cached| cached.edit_count == edit_count)
    {
        return (cached.text.clone(), Some(cached.block_tree.clone()));
    }

    let text: Arc<str> = Arc::from(snapshot.text());
    let block_tree = plan::parse_blocks(&text);
    if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
        addon.parsed = block_tree.clone().map(|block_tree| ParsedDocument {
            edit_count,
            text: text.clone(),
            block_tree,
        });
    }
    (text, block_tree)
}

/// Handles a scroll: re-plans only once the visible rows reach outside the
/// range the last refresh planned (which already includes an overscan margin),
/// since nothing a scroll alone changes is inside that range.
fn refresh_after_scroll(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let still_covered = editor
        .addon::<VisualMdAddon>()
        .filter(|addon| addon.active)
        .and_then(|addon| addon.planned.clone())
        .is_some_and(|(edit_count, planned_range)| {
            if edit_count != snapshot.edit_count() {
                return false;
            }
            let display_snapshot = editor.display_snapshot(cx);
            let visible_range =
                visible_byte_range(editor, &display_snapshot, snapshot.len().0, 0, cx);
            planned_range.start <= visible_range.start && visible_range.end <= planned_range.end
        });
    if !still_covered {
        refresh(editor, window, cx);
    }
}

/// Applies a `VisualMd` highlight, except that clearing a key that is already
/// empty is skipped: every refresh sets all of them, and each call would
/// otherwise rebuild the display map's highlights and notify for nothing.
fn set_visual_md_highlight(
    editor: &mut Editor,
    key: usize,
    ranges: Vec<Range<Anchor>>,
    style: HighlightStyle,
    cx: &mut Context<Editor>,
) {
    let is_empty = ranges.is_empty();
    let was_nonempty = editor.addon_mut::<VisualMdAddon>().is_some_and(|addon| {
        if is_empty {
            addon.nonempty_highlight_keys.remove(&key)
        } else {
            addon.nonempty_highlight_keys.insert(key);
            true
        }
    });
    if is_empty && !was_nonempty {
        return;
    }
    editor.highlight_text_key(HighlightKey::VisualMd(key), ranges, style, false, cx);
}

fn to_anchor_range(snapshot: &MultiBufferSnapshot, range: &Range<usize>) -> Range<Anchor> {
    snapshot.anchor_before(MultiBufferOffset(range.start))
        ..snapshot.anchor_after(MultiBufferOffset(range.end))
}

fn to_anchor_ranges(snapshot: &MultiBufferSnapshot, ranges: &[Range<usize>]) -> Vec<Range<Anchor>> {
    ranges
        .iter()
        .map(|range| to_anchor_range(snapshot, range))
        .collect()
}

/// Base settings shared by every visual_md fold placeholder.
///
/// A *genuinely* zero-width fold (`gpui::Empty`, or an empty `div()`)
/// reliably panics deep in `tab_map.rs` ("attempt to subtract with
/// overflow") the moment it's folded — confirmed by bisection down to a
/// single `**bold**` span, and independent of anything in this crate's own
/// ranges (verified overlap-free by the planner's own tests). Zed's fold
/// engine appears to require folds to have some positive measured width, so
/// every placeholder here renders at least one character.
fn base_placeholder() -> editor::FoldPlaceholder {
    editor::FoldPlaceholder {
        constrain_width: false,
        merge_adjacent: false,
        ..editor::FoldPlaceholder::default()
    }
}

fn space_placeholder() -> editor::FoldPlaceholder {
    editor::FoldPlaceholder {
        render: std::sync::Arc::new(|_, _, _| {
            div().child(SharedString::from(" ")).into_any_element()
        }),
        collapsed_text: Some(SharedString::from(" ")),
        ..base_placeholder()
    }
}

/// A list bullet, drawn as a real filled dot rather than a Unicode `•`:
/// glyph coverage for symbol characters varies enough across fonts (see
/// `checkbox_placeholder`'s doc comment for a case where it silently failed
/// entirely) that a drawn shape is the more robust choice, not just a
/// stylistic one.
fn bullet_placeholder() -> editor::FoldPlaceholder {
    editor::FoldPlaceholder {
        render: std::sync::Arc::new(|_, _, cx| {
            let color = {
                use theme::ActiveTheme;
                cx.theme().colors().icon_muted
            };
            div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(14.))
                .h_full()
                .child(div().size(px(5.)).rounded_full().bg(color))
                .into_any_element()
        }),
        collapsed_text: Some(SharedString::from("• ")),
        ..base_placeholder()
    }
}

/// An ordered list item's renumbered marker (e.g. `"3. "`). Kept as plain
/// text — digits are plain ASCII with no font-coverage risk, unlike the
/// symbol glyphs `bullet_placeholder`/`blockquote_bar_placeholder`/
/// `checkbox_placeholder` deliberately avoid.
fn ordinal_placeholder(text: String) -> editor::FoldPlaceholder {
    let text = SharedString::from(text);
    editor::FoldPlaceholder {
        render: {
            let text = text.clone();
            std::sync::Arc::new(move |_, _, _| div().child(text.clone()).into_any_element())
        },
        collapsed_text: Some(text),
        ..base_placeholder()
    }
}

/// A blockquote/callout's left bar, drawn as a real filled rectangle rather
/// than the Unicode block-drawing character `▎` — see `bullet_placeholder`'s
/// doc comment for why a drawn shape is preferred over a symbol glyph here.
fn blockquote_bar_placeholder(style: Arc<ArcSwap<ResolvedStyle>>) -> editor::FoldPlaceholder {
    editor::FoldPlaceholder {
        render: std::sync::Arc::new(move |_, _, _| {
            let color = style.load().blockquote_bar_color;
            div()
                .flex()
                .items_center()
                .w(px(14.))
                .h_full()
                .child(div().w(px(3.)).h_full().bg(color))
                .into_any_element()
        }),
        collapsed_text: Some(SharedString::from("▎ ")),
        ..base_placeholder()
    }
}

/// A GFM table's `|` column separator, folded to a thin centered vertical
/// bar -- same technique as `blockquote_bar_placeholder`, just centered in
/// its own narrow width rather than left-anchored, since it sits *between*
/// two cells' text instead of at the start of an indented line.
fn table_pipe_placeholder(style: Arc<ArcSwap<ResolvedStyle>>) -> editor::FoldPlaceholder {
    editor::FoldPlaceholder {
        render: std::sync::Arc::new(move |_, _, _| {
            let color = style.load().table_border_color;
            div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(9.))
                .h_full()
                .child(div().w(px(1.)).h_full().bg(color))
                .into_any_element()
        }),
        collapsed_text: Some(SharedString::from("│")),
        ..base_placeholder()
    }
}

/// An interactive checkbox for a `[ ]`/`[x]` task marker. Per spec these stay
/// clickable in both raw and rendered states, so unlike every other
/// placeholder here it is never conditionally hidden vs. dimmed — it is
/// always folded.
///
/// `checked` is captured by value from the plan rather than read live from
/// the buffer inside `render`: `render` runs *during* the editor's own paint
/// pass, and reading the same editor entity back out from inside that —
/// `editor.read_with(cx, ...)` — is a genuine re-entrant borrow conflict, not
/// a hypothetical one. It failed exactly like CLAUDE.md's guidance on this
/// predicts: silently (`render` producing nothing visible, since the
/// contained `read_with` never got a chance to run its closure), not with a
/// crash, which made it look at first like the whole placeholder mechanism
/// was broken rather than this one call. `refresh` recreates this crease
/// with a freshly-captured `checked` on every edit (see its own comment on
/// why the fold-diffing key includes checked state), so this stays correct
/// across a toggle without ever reading live state from inside render.
///
/// Drawn as a real bordered box (with a real `check.svg` icon when checked)
/// rather than the Unicode `☐`/`☑` characters this used to render: `☑`
/// (U+2611) is a much rarer symbol than `☐` (U+2610) in typical font glyph
/// coverage, and silently rendering nothing when a fold's `render` closure
/// produces content the shaper can't display looks identical to the
/// re-entrancy failure mode described above — confirmed the checked state
/// specifically was the one silently blank in the real app while the
/// unchecked box rendered fine. A drawn shape has no font dependency at all.
fn checkbox_placeholder(
    editor: WeakEntity<Editor>,
    checked: bool,
    style: Arc<ArcSwap<ResolvedStyle>>,
) -> editor::FoldPlaceholder {
    editor::FoldPlaceholder {
        render: std::sync::Arc::new(move |fold_id, range, cx| {
            let editor = editor.clone();
            let checked_color = style.load().task_checked_color;
            let colors = {
                use theme::ActiveTheme;
                cx.theme().colors()
            };
            let box_size = px(13.);
            div()
                .id(fold_id)
                .cursor_pointer()
                .flex()
                .items_center()
                .justify_center()
                .w(px(18.))
                .h_full()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(box_size)
                        .rounded(px(3.))
                        .when(!checked, |el| el.border_1().border_color(colors.icon_muted))
                        .when(checked, |el| el.bg(checked_color))
                        .when(checked, |el| {
                            el.child(
                                svg()
                                    .path("icons/check.svg")
                                    .size(px(9.))
                                    .text_color(colors.editor_background),
                            )
                        }),
                )
                .on_click(move |_event, _window, cx| {
                    let new_text = if checked { "[ ]" } else { "[x]" };
                    editor
                        .update(cx, |editor, cx| {
                            editor.edit([(range.clone(), new_text)], cx)
                        })
                        .log_err();
                })
                .into_any_element()
        }),
        collapsed_text: Some(SharedString::from(if checked { "[x]" } else { "[ ]" })),
        ..base_placeholder()
    }
}

/// An untouched callout's `[!type]` (plus any `+`/`-` suffix) marker range,
/// replacing it with a chevron + icon + capitalized title -- modeled
/// directly on `checkbox_placeholder`: the chevron is a real click target
/// that writes the fold suffix back to the buffer the same one-edit way the
/// checkbox writes `[ ]`/`[x]`, rather than a read-only glyph. There's
/// deliberately no right-click "change type" menu (see this module's M11
/// doc section) -- touching this line at all (a plain click that lands the
/// cursor, not a click on this widget) reveals it as raw, dimmed `[!type]`
/// text instead of this widget (see `plan_block_quote`), which is how a
/// user actually retypes the type name or hand-edits the suffix.
fn callout_title_placeholder(
    editor: WeakEntity<Editor>,
    kind: CalloutKind,
    raw_type_name: String,
    fold: CalloutFold,
    suffix_range: Range<usize>,
    style: Arc<ArcSwap<ResolvedStyle>>,
) -> editor::FoldPlaceholder {
    let collapsed = fold.is_collapsed();
    let label = SharedString::from(capitalize(&raw_type_name));
    let collapsed_text = label.clone();
    editor::FoldPlaceholder {
        render: std::sync::Arc::new(move |fold_id, _range, _| {
            let editor = editor.clone();
            let suffix_range = suffix_range.clone();
            let label = label.clone();
            let look = style.load().callout_look(kind, &raw_type_name);
            let (icon_path, color) = (look.icon_path, look.accent);
            let chevron_path = if collapsed {
                "icons/chevron_right.svg"
            } else {
                "icons/chevron_down.svg"
            };
            div()
                .id(fold_id)
                .flex()
                .items_center()
                .gap(px(4.))
                .h_full()
                .child(
                    div()
                        .id("callout-fold-toggle")
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(14.))
                        .child(svg().path(chevron_path).size(px(12.)).text_color(color))
                        .on_click(move |_event, _window, cx| {
                            let new_suffix = if collapsed { "" } else { "-" };
                            let edit_range = MultiBufferOffset(suffix_range.start)
                                ..MultiBufferOffset(suffix_range.end);
                            editor
                                .update(cx, |editor, cx| {
                                    editor.edit([(edit_range, new_suffix)], cx)
                                })
                                .log_err();
                        }),
                )
                .child(svg().path(icon_path).size(px(13.)).text_color(color))
                .child(
                    div()
                        .text_color(color)
                        .font_weight(FontWeight::BOLD)
                        .child(label),
                )
                .into_any_element()
        }),
        // Deliberately the capitalized label, not literal `[!type]` bracket
        // text -- keeps this distinguishable from the *touched*, raw-dimmed
        // rendering of the same marker (which does show the literal
        // brackets), both for a real viewer scanning the buffer and for
        // tests asserting on `display_text()`.
        collapsed_text: Some(collapsed_text),
        ..base_placeholder()
    }
}

/// A collapsed callout's body (`fold == Collapsed`), from just after the
/// title line's marker through the end of the callout -- a genuine
/// multi-line crease, the same mechanism Zed's own code folding uses to
/// collapse a function body, not a new capability. No kind-specific styling
/// here: the callout's background tint (`SpanStyle::Callout`) already covers
/// this row regardless of fold state, so it shows through underneath this
/// chip exactly like it does under the title widget above.
fn callout_collapsed_body_placeholder() -> editor::FoldPlaceholder {
    editor::FoldPlaceholder {
        render: std::sync::Arc::new(|_, _, cx| {
            let color = {
                use theme::ActiveTheme;
                cx.theme().colors().text_muted
            };
            div()
                .px(px(4.))
                .h_full()
                .flex()
                .items_center()
                .text_color(color)
                .child(SharedString::from("(collapsed)"))
                .into_any_element()
        }),
        collapsed_text: Some(SharedString::from("(collapsed)")),
        ..base_placeholder()
    }
}

/// Capitalizes only the first character, leaving the rest as typed (so a
/// deliberately-stylized custom type like `[!tODO]` isn't mangled into
/// something the user didn't write).
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Diffs `folds` against the creases folded by the previous refresh (tracked
/// on [`VisualMdAddon`]), removing (and unfolding) whichever ranges are no
/// longer wanted and folding whichever are newly wanted, leaving unchanged
/// ranges alone entirely.
fn apply_folds(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    folds: Vec<(Range<usize>, String, editor::FoldPlaceholder)>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let previous = editor
        .addon_mut::<VisualMdAddon>()
        .map(|addon| std::mem::take(&mut addon.folded_markers))
        .unwrap_or_default();

    // Diffed on `(range, key)` together, not range alone: see `refresh`'s
    // comment on why a checkbox's content key changes with its checked
    // state even though its range doesn't, so a toggle recreates its crease
    // (with the flipped glyph) instead of being mistaken for "unchanged".
    //
    // A `HashSet` here, not a `Vec` scanned with `.contains`: with viewport
    // scoping (see `refresh`) this still runs on every scroll/edit/selection
    // change, and a linear-scan diff makes the whole function O(n²) in the
    // number of decorations in view — negligible for a handful of markers,
    // but exactly the kind of thing that turns into visible typing lag once
    // a screenful of a dense document is on screen.
    let wanted: HashSet<(Range<usize>, String)> = folds
        .iter()
        .map(|(range, key, _)| (range.clone(), key.clone()))
        .collect();
    let mut kept = Vec::new();
    let mut stale_ids = Vec::new();
    for (range, key, id) in previous {
        if wanted.contains(&(range.clone(), key.clone())) {
            kept.push((range, key, id));
        } else {
            stale_ids.push(id);
        }
    }

    if !stale_ids.is_empty() {
        let removed_ranges: Vec<Range<Anchor>> = editor
            .remove_creases(stale_ids, cx)
            .into_iter()
            .map(|(_, range)| range)
            .collect();
        // `inclusive: false`, not `true`: `unfold_ranges` unfolds every fold
        // that *intersects* the given ranges, and with `inclusive: true`
        // that includes folds merely touching a range's boundary, not just
        // ones overlapping it. visual_md's own folds routinely sit right next
        // to each other byte-for-byte (e.g. a task item's hidden bullet
        // ending exactly where its checkbox crease begins), so
        // `inclusive: true` here was unfolding a perfectly valid *adjacent*
        // crease every time a neighboring one went stale — e.g. every
        // checkbox toggle spuriously unfolded the task's own bullet marker,
        // even though that crease's id was never in `stale_ids` and
        // `addon.folded_markers` still (incorrectly) believed it was folded.
        // The removed ranges here always come from creases that genuinely,
        // strictly overlap themselves (they're each fold's own exact range),
        // so `inclusive: false` still finds and removes exactly the stale
        // folds without also catching their neighbors.
        editor.unfold_ranges(&removed_ranges, false, false, cx);
    }

    let already_kept: HashSet<(Range<usize>, String)> = kept
        .iter()
        .map(|(range, key, _)| (range.clone(), key.clone()))
        .collect();
    // `insert_creases` requires its input sorted by position — its
    // underlying sum-tree cursor can only seek forward and panics
    // ("cannot seek backward") otherwise. `folds` arrives here as several
    // categories concatenated together (hidden markers, glyphs, checkboxes),
    // not globally sorted.
    let mut to_add: Vec<(Range<usize>, String, editor::FoldPlaceholder)> = folds
        .into_iter()
        .filter(|(range, key, _)| !already_kept.contains(&(range.clone(), key.clone())))
        .collect();
    to_add.sort_by_key(|(range, _, _)| range.start);

    if !to_add.is_empty() {
        let creases: Vec<Crease<Anchor>> = to_add
            .iter()
            .map(|(range, _, placeholder)| {
                // These are permanent decorative replacements (a hidden
                // marker, a bullet glyph, a checkbox), never a
                // user-collapsible region, so the gutter's default
                // "any folded row gets a toggle" fallback is wrong here —
                // see `Crease::hide_gutter_toggle`'s doc comment.
                Crease::simple(to_anchor_range(snapshot, range), placeholder.clone())
                    .without_gutter_toggle()
            })
            .collect();
        let ids = editor.insert_creases(creases.clone(), cx);
        editor.fold_creases(creases, false, window, cx);
        kept.extend(
            to_add
                .into_iter()
                .map(|(range, key, _)| (range, key))
                .zip(ids)
                .map(|((range, key), id)| (range, key, id)),
        );
    }

    if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
        addon.folded_markers = kept;
    }
}

/// Diffs `computed.horizontal_rules` against the blocks inserted by the
/// previous refresh, same shape as `apply_folds`'s diff (`std::mem::take`
/// the addon's previous list, split into kept vs. stale by whether the range
/// is still wanted, remove the stale ones, insert the newly wanted ones).
///
/// This is a real, separate mechanism from every other decoration in this
/// file, not a stylistic choice: a `FoldPlaceholder` (used everywhere else —
/// bullets, blockquote bars, checkboxes) can only size itself to its own
/// content (`AvailableSpace::MinContent` unless `constrain_width` asks it to
/// match a specific *collapsed text* width — see
/// `crates/editor/src/element.rs`'s `ChunkReplacement::Renderer` handling),
/// so there is no way to make one stretch to the editor's actual visible
/// width for a full-width `<hr>`. The editor's block-decoration API
/// (`insert_blocks`/`BlockProperties`) is built for exactly this — its
/// render callback receives a real `max_width: Pixels` to draw against (see
/// `render_horizontal_rule`), and a `BlockPlacement::Replace` swaps out the
/// entire visual row rather than decorating text within it.
fn apply_horizontal_rules(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    computed: &Plan,
    style: Arc<ArcSwap<ResolvedStyle>>,
    cx: &mut Context<Editor>,
) {
    let previous = editor
        .addon_mut::<VisualMdAddon>()
        .map(|addon| std::mem::take(&mut addon.hr_blocks))
        .unwrap_or_default();

    let wanted: HashSet<Range<usize>> = computed.horizontal_rules.iter().cloned().collect();
    let mut kept = Vec::new();
    // `remove_blocks` specifically wants `collections::HashSet` (an
    // `FxHashSet`), not `std::collections::HashSet` -- the type this file
    // otherwise uses everywhere else (e.g. `apply_folds`'s own `wanted` set).
    let mut stale_ids: collections::HashSet<CustomBlockId> = collections::HashSet::default();
    for (range, id) in previous {
        if wanted.contains(&range) {
            kept.push((range, id));
        } else {
            stale_ids.insert(id);
        }
    }
    if !stale_ids.is_empty() {
        editor.remove_blocks(stale_ids, None, cx);
    }

    let already_kept: HashSet<Range<usize>> = kept.iter().map(|(range, _)| range.clone()).collect();
    let new_ranges: Vec<Range<usize>> = computed
        .horizontal_rules
        .iter()
        .filter(|range| !already_kept.contains(*range))
        .cloned()
        .collect();

    if !new_ranges.is_empty() {
        let new_blocks: Vec<BlockProperties<Anchor>> = new_ranges
            .iter()
            .map(|range| {
                let anchor_range = to_anchor_range(snapshot, range);
                let style = style.clone();
                BlockProperties {
                    placement: BlockPlacement::Replace(anchor_range.start..=anchor_range.end),
                    height: Some(1),
                    style: BlockStyle::Fixed,
                    render: std::sync::Arc::new(move |cx: &mut BlockContext| {
                        let color = style.load().rule_color;
                        render_horizontal_rule(cx, color)
                    }),
                    priority: 0,
                }
            })
            .collect();
        let ids = editor.insert_blocks(new_blocks, None, cx);
        kept.extend(new_ranges.into_iter().zip(ids));
    }

    if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
        addon.hr_blocks = kept;
    }
}

/// Renders a `---`/`***`/`___` line as a full-width thin rule, using
/// `BlockContext::max_width` (the real remaining editor width, given to this
/// callback directly by the block-decoration layer) rather than a fixed
/// pixel guess or a flex `w_full()` that can't resolve against the
/// indeterminate width a fold placeholder would otherwise be laid out in.
fn render_horizontal_rule(cx: &mut BlockContext, color: Hsla) -> AnyElement {
    div()
        .w(cx.max_width)
        .h(cx.line_height)
        .flex()
        .items_center()
        .child(div().w_full().h(px(1.)).bg(color))
        .into_any_element()
}

/// How many editor rows an image block occupies. Block heights are whole
/// rows fixed at insertion time, and the image's real size isn't known until
/// it has loaded, so every image gets the same box and is fit inside it.
const IMAGE_BLOCK_ROWS: u32 = 10;

/// How many parent directories an `![[embed]]` name is searched through,
/// nearest first, approximating Obsidian's vault-wide lookup by name.
const EMBED_SEARCH_DEPTH: usize = 8;

/// Turns an image line's target into something the image loader can open, or
/// `None` if it can't be resolved at all. A relative path is joined onto the
/// note's own directory; an unsaved buffer has no directory to join onto.
fn resolve_image_source(
    image: &ImageInfo,
    note_directory: Option<&std::path::Path>,
) -> Option<String> {
    let target = image.target.as_str();
    if target.starts_with("http://") || target.starts_with("https://") {
        return Some(target.to_string());
    }
    let target_path = std::path::Path::new(target);
    if target_path.is_absolute() {
        return Some(target.to_string());
    }
    let note_directory = note_directory?;
    if image.is_embed {
        let found = note_directory
            .ancestors()
            .take(EMBED_SEARCH_DEPTH)
            .map(|directory| directory.join(target_path))
            .find(|candidate| candidate.exists());
        if let Some(found) = found {
            return Some(found.to_string_lossy().into_owned());
        }
    }
    Some(
        note_directory
            .join(target_path)
            .to_string_lossy()
            .into_owned(),
    )
}

/// Diffs `computed.images` against the blocks inserted by the previous
/// refresh, same shape as `apply_horizontal_rules` but keyed on the resolved
/// source as well as the range.
fn apply_images(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    computed: &Plan,
    cx: &mut Context<Editor>,
) {
    let note_directory = editor
        .buffer()
        .read(cx)
        .as_singleton()
        .and_then(|buffer| {
            let buffer = buffer.read(cx);
            let local_file = buffer.file()?.as_local()?;
            Some(local_file.abs_path(cx))
        })
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf));

    let wanted: Vec<(Range<usize>, String)> = computed
        .images
        .iter()
        .filter_map(|image| {
            let source = resolve_image_source(image, note_directory.as_deref())?;
            Some((image.range.clone(), source))
        })
        .collect();

    let previous = editor
        .addon_mut::<VisualMdAddon>()
        .map(|addon| std::mem::take(&mut addon.image_blocks))
        .unwrap_or_default();

    let wanted_keys: HashSet<&(Range<usize>, String)> = wanted.iter().collect();
    let mut kept = Vec::new();
    let mut stale_ids: collections::HashSet<CustomBlockId> = collections::HashSet::default();
    for (range, source, id) in previous {
        if wanted_keys.contains(&(range.clone(), source.clone())) {
            kept.push((range, source, id));
        } else {
            stale_ids.insert(id);
        }
    }
    if !stale_ids.is_empty() {
        editor.remove_blocks(stale_ids, None, cx);
    }

    let already_kept: HashSet<(Range<usize>, String)> = kept
        .iter()
        .map(|(range, source, _)| (range.clone(), source.clone()))
        .collect();
    let new_images: Vec<(Range<usize>, String)> = wanted
        .iter()
        .filter(|key| !already_kept.contains(*key))
        .cloned()
        .collect();

    if !new_images.is_empty() {
        let new_blocks: Vec<BlockProperties<Anchor>> = new_images
            .iter()
            .map(|(range, source)| {
                let anchor_range = to_anchor_range(snapshot, range);
                let source = source.clone();
                BlockProperties {
                    placement: BlockPlacement::Replace(anchor_range.start..=anchor_range.end),
                    height: Some(IMAGE_BLOCK_ROWS),
                    style: BlockStyle::Fixed,
                    render: std::sync::Arc::new(move |cx: &mut BlockContext| {
                        render_image(&source, cx)
                    }),
                    priority: 0,
                }
            })
            .collect();
        let ids = editor.insert_blocks(new_blocks, None, cx);
        kept.extend(
            new_images
                .into_iter()
                .zip(ids)
                .map(|((range, source), id)| (range, source, id)),
        );
    }

    if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
        addon.image_blocks = kept;
    }
}

fn render_image(source: &str, cx: &mut BlockContext) -> AnyElement {
    let (border, muted) = {
        use theme::ActiveTheme;
        (cx.theme().colors().border, cx.theme().colors().text_muted)
    };
    let line_height = cx.line_height;
    let unavailable = {
        let source = source.to_string();
        move || {
            div()
                .px_2()
                .py_1()
                .border_1()
                .border_color(border)
                .rounded_md()
                .text_color(muted)
                .child(SharedString::from(format!("Image not found: {source}")))
                .into_any_element()
        }
    };
    let image_source: ImageSource =
        if source.starts_with("http://") || source.starts_with("https://") {
            ImageSource::from(SharedString::from(source.to_string()))
        } else {
            ImageSource::from(std::path::PathBuf::from(source))
        };
    div()
        .w(cx.max_width)
        .h(line_height * IMAGE_BLOCK_ROWS as f32)
        .flex()
        .items_center()
        .child(
            img(image_source)
                .max_w_full()
                .max_h_full()
                .object_fit(ObjectFit::Contain)
                .with_fallback(unavailable),
        )
        .into_any_element()
}

/// Diffs each table's `delimiter_line` against the divider blocks inserted
/// by the previous refresh -- structurally identical to
/// `apply_horizontal_rules` (same reasoning: a divider line can't stretch to
/// the real editor width via a `FoldPlaceholder`), just sourced from
/// `computed.tables` instead of `computed.horizontal_rules`.
fn apply_table_dividers(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    computed: &Plan,
    style: Arc<ArcSwap<ResolvedStyle>>,
    cx: &mut Context<Editor>,
) {
    let wanted_ranges: Vec<Range<usize>> = computed
        .tables
        .iter()
        .filter_map(|table| table.delimiter_line.clone())
        .collect();

    let previous = editor
        .addon_mut::<VisualMdAddon>()
        .map(|addon| std::mem::take(&mut addon.table_dividers))
        .unwrap_or_default();

    let wanted: HashSet<Range<usize>> = wanted_ranges.iter().cloned().collect();
    let mut kept = Vec::new();
    let mut stale_ids: collections::HashSet<CustomBlockId> = collections::HashSet::default();
    for (range, id) in previous {
        if wanted.contains(&range) {
            kept.push((range, id));
        } else {
            stale_ids.insert(id);
        }
    }
    if !stale_ids.is_empty() {
        editor.remove_blocks(stale_ids, None, cx);
    }

    let already_kept: HashSet<Range<usize>> = kept.iter().map(|(range, _)| range.clone()).collect();
    let new_ranges: Vec<Range<usize>> = wanted_ranges
        .into_iter()
        .filter(|range| !already_kept.contains(range))
        .collect();

    if !new_ranges.is_empty() {
        let new_blocks: Vec<BlockProperties<Anchor>> = new_ranges
            .iter()
            .map(|range| {
                let anchor_range = to_anchor_range(snapshot, range);
                let style = style.clone();
                BlockProperties {
                    placement: BlockPlacement::Replace(anchor_range.start..=anchor_range.end),
                    height: Some(1),
                    style: BlockStyle::Fixed,
                    render: std::sync::Arc::new(move |cx: &mut BlockContext| {
                        let color = style.load().table_border_color;
                        render_horizontal_rule(cx, color)
                    }),
                    priority: 0,
                }
            })
            .collect();
        let ids = editor.insert_blocks(new_blocks, None, cx);
        kept.extend(new_ranges.into_iter().zip(ids));
    }

    if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
        addon.table_dividers = kept;
    }
}

/// Computes the column-alignment spacer folds for every table in `computed`.
/// Not an `apply_*` function like its siblings -- it just returns fold
/// entries to fold into `refresh`'s own `folds` list, so they go through
/// `apply_folds`'s existing diffing (`VisualMdAddon::folded_markers`) instead
/// of needing a dedicated addon field of their own.
///
/// For each column, every cell's *trimmed* content (`TableCell::content`) is
/// shaped at the prose font (`ThemeSettings::ui_font`/`ui_font_size` -- the
/// same font M7's `KEY_PROSE_FONT` gives table text, since cells are prose,
/// not code) to find its rendered pixel width; the column's widest cell sets
/// the target every other cell in that column pads out to. A cell that's
/// already the widest (or the only one) gets no spacer at all. Which side
/// gets widened follows the column's alignment: trailing (left-align,
/// default), leading (right-align), or both, split evenly (center). A gap
/// that's empty -- no existing whitespace there in the source to widen --
/// is skipped for that side rather than guessed at; see `TableCell`'s own
/// doc comment.
fn table_alignment_spacer_folds(
    computed: &Plan,
    text: &str,
    style: &ResolvedStyle,
    window: &Window,
    cx: &mut Context<Editor>,
) -> Vec<(Range<usize>, String, editor::FoldPlaceholder)> {
    if computed.tables.is_empty() {
        return Vec::new();
    }

    let (font, font_size) = {
        let settings = theme_settings::ThemeSettings::get_global(cx);
        let mut font = settings.ui_font.clone();
        font.family = SharedString::from(style.prose_font_family.as_str());
        if let Some(weight) = style.prose_font_weight {
            font.weight = weight;
        }
        (
            font,
            style
                .prose_font_size
                .unwrap_or_else(|| settings.ui_font_size(cx)),
        )
    };
    let measure = |range: &Range<usize>| -> f32 {
        let Some(cell_text) = text.get(range.clone()) else {
            return 0.0;
        };
        if cell_text.is_empty() {
            return 0.0;
        }
        let run = TextRun {
            len: cell_text.len(),
            font: font.clone(),
            color: black(),
            background_color: None,
            underline: None,
            strikethrough: None,
            font_size: None,
        };
        let shaped = window.text_system().shape_line(
            SharedString::from(cell_text.to_string()),
            font_size,
            &[run],
            None,
        );
        f32::from(shaped.width)
    };

    let mut folds = Vec::new();
    for table in &computed.tables {
        let num_cols = table.alignments.len();
        if num_cols == 0 {
            continue;
        }
        let mut widths: Vec<Vec<f32>> = Vec::with_capacity(table.rows.len());
        let mut col_max = vec![0.0f32; num_cols];
        for row in &table.rows {
            let row_widths: Vec<f32> = row.iter().map(|cell| measure(&cell.content)).collect();
            for (col, &w) in row_widths.iter().enumerate() {
                if let Some(max) = col_max.get_mut(col) {
                    *max = max.max(w);
                }
            }
            widths.push(row_widths);
        }

        for (row, row_widths) in table.rows.iter().zip(widths.iter()) {
            for (col, (cell, &width)) in row.iter().zip(row_widths.iter()).enumerate() {
                let Some(&max_width) = col_max.get(col) else {
                    continue;
                };
                let deficit = max_width - width;
                // Sub-pixel deficits aren't worth a fold (and would just
                // churn the diff every refresh from float jitter).
                if deficit < 1.0 {
                    continue;
                }
                let alignment = table
                    .alignments
                    .get(col)
                    .copied()
                    .unwrap_or(TableAlignment::Default);
                let mut push_spacer = |gap: &Range<usize>, width: f32| {
                    if gap.is_empty() || width < 1.0 {
                        return;
                    }
                    folds.push((
                        gap.clone(),
                        format!("table_spacer:{:.1}", width),
                        table_spacer_placeholder(px(width)),
                    ));
                };
                match alignment {
                    TableAlignment::Right => push_spacer(&cell.leading_gap, deficit),
                    TableAlignment::Center => {
                        let half = deficit / 2.0;
                        push_spacer(&cell.leading_gap, half);
                        push_spacer(&cell.trailing_gap, deficit - half);
                    }
                    TableAlignment::Left | TableAlignment::Default => {
                        push_spacer(&cell.trailing_gap, deficit)
                    }
                }
            }
        }
    }
    folds
}

/// An invisible fixed-width spacer, used to pad a table cell's existing
/// whitespace out to its column's width -- same "arbitrary computed-width
/// `div()` in a fold's render closure" technique `bullet_placeholder`/
/// `blockquote_bar_placeholder` already use for a fixed constant, just with
/// a width computed per-instance from real text measurement instead.
fn table_spacer_placeholder(width: Pixels) -> editor::FoldPlaceholder {
    editor::FoldPlaceholder {
        render: std::sync::Arc::new(move |_, _, _| div().w(width).h_full().into_any_element()),
        collapsed_text: Some(SharedString::from(" ")),
        ..base_placeholder()
    }
}

/// Diffs `computed.code_fence_borders` against the blocks inserted by the
/// previous refresh -- same shape as `apply_horizontal_rules`, keyed on
/// `(range, language)` together instead of range alone for the same reason
/// `apply_folds` diffs folds on `(range, key)`: editing a fence's info
/// string changes its language without moving the border's byte range, and
/// that edit needs to recreate the block with the new chip text rather than
/// being mistaken for "unchanged".
fn apply_code_fence_borders(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    computed: &Plan,
    style: Arc<ArcSwap<ResolvedStyle>>,
    cx: &mut Context<Editor>,
) {
    let previous = editor
        .addon_mut::<VisualMdAddon>()
        .map(|addon| std::mem::take(&mut addon.code_fence_borders))
        .unwrap_or_default();

    let wanted: HashSet<(Range<usize>, Option<String>)> =
        computed.code_fence_borders.iter().cloned().collect();
    let mut kept = Vec::new();
    let mut stale_ids: collections::HashSet<CustomBlockId> = collections::HashSet::default();
    for (range, language, id) in previous {
        if wanted.contains(&(range.clone(), language.clone())) {
            kept.push((range, language, id));
        } else {
            stale_ids.insert(id);
        }
    }
    if !stale_ids.is_empty() {
        editor.remove_blocks(stale_ids, None, cx);
    }

    let already_kept: HashSet<(Range<usize>, Option<String>)> = kept
        .iter()
        .map(|(range, language, _)| (range.clone(), language.clone()))
        .collect();
    let new_entries: Vec<(Range<usize>, Option<String>)> = computed
        .code_fence_borders
        .iter()
        .filter(|entry| !already_kept.contains(*entry))
        .cloned()
        .collect();

    if !new_entries.is_empty() {
        let new_blocks: Vec<BlockProperties<Anchor>> = new_entries
            .iter()
            .map(|(range, language)| {
                let anchor_range = to_anchor_range(snapshot, range);
                let language = language.clone();
                let style = style.clone();
                BlockProperties {
                    placement: BlockPlacement::Replace(anchor_range.start..=anchor_range.end),
                    height: Some(1),
                    style: BlockStyle::Fixed,
                    render: Arc::new(move |cx: &mut BlockContext| {
                        let line_color = style.load().code_block_border_color;
                        render_code_fence_border(cx, language.clone(), line_color)
                    }),
                    priority: 0,
                }
            })
            .collect();
        let ids = editor.insert_blocks(new_blocks, None, cx);
        kept.extend(
            new_entries
                .into_iter()
                .zip(ids)
                .map(|((range, language), id)| (range, language, id)),
        );
    }

    if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
        addon.code_fence_borders = kept;
    }
}

/// Renders a fenced code block's fence line the same way
/// `render_horizontal_rule` renders a `---`: a full-width thin border using
/// `BlockContext::max_width`, plus (only on the opening line, when a
/// language was recognized in the info string) a small text chip.
fn render_code_fence_border(
    cx: &mut BlockContext,
    language: Option<String>,
    line_color: Hsla,
) -> AnyElement {
    let colors = {
        use theme::ActiveTheme;
        cx.theme().colors()
    };
    div()
        .w(cx.max_width)
        .h(cx.line_height)
        .flex()
        .items_center()
        .gap_2()
        .when_some(language, |row, language| {
            row.child(
                div()
                    .px_1()
                    .rounded_sm()
                    .bg(colors.surface_background)
                    .text_color(colors.text_muted)
                    .text_xs()
                    .child(language),
            )
        })
        .child(div().flex_1().h(px(1.)).bg(line_color))
        .into_any_element()
}

/// Kicks off (and caches) `LanguageRegistry` resolution for every fenced
/// code block's language name that isn't already cached or in flight.
/// Resolution is inherently async — grammars load/compile lazily — so this
/// only *starts* the load; `apply_code_syntax_highlights` picks up whatever
/// is already cached by the time it runs, and the load's own completion
/// callback triggers a fresh `refresh` so a block's highlighting appears the
/// moment its language finishes loading rather than waiting on the next
/// edit or scroll.
fn ensure_code_languages_loaded(
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut Context<Editor>,
    names: HashSet<String>,
) {
    // Prefer the buffer's own registry (works even for a bare buffer with no
    // project attached -- the same setup
    // `test_move_to_enclosing_bracket_in_markdown_code_block` in
    // `crates/editor/src/editor_tests.rs` uses); fall back to the project's.
    let registry = editor
        .buffer()
        .read(cx)
        .as_singleton()
        .and_then(|buffer| buffer.read(cx).language_registry())
        .or_else(|| {
            editor
                .project()
                .map(|project| project.read(cx).languages().clone())
        });

    for name in names {
        let already_known = editor.addon::<VisualMdAddon>().is_some_and(|addon| {
            addon.code_languages.contains_key(&name)
                || addon.pending_language_tasks.contains_key(&name)
        });
        if already_known {
            continue;
        }

        let Some(registry) = registry.clone() else {
            // No registry at all (no project, and the buffer never had one
            // set) -- there's nothing to resolve against, ever, so cache
            // `None` immediately rather than silently retrying every
            // refresh.
            if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
                addon.code_languages.insert(name, None);
            }
            continue;
        };

        let task = cx.spawn_in(window, {
            let name = name.clone();
            async move |editor, cx| {
                let language = registry.language_for_name_or_extension(&name).await.ok();
                editor
                    .update_in(cx, |editor, window, cx| {
                        if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
                            addon.code_languages.insert(name.clone(), language);
                            addon.pending_language_tasks.remove(&name);
                        }
                        force_refresh(editor, window, cx);
                    })
                    .ok();
            }
        });

        if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
            addon.pending_language_tasks.insert(name, task);
        }
    }
}

/// For every fenced code block whose language has already resolved (per
/// `ensure_code_languages_loaded`'s cache), runs the language's own
/// tree-sitter highlighter over its content and applies the result via
/// `highlight_text_key` -- one key per distinct `HighlightId` actually
/// present, using its numeric value directly as the key (bounded by however
/// many named highlight categories the current theme defines, `HighlightId`
/// being just an index into `SyntaxTheme::highlights`) rather than the small
/// fixed `KEY_*` constants the rest of this file uses, via its own key variant
/// (`HighlightKey::VisualMdCodeSyntax`) so the two key spaces can never collide.
/// Diffed against the previous refresh's active id set so a refresh only
/// touches however many distinct syntax categories are actually in view,
/// not the theme's whole catalog.
fn apply_code_syntax_highlights(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    text: &str,
    computed: &Plan,
    cx: &mut Context<Editor>,
) {
    let syntax_theme = {
        use theme::ActiveTheme;
        cx.theme().syntax().clone()
    };
    let mut ranges_by_id: HashMap<usize, Vec<Range<usize>>> = HashMap::new();
    for (content_range, language_name) in &computed.code_fence_content {
        let Some(name) = language_name else { continue };
        let Some(language) = editor
            .addon::<VisualMdAddon>()
            .and_then(|addon| addon.code_languages.get(name))
            .cloned()
            .flatten()
        else {
            continue;
        };
        let Some(content_text) = text.get(content_range.clone()) else {
            continue;
        };
        let rope = Rope::from(content_text);
        for (local_range, highlight_id) in language.highlight_text(&rope, 0..content_text.len()) {
            ranges_by_id
                .entry(usize::from(highlight_id))
                .or_default()
                .push(
                    local_range.start + content_range.start..local_range.end + content_range.start,
                );
        }
    }

    let active_ids: HashSet<usize> = ranges_by_id.keys().copied().collect();
    let previous_ids = editor
        .addon_mut::<VisualMdAddon>()
        .map(|addon| std::mem::replace(&mut addon.active_syntax_ids, active_ids.clone()))
        .unwrap_or_default();

    for id in previous_ids.difference(&active_ids) {
        editor.highlight_text_key(
            HighlightKey::VisualMdCodeSyntax(*id),
            Vec::new(),
            HighlightStyle::default(),
            false,
            cx,
        );
    }
    for (id, ranges) in &ranges_by_id {
        let Some(style) = syntax_theme.get(*id).copied() else {
            continue;
        };
        let anchor_ranges = ranges
            .iter()
            .map(|range| to_anchor_range(snapshot, range))
            .collect();
        editor.highlight_text_key(
            HighlightKey::VisualMdCodeSyntax(*id),
            anchor_ranges,
            style,
            false,
            cx,
        );
    }
}

fn apply_style_highlights(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    computed: &Plan,
    enabled: bool,
    style: &ResolvedStyle,
    cx: &mut Context<Editor>,
) {
    let spans_of = |span_style: SpanStyle| -> Vec<Range<usize>> {
        computed
            .styled_spans
            .iter()
            .filter(|(_, candidate)| *candidate == span_style)
            .map(|(range, _)| range.clone())
            .collect()
    };

    // Markdown prose is shown in `ui_font`, the proportional font Zed's own UI
    // chrome uses, unless a prose font is configured. The layer covers the
    // *whole buffer*, not just the viewport: it is a single O(1) highlight
    // entry regardless of document size (unlike the tree-walked per-construct
    // decorations below), so there's no perf reason to scope it, and doing so
    // would risk a font flicker right at the viewport boundary while scrolling.
    let prose_ranges: Vec<Range<Anchor>> = if enabled {
        vec![snapshot.anchor_before(MultiBufferOffset(0))..snapshot.anchor_after(snapshot.len())]
    } else {
        Vec::new()
    };
    set_visual_md_highlight(
        editor,
        KEY_PROSE_FONT,
        prose_ranges,
        HighlightStyle {
            font_family: Some(style.prose_font_family),
            font_weight: style.prose_font_weight,
            ..Default::default()
        },
        cx,
    );

    let inline_code = spans_of(SpanStyle::InlineCode);
    let code_block_content: Vec<Range<usize>> = computed
        .code_fence_content
        .iter()
        .map(|(range, _)| range.clone())
        .collect();
    let code_weight_ranges = if style.code_font_weight.is_some() {
        inline_code
            .iter()
            .chain(&code_block_content)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    set_visual_md_highlight(
        editor,
        KEY_CODE_WEIGHT,
        to_anchor_ranges(snapshot, &code_weight_ranges),
        HighlightStyle {
            font_weight: style.code_font_weight,
            ..Default::default()
        },
        cx,
    );

    set_visual_md_highlight(
        editor,
        KEY_DIMMED_MARKER,
        to_anchor_ranges(snapshot, &computed.dimmed_markers),
        style.dim_marker_style(),
        cx,
    );

    // Zed's own tree-sitter-based Markdown syntax theme already colors
    // headings/bold/italic/strikethrough distinctly (that's a separate layer
    // from these `highlight_text` calls, driven by the buffer's language
    // grammar). Per explicit product direction, matching the Obsidian
    // live-preview reference, where these render in the same color as
    // surrounding prose, a `None` color here would leave that underlying
    // syntax color showing through unblended. Pinning to the editor's normal
    // foreground color (the default for each of these) is what cancels it out.
    for level in 1..=6u8 {
        set_visual_md_highlight(
            editor,
            heading_key(level),
            to_anchor_ranges(snapshot, &spans_of(SpanStyle::Heading(level))),
            style.heading_style(level),
            cx,
        );
    }

    set_visual_md_highlight(
        editor,
        KEY_BOLD,
        to_anchor_ranges(snapshot, &spans_of(SpanStyle::Bold)),
        style.bold_style(),
        cx,
    );
    set_visual_md_highlight(
        editor,
        KEY_ITALIC,
        to_anchor_ranges(snapshot, &spans_of(SpanStyle::Italic)),
        style.italic_style(),
        cx,
    );
    set_visual_md_highlight(
        editor,
        KEY_STRIKETHROUGH,
        to_anchor_ranges(snapshot, &spans_of(SpanStyle::Strikethrough)),
        style.strikethrough_style(),
        cx,
    );
    // Links are the one deliberate, narrow exception to "no added color": color
    // is a link's only non-structural cue (no weight/slant distinguishes it the
    // way bold/italic have their own), so by default a real link is colored
    // using `link_text_hover`, the same theme token Zed's own generic cmd+hover
    // link highlight already uses (`crates/editor/src/hover_links.rs`).
    set_visual_md_highlight(
        editor,
        KEY_LINK,
        to_anchor_ranges(snapshot, &spans_of(SpanStyle::Link)),
        style.link_style(),
        cx,
    );

    apply_callout_backgrounds(editor, snapshot, computed, style, cx);

    // Highlights, inline code and code blocks carry no color or background
    // unless a setting or theme token supplies one: the live-preview reference
    // (Obsidian) leaves them untinted, so only the structural styling
    // (the code font and size) applies by default.
    let highlight_ranges = if style.highlight_background.is_some() {
        spans_of(SpanStyle::Highlight)
    } else {
        Vec::new()
    };
    set_visual_md_highlight(
        editor,
        KEY_HIGHLIGHT,
        to_anchor_ranges(snapshot, &highlight_ranges),
        HighlightStyle {
            background_color: style.highlight_background,
            ..Default::default()
        },
        cx,
    );
    set_visual_md_highlight(
        editor,
        KEY_INLINE_CODE,
        to_anchor_ranges(snapshot, &inline_code),
        style.inline_code_style(),
        cx,
    );
    set_visual_md_highlight(
        editor,
        KEY_CODE_BLOCK,
        to_anchor_ranges(snapshot, &code_block_content),
        style.code_block_style(),
        cx,
    );
}

/// Callout boxes (M11) are a second deliberate color exception alongside
/// links: a callout's whole purpose is visually standing out. The background
/// covers the callout's *whole* node range (title row included). There is one
/// key per distinct type name in view, built-in kinds first, so a callout
/// nested in another resolves in the same order it always has.
fn apply_callout_backgrounds(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    computed: &Plan,
    style: &ResolvedStyle,
    cx: &mut Context<Editor>,
) {
    let mut groups: BTreeMap<(usize, String), (Hsla, Vec<Range<usize>>)> = BTreeMap::new();
    for callout in &computed.callouts {
        let look = style.callout_look(callout.kind, &callout.raw_type_name);
        groups
            .entry((callout.kind as usize, callout.raw_type_name.to_lowercase()))
            .or_insert_with(|| (look.background, Vec::new()))
            .1
            .push(callout.node_range.clone());
    }

    let used_keys = groups.len();
    for (index, (background, ranges)) in groups.into_values().enumerate() {
        set_visual_md_highlight(
            editor,
            KEY_CALLOUT_FIRST + index,
            to_anchor_ranges(snapshot, &ranges),
            HighlightStyle {
                background_color: Some(background),
                ..HighlightStyle::default()
            },
            cx,
        );
    }

    let previous_keys = editor
        .addon_mut::<VisualMdAddon>()
        .map(|addon| std::mem::replace(&mut addon.callout_key_count, used_keys))
        .unwrap_or(0);
    for index in used_keys..previous_keys {
        set_visual_md_highlight(
            editor,
            KEY_CALLOUT_FIRST + index,
            Vec::new(),
            HighlightStyle::default(),
            cx,
        );
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use editor::test::editor_test_context::EditorTestContext;
    use gpui::{FontStyle, StrikethroughStyle, TestAppContext, rgb};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            // Registers the real `cx.observe_new::<Editor>` wiring so
            // `VisualMdAddon` actually exists on the test editor and
            // `apply_folds`'s diffing has real previous-state to diff
            // against (M3's viewport-scoping tests read
            // `VisualMdAddon::folded_markers` directly). Harmless for tests
            // that call `refresh` directly without ever inspecting the
            // addon: `refresh` is idempotent, and this only makes those
            // tests more representative of the real registration path
            // rather than changing what they assert.
            init(cx);
        });
    }

    fn markdown_language() -> std::sync::Arc<language::Language> {
        std::sync::Arc::new(language::Language::new(
            language::LanguageConfig {
                name: "Markdown".into(),
                ..Default::default()
            },
            None,
        ))
    }

    /// Reproduces a real crash: folding the M1 test fixture (headings plus
    /// every inline construct) panicked deep in `tab_map.rs` with "attempt
    /// to subtract with overflow", even though the *planner's* output was
    /// already verified overlap-free (see `plan::tests::no_hidden_range_ever_overlaps_another`).
    /// This test exists to catch that class of bug directly against the
    /// real `Editor`/fold machinery, in seconds, instead of only via a full
    /// GUI relaunch.
    #[gpui::test]
    async fn folding_the_full_test_fixture_does_not_panic(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state(concat!(
            "ˇ# Heading One\n\n",
            "Some regular paragraph text that should render completely unstyled by visual_md.\n\n",
            "Some **bold**, *italic*, ***both***, ~~strike~~, ==highlight==, and `code`.\n\n",
            "## Heading Two\n\n",
            "Not a heading: this line starts with a hash but no space:\n#nope\n",
        ));
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });
    }

    /// Same class of regression as the M1 test above, covering M2's new
    /// fold-inducing constructs (list bullets/ordinals, task checkboxes,
    /// blockquote/callout bars) against the real `Editor`/fold machinery —
    /// exactly where the M1 crashes actually surfaced, not caught by the
    /// pure planner's own tests.
    #[gpui::test]
    async fn folding_lists_tasks_and_callouts_does_not_panic(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        // Headings interspersed with lists is deliberate, not incidental:
        // it's exactly the mix that surfaced a real bug — `insert_creases`
        // requires its input sorted by position (its underlying sum-tree
        // cursor can only seek forward and panics "cannot seek backward"
        // otherwise), and concatenating hidden_markers (from headings) with
        // glyph_markers (from lists) as whole groups, rather than merging
        // them by position, produced exactly this out-of-order input the
        // moment a heading and a list traded places in the document like
        // they do here.
        cx.set_state(concat!(
            "ˇ# M2 test\n\n",
            "## Lists\n\n",
            "1. one\n1. two\n1. three\n\n",
            "## Tasks\n\n",
            "- [ ] unchecked\n- [x] checked\n- plain item\n\n",
            "## Quotes\n\n",
            "> [!warning] Careful\n> multi-line body with **bold** text\n\n",
            "> plain quote\n> > nested quote\n",
        ));
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });
    }

    /// The checkbox click handler edits the buffer with a same-length
    /// `[ ]`/`[x]` replacement at the marker's anchor range; this exercises
    /// that same edit path directly (real click/mouse-event simulation isn't
    /// available in this harness) to confirm it round-trips correctly and
    /// that a follow-up refresh - reacting to the resulting `BufferEdited`
    /// event - doesn't panic either.
    ///
    /// This is also the regression test for two real M3-era bugs in the
    /// diffing path itself, both only observable once `VisualMdAddon` is
    /// actually registered (see `init_test`'s own comment): (1) `Addon`'s
    /// `to_any_mut` defaults to `None`, and this impl never overrode it, so
    /// `apply_folds` never actually had previous state to diff against —
    /// every refresh recreated every crease from scratch, unboundedly; (2)
    /// `apply_folds` called `unfold_ranges(.., inclusive: true, ..)` when
    /// removing the checkbox's now-stale crease, which also unfolds any
    /// fold merely *touching* that range's boundary — exactly the task
    /// item's own hidden bullet, which sits byte-for-byte adjacent to the
    /// checkbox. Toggling the checkbox was spuriously un-hiding the bullet
    /// (`"- ☑ task\n"` instead of `" ☑ task\n"`) even though its crease id
    /// was never marked stale. Fixed by switching to `inclusive: false`.
    #[gpui::test]
    async fn toggling_a_checkbox_edits_the_buffer_and_survives_refresh(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ- [ ] task\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });
        // A real bug lived here: the checkbox's `render` closure used to
        // read the editor's own live buffer content via `editor.read_with`
        // to determine checked state — a genuine re-entrant borrow (render
        // runs *during* the editor's own paint pass), which failed silently
        // rather than panicking, producing no visible glyph at all. Checking
        // `display_text` here, not just that nothing panics, is what catches
        // that class of bug.
        //
        // `display_text` reflects each fold's `collapsed_text`, not the
        // actual painted widget tree (the checkbox itself is a drawn
        // box/SVG now, not text — see `checkbox_placeholder`) — this is
        // still a real, valuable check that the fold exists with the right
        // checked state, just not a substitute for the real GUI screenshot
        // verification of the actual graphic.
        assert_eq!(cx.display_text(), " [ ] task\n");

        cx.update_editor(|editor, window, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let range = to_anchor_range(&snapshot, &(2..5));
            editor.edit([(range, "[x]")], cx);
            refresh(editor, window, cx);
        });
        cx.assert_editor_state("ˇ- [x] task\n");
        // The checked glyph must also update after the toggle: the
        // fold-diffing key includes checked state precisely so a same-length
        // `[ ]` -> `[x]` edit (whose byte range is unchanged) still gets its
        // crease recreated rather than being treated as "nothing to do".
        assert_eq!(cx.display_text(), " [x] task\n");
    }

    /// Diagnostic for a real report: in the actual app, list/checkbox/
    /// blockquote glyphs showed as Zed's own default "⋯" fold placeholder
    /// instead of this crate's intended glyphs, and copying returned raw
    /// source rather than even the fallback space/glyph text — suggesting
    /// the custom `render`/`collapsed_text` on these placeholders isn't
    /// taking effect. Checks the actual rendered `display_text` (not just
    /// "does it panic") to find out directly.
    #[gpui::test]
    async fn rendered_text_shows_bullet_glyph_not_default_ellipsis(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ- one\n- two\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });
        let displayed = cx.display_text();
        eprintln!("visual_md diagnostic: display_text={displayed:?}");
        assert!(
            displayed.contains('•'),
            "expected a bullet glyph in {displayed:?}"
        );
        assert!(
            !displayed.contains('⋯'),
            "found Zed's default fold ellipsis in {displayed:?}"
        );
    }

    /// Exercises links/autolinks (M6) through a real `refresh()`, not just
    /// the pure planner directly -- confirms `apply_style_highlights`
    /// actually applies `KEY_LINK` and that the fold-based hiding of the
    /// bracket/paren/angle-bracket syntax reaches the real display text.
    #[gpui::test]
    async fn link_and_autolink_hide_syntax_but_keep_text_visible(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇSee [Zed](https://zed.dev) or <https://zed.dev> for more.\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });

        let displayed = cx.display_text();
        assert!(
            displayed.contains("Zed"),
            "link text should stay visible in {displayed:?}"
        );
        assert!(
            displayed.contains("https://zed.dev"),
            "the bare autolink URL should stay visible in {displayed:?}"
        );
        assert!(
            !displayed.contains('['),
            "markdown link brackets should be hidden in {displayed:?}"
        );
        assert!(
            !displayed.contains('<'),
            "autolink angle brackets should be hidden in {displayed:?}"
        );
        assert!(
            !displayed.contains("(https://zed.dev)"),
            "the markdown link's own URL should be hidden, only its text kept, in {displayed:?}"
        );
    }

    /// Exercises the prose/code font split (M7) through a real `refresh()`:
    /// confirms `apply_style_highlights` actually reaches `highlight_text_key`
    /// with `KEY_PROSE_FONT`/`KEY_CODE_FONT`, using the same
    /// `all_text_highlights` test-support accessor `signature_help`'s own
    /// tests rely on for the equivalent inspection.
    #[gpui::test]
    async fn prose_and_inline_code_get_different_fonts(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇSome prose with `code` inside.\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);

            let (ui_font_family, buffer_font_family) = {
                let settings = theme_settings::ThemeSettings::get_global(cx);
                (settings.ui_font.family.clone(), settings.buffer_font.family.clone())
            };
            let highlights = editor.all_text_highlights(window, cx);

            assert!(
                highlights
                    .iter()
                    .any(|(style, ranges)| style.font_family.map(|family| family.as_str()) == Some(ui_font_family.as_ref()) && !ranges.is_empty()),
                "expected a highlight covering prose text in the UI font, got {highlights:?}"
            );
            assert!(
                highlights.iter().any(|(style, ranges)| style.font_family.map(|family| family.as_str()) == Some(buffer_font_family.as_ref())
                    && !ranges.is_empty()),
                "expected a highlight covering the inline code span in the buffer font, got {highlights:?}"
            );
        });
    }

    #[gpui::test]
    async fn horizontal_rule_inserts_exactly_one_block(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇtext above\n\n---\n\ntext below\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
            let hr_blocks = &editor.addon::<VisualMdAddon>().unwrap().hr_blocks;
            assert_eq!(
                hr_blocks.len(),
                1,
                "expected exactly one horizontal-rule block, got {hr_blocks:?}"
            );
        });
    }

    #[gpui::test]
    async fn standalone_image_line_inserts_a_block_only_while_untouched(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇtext above\n\n![alt](https://example.com/a.png)\n\ntext below\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
            let image_blocks = &editor.addon::<VisualMdAddon>().unwrap().image_blocks;
            assert_eq!(image_blocks.len(), 1, "got {image_blocks:?}");
            assert_eq!(image_blocks[0].1, "https://example.com/a.png");
        });

        cx.set_state("text above\n\n![alˇt](https://example.com/a.png)\n\ntext below\n");
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
            let image_blocks = &editor.addon::<VisualMdAddon>().unwrap().image_blocks;
            assert!(image_blocks.is_empty(), "got {image_blocks:?}");
        });
    }

    /// A thematic break the cursor is touching should not become a block at
    /// all (its raw `---` shows through instead, matching the untouched-vs-
    /// touched convention every other construct follows).
    #[gpui::test]
    async fn touched_horizontal_rule_is_not_blocked(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ---\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
            let hr_blocks = &editor.addon::<VisualMdAddon>().unwrap().hr_blocks;
            assert!(
                hr_blocks.is_empty(),
                "a touched thematic break should not be blocked, got {hr_blocks:?}"
            );
        });
    }

    /// Exercises the fenced-code-block border/chip block path (M8) end to
    /// end: confirms `apply_code_fence_borders` reaches `insert_blocks`
    /// through a real `refresh()`, same established pattern as the
    /// horizontal-rule block test above.
    #[gpui::test]
    async fn fenced_code_block_inserts_two_border_blocks(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇtext above\n\n```rust\nfn main() {}\n```\n\ntext below\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
            let borders = &editor.addon::<VisualMdAddon>().unwrap().code_fence_borders;
            assert_eq!(
                borders.len(),
                2,
                "expected an opening and a closing border block, got {borders:?}"
            );
            assert!(
                borders
                    .iter()
                    .any(|(_, language, _)| language.as_deref() == Some("rust")),
                "the opening border should carry the language name, got {borders:?}"
            );
        });
    }

    /// A fence line the cursor is touching should not become a border block
    /// (its raw ` ``` ` shows through instead), matching the same
    /// untouched-vs-touched convention the horizontal rule follows.
    #[gpui::test]
    async fn touched_fence_line_drops_its_border_block(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ```rust\nfn main() {}\n```\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
            let borders = &editor.addon::<VisualMdAddon>().unwrap().code_fence_borders;
            assert_eq!(
                borders.len(),
                1,
                "only the untouched closing line should be blocked, got {borders:?}"
            );
            assert!(
                borders[0].1.is_none(),
                "the closing border never carries a language, got {borders:?}"
            );
        });
    }

    /// Exercises the table decoration path (M9) end to end: confirms pipes
    /// fold to bars (through the ordinary `folded_markers`/`apply_folds`
    /// pipeline, via `GlyphKind::TablePipe`) and the delimiter row becomes a
    /// divider block (`apply_table_dividers`, mirroring the horizontal-rule
    /// test above), through a real `refresh()`.
    #[gpui::test]
    async fn table_pipes_fold_and_delimiter_becomes_a_block(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ| Name | Role |\n|------|-----:|\n| Ada  | Dev  |\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
            let addon = editor.addon::<VisualMdAddon>().unwrap();
            let pipe_count = addon
                .folded_markers
                .iter()
                .filter(|(_, key, _)| key == "table_pipe")
                .count();
            assert_eq!(
                pipe_count, 6,
                "3 pipes per row * 2 rows, got {:?}",
                addon.folded_markers
            );
            assert_eq!(
                addon.table_dividers.len(),
                1,
                "expected one delimiter-row divider block"
            );
        });
    }

    /// A delimiter row the cursor is touching should not become a block
    /// (its raw `|---|---:|` shows through instead), matching the same
    /// untouched-vs-touched convention the horizontal rule and fence borders
    /// follow.
    #[gpui::test]
    async fn touched_delimiter_row_drops_its_divider_block(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("| Name | Role |\nˇ|------|-----:|\n| Ada  | Dev  |\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
            let table_dividers = &editor.addon::<VisualMdAddon>().unwrap().table_dividers;
            assert!(
                table_dividers.is_empty(),
                "a touched delimiter row should not be blocked, got {table_dividers:?}"
            );
        });
    }

    /// The core alignment mechanism: a column whose cells differ in width
    /// should fold a spacer into every *shorter* cell's existing whitespace
    /// (proving real text measurement drives this, not a fixed guess), and
    /// leave the widest cell in that column alone.
    #[gpui::test]
    async fn shorter_cell_in_a_column_gets_an_alignment_spacer(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        // "Alexandria" is the widest cell in the column, including the
        // header -- it alone should get no spacer; "Name" and "A" both
        // should.
        cx.set_state("ˇ| Name |\n|------|\n| A    |\n| Alexandria |\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
            let addon = editor.addon::<VisualMdAddon>().unwrap();
            let spacer_count = addon.folded_markers.iter().filter(|(_, key, _)| key.starts_with("table_spacer:")).count();
            assert_eq!(
                spacer_count, 2,
                "'Name' and 'A' should each get a spacer, 'Alexandria' (the widest) should not, got {:?}",
                addon.folded_markers
            );
        });
    }

    /// A minimally-spaced table (no whitespace around any cell's content at
    /// all) has no existing gap bytes to widen -- this should degrade
    /// gracefully (no spacers, no panic) rather than error.
    #[gpui::test]
    async fn minimally_spaced_table_skips_padding_without_panicking(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ|A|B|\n|-|-|\n|1|22|\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
            let addon = editor.addon::<VisualMdAddon>().unwrap();
            let spacer_count = addon
                .folded_markers
                .iter()
                .filter(|(_, key, _)| key.starts_with("table_spacer:"))
                .count();
            assert_eq!(
                spacer_count, 0,
                "no source whitespace anywhere to widen, got {:?}",
                addon.folded_markers
            );
        });
    }

    /// With no `LanguageRegistry` attached at all (the default for a bare
    /// test buffer), an unresolvable language name should degrade cleanly:
    /// cached as `None` and never retried, no panic anywhere in the
    /// highlighting path.
    #[gpui::test]
    async fn unresolvable_language_is_cached_as_none_without_panicking(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ```not-a-real-language\ncode\n```\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
            let addon = editor.addon::<VisualMdAddon>().unwrap();
            assert_eq!(addon.code_languages.get("not-a-real-language"), Some(&None));
            assert!(addon.pending_language_tasks.is_empty());
        });
    }

    /// End-to-end through real async language resolution: attaches a test
    /// `LanguageRegistry` (`language::LanguageRegistry::test`, registered
    /// with `language::rust_lang()` -- the same test-support helper
    /// `crates/editor/src/editor_tests.rs`'s own markdown-code-block test
    /// uses) directly on the buffer (no `Project` needed, mirroring
    /// `test_move_to_enclosing_bracket_in_markdown_code_block`), lets the
    /// spawned load complete via `run_until_parked`, and confirms real
    /// syntax-highlight ranges reach `highlight_text_key`.
    #[gpui::test]
    async fn resolved_language_produces_real_syntax_highlighting(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ```rust\nfn main() {}\n```\n");
        let registry = std::sync::Arc::new(language::LanguageRegistry::test(cx.executor()));
        registry.add(language::rust_lang());
        // A freshly-constructed test registry has no theme wired in, so
        // every loaded grammar's `highlight_map` stays empty and
        // `highlight_text` returns nothing -- in the real app this happens
        // via a global theme-change observer; tests need to do it
        // explicitly.
        cx.update_editor(|_editor, _window, cx| {
            use theme::ActiveTheme;
            registry.set_theme(cx.theme().clone());
        });
        cx.update_buffer(|buffer, cx| {
            buffer.set_language_registry(registry);
            buffer.set_language(Some(markdown_language()), cx);
        });
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });
        // Let the spawned `language_for_name_or_extension` future (and the
        // `refresh` it triggers on completion) run to completion.
        cx.run_until_parked();

        cx.update_editor(|editor, window, cx| {
            assert!(
                editor.addon::<VisualMdAddon>().unwrap().code_languages.contains_key("rust"),
                "rust should have been resolved (or at least attempted) by now"
            );
            let highlights = editor.all_text_highlights(window, cx);
            let syntax_theme = {
                use theme::ActiveTheme;
                cx.theme().syntax().clone()
            };
            assert!(
                highlights.iter().any(|(style, ranges)| {
                    !ranges.is_empty()
                        && (0..)
                            .map_while(|ix: usize| syntax_theme.get(ix))
                            .any(|theme_style| theme_style == style)
                }),
                "expected at least one real syntax-theme-derived highlight from the rust fence's content"
            );
        });
    }

    /// Combines every construct into one document, unlike the other tests
    /// which each exercise one construct in isolation. Added while chasing a
    /// real user report of every fold rendering as Zed's default ellipsis —
    /// note that this test alone did *not* reproduce it even with matching
    /// content, confirming that bug was specific to the real GUI's paint
    /// pass and not reachable from this headless harness at all (see the
    /// commit message / memory notes for the actual root cause and fix).
    /// Kept anyway since combining constructs is still worth covering for
    /// its own sake.
    #[gpui::test]
    async fn folding_a_document_with_every_construct_combined_does_not_panic(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state(concat!(
            "ˇ# Heading level one\n\n",
            "Some plain paragraph text with **bold**, *italic*, and `inline code`.\n\n",
            "## Heading level two\n\n",
            "- [ ] an unchecked task\n",
            "- [x] a checked task\n",
            "- a plain bullet\n\n",
            "> [!warning] A callout\n",
            "> body text with **bold** inside it\n\n",
            "1. first item\n",
            "2. second item\n",
        ));
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });
        let displayed = cx.display_text();
        assert!(
            !displayed.contains('⋯'),
            "found Zed's default fold ellipsis in {displayed:?}"
        );
    }

    /// M3: `refresh` scopes decoration generation to the scrolled viewport
    /// (see `visible_byte_range`), so a construct far above the current
    /// scroll position (well beyond the overscan margin) should get no
    /// decorations at all, while one at the current scroll position still
    /// does. Reads `VisualMdAddon::folded_markers` directly (available since
    /// this test lives in the same crate) rather than inferring it from
    /// `display_text`, so it can name exactly which marker is/isn't present.
    #[gpui::test]
    async fn refresh_scopes_decorations_to_the_scrolled_viewport(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;

        let mut text = String::from("# Top Heading\n");
        for i in 0..600 {
            text.push_str(&format!("filler line number {i}\n"));
        }
        let bottom_heading_row = text.matches('\n').count() as u32;
        // The cursor sits on the line after the blank line below the heading:
        // this test is about viewport visibility, not selection-revealing, so
        // it deliberately keeps the cursor off the heading's own line.
        text.push_str("# Bottom Heading\n\nˇfiller line after bottom heading\n");

        cx.set_state(&text);
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        let (top_marker_start, bottom_marker_start) =
            (0usize, text.rfind("# Bottom Heading").unwrap());
        cx.update_editor(|editor, window, cx| {
            editor.set_scroll_position(
                gpui::Point::new(0.0, bottom_heading_row as f64),
                window,
                cx,
            );
            refresh(editor, window, cx);
            let folded = &editor.addon::<VisualMdAddon>().unwrap().folded_markers;
            assert!(
                !folded
                    .iter()
                    .any(|(range, _, _)| range.start == top_marker_start),
                "the offscreen top heading's marker should not have been folded: {folded:?}"
            );
            assert!(
                folded
                    .iter()
                    .any(|(range, _, _)| range.start == bottom_marker_start),
                "the on-screen bottom heading's marker should have been folded: {folded:?}"
            );
        });
    }

    /// Dragging a selection across text on one heading line (many selection
    /// changes, no edits) must leave the applied folds untouched, and moving
    /// the cursor off the heading must still re-hide its marker.
    #[gpui::test]
    async fn selection_changes_within_a_heading_line_keep_folds_stable(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;

        cx.set_state("# Heading text\n\nˇother paragraph\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        let folded_ids = |cx: &mut EditorTestContext| {
            cx.update_editor(|editor, _window, _cx| {
                editor
                    .addon::<VisualMdAddon>()
                    .unwrap()
                    .folded_markers
                    .iter()
                    .map(|(range, key, id)| (range.clone(), key.clone(), *id))
                    .collect::<Vec<_>>()
            })
        };
        let select = |cx: &mut EditorTestContext, range: Range<usize>| {
            cx.update_editor(|editor, window, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([
                        MultiBufferOffset(range.start)..MultiBufferOffset(range.end)
                    ]);
                });
            });
            // Selection refreshes are queued for the next frame.
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            cx.run_until_parked();
        };

        select(&mut cx, 4..4);
        let on_heading = folded_ids(&mut cx);
        assert!(
            !on_heading.iter().any(|(range, _, _)| range.start == 0),
            "the marker must be revealed while the cursor is on the heading line: {on_heading:?}"
        );
        for end in 5..=12 {
            select(&mut cx, 4..end);
        }
        assert_eq!(on_heading, folded_ids(&mut cx));

        select(&mut cx, 16..16);
        assert!(
            folded_ids(&mut cx)
                .iter()
                .any(|(range, _, _)| range.start == 0),
            "the marker must be hidden again once the cursor leaves the heading line"
        );
    }

    /// A scroll alone (no `refresh` call from the test) must still decorate
    /// newly visible content, and a scroll that stays inside the planned range
    /// must not replace the plan.
    #[gpui::test]
    async fn scroll_events_replan_only_when_leaving_the_planned_range(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;

        let mut text = String::from("# Top Heading\n");
        for i in 0..600 {
            text.push_str(&format!("filler line number {i}\n"));
        }
        let bottom_heading_row = text.matches('\n').count() as u32;
        text.push_str("# Bottom Heading\n\nˇfiller line after bottom heading\n");

        cx.set_state(&text);
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        // The scroll event's handler runs once `update_editor`'s closure has
        // returned, so each scroll and the read of its effect are separate calls.
        let planned = |cx: &mut EditorTestContext| {
            cx.update_editor(|editor, _window, _cx| {
                editor.addon::<VisualMdAddon>().unwrap().planned.clone()
            })
        };
        cx.update_editor(|editor, window, cx| {
            editor.set_scroll_position(gpui::Point::new(0.0, 1.0), window, cx);
        });
        let planned_before = planned(&mut cx);
        cx.update_editor(|editor, window, cx| {
            editor.set_scroll_position(gpui::Point::new(0.0, 2.0), window, cx);
        });
        assert!(planned_before.is_some());
        assert_eq!(planned_before, planned(&mut cx));

        let bottom_marker_start = text.rfind("# Bottom Heading").unwrap();
        cx.update_editor(|editor, window, cx| {
            editor.set_scroll_position(
                gpui::Point::new(0.0, bottom_heading_row as f64),
                window,
                cx,
            );
        });
        cx.update_editor(|editor, _window, _cx| {
            let folded = &editor.addon::<VisualMdAddon>().unwrap().folded_markers;
            assert!(
                folded
                    .iter()
                    .any(|(range, _, _)| range.start == bottom_marker_start),
                "scrolling alone should decorate the newly visible heading: {folded:?}"
            );
        });
    }

    /// M3 correctness invariant named directly in the milestone plan:
    /// decorations are view-only, so no sequence of refreshes driven purely
    /// by scrolling should ever change the underlying buffer text. This is
    /// the regression guard for that, exercised across several scroll
    /// positions on a real (viewport-scoped) `refresh` path.
    #[gpui::test]
    async fn scrolling_and_refreshing_never_mutates_the_buffer_text(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;

        let mut text = String::from("ˇ# Heading\n\nSome **bold** and *italic* text.\n\n");
        for i in 0..300 {
            text.push_str(&format!("- [ ] task number {i}\n"));
        }
        cx.set_state(&text);
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        let original_text =
            cx.update_editor(|editor, _window, cx| editor.buffer().read(cx).snapshot(cx).text());

        for scroll_row in [0.0, 50.0, 150.0, 280.0, 10.0] {
            cx.update_editor(|editor, window, cx| {
                editor.set_scroll_position(gpui::Point::new(0.0, scroll_row), window, cx);
                refresh(editor, window, cx);
            });
            let current_text = cx
                .update_editor(|editor, _window, cx| editor.buffer().read(cx).snapshot(cx).text());
            assert_eq!(
                current_text, original_text,
                "buffer text changed after scrolling to row {scroll_row} and refreshing"
            );
        }
    }

    /// A real user report: copying a multi-line selection through
    /// visual_md-decorated content (folded markers, bullet glyphs) was
    /// losing line breaks. `Editor::copy` reads straight from the
    /// underlying buffer rope (`text_for_range`), which has no knowledge of
    /// folds at all, so this exercises the one place visual_md's own
    /// decorations *could* plausibly interfere: whether a fold-inducing
    /// range ever swallows a `\n` byte (see
    /// `plan::no_fold_inducing_range_contains_a_newline_byte` for the same
    /// invariant checked directly against the planner's output) and whether
    /// a selection spanning decorated, multi-row content still round-trips
    /// through copy with every line break intact.
    #[gpui::test]
    async fn copying_a_multiline_selection_through_decorated_content_preserves_line_breaks(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state(concat!(
            "# Heading one\n",
            "Some «bold** text\n",
            "- [ ] taˇ»sk item\n",
            "Trailing paragraph.\n",
        ));
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            refresh(editor, window, cx);
        });

        cx.update_editor(|_editor, window, cx| {
            window.dispatch_action(Box::new(editor::actions::Copy), cx);
        });

        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text().as_deref().map(str::to_string))
            .expect("copy should have written text to the clipboard");
        assert_eq!(
            copied.matches('\n').count(),
            1,
            "expected exactly one line break in the copied text (selection spans two source \
             lines), got {copied:?}"
        );
        assert_eq!(copied, "bold** text\n- [ ] ta");
    }

    /// Exercises "smart list continuation" (see `list_continuation`) through
    /// the real `Newline` action dispatch, not just the pure planner
    /// function directly -- confirming `intercept_newline` is actually wired
    /// up ahead of `Editor::newline` via `editor.register_action`.
    #[gpui::test]
    async fn newline_continues_a_bullet_list(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("- oneˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(editor::actions::Newline);
        cx.assert_editor_state("- one\n- ˇ\n");
    }

    #[gpui::test]
    async fn newline_continues_an_ordered_list(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("1. oneˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(editor::actions::Newline);
        cx.assert_editor_state("1. one\n2. ˇ\n");
    }

    #[gpui::test]
    async fn newline_continues_a_task_item_unchecked(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("- [x] oneˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(editor::actions::Newline);
        cx.assert_editor_state("- [x] one\n- [ ] ˇ\n");
    }

    #[gpui::test]
    async fn newline_on_empty_item_exits_the_list(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("- one\n- ˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(editor::actions::Newline);
        cx.assert_editor_state("- one\nˇ\n");
    }

    #[gpui::test]
    async fn newline_on_empty_nested_item_outdents(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("- one\n  - nested\n  - ˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(editor::actions::Newline);
        cx.assert_editor_state("- one\n  - nested\n- ˇ\n");
    }

    /// Outside a list -- and in a non-Markdown buffer -- a plain `Enter`
    /// keeps behaving exactly like `Editor::newline` on its own: confirms
    /// `intercept_newline` genuinely falls through (`cx.propagate()`) rather
    /// than swallowing the action whenever it doesn't apply.
    #[gpui::test]
    async fn newline_is_unaffected_outside_lists(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("just a paragraphˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(editor::actions::Newline);
        cx.assert_editor_state("just a paragraph\nˇ\n");
    }

    #[gpui::test]
    async fn newline_is_unaffected_in_a_non_markdown_buffer(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("- oneˇ\n");
        cx.run_until_parked();

        cx.dispatch_action(editor::actions::Newline);
        cx.assert_editor_state("- one\nˇ\n");
    }

    /// Exercises `ToggleBold` (see `format_toggle`) through the real action
    /// dispatch, confirming `intercept_toggle_bold` is actually wired up via
    /// `editor.register_action` and applies `format_toggle::toggle`'s edits
    /// and selections correctly against a live buffer.
    #[gpui::test]
    async fn toggle_bold_wraps_a_selection(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("Hello «worldˇ» now\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(ToggleBold);
        cx.assert_editor_state("Hello **«worldˇ»** now\n");
    }

    #[gpui::test]
    async fn toggle_bold_twice_returns_to_the_original_text(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("Hello «worldˇ» now\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(ToggleBold);
        cx.run_until_parked();
        cx.assert_editor_state("Hello **«worldˇ»** now\n");

        cx.dispatch_action(ToggleBold);
        cx.assert_editor_state("Hello «worldˇ» now\n");
    }

    #[gpui::test]
    async fn toggle_bold_from_a_bare_cursor_inserts_then_removes_empty_markers(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("Helloˇ world\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(ToggleBold);
        cx.assert_editor_state("Hello**ˇ** world\n");

        cx.dispatch_action(ToggleBold);
        cx.assert_editor_state("Helloˇ world\n");
    }

    #[gpui::test]
    async fn toggle_italic_wraps_a_selection(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("Hello «worldˇ» now\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(ToggleItalic);
        cx.assert_editor_state("Hello *«worldˇ»* now\n");
    }

    /// One `editor.transact` per dispatch (see `intercept_toggle`) means a
    /// single `Undo` reverts the whole wrap in one step, not two separate
    /// marker-insertion edits.
    #[gpui::test]
    async fn undo_after_toggle_bold_reverts_in_one_step(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("Hello «worldˇ» now\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(ToggleBold);
        cx.assert_editor_state("Hello **«worldˇ»** now\n");

        cx.dispatch_action(editor::actions::Undo);
        cx.assert_editor_state("Hello «worldˇ» now\n");
    }

    /// Confirms `intercept_toggle` genuinely falls through (`cx.propagate()`)
    /// in a non-Markdown buffer rather than swallowing the action, the same
    /// invariant `newline_is_unaffected_in_a_non_markdown_buffer` checks for
    /// `intercept_newline`.
    #[gpui::test]
    async fn toggle_bold_does_nothing_in_a_non_markdown_buffer(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("Hello «worldˇ» now\n");
        cx.run_until_parked();

        cx.dispatch_action(ToggleBold);
        cx.assert_editor_state("Hello «worldˇ» now\n");
    }

    /// The keymap's `Editor && visual_md` bindings (`ToggleBold`/
    /// `ToggleItalic`) depend on `VisualMdAddon::extend_key_context` adding
    /// this key exactly when visual_md is actively decorating the buffer —
    /// confirms both the present and absent cases directly against a real
    /// `Editor::key_context`.
    #[gpui::test]
    async fn visual_md_key_context_is_present_on_markdown_and_absent_otherwise(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("Helloˇ world\n");
        cx.run_until_parked();

        let context_without_language = cx.update_editor(|editor, window, cx| {
            editor.key_context(window, cx).contains("visual_md")
        });
        assert!(!context_without_language);

        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        let context_with_markdown = cx.update_editor(|editor, window, cx| {
            editor.key_context(window, cx).contains("visual_md")
        });
        assert!(context_with_markdown);
    }

    #[derive(Debug, PartialEq)]
    struct PreviewState {
        active: bool,
        folded_markers: usize,
        highlight_keys: usize,
    }

    fn preview_state(cx: &mut EditorTestContext) -> PreviewState {
        cx.update_editor(|editor, window, cx| PreviewState {
            active: editor.key_context(window, cx).contains("visual_md"),
            folded_markers: editor
                .addon::<VisualMdAddon>()
                .map_or(0, |addon| addon.folded_markers.len()),
            highlight_keys: editor
                .addon::<VisualMdAddon>()
                .map_or(0, |addon| addon.nonempty_highlight_keys.len()),
        })
    }

    fn assert_preview_on(cx: &mut EditorTestContext) {
        let state = preview_state(cx);
        assert!(state.active, "{state:?}");
        assert!(state.folded_markers > 0, "{state:?}");
        assert!(state.highlight_keys > 0, "{state:?}");
    }

    fn assert_preview_off(cx: &mut EditorTestContext) {
        assert_eq!(
            preview_state(cx),
            PreviewState {
                active: false,
                folded_markers: 0,
                highlight_keys: 0,
            }
        );
    }

    async fn markdown_preview_context(cx: &mut TestAppContext) -> EditorTestContext {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("# Heading text\n\nˇother paragraph\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx
    }

    fn update_user_settings(
        cx: &mut EditorTestContext,
        update: impl FnOnce(&mut settings::SettingsContent),
    ) {
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, update);
        });
        cx.run_until_parked();
    }

    fn set_project_settings(cx: &mut EditorTestContext, content: Option<&str>) {
        let worktree_id = cx
            .update_buffer(|buffer, cx| buffer.file().map(|file| file.worktree_id(cx)))
            .expect("the test buffer should belong to a worktree");
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store
                .set_local_settings(
                    worktree_id,
                    settings::LocalSettingsPath::InWorktree(util::rel_path::RelPath::empty_arc()),
                    settings::LocalSettingsKind::Settings,
                    content,
                    cx,
                )
                .expect("project settings should load");
        });
        cx.run_until_parked();
    }

    fn visual_md_setting(enabled: bool) -> Option<settings::VisualMdSettingsContent> {
        Some(settings::VisualMdSettingsContent {
            enabled: Some(enabled),
            ..Default::default()
        })
    }

    #[gpui::test]
    async fn user_setting_toggles_live_preview_without_an_edit(cx: &mut TestAppContext) {
        let mut cx = markdown_preview_context(cx).await;
        assert_preview_on(&mut cx);

        update_user_settings(&mut cx, |content| {
            content.project.all_languages.defaults.visual_md = visual_md_setting(false);
        });
        assert_preview_off(&mut cx);

        update_user_settings(&mut cx, |content| {
            content.project.all_languages.defaults.visual_md = visual_md_setting(true);
        });
        assert_preview_on(&mut cx);
    }

    #[gpui::test]
    async fn project_setting_overrides_user_setting(cx: &mut TestAppContext) {
        let mut cx = markdown_preview_context(cx).await;

        set_project_settings(&mut cx, Some(r#"{"visual_md":{"enabled":false}}"#));
        assert_preview_off(&mut cx);

        // An emptied file rather than `None`: `SettingsStore::set_local_settings`
        // with no content drops the file but leaves its previously resolved
        // values in effect, so it cannot model "the override went away".
        set_project_settings(&mut cx, Some("{}"));
        assert_preview_on(&mut cx);

        update_user_settings(&mut cx, |content| {
            content.project.all_languages.defaults.visual_md = visual_md_setting(false);
        });
        assert_preview_off(&mut cx);

        set_project_settings(&mut cx, Some(r#"{"visual_md":{"enabled":true}}"#));
        assert_preview_on(&mut cx);

        set_project_settings(&mut cx, Some("{}"));
        assert_preview_off(&mut cx);
    }

    #[gpui::test]
    async fn language_setting_overrides_the_default(cx: &mut TestAppContext) {
        let mut cx = markdown_preview_context(cx).await;

        update_user_settings(&mut cx, |content| {
            content
                .languages_mut()
                .entry("Rust".to_string())
                .or_default()
                .visual_md = visual_md_setting(false);
        });
        assert_preview_on(&mut cx);

        update_user_settings(&mut cx, |content| {
            content
                .languages_mut()
                .entry("Markdown".to_string())
                .or_default()
                .visual_md = visual_md_setting(false);
        });
        assert_preview_off(&mut cx);

        update_user_settings(&mut cx, |content| {
            content
                .languages_mut()
                .entry("Markdown".to_string())
                .or_default()
                .visual_md = None;
        });
        assert_preview_on(&mut cx);

        set_project_settings(
            &mut cx,
            Some(r#"{"languages":{"Markdown":{"visual_md":{"enabled":false}}}}"#),
        );
        assert_preview_off(&mut cx);
    }

    #[gpui::test]
    async fn toggle_live_preview_overrides_one_editor_without_touching_settings(
        cx: &mut TestAppContext,
    ) {
        let mut cx = markdown_preview_context(cx).await;
        let override_state = |cx: &mut EditorTestContext| {
            cx.update_editor(|editor, _window, _cx| {
                editor
                    .addon::<VisualMdAddon>()
                    .and_then(|addon| addon.enabled_override)
            })
        };
        let setting = |cx: &mut EditorTestContext| {
            cx.update_editor(|editor, _window, cx| live_preview_setting(editor, cx))
        };

        cx.dispatch_action(ToggleLivePreview);
        cx.run_until_parked();
        assert_preview_off(&mut cx);
        assert_eq!(override_state(&mut cx), Some(false));
        assert!(setting(&mut cx), "toggling must not change the setting");

        cx.dispatch_action(ToggleLivePreview);
        cx.run_until_parked();
        assert_preview_on(&mut cx);
        assert_eq!(override_state(&mut cx), None);

        update_user_settings(&mut cx, |content| {
            content.project.all_languages.defaults.visual_md = visual_md_setting(false);
        });
        assert_preview_off(&mut cx);
        cx.dispatch_action(ToggleLivePreview);
        cx.run_until_parked();
        assert_preview_on(&mut cx);
        assert_eq!(override_state(&mut cx), Some(true));

        // Once the setting agrees with the override it must stop overriding,
        // or the next settings change would be masked.
        update_user_settings(&mut cx, |content| {
            content.project.all_languages.defaults.visual_md = visual_md_setting(true);
        });
        assert_preview_on(&mut cx);
        assert_eq!(override_state(&mut cx), None);
        update_user_settings(&mut cx, |content| {
            content.project.all_languages.defaults.visual_md = visual_md_setting(false);
        });
        assert_preview_off(&mut cx);
    }

    #[gpui::test]
    async fn toggle_live_preview_does_nothing_in_a_non_markdown_buffer(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("# Heading text\n\nˇother paragraph\n");
        cx.run_until_parked();

        cx.dispatch_action(ToggleLivePreview);
        cx.run_until_parked();

        assert_preview_off(&mut cx);
        let override_state = cx.update_editor(|editor, _window, _cx| {
            editor
                .addon::<VisualMdAddon>()
                .and_then(|addon| addon.enabled_override)
        });
        assert_eq!(override_state, None);
    }

    #[gpui::test]
    async fn toggle_bold_falls_through_while_live_preview_is_off(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("Hello «worldˇ» now\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.dispatch_action(ToggleLivePreview);
        cx.run_until_parked();
        cx.dispatch_action(ToggleBold);
        cx.assert_editor_state("Hello «worldˇ» now\n");

        cx.dispatch_action(ToggleLivePreview);
        cx.run_until_parked();
        cx.dispatch_action(ToggleBold);
        cx.assert_editor_state("Hello **«worldˇ»** now\n");
    }

    /// The style the editor paints at `offset`: visual_md's highlight keys
    /// merged in ascending order, a later key winning each field, exactly as
    /// `CustomHighlightsChunks` does. Asserting on the merged result rather
    /// than on individual keys keeps these golden tests valid when keys are
    /// renumbered.
    fn merged_visual_md_style(editor: &Editor, offset: usize, cx: &App) -> HighlightStyle {
        use editor::ToOffset as _;
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        (0..2048)
            .filter_map(|key| editor.text_highlights(HighlightKey::VisualMd(key), cx))
            .fold(HighlightStyle::default(), |merged, (style, ranges)| {
                let covers_offset = ranges.iter().any(|range| {
                    range.start.to_offset(&snapshot).0 <= offset
                        && offset < range.end.to_offset(&snapshot).0
                });
                if covers_offset {
                    merged.highlight(style)
                } else {
                    merged
                }
            })
    }

    const STYLING_FIXTURE: &str = "# One\n## Two\n### Three\n#### Four\n##### Five\n###### Six\n\n\
        plain **bold** *italic* ~~struck~~ [link text](https://example.com) `code` ==marked==\n\n\
        ```rust\nlet fenced = 1;\n```\n\n\
        > [!note]\n> note body\n\n\
        > [!tip]\n> tip body\n\n\
        > [!warning]\n> warning body\n\n\
        > [!danger]\n> danger body\n\n\
        > [!custom]\n> custom body\n\n\
        tailˇ\n";

    async fn styling_context(cx: &mut TestAppContext) -> EditorTestContext {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state(STYLING_FIXTURE);
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx
    }

    fn style_at(cx: &mut EditorTestContext, needle: &str) -> HighlightStyle {
        let offset = cx
            .buffer_text()
            .find(needle)
            .unwrap_or_else(|| panic!("fixture has no {needle:?}"));
        cx.update_editor(|editor, _window, cx| merged_visual_md_style(editor, offset, cx))
    }

    fn family_name(style: &HighlightStyle) -> Option<String> {
        style.font_family.map(|family| family.as_str().to_string())
    }

    #[gpui::test]
    async fn styling_of_prose_headings_and_emphasis_is_pinned(cx: &mut TestAppContext) {
        use theme::ActiveTheme;
        let mut cx = styling_context(cx).await;
        let (foreground, ui_family, buffer_family) = cx.update(|_window, cx| {
            let settings = theme_settings::ThemeSettings::get_global(cx);
            (
                cx.theme().colors().editor_foreground,
                settings.ui_font.family.to_string(),
                settings.buffer_font.family.to_string(),
            )
        });

        let plain = style_at(&mut cx, "plain");
        assert_eq!(family_name(&plain), Some(ui_family.clone()));
        assert_eq!(plain.color, None);
        assert_eq!(plain.font_weight, None);
        assert_eq!(plain.font_size_scale, None);

        for (needle, scale) in [
            ("One", 1.8),
            ("Two", 1.5),
            ("Three", 1.3),
            ("Four", 1.15),
            ("Five", 1.05),
            ("Six", 1.0),
        ] {
            let style = style_at(&mut cx, needle);
            assert_eq!(style.color, Some(foreground), "{needle}");
            assert_eq!(style.font_weight, Some(FontWeight::BOLD), "{needle}");
            assert_eq!(style.font_size_scale, Some(scale), "{needle}");
            assert_eq!(style.background_color, None, "{needle}");
            assert_eq!(family_name(&style), Some(ui_family.clone()), "{needle}");
        }

        let bold = style_at(&mut cx, "bold");
        assert_eq!(bold.color, Some(foreground));
        assert_eq!(bold.font_weight, Some(FontWeight::BOLD));
        assert_eq!(bold.font_style, None);
        assert_eq!(bold.font_size_scale, None);

        let italic = style_at(&mut cx, "italic");
        assert_eq!(italic.color, Some(foreground));
        assert_eq!(italic.font_style, Some(FontStyle::Italic));
        assert_eq!(italic.font_weight, None);

        let struck = style_at(&mut cx, "struck");
        assert_eq!(struck.color, Some(foreground));
        assert_eq!(
            struck.strikethrough,
            Some(StrikethroughStyle {
                thickness: px(1.),
                color: None,
            })
        );

        let link = style_at(&mut cx, "link text");
        assert_eq!(
            link.color,
            Some(cx.update(|_window, cx| cx.theme().colors().link_text_hover))
        );
        assert_eq!(link.font_weight, None);

        let inline_code = style_at(&mut cx, "code");
        assert_eq!(family_name(&inline_code), Some(buffer_family.clone()));
        assert_eq!(inline_code.color, None);
        assert_eq!(inline_code.background_color, None);
        assert_eq!(inline_code.font_size_scale, None);

        let marked = style_at(&mut cx, "marked");
        assert_eq!(marked.color, None);
        assert_eq!(marked.background_color, None);

        let fenced = style_at(&mut cx, "fenced");
        assert_eq!(family_name(&fenced), Some(buffer_family));
        assert_eq!(fenced.background_color, None);

        let has_text_style_refinement =
            cx.update_editor(|editor, _window, _cx| editor.text_style_refinement().is_some());
        assert!(!has_text_style_refinement);
    }

    #[gpui::test]
    async fn touched_marker_dims_to_a_fixed_gray(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("# Hˇeading\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();

        cx.update_editor(|editor, _window, cx| {
            let (style, ranges) = editor
                .text_highlights(HighlightKey::VisualMd(KEY_DIMMED_MARKER), cx)
                .expect("the touched heading marker should be dimmed");
            assert_eq!(style.color, Some(rgb(0x6b7280).into()));
            assert!(!ranges.is_empty());
        });
    }

    #[gpui::test]
    async fn callout_looks_are_pinned(cx: &mut TestAppContext) {
        use theme::ActiveTheme;
        let mut cx = styling_context(cx).await;
        let status = cx.update(|_window, cx| cx.theme().status().clone());

        for (needle, kind, icon, accent, background) in [
            (
                "note body",
                CalloutKind::Note,
                "icons/info.svg",
                status.info,
                status.info_background,
            ),
            (
                "tip body",
                CalloutKind::Tip,
                "icons/sparkle.svg",
                status.success,
                status.success_background,
            ),
            (
                "warning body",
                CalloutKind::Warning,
                "icons/warning.svg",
                status.warning,
                status.warning_background,
            ),
            (
                "danger body",
                CalloutKind::Danger,
                "icons/x_circle_filled.svg",
                status.error,
                status.error_background,
            ),
            (
                "custom body",
                CalloutKind::Other,
                "icons/quote.svg",
                status.hint,
                status.hint_background,
            ),
        ] {
            let type_name = needle.split(' ').next().unwrap_or_default();
            let look = cx.update(|_window, cx| {
                ResolvedStyle::resolve(&Default::default(), cx).callout_look(kind, type_name)
            });
            assert_eq!(
                (look.icon_path.as_ref(), look.accent, look.background),
                (icon, accent, background),
                "{needle}"
            );
            assert_eq!(
                style_at(&mut cx, needle).background_color,
                Some(background),
                "{needle}"
            );
        }
    }

    fn set_visual_md(cx: &mut EditorTestContext, visual_md: settings::VisualMdSettingsContent) {
        update_user_settings(cx, |content| {
            content.project.all_languages.defaults.visual_md = Some(visual_md);
        });
    }

    fn hex(color: &str) -> Hsla {
        theme::try_parse_color(color).expect("test colors are valid hex")
    }

    fn colors(colors: settings::VisualMdColorsContent) -> settings::VisualMdSettingsContent {
        settings::VisualMdSettingsContent {
            colors: Some(colors),
            ..Default::default()
        }
    }

    /// Replaces the active theme's `syntax` map with just these tokens.
    fn set_theme_tokens(cx: &mut EditorTestContext, tokens: Vec<(&str, HighlightStyle)>) {
        use theme::ActiveTheme as _;
        cx.update(|_window, cx| {
            let mut theme = (**cx.theme()).clone();
            theme.styles.syntax = Arc::new(theme::SyntaxTheme::new(
                tokens
                    .into_iter()
                    .map(|(name, style)| (name.to_string(), style)),
            ));
            theme::GlobalTheme::update_theme(cx, Arc::new(theme));
        });
        cx.run_until_parked();
    }

    fn token_color(color: &str) -> HighlightStyle {
        HighlightStyle {
            color: Some(hex(color)),
            ..Default::default()
        }
    }

    #[gpui::test]
    async fn font_settings_restyle_a_running_editor(cx: &mut TestAppContext) {
        let mut cx = styling_context(cx).await;

        set_visual_md(
            &mut cx,
            settings::VisualMdSettingsContent {
                prose_font_family: Some("Prose Font".to_string().into()),
                prose_font_weight: Some(settings::FontWeightContent(300.)),
                code_font_family: Some("Code Font".to_string().into()),
                heading_font_family: Some("Heading Font".to_string().into()),
                heading_sizes: Some(settings::VisualMdHeadingSizesContent {
                    h1: Some(settings::HeadingScale(2.5)),
                    ..Default::default()
                }),
                heading_weights: Some(settings::VisualMdHeadingWeightsContent {
                    h2: Some(settings::FontWeightContent(500.)),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );

        let plain = style_at(&mut cx, "plain");
        assert_eq!(family_name(&plain), Some("Prose Font".to_string()));
        assert_eq!(plain.font_weight, Some(FontWeight(300.)));

        let heading = style_at(&mut cx, "One");
        assert_eq!(family_name(&heading), Some("Heading Font".to_string()));
        assert_eq!(heading.font_size_scale, Some(2.5));
        assert_eq!(heading.font_weight, Some(FontWeight::BOLD));
        assert_eq!(style_at(&mut cx, "Two").font_weight, Some(FontWeight(500.)));
        assert_eq!(style_at(&mut cx, "Three").font_size_scale, Some(1.3));

        // Bold keeps its own weight over the prose weight, and code gets the
        // buffer font's weight back instead of the prose weight.
        assert_eq!(
            style_at(&mut cx, "bold").font_weight,
            Some(FontWeight::BOLD)
        );
        let buffer_weight = cx.update(|_window, cx| {
            theme_settings::ThemeSettings::get_global(cx)
                .buffer_font
                .weight
        });
        let inline_code = style_at(&mut cx, "code");
        assert_eq!(family_name(&inline_code), Some("Code Font".to_string()));
        assert_eq!(inline_code.font_weight, Some(buffer_weight));

        set_visual_md(&mut cx, Default::default());
        let plain = style_at(&mut cx, "plain");
        assert_eq!(plain.font_weight, None);
        assert_eq!(style_at(&mut cx, "One").font_size_scale, Some(1.8));
    }

    #[gpui::test]
    async fn color_settings_restyle_a_running_editor(cx: &mut TestAppContext) {
        let mut cx = styling_context(cx).await;

        set_visual_md(
            &mut cx,
            colors(settings::VisualMdColorsContent {
                heading: Some("#101010".into()),
                heading_2: Some("#202020".into()),
                bold: Some("#303030".into()),
                italic: Some("#404040".into()),
                strikethrough: Some("#505050".into()),
                link: Some("#606060".into()),
                inline_code: Some("#707070".into()),
                inline_code_background: Some("#80808080".into()),
                highlight_background: Some("#909090".into()),
                code_block_background: Some("#a0a0a0".into()),
                ..Default::default()
            }),
        );

        assert_eq!(style_at(&mut cx, "One").color, Some(hex("#101010")));
        assert_eq!(style_at(&mut cx, "Two").color, Some(hex("#202020")));
        assert_eq!(style_at(&mut cx, "Three").color, Some(hex("#101010")));
        assert_eq!(style_at(&mut cx, "bold").color, Some(hex("#303030")));
        assert_eq!(style_at(&mut cx, "italic").color, Some(hex("#404040")));
        assert_eq!(style_at(&mut cx, "struck").color, Some(hex("#505050")));
        assert_eq!(style_at(&mut cx, "link text").color, Some(hex("#606060")));
        let inline_code = style_at(&mut cx, "code");
        assert_eq!(inline_code.color, Some(hex("#707070")));
        assert_eq!(inline_code.background_color, Some(hex("#80808080")));
        assert_eq!(
            style_at(&mut cx, "marked").background_color,
            Some(hex("#909090"))
        );
        assert_eq!(
            style_at(&mut cx, "fenced").background_color,
            Some(hex("#a0a0a0"))
        );

        set_visual_md(&mut cx, Default::default());
        assert_eq!(style_at(&mut cx, "link text").color, {
            use theme::ActiveTheme as _;
            Some(cx.update(|_window, cx| cx.theme().colors().link_text_hover))
        });
        assert_eq!(style_at(&mut cx, "code").color, None);
        assert_eq!(style_at(&mut cx, "marked").background_color, None);
    }

    #[gpui::test]
    async fn invalid_color_values_are_ignored(cx: &mut TestAppContext) {
        use theme::ActiveTheme as _;
        let mut cx = styling_context(cx).await;
        let link_default = cx.update(|_window, cx| cx.theme().colors().link_text_hover);

        set_visual_md(
            &mut cx,
            colors(settings::VisualMdColorsContent {
                link: Some("not a color".into()),
                ..Default::default()
            }),
        );

        assert_eq!(style_at(&mut cx, "link text").color, Some(link_default));
    }

    #[gpui::test]
    async fn theme_tokens_restyle_an_editor_and_settings_beat_them(cx: &mut TestAppContext) {
        use theme::ActiveTheme as _;
        let mut cx = styling_context(cx).await;
        let link_default = cx.update(|_window, cx| cx.theme().colors().link_text_hover);
        assert_eq!(style_at(&mut cx, "link text").color, Some(link_default));

        set_theme_tokens(
            &mut cx,
            vec![
                ("visual_md.link", token_color("#0000ff")),
                ("visual_md.heading", token_color("#00ff00")),
                ("visual_md.heading.3", token_color("#00ffff")),
                (
                    "visual_md.inline_code",
                    HighlightStyle {
                        color: Some(hex("#ff00ff")),
                        background_color: Some(hex("#11223344")),
                        ..Default::default()
                    },
                ),
                (
                    "visual_md.callout.note",
                    HighlightStyle {
                        color: Some(hex("#ff0000")),
                        background_color: Some(hex("#ffff0055")),
                        ..Default::default()
                    },
                ),
            ],
        );

        assert_eq!(style_at(&mut cx, "link text").color, Some(hex("#0000ff")));
        assert_eq!(style_at(&mut cx, "One").color, Some(hex("#00ff00")));
        assert_eq!(style_at(&mut cx, "Three").color, Some(hex("#00ffff")));
        let inline_code = style_at(&mut cx, "code");
        assert_eq!(inline_code.color, Some(hex("#ff00ff")));
        assert_eq!(inline_code.background_color, Some(hex("#11223344")));
        assert_eq!(
            style_at(&mut cx, "note body").background_color,
            Some(hex("#ffff0055"))
        );

        set_visual_md(
            &mut cx,
            colors(settings::VisualMdColorsContent {
                link: Some("#abcdef".into()),
                ..Default::default()
            }),
        );
        assert_eq!(style_at(&mut cx, "link text").color, Some(hex("#abcdef")));
        assert_eq!(style_at(&mut cx, "One").color, Some(hex("#00ff00")));
    }

    #[gpui::test]
    async fn changing_font_settings_restyles_without_an_edit(cx: &mut TestAppContext) {
        let mut cx = styling_context(cx).await;

        update_user_settings(&mut cx, |content| {
            content.theme.ui_font_family = Some("Changed UI Font".to_string().into());
        });

        assert_eq!(
            family_name(&style_at(&mut cx, "plain")),
            Some("Changed UI Font".to_string())
        );
    }

    #[gpui::test]
    async fn prose_size_and_line_height_refine_the_editor_and_follow_zoom(cx: &mut TestAppContext) {
        use gpui::{AbsoluteLength, relative};
        let mut cx = styling_context(cx).await;
        let refinement = |cx: &mut EditorTestContext| {
            cx.update_editor(|editor, _window, _cx| editor.text_style_refinement().cloned())
        };
        assert_eq!(refinement(&mut cx), None);

        set_visual_md(
            &mut cx,
            settings::VisualMdSettingsContent {
                prose_font_size: Some(settings::FontSize(20.)),
                prose_line_height: Some(settings::BufferLineHeight::Custom(1.5)),
                ..Default::default()
            },
        );
        let applied = refinement(&mut cx).expect("a prose size applies a refinement");
        assert_eq!(applied.font_size, Some(AbsoluteLength::Pixels(px(20.))));
        assert_eq!(applied.line_height, Some(relative(1.5)));

        cx.update(|_window, cx| {
            theme_settings::adjust_buffer_font_size(cx, |size| size + px(2.));
        });
        cx.run_until_parked();
        let zoomed = refinement(&mut cx).expect("the refinement stays applied");
        assert_eq!(zoomed.font_size, Some(AbsoluteLength::Pixels(px(22.))));

        set_visual_md(&mut cx, Default::default());
        assert_eq!(refinement(&mut cx), None);
    }

    #[gpui::test]
    async fn zoom_leaves_the_refinement_alone_without_a_prose_size(cx: &mut TestAppContext) {
        let mut cx = styling_context(cx).await;

        cx.update(|_window, cx| {
            theme_settings::adjust_buffer_font_size(cx, |size| size + px(2.));
        });
        cx.run_until_parked();

        let refinement =
            cx.update_editor(|editor, _window, _cx| editor.text_style_refinement().cloned());
        assert_eq!(refinement, None);
    }

    #[gpui::test]
    async fn an_existing_text_style_refinement_is_restored_when_live_preview_ends(
        cx: &mut TestAppContext,
    ) {
        use gpui::AbsoluteLength;
        let mut cx = styling_context(cx).await;
        let original = TextStyleRefinement {
            font_weight: Some(FontWeight(600.)),
            ..Default::default()
        };
        cx.update_editor(|editor, _window, cx| {
            editor.set_text_style_refinement(original.clone());
            cx.notify();
        });

        set_visual_md(
            &mut cx,
            settings::VisualMdSettingsContent {
                prose_font_size: Some(settings::FontSize(20.)),
                ..Default::default()
            },
        );
        let refined =
            cx.update_editor(|editor, _window, _cx| editor.text_style_refinement().cloned());
        let refined = refined.expect("the refinement is applied");
        assert_eq!(refined.font_weight, Some(FontWeight(600.)));
        assert_eq!(refined.font_size, Some(AbsoluteLength::Pixels(px(20.))));

        cx.dispatch_action(ToggleLivePreview);
        cx.run_until_parked();
        let restored =
            cx.update_editor(|editor, _window, _cx| editor.text_style_refinement().cloned());
        assert_eq!(restored, Some(original));
    }

    #[gpui::test]
    async fn code_font_size_scales_inline_code_and_code_blocks_against_prose(
        cx: &mut TestAppContext,
    ) {
        let mut cx = styling_context(cx).await;
        let buffer_size = cx.update(|_window, cx| {
            f32::from(theme_settings::ThemeSettings::get_global(cx).buffer_font_size(cx))
        });

        set_visual_md(
            &mut cx,
            settings::VisualMdSettingsContent {
                prose_font_size: Some(settings::FontSize(16.)),
                code_font_size: Some(settings::FontSize(12.)),
                ..Default::default()
            },
        );
        let expected = 12.0 / 16.0;
        assert_eq!(
            style_at(&mut cx, "code").run_font_size_scale,
            Some(expected)
        );
        assert_eq!(style_at(&mut cx, "fenced").font_size_scale, Some(expected));
        let plain = style_at(&mut cx, "plain");
        assert_eq!(plain.run_font_size_scale, None);
        assert_eq!(plain.font_size_scale, None);

        // With no code size, code keeps the buffer size while prose changes.
        set_visual_md(
            &mut cx,
            settings::VisualMdSettingsContent {
                prose_font_size: Some(settings::FontSize(16.)),
                ..Default::default()
            },
        );
        assert_eq!(
            style_at(&mut cx, "code").run_font_size_scale,
            Some(buffer_size / 16.0)
        );

        set_visual_md(&mut cx, Default::default());
        assert_eq!(style_at(&mut cx, "code").run_font_size_scale, None);
        assert_eq!(style_at(&mut cx, "fenced").font_size_scale, None);
    }

    #[gpui::test]
    async fn custom_callout_types_get_their_own_look(cx: &mut TestAppContext) {
        use theme::ActiveTheme as _;
        let mut cx = styling_context(cx).await;
        let hint_background = cx.update(|_window, cx| cx.theme().status().hint_background);
        assert_eq!(
            style_at(&mut cx, "custom body").background_color,
            Some(hint_background)
        );

        set_visual_md(
            &mut cx,
            settings::VisualMdSettingsContent {
                callouts: Some(
                    [(
                        "Custom".to_string(),
                        settings::VisualMdCalloutContent {
                            icon: Some("star".to_string()),
                            accent: Some("#ff0000".into()),
                            background: Some("#00ff0080".into()),
                        },
                    )]
                    .into_iter()
                    .collect(),
                ),
                colors: Some(settings::VisualMdColorsContent {
                    callout: Some(
                        [(
                            "note".to_string(),
                            settings::VisualMdCalloutColorsContent {
                                accent: None,
                                background: Some("#0000ff80".into()),
                            },
                        )]
                        .into_iter()
                        .collect(),
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );

        assert_eq!(
            style_at(&mut cx, "custom body").background_color,
            Some(hex("#00ff0080"))
        );
        assert_eq!(
            style_at(&mut cx, "note body").background_color,
            Some(hex("#0000ff80"))
        );
        let tip_background = cx.update(|_window, cx| cx.theme().status().success_background);
        assert_eq!(
            style_at(&mut cx, "tip body").background_color,
            Some(tip_background)
        );

        let look = cx.update_editor(|editor, _window, cx| {
            let settings = editor
                .buffer()
                .read(cx)
                .language_settings_at(MultiBufferOffset(0), cx);
            ResolvedStyle::resolve(&settings.visual_md, cx)
                .callout_look(CalloutKind::Other, "custom")
        });
        assert_eq!(
            look.icon_path.as_ref(),
            icons::IconName::Star.path().as_ref()
        );
        assert_eq!(look.accent, hex("#ff0000"));

        set_visual_md(&mut cx, Default::default());
        assert_eq!(
            style_at(&mut cx, "custom body").background_color,
            Some(hint_background)
        );
    }

    #[gpui::test]
    async fn an_unknown_callout_icon_falls_back_to_the_kind_icon(cx: &mut TestAppContext) {
        let mut cx = styling_context(cx).await;

        let look = cx.update(|_window, cx| {
            let content = settings::VisualMdSettingsContent {
                callouts: Some(
                    [(
                        "note".to_string(),
                        settings::VisualMdCalloutContent {
                            icon: Some("definitely_not_an_icon".to_string()),
                            ..Default::default()
                        },
                    )]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            };
            ResolvedStyle::resolve(&content, cx).callout_look(CalloutKind::Note, "note")
        });

        assert_eq!(look.icon_path.as_ref(), "icons/info.svg");
    }

    /// Exercises the M11 callout title widget through a real `refresh()`,
    /// confirming an untouched callout renders its icon/chevron/label chip
    /// (`collapsed_text` is the capitalized label, e.g. "Warning" -- see
    /// `callout_title_placeholder`'s own comment for why that's
    /// deliberately *not* the literal `[!warning]` bracket text) rather than
    /// Zed's default fold ellipsis or raw source.
    #[gpui::test]
    async fn untouched_callout_title_folds_to_its_icon_chip(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("Otherˇ line\n> [!warning] Be careful\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| refresh(editor, window, cx));

        let displayed = cx.display_text();
        assert!(
            displayed.contains("Warning"),
            "expected the capitalized label chip, got {displayed:?}"
        );
        assert!(
            !displayed.contains("[!warning]"),
            "raw bracket text leaked through: {displayed:?}"
        );
        assert!(
            !displayed.contains('⋯'),
            "found Zed's default fold ellipsis in {displayed:?}"
        );
    }

    /// The other half of the pair above: touching the title line (a cursor
    /// anywhere on it, not just overlapping the marker's own bytes -- see
    /// `plan::cursor_anywhere_on_the_title_line_touches_it_not_just_the_marker_bytes`)
    /// reveals the literal `[!warning]` text instead of the chip, which is
    /// how a user actually retypes the type name -- there's no right-click
    /// "change type" menu in this milestone's scope.
    #[gpui::test]
    async fn touched_callout_title_reveals_raw_bracket_text(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("> [!warning] ˇBe careful\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| refresh(editor, window, cx));

        let displayed = cx.display_text();
        assert!(
            displayed.contains("[!warning]"),
            "expected raw bracket text, got {displayed:?}"
        );
        assert!(
            !displayed.contains("Warning "),
            "chip should not render while touched: {displayed:?}"
        );
    }

    /// Clicking the chevron isn't simulable in this harness (no real mouse
    /// events -- see `toggling_a_checkbox_edits_the_buffer_and_survives_refresh`'s
    /// own note), so this exercises the exact edit the click handler itself
    /// performs (writing `-` into `suffix_range`) and confirms it actually
    /// collapses the body into a crease, then that editing the suffix back
    /// out (`""`) restores it -- the full round trip the button drives.
    #[gpui::test]
    async fn collapsing_a_callout_folds_its_body_and_expanding_restores_it(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇOther line\n> [!note] Title\n> Body line one\n> Body line two\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| refresh(editor, window, cx));
        assert!(cx.display_text().contains("Body line one"));

        // Same single-edit shape `callout_title_placeholder`'s chevron
        // `on_click` performs: insert `-` right after `]`.
        cx.update_editor(|editor, window, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let suffix_at = "Other line\n> [!note]".len();
            let range = to_anchor_range(&snapshot, &(suffix_at..suffix_at));
            editor.edit([(range, "-")], cx);
            refresh(editor, window, cx);
        });
        let collapsed_text = cx.display_text();
        assert!(
            !collapsed_text.contains("Body line one"),
            "body should be folded away: {collapsed_text:?}"
        );
        assert!(
            !collapsed_text.contains("Body line two"),
            "body should be folded away: {collapsed_text:?}"
        );
        assert!(
            collapsed_text.contains("(collapsed)"),
            "expected the collapsed-body chip: {collapsed_text:?}"
        );

        // Expanding again: delete the `-` suffix (what clicking the chevron
        // a second time does).
        cx.update_editor(|editor, window, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let suffix_at = "Other line\n> [!note]".len();
            let range = to_anchor_range(&snapshot, &(suffix_at..suffix_at + 1));
            editor.edit([(range, "")], cx);
            refresh(editor, window, cx);
        });
        let expanded_text = cx.display_text();
        assert!(
            expanded_text.contains("Body line one"),
            "body should be back: {expanded_text:?}"
        );
        assert!(
            expanded_text.contains("Body line two"),
            "body should be back: {expanded_text:?}"
        );
        assert!(!expanded_text.contains("(collapsed)"));
    }

    /// An unrecognized `[!todo]` still gets a real callout box (`Other`
    /// kind, generic styling) rather than falling back to a plain
    /// blockquote with no title treatment at all.
    #[gpui::test]
    async fn unrecognized_callout_type_still_gets_a_title_chip(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇOther line\n> [!todo] buy milk\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| refresh(editor, window, cx));

        let displayed = cx.display_text();
        assert!(
            displayed.contains("Todo"),
            "expected the raw type name, capitalized, got {displayed:?}"
        );
        assert!(!displayed.contains("[!todo]"));
    }

    /// A callout's body containing other live constructs (bold text, a
    /// nested list) keeps decorating them correctly through a collapse and
    /// re-expand -- collapsing shouldn't corrupt or drop unrelated
    /// decoration state for content that becomes visible again.
    #[gpui::test]
    async fn collapsing_and_expanding_preserves_nested_decorations(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇOther line\n> [!note] Title\n> **bold** text\n> - a list item\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| refresh(editor, window, cx));
        let before = cx.display_text();
        assert!(before.contains("bold"));
        assert!(before.contains("a list item"));

        let suffix_at = "Other line\n> [!note]".len();
        cx.update_editor(|editor, window, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let range = to_anchor_range(&snapshot, &(suffix_at..suffix_at));
            editor.edit([(range, "-")], cx);
            refresh(editor, window, cx);
        });
        assert!(!cx.display_text().contains("bold"));

        cx.update_editor(|editor, window, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let range = to_anchor_range(&snapshot, &(suffix_at..suffix_at + 1));
            editor.edit([(range, "")], cx);
            refresh(editor, window, cx);
        });
        let after = cx.display_text();
        assert!(
            after.contains("bold"),
            "bold text should still decorate after re-expanding: {after:?}"
        );
        assert!(
            after.contains("a list item"),
            "the nested list should still decorate: {after:?}"
        );
    }
}
