//! Fenced code blocks that an extension renders: the block standing in for the
//! fence, what extensions returned for each distinct fence, and the checks
//! that output has to pass before anything is drawn from it.
//!
//! `refresh` only ever reads the cache and starts a request for a fence it has
//! no entry for. The request runs as its own task, so an extension that is slow
//! or broken delays only its own blocks.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use editor::display_map::{
    BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId,
};
use editor::{Anchor, Editor, MultiBufferOffset, MultiBufferSnapshot, SelectionEffects};
use extension::{
    VisualMdAppearance, VisualMdFenceOutput, VisualMdFenceRequest, VisualMdFenceResult,
    VisualMdImageFormat, VisualMdSpanStyle, VisualMdStyledText,
};
use gpui::{
    AnyElement, App, AppContext as _, AsyncApp, Context, ElementId, Entity, Focusable as _,
    FontStyle, FontWeight, Global, HighlightStyle, ImageSource, InteractiveElement as _,
    IntoElement as _, ObjectFit, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    StrikethroughStyle, Styled as _, StyledImage as _, StyledText, Task, UnderlineStyle,
    WeakEntity, Window, div, img, px,
};
use language::LanguageRegistry;
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use parking_lot::Mutex;
use settings::Settings as _;
use syntax_theme::SyntaxTheme;
use theme::ActiveTheme as _;
use util::ResultExt as _;

use crate::VisualMdAddon;
use crate::extensions::{FenceRenderer, HookError, VisualMdExtensions};
use crate::plan::{Plan, RenderedFence};

const FENCE_CACHE_CAPACITY: usize = 256;

pub const MAX_TEXT_OUTPUT_BYTES: usize = 1024 * 1024;
pub const MAX_IMAGE_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_HEIGHT_ROWS: u32 = 200;

/// An extension answers "busy" while it has several requests running, which
/// happens when a document shows more fences than it may render at once. The
/// request waits its turn instead of failing.
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(100);
const BUSY_RETRIES: usize = 100;

/// Everything a rendering depends on. Two fences with the same key look the
/// same, so they share one request and one result, across editors too.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FenceKey {
    extension_id: Arc<str>,
    generation: u64,
    language: String,
    info: String,
    content_hash: u64,
    content_len: usize,
    appearance: VisualMdAppearance,
    path: Option<String>,
}

pub(crate) enum FenceState {
    Pending,
    StyledText(VisualMdStyledText),
    Markdown(Entity<Markdown>),
    Image(Arc<gpui::Image>),
    Failed(SharedString),
}

/// An extension's output after the checks in [`validate_output`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ValidOutput {
    StyledText(VisualMdStyledText),
    Markdown(String),
    Svg(String),
    Image(VisualMdImageFormat, Vec<u8>),
}

/// Checks everything the host cannot trust an extension to have got right,
/// and drops what can be dropped without losing the rest: styled spans that are
/// out of bounds, empty, off a character boundary or overlapping an earlier
/// one, which `StyledText` would otherwise panic on or mis-paint.
pub(crate) fn validate_output(output: VisualMdFenceOutput) -> Result<ValidOutput, String> {
    match output {
        VisualMdFenceOutput::StyledText(mut styled) => {
            if styled.text.len() > MAX_TEXT_OUTPUT_BYTES {
                return Err(format!(
                    "the styled text is {} bytes, over the limit of {MAX_TEXT_OUTPUT_BYTES}",
                    styled.text.len()
                ));
            }
            let text = styled.text.as_str();
            styled
                .spans
                .sort_by_key(|span| (span.range.start, span.range.end));
            let mut covered_until = 0;
            styled.spans.retain(|span| {
                let is_valid = span.range.start < span.range.end
                    && span.range.start >= covered_until
                    && span.range.end <= text.len()
                    && text.is_char_boundary(span.range.start)
                    && text.is_char_boundary(span.range.end);
                if is_valid {
                    covered_until = span.range.end;
                } else {
                    log::warn!(
                        "ignoring a styled span {:?} of a fence rendering: it is empty, out of \
                        bounds, off a character boundary or overlaps an earlier span",
                        span.range
                    );
                }
                is_valid
            });
            Ok(ValidOutput::StyledText(styled))
        }
        VisualMdFenceOutput::Markdown(source) => {
            check_text_size("the Markdown", &source)?;
            Ok(ValidOutput::Markdown(source))
        }
        VisualMdFenceOutput::Svg(source) => {
            check_text_size("the SVG", &source)?;
            Ok(ValidOutput::Svg(source))
        }
        VisualMdFenceOutput::Image(image) => {
            if image.bytes.is_empty() {
                return Err("the image is empty".to_string());
            }
            if image.bytes.len() > MAX_IMAGE_OUTPUT_BYTES {
                return Err(format!(
                    "the image is {} bytes, over the limit of {MAX_IMAGE_OUTPUT_BYTES}",
                    image.bytes.len()
                ));
            }
            Ok(ValidOutput::Image(image.format, image.bytes))
        }
    }
}

fn check_text_size(what: &str, text: &str) -> Result<(), String> {
    if text.len() > MAX_TEXT_OUTPUT_BYTES {
        return Err(format!(
            "{what} is {} bytes, over the limit of {MAX_TEXT_OUTPUT_BYTES}",
            text.len()
        ));
    }
    Ok(())
}

fn clamp_height(rows: u32) -> u32 {
    rows.clamp(1, MAX_HEIGHT_ROWS)
}

/// One distinct fence's request and result, shared by every block showing it.
pub(crate) struct FenceEntry {
    state: ArcSwap<FenceState>,
    height_hint: AtomicU32,
    /// The editors showing this fence while it is pending, to repaint when the
    /// result lands.
    waiters: Mutex<Vec<WeakEntity<Editor>>>,
    /// Kept so the request is not cancelled, and not tied to whichever editor
    /// happened to ask first.
    request: Mutex<Option<Task<()>>>,
}

impl FenceEntry {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: ArcSwap::from_pointee(FenceState::Pending),
            height_hint: AtomicU32::new(0),
            waiters: Mutex::new(Vec::new()),
            request: Mutex::new(None),
        })
    }

    pub(crate) fn state(&self) -> Arc<FenceState> {
        self.state.load_full()
    }

    fn height_hint(&self) -> Option<u32> {
        match self.height_hint.load(Ordering::Relaxed) {
            0 => None,
            rows => Some(rows),
        }
    }

    #[cfg(test)]
    fn waiter_count(&self) -> usize {
        self.waiters.lock().len()
    }

    fn add_waiter(&self, editor: &WeakEntity<Editor>) {
        let mut waiters = self.waiters.lock();
        if !waiters
            .iter()
            .any(|waiter| waiter.entity_id() == editor.entity_id())
        {
            waiters.push(editor.clone());
        }
    }

    fn finish(&self, state: FenceState, height_hint: Option<u32>, cx: &mut AsyncApp) {
        self.state.store(Arc::new(state));
        if let Some(rows) = height_hint {
            self.height_hint
                .store(clamp_height(rows), Ordering::Relaxed);
        }
        let waiters = std::mem::take(&mut *self.waiters.lock());
        for waiter in waiters {
            if let Some(editor) = waiter.upgrade() {
                editor.update(cx, |_, cx| cx.notify());
            }
        }
    }
}

#[derive(Default)]
struct FenceCacheInner {
    entries: HashMap<FenceKey, Arc<FenceEntry>>,
    /// Keys from least to most recently used.
    recency: VecDeque<FenceKey>,
}

/// The renderings of every fence any editor has asked for, most recently used
/// 256 of them.
#[derive(Clone, Default)]
pub(crate) struct FenceCache(Arc<Mutex<FenceCacheInner>>);

impl Global for FenceCache {}

impl FenceCache {
    /// The entry for `key`, and whether this call created it, in which case
    /// the caller must start its request.
    fn entry(&self, key: &FenceKey) -> (Arc<FenceEntry>, bool) {
        let mut inner = self.0.lock();
        if let Some(entry) = inner.entries.get(key).cloned() {
            if let Some(position) = inner.recency.iter().position(|recent| recent == key)
                && let Some(recent) = inner.recency.remove(position)
            {
                inner.recency.push_back(recent);
            }
            return (entry, false);
        }

        let entry = FenceEntry::new();
        inner.entries.insert(key.clone(), entry.clone());
        inner.recency.push_back(key.clone());
        while inner.entries.len() > FENCE_CACHE_CAPACITY {
            let Some(oldest) = inner.recency.pop_front() else {
                break;
            };
            inner.entries.remove(&oldest);
        }
        (entry, true)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.lock().entries.len()
    }

    #[cfg(test)]
    pub(crate) fn entries(&self) -> Vec<Arc<FenceEntry>> {
        self.0.lock().entries.values().cloned().collect()
    }
}

pub(crate) fn init(cx: &mut App) {
    if cx.try_global::<FenceCache>().is_none() {
        cx.set_global(FenceCache::default());
    }
}

/// A fence block currently inserted, diffed on its range and key together: a
/// different key means a different rendering, a different range means the
/// block's placement is stale.
pub(crate) struct RenderedFenceBlock {
    range: Range<usize>,
    key: FenceKey,
    id: CustomBlockId,
}

struct WantedFence {
    range: Range<usize>,
    content_start: usize,
    key: FenceKey,
    entry: Arc<FenceEntry>,
}

fn note_path(editor: &Editor, cx: &App) -> Option<String> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?.as_local()?;
    Some(file.abs_path(cx).to_string_lossy().into_owned())
}

fn appearance(cx: &App) -> VisualMdAppearance {
    match cx.theme().appearance() {
        theme::Appearance::Light => VisualMdAppearance::Light,
        theme::Appearance::Dark => VisualMdAppearance::Dark,
    }
}

/// Finds the entry for each fence an extension renders, starting a request for
/// each that has none yet.
fn wanted_fences(
    editor: &Editor,
    text: &str,
    fences: &[RenderedFence],
    cx: &mut Context<Editor>,
) -> Vec<WantedFence> {
    let renderers: Vec<Option<FenceRenderer>> = {
        let registry = cx.try_global::<VisualMdExtensions>();
        fences
            .iter()
            .map(|fence| registry?.renderer_for_language(&fence.language))
            .collect()
    };
    if renderers.iter().all(Option::is_none) {
        return Vec::new();
    }

    let cache = cx.default_global::<FenceCache>().clone();
    let appearance = appearance(cx);
    let path = note_path(editor, cx);
    let language_registry = editor
        .buffer()
        .read(cx)
        .as_singleton()
        .and_then(|buffer| buffer.read(cx).language_registry());
    let editor_handle = cx.weak_entity();

    let mut wanted = Vec::new();
    for (fence, renderer) in fences.iter().zip(renderers) {
        let Some(renderer) = renderer else {
            continue;
        };
        let key = FenceKey {
            extension_id: renderer.extension_id.clone(),
            generation: renderer.generation,
            language: fence.language.clone(),
            info: fence.info.clone(),
            content_hash: fence.content_hash,
            content_len: fence.content_range.len(),
            appearance,
            path: path.clone(),
        };
        let (entry, created) = cache.entry(&key);
        if created {
            let content = text
                .get(fence.content_range.clone())
                .unwrap_or_default()
                .to_string();
            start_request(
                &entry,
                renderer,
                VisualMdFenceRequest {
                    language: fence.language.clone(),
                    info: fence.info.clone(),
                    content,
                    appearance,
                    path: path.clone(),
                },
                language_registry.clone(),
                cx,
            );
        }
        if matches!(*entry.state(), FenceState::Pending) {
            entry.add_waiter(&editor_handle);
        }
        wanted.push(WantedFence {
            range: fence.range.clone(),
            content_start: fence.content_range.start,
            key,
            entry,
        });
    }
    wanted
}

fn start_request(
    entry: &Arc<FenceEntry>,
    renderer: FenceRenderer,
    request: VisualMdFenceRequest,
    language_registry: Option<Arc<LanguageRegistry>>,
    cx: &mut Context<Editor>,
) {
    let task = cx.spawn({
        let entry = entry.clone();
        async move |_, cx| {
            let outcome = render_when_not_busy(&renderer, &request, cx).await;
            let (state, height_hint) = match outcome {
                Ok(result) => match validate_output(result.output) {
                    Ok(output) => (
                        cx.update(|cx| build_state(output, language_registry, cx)),
                        result.height_hint,
                    ),
                    Err(message) => (FenceState::Failed(message.into()), None),
                },
                Err(error) => {
                    log::warn!(
                        "extension {} could not render a `{}` fence: {error}",
                        renderer.extension_id,
                        request.language
                    );
                    (FenceState::Failed(error.to_string().into()), None)
                }
            };
            entry.finish(state, height_hint, cx);
        }
    });
    *entry.request.lock() = Some(task);
}

async fn render_when_not_busy(
    renderer: &FenceRenderer,
    request: &VisualMdFenceRequest,
    cx: &mut AsyncApp,
) -> Result<VisualMdFenceResult, HookError> {
    let mut retries = 0;
    loop {
        let call = cx.update(|cx| {
            cx.try_global::<VisualMdExtensions>().map(|registry| {
                registry.render_fence(
                    &renderer.extension_id,
                    renderer.renderer.clone(),
                    request.clone(),
                    cx,
                )
            })
        });
        let Some(call) = call else {
            return Err(HookError::NotRegistered(renderer.extension_id.clone()));
        };
        match call.await {
            Err(HookError::Busy(_)) if retries < BUSY_RETRIES => {
                retries += 1;
                cx.background_executor().timer(BUSY_RETRY_DELAY).await;
            }
            result => return result,
        }
    }
}

fn build_state(
    output: ValidOutput,
    language_registry: Option<Arc<LanguageRegistry>>,
    cx: &mut App,
) -> FenceState {
    match output {
        ValidOutput::StyledText(text) => FenceState::StyledText(text),
        ValidOutput::Markdown(source) => FenceState::Markdown(
            cx.new(|cx| Markdown::new(source.into(), language_registry, None, cx)),
        ),
        ValidOutput::Svg(source) => FenceState::Image(Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Svg,
            source.into_bytes(),
        ))),
        ValidOutput::Image(format, bytes) => {
            let format = match format {
                VisualMdImageFormat::Png => gpui::ImageFormat::Png,
                VisualMdImageFormat::Jpeg => gpui::ImageFormat::Jpeg,
                VisualMdImageFormat::Gif => gpui::ImageFormat::Gif,
                VisualMdImageFormat::Webp => gpui::ImageFormat::Webp,
            };
            FenceState::Image(Arc::new(gpui::Image::from_bytes(format, bytes)))
        }
    }
}

/// Diffs the fences an extension renders against the blocks the previous
/// refresh inserted, the same way `apply_images` does for images.
pub(crate) fn apply_rendered_fences(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    text: &str,
    computed: &Plan,
    cx: &mut Context<Editor>,
) {
    let previous = editor
        .addon_mut::<VisualMdAddon>()
        .map(|addon| std::mem::take(&mut addon.rendered_fence_blocks))
        .unwrap_or_default();
    let blocks = diff_rendered_fences(editor, snapshot, text, computed, previous, cx);
    if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
        addon.rendered_fence_blocks = blocks;
    }
}

fn diff_rendered_fences(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    text: &str,
    computed: &Plan,
    previous: Vec<RenderedFenceBlock>,
    cx: &mut Context<Editor>,
) -> Vec<RenderedFenceBlock> {
    let wanted = wanted_fences(editor, text, &computed.rendered_fences, cx);

    let wanted_keys: std::collections::HashSet<(&Range<usize>, &FenceKey)> = wanted
        .iter()
        .map(|fence| (&fence.range, &fence.key))
        .collect();
    let mut kept = Vec::new();
    let mut stale_ids: collections::HashSet<CustomBlockId> = collections::HashSet::default();
    for block in previous {
        if wanted_keys.contains(&(&block.range, &block.key)) {
            kept.push(block);
        } else {
            stale_ids.insert(block.id);
        }
    }
    if !stale_ids.is_empty() {
        editor.remove_blocks(stale_ids, None, cx);
    }

    let new_fences: Vec<&WantedFence> = wanted
        .iter()
        .filter(|fence| {
            !kept
                .iter()
                .any(|block| block.range == fence.range && block.key == fence.key)
        })
        .collect();
    if new_fences.is_empty() {
        return kept;
    }

    let editor_handle = cx.weak_entity();
    let properties: Vec<BlockProperties<Anchor>> = new_fences
        .iter()
        .map(|fence| {
            let anchors = crate::to_anchor_range(snapshot, &fence.range);
            let source_rows = text
                .get(fence.range.clone())
                .map_or(1, |source| source.matches('\n').count() as u32 + 1);
            let content_start = snapshot.anchor_before(MultiBufferOffset(fence.content_start));
            let entry = fence.entry.clone();
            let editor_handle = editor_handle.clone();
            BlockProperties {
                placement: BlockPlacement::Replace(anchors.start..=anchors.end),
                height: Some(clamp_height(entry.height_hint().unwrap_or(source_rows))),
                style: BlockStyle::Fixed,
                render: Arc::new(move |cx: &mut BlockContext| {
                    render_fence_block(&entry, &editor_handle, content_start, cx)
                }),
                priority: 0,
            }
        })
        .collect();
    let ids = editor.insert_blocks(properties, None, cx);
    kept.extend(
        new_fences
            .into_iter()
            .zip(ids)
            .map(|(fence, id)| RenderedFenceBlock {
                range: fence.range.clone(),
                key: fence.key.clone(),
                id,
            }),
    );
    kept
}

/// Puts the cursor inside the fence, so its source shows instead of the
/// rendering.
fn reveal_source(
    editor: &mut Editor,
    position: Anchor,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let select_position = |editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>| {
        editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
            selections.select_anchor_ranges([position..position]);
        });
    };

    // While the block is up, the editor snaps a selection inside it to the
    // whole block, and typing over that would delete the fence. So the block
    // is removed first, and only then is the cursor put where it was meant to go.
    select_position(editor, window, cx);
    crate::refresh(editor, window, cx);
    select_position(editor, window, cx);
    crate::refresh(editor, window, cx);
    window.focus(&editor.focus_handle(cx), cx);
}

fn render_fence_block(
    entry: &FenceEntry,
    editor: &WeakEntity<Editor>,
    content_start: Anchor,
    cx: &mut BlockContext,
) -> AnyElement {
    let (border, muted) = {
        let colors = cx.app.theme().colors();
        (colors.border, colors.text_muted)
    };
    let message_box = move |message: SharedString| {
        div()
            .px_2()
            .py_1()
            .border_1()
            .border_color(border)
            .rounded_md()
            .text_color(muted)
            .child(message)
            .into_any_element()
    };

    let content = match &*entry.state() {
        FenceState::Pending => message_box("Rendering…".into()),
        FenceState::Failed(message) => {
            message_box(format!("Could not render this block: {message}").into())
        }
        FenceState::StyledText(styled) => {
            let syntax = cx.app.theme().syntax().clone();
            let settings = theme_settings::ThemeSettings::get_global(cx.app);
            let font_family = settings.buffer_font.family.clone();
            let font_size = settings.buffer_font_size(cx.app);
            let highlights = styled
                .spans
                .iter()
                .map(|span| (span.range.clone(), highlight_style(&span.style, &syntax)))
                .collect::<Vec<_>>();
            div()
                .font_family(font_family)
                .text_size(font_size)
                .child(StyledText::new(styled.text.clone()).with_highlights(highlights))
                .into_any_element()
        }
        FenceState::Markdown(markdown) => MarkdownElement::new(
            markdown.clone(),
            MarkdownStyle::themed(MarkdownFont::Preview, cx.window, cx.app),
        )
        .into_any_element(),
        FenceState::Image(image) => img(ImageSource::Image(image.clone()))
            .max_w_full()
            .object_fit(ObjectFit::Contain)
            .with_fallback(move || message_box("The image could not be shown".into()))
            .into_any_element(),
    };

    let editor = editor.clone();
    div()
        .id(ElementId::from(cx.block_id))
        .w(cx.max_width)
        .on_click(move |_, window, cx| {
            if let Some(editor) = editor.upgrade() {
                editor.update(cx, |editor, cx| {
                    reveal_source(editor, content_start, window, cx)
                });
            }
        })
        .child(content)
        .into_any_element()
}

fn highlight_style(style: &VisualMdSpanStyle, syntax: &SyntaxTheme) -> HighlightStyle {
    let parse_color = |color: &str| theme::try_parse_color(color).log_err();
    let mut highlight = style
        .theme_token
        .as_deref()
        .and_then(|token| syntax.style_for_name(token))
        .unwrap_or_default();
    if let Some(color) = style.color.as_deref().and_then(parse_color) {
        highlight.color = Some(color);
    }
    if let Some(color) = style.background_color.as_deref().and_then(parse_color) {
        highlight.background_color = Some(color);
    }
    if let Some(weight) = style.font_weight {
        highlight.font_weight = Some(FontWeight(f32::from(weight.clamp(100, 900))));
    }
    if let Some(italic) = style.italic {
        highlight.font_style = Some(if italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        });
    }
    if style.underline == Some(true) {
        highlight.underline = Some(UnderlineStyle {
            thickness: px(1.),
            color: None,
            wavy: false,
        });
    }
    if style.strikethrough == Some(true) {
        highlight.strikethrough = Some(StrikethroughStyle {
            thickness: px(1.),
            color: None,
        });
    }
    highlight
}

#[cfg(test)]
mod tests {
    use extension::{VisualMdImage, VisualMdStyledSpan};

    use super::*;

    fn styled(text: &str, spans: &[Range<usize>]) -> VisualMdFenceOutput {
        VisualMdFenceOutput::StyledText(VisualMdStyledText {
            text: text.to_string(),
            spans: spans
                .iter()
                .map(|range| VisualMdStyledSpan {
                    range: range.clone(),
                    style: VisualMdSpanStyle::default(),
                })
                .collect(),
        })
    }

    fn valid_spans(output: Result<ValidOutput, String>) -> Vec<Range<usize>> {
        match output {
            Ok(ValidOutput::StyledText(styled)) => {
                styled.spans.into_iter().map(|span| span.range).collect()
            }
            other => panic!("expected styled text, got {other:?}"),
        }
    }

    #[test]
    fn test_valid_styled_spans_are_kept_in_order() {
        assert_eq!(
            valid_spans(validate_output(styled("hello world", &[6..11, 0..5]))),
            vec![0..5, 6..11]
        );
    }

    #[test]
    fn test_invalid_styled_spans_are_dropped_and_the_rest_kept() {
        // `é` is two bytes, so 1..2 splits it.
        let text = "aéb";
        let reversed = Range { start: 5, end: 4 };
        assert_eq!(
            valid_spans(validate_output(styled(
                text,
                &[0..1, 1..2, 2..2, 3..9, 3..4, 0..4, 4..4, reversed]
            ))),
            vec![0..1, 3..4],
            "out-of-bounds, empty, mid-character, reversed and overlapping spans go"
        );
    }

    #[test]
    fn test_a_span_may_touch_the_one_before_it() {
        assert_eq!(
            valid_spans(validate_output(styled("abcd", &[0..2, 2..4]))),
            vec![0..2, 2..4]
        );
    }

    #[test]
    fn test_oversized_text_outputs_are_rejected() {
        let oversized = "x".repeat(MAX_TEXT_OUTPUT_BYTES + 1);
        assert!(validate_output(VisualMdFenceOutput::Markdown(oversized.clone())).is_err());
        assert!(validate_output(VisualMdFenceOutput::Svg(oversized.clone())).is_err());
        assert!(validate_output(styled(&oversized, &[])).is_err());

        let at_limit = "x".repeat(MAX_TEXT_OUTPUT_BYTES);
        assert!(validate_output(VisualMdFenceOutput::Markdown(at_limit)).is_ok());
    }

    #[test]
    fn test_images_must_be_present_and_within_the_limit() {
        let image = |bytes: Vec<u8>| {
            VisualMdFenceOutput::Image(VisualMdImage {
                format: VisualMdImageFormat::Png,
                bytes,
            })
        };

        assert!(validate_output(image(Vec::new())).is_err());
        assert!(validate_output(image(vec![0; MAX_IMAGE_OUTPUT_BYTES + 1])).is_err());
        assert_eq!(
            validate_output(image(vec![1, 2, 3])),
            Ok(ValidOutput::Image(VisualMdImageFormat::Png, vec![1, 2, 3]))
        );
    }

    #[test]
    fn test_height_hints_are_clamped_to_a_sensible_range() {
        assert_eq!(clamp_height(0), 1);
        assert_eq!(clamp_height(7), 7);
        assert_eq!(clamp_height(u32::MAX), MAX_HEIGHT_ROWS);
    }

    fn key(content_hash: u64) -> FenceKey {
        FenceKey {
            extension_id: "notes".into(),
            generation: 1,
            language: "flow".into(),
            info: "flow".into(),
            content_hash,
            content_len: 6,
            appearance: VisualMdAppearance::Dark,
            path: None,
        }
    }

    #[test]
    fn test_the_cache_shares_an_entry_between_equal_keys() {
        let cache = FenceCache::default();
        let (first, created) = cache.entry(&key(1));
        let (second, created_again) = cache.entry(&key(1));

        assert!(created);
        assert!(!created_again);
        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &cache.entry(&key(2)).0));
    }

    #[test]
    fn test_the_cache_evicts_the_least_recently_used_entry() {
        let cache = FenceCache::default();
        let (first, _) = cache.entry(&key(0));
        for hash in 1..FENCE_CACHE_CAPACITY as u64 {
            cache.entry(&key(hash));
        }
        assert_eq!(cache.len(), FENCE_CACHE_CAPACITY);

        // Using the oldest entry again makes the next one the oldest.
        cache.entry(&key(0));
        cache.entry(&key(FENCE_CACHE_CAPACITY as u64));

        assert_eq!(cache.len(), FENCE_CACHE_CAPACITY);
        let (still_there, created) = cache.entry(&key(0));
        assert!(!created);
        assert!(Arc::ptr_eq(&first, &still_there));
        assert!(cache.entry(&key(1)).1, "key 1 was the least recently used");
    }

    #[test]
    fn test_a_fresh_entry_is_pending_with_no_height_hint() {
        let entry = FenceEntry::new();

        assert!(matches!(*entry.state(), FenceState::Pending));
        assert_eq!(entry.height_hint(), None);
    }

    #[test]
    fn test_span_styles_become_highlights() {
        let syntax = SyntaxTheme::default();
        let highlight = highlight_style(
            &VisualMdSpanStyle {
                color: Some("#ff0000".into()),
                background_color: Some("not a color".into()),
                font_weight: Some(5000),
                italic: Some(true),
                underline: Some(true),
                strikethrough: Some(false),
                theme_token: Some("unknown-token".into()),
            },
            &syntax,
        );

        assert_eq!(
            highlight.color,
            theme::try_parse_color("#ff0000").ok(),
            "an explicit color is used"
        );
        assert_eq!(highlight.background_color, None, "a bad color is ignored");
        assert_eq!(highlight.font_weight, Some(FontWeight(900.)));
        assert_eq!(highlight.font_style, Some(FontStyle::Italic));
        assert!(highlight.underline.is_some());
        assert!(highlight.strikethrough.is_none());
    }
}

#[cfg(test)]
mod integration_tests {
    use std::sync::atomic::AtomicUsize;

    use editor::test::editor_test_context::EditorTestContext;
    use gpui::TestAppContext;

    use crate::extensions::test_support::{Behavior, FakeHooks, register};
    use crate::integration_tests::{init_test, markdown_language};

    use super::*;

    const DOCUMENT: &str = "ˇtext above\n\n```flow\na -> b\n```\n\ntext below\n";

    async fn editor_showing(cx: &mut TestAppContext, text: &str) -> EditorTestContext {
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state(text);
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx
    }

    fn rendered_blocks(cx: &mut EditorTestContext) -> usize {
        cx.update_editor(|editor, _, _| {
            editor
                .addon::<VisualMdAddon>()
                .map_or(0, |addon| addon.rendered_fence_blocks.len())
        })
    }

    fn border_blocks(cx: &mut EditorTestContext) -> usize {
        cx.update_editor(|editor, _, _| {
            editor
                .addon::<VisualMdAddon>()
                .map_or(0, |addon| addon.code_fence_borders.len())
        })
    }

    /// `set_state` moves the selection after the edit it makes, and a plain
    /// selection change only refreshes on the next frame, so refresh by hand
    /// once the selection is where the test wants it.
    fn refresh_now(cx: &mut EditorTestContext) {
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));
        cx.run_until_parked();
    }

    fn cached_entries(cx: &mut EditorTestContext) -> Vec<Arc<FenceEntry>> {
        cx.update(|_, cx| cx.global::<FenceCache>().entries())
    }

    fn cached_states(cx: &mut EditorTestContext) -> Vec<Arc<FenceState>> {
        cached_entries(cx)
            .iter()
            .map(|entry| entry.state())
            .collect()
    }

    /// Paints the editor, which runs every block's render closure.
    fn draw(cx: &mut EditorTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
    }

    fn setup(cx: &mut TestAppContext, behavior: Behavior) -> Arc<FakeHooks> {
        init_test(cx);
        let hooks = FakeHooks::new(&cx.executor(), behavior);
        register(
            cx,
            "notes",
            "fence_renderers = [\"flow\"]",
            Some(hooks.clone()),
        );
        hooks
    }

    #[gpui::test]
    async fn test_a_claimed_fence_is_replaced_by_a_block_showing_the_output(
        cx: &mut TestAppContext,
    ) {
        let hooks = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, DOCUMENT).await;

        assert_eq!(rendered_blocks(&mut cx), 1);
        assert_eq!(
            border_blocks(&mut cx),
            0,
            "no fence borders around a rendering"
        );
        let requests = hooks.fence_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].language, "flow");
        assert_eq!(requests[0].info, "flow");
        assert_eq!(requests[0].content, "a -> b\n");
        let states = cached_states(&mut cx);
        assert!(matches!(*states[0], FenceState::Markdown(_)));

        draw(&mut cx);
    }

    #[gpui::test]
    async fn test_every_kind_of_output_can_be_drawn(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, DOCUMENT).await;

        let outputs = [
            VisualMdFenceOutput::Svg(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"></svg>"
                    .into(),
            ),
            VisualMdFenceOutput::StyledText(VisualMdStyledText {
                text: "a -> b".into(),
                spans: vec![extension::VisualMdStyledSpan {
                    range: 0..1,
                    style: VisualMdSpanStyle {
                        color: Some("#ff0000".into()),
                        font_weight: Some(700),
                        ..Default::default()
                    },
                }],
            }),
            VisualMdFenceOutput::Image(extension::VisualMdImage {
                format: VisualMdImageFormat::Png,
                bytes: vec![0, 1, 2],
            }),
        ];
        for (index, output) in outputs.into_iter().enumerate() {
            hooks.set_fence_output(output);
            // A different path gives each output a key of its own.
            cx.set_state(&format!("ˇtext {index}\n\n```flow\nline {index}\n```\n"));
            cx.run_until_parked();
            assert_eq!(rendered_blocks(&mut cx), 1, "output {index}");
            draw(&mut cx);
        }
    }

    #[gpui::test]
    async fn test_the_block_is_pending_until_the_extension_answers(cx: &mut TestAppContext) {
        let delay = Duration::from_secs(2);
        setup(cx, Behavior::TakeLongerThan(delay));
        let mut cx = editor_showing(cx, DOCUMENT).await;

        assert_eq!(
            rendered_blocks(&mut cx),
            1,
            "a block stands in while waiting"
        );
        assert!(matches!(*cached_states(&mut cx)[0], FenceState::Pending));
        draw(&mut cx);

        cx.executor().advance_clock(delay);
        cx.run_until_parked();

        assert!(matches!(
            *cached_states(&mut cx)[0],
            FenceState::Markdown(_)
        ));
        draw(&mut cx);
    }

    #[gpui::test]
    async fn test_an_editor_waits_on_a_pending_rendering_until_it_lands(cx: &mut TestAppContext) {
        let delay = Duration::from_secs(2);
        setup(cx, Behavior::TakeLongerThan(delay));
        let mut cx = editor_showing(cx, DOCUMENT).await;

        let entries = cached_entries(&mut cx);
        assert_eq!(entries[0].waiter_count(), 1);

        cx.executor().advance_clock(delay);
        cx.run_until_parked();

        assert_eq!(entries[0].waiter_count(), 0, "waiters are told and let go");
    }

    #[gpui::test]
    async fn test_finishing_a_rendering_repaints_the_editors_waiting_on_it(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let cx = EditorTestContext::new(cx).await;
        let repaints = Arc::new(AtomicUsize::new(0));
        let editor = cx.editor.clone();
        let mut cx = cx;
        cx.update(|_, app| {
            let repaints = repaints.clone();
            app.observe(&editor, move |_, _| {
                repaints.fetch_add(1, Ordering::Relaxed);
            })
            .detach();
        });
        let entry = FenceEntry::new();
        entry.add_waiter(&editor.downgrade());
        entry.add_waiter(&editor.downgrade());
        assert_eq!(entry.waiter_count(), 1, "an editor waits only once");
        cx.run_until_parked();
        let before = repaints.load(Ordering::Relaxed);

        entry.finish(
            FenceState::Failed("done".into()),
            Some(5),
            &mut cx.to_async(),
        );
        cx.run_until_parked();

        assert_eq!(repaints.load(Ordering::Relaxed), before + 1);
        assert!(matches!(*entry.state(), FenceState::Failed(_)));
        assert_eq!(entry.height_hint(), Some(5));
    }

    #[gpui::test]
    async fn test_fences_beyond_the_in_flight_limit_wait_for_a_turn(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::TakeLongerThan(Duration::from_secs(1)));
        let fences = (0..6)
            .map(|index| format!("```flow\nfence {index}\n```\n\n"))
            .collect::<String>();
        let mut cx = editor_showing(cx, &format!("ˇabove\n\n{fences}")).await;
        assert_eq!(rendered_blocks(&mut cx), 6);

        for _ in 0..10 {
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.run_until_parked();
        }

        let states = cached_states(&mut cx);
        assert_eq!(states.len(), 6);
        assert!(
            states
                .iter()
                .all(|state| matches!(**state, FenceState::Markdown(_))),
            "every fence is rendered in the end, none is refused as busy"
        );
        assert_eq!(hooks.calls(), 6);
    }

    #[gpui::test]
    async fn test_touching_the_fence_reveals_its_source_and_leaving_hides_it_again(
        cx: &mut TestAppContext,
    ) {
        let hooks = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, DOCUMENT).await;
        assert_eq!(rendered_blocks(&mut cx), 1);

        cx.set_state("text above\n\n```flow\na ˇ-> b\n```\n\ntext below\n");
        refresh_now(&mut cx);
        assert_eq!(rendered_blocks(&mut cx), 0);
        assert_eq!(
            border_blocks(&mut cx),
            2,
            "the source is shown with its borders"
        );

        cx.set_state("text aboveˇ\n\n```flow\na -> b\n```\n\ntext below\n");
        refresh_now(&mut cx);
        assert_eq!(rendered_blocks(&mut cx), 1);
        assert_eq!(border_blocks(&mut cx), 0);
        assert_eq!(hooks.calls(), 1, "the cached rendering is reused");
    }

    #[gpui::test]
    async fn test_unregistering_the_extension_restores_the_default_rendering(
        cx: &mut TestAppContext,
    ) {
        setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, DOCUMENT).await;
        assert_eq!(rendered_blocks(&mut cx), 1);

        cx.update(|_, cx| VisualMdExtensions::unregister("notes", cx));
        cx.run_until_parked();

        assert_eq!(rendered_blocks(&mut cx), 0);
        assert_eq!(border_blocks(&mut cx), 2);
    }

    #[gpui::test]
    async fn test_registering_the_extension_again_renders_with_the_new_build(
        cx: &mut TestAppContext,
    ) {
        let old_build = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, DOCUMENT).await;
        let new_build = FakeHooks::new(&cx.executor(), Behavior::Succeed);
        new_build.set_fence_output(VisualMdFenceOutput::Markdown("new".into()));

        register(
            &mut cx,
            "notes",
            "fence_renderers = [\"flow\"]",
            Some(new_build.clone()),
        );
        cx.run_until_parked();

        assert_eq!(rendered_blocks(&mut cx), 1);
        assert_eq!(new_build.calls(), 1);
        assert_eq!(old_build.calls(), 1, "the old build is not asked again");
    }

    #[gpui::test]
    async fn test_a_fence_renderer_registered_after_the_document_opened_takes_effect(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let mut cx = editor_showing(cx, DOCUMENT).await;
        assert_eq!(rendered_blocks(&mut cx), 0);
        assert_eq!(border_blocks(&mut cx), 2);

        let hooks = FakeHooks::new(&cx.executor(), Behavior::Succeed);
        register(
            &mut cx,
            "notes",
            "fence_renderers = [\"flow\"]",
            Some(hooks),
        );
        cx.run_until_parked();

        assert_eq!(rendered_blocks(&mut cx), 1);
        assert_eq!(border_blocks(&mut cx), 0);
    }

    #[gpui::test]
    async fn test_the_language_tag_matches_ignoring_case(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, "ˇabove\n\n```FLOW\na -> b\n```\n").await;

        assert_eq!(rendered_blocks(&mut cx), 1);
        assert_eq!(hooks.fence_requests()[0].language, "flow");
        assert_eq!(hooks.fence_requests()[0].info, "FLOW");
    }

    #[gpui::test]
    async fn test_editing_the_fence_asks_the_extension_again_with_the_new_content(
        cx: &mut TestAppContext,
    ) {
        let hooks = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, DOCUMENT).await;

        cx.update_buffer(|buffer, cx| {
            let content = buffer
                .text()
                .find("a -> b")
                .expect("the content is in the buffer");
            buffer.edit([(content..content + 6, "x -> y")], None, cx);
        });
        cx.run_until_parked();

        let requests = hooks.fence_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].content, "x -> y\n");
        assert_eq!(rendered_blocks(&mut cx), 1);
    }

    #[gpui::test]
    async fn test_a_failing_extension_leaves_an_error_in_place_of_the_block(
        cx: &mut TestAppContext,
    ) {
        setup(cx, Behavior::Fail);
        let mut cx = editor_showing(cx, DOCUMENT).await;

        assert_eq!(rendered_blocks(&mut cx), 1);
        let states = cached_states(&mut cx);
        assert!(
            matches!(&*states[0], FenceState::Failed(message) if message.contains("the extension trapped")),
            "the error says why"
        );
        draw(&mut cx);
    }

    #[gpui::test]
    async fn test_an_extension_that_keeps_failing_is_disabled_for_further_fences(
        cx: &mut TestAppContext,
    ) {
        setup(cx, Behavior::Fail);
        let mut cx = editor_showing(
            cx,
            "ˇabove\n\n```flow\none\n```\n\n```flow\ntwo\n```\n\n```flow\nthree\n```\n",
        )
        .await;
        assert_eq!(rendered_blocks(&mut cx), 3);

        cx.set_state("ˇabove\n\n```flow\nfour\n```\n");
        cx.run_until_parked();

        let disabled = cached_states(&mut cx).into_iter().any(
            |state| matches!(&*state, FenceState::Failed(message) if message.contains("disabled")),
        );
        assert!(
            disabled,
            "the fourth fence is refused without calling the extension"
        );
    }

    #[gpui::test]
    async fn test_clicking_a_block_puts_the_cursor_in_the_fence_source(cx: &mut TestAppContext) {
        setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, DOCUMENT).await;
        assert_eq!(rendered_blocks(&mut cx), 1);

        cx.update_editor(|editor, window, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let content_start = snapshot
                .text()
                .find("a -> b")
                .expect("the content is there");
            reveal_source(
                editor,
                snapshot.anchor_before(MultiBufferOffset(content_start)),
                window,
                cx,
            );
        });
        assert_eq!(rendered_blocks(&mut cx), 0, "the source is revealed");
        assert_eq!(
            border_blocks(&mut cx),
            1,
            "the cursor at the start of the content touches the opening line, so only the closing fence keeps its border"
        );
        cx.assert_editor_state("text above\n\n```flow\nˇa -> b\n```\n\ntext below\n");
    }
}
