//! Embeds: a line that holds only `![alt](path)` or `![[name]]` becomes a block
//! while the cursor is off it. An image is drawn, a note (or a heading or block
//! of one) is shown as rendered Markdown, and audio, video, PDF and other files
//! are a chip that opens the file.
//!
//! `refresh` only ever reads what is cached and starts loading what is not. A
//! note is read through the project, so an edit to it, in this editor or any
//! other, updates every block showing it.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use editor::display_map::{
    BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId,
};
use editor::{Anchor, Editor, MultiBufferSnapshot};
use gpui::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, Global, ImageSource,
    InteractiveElement as _, IntoElement as _, ObjectFit, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription, Task,
    TaskExt as _, WeakEntity, Window, div, img, px,
};
use language::{Buffer, BufferEvent, LanguageRegistry};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use parking_lot::Mutex;
use project::{Project, ProjectPath};
use theme::ActiveTheme as _;
use util::ResultExt as _;

use crate::VisualMdAddon;
use crate::fence_render::reveal_source;
use crate::note_contents;
use crate::notes::{self, NoteIndex, Resolution};
use crate::plan::{EmbedInfo, EmbedKind, EmbedSize, Plan, Subpath};

/// The rows an image block is first given. The editor measures the block once it
/// is drawn and resizes it, so this only has to be a fair guess.
const IMAGE_BLOCK_ROWS: u32 = 6;
const NOTE_BLOCK_ROWS: u32 = 6;
const CHIP_BLOCK_ROWS: u32 = 2;

/// The tallest an image or an embedded note is drawn, in rows.
const MAX_IMAGE_ROWS: f32 = 30.;
const MAX_NOTE_ROWS: f32 = 24.;

/// The most of a note that is shown in an embed or a preview.
pub(crate) const MAX_NOTE_BYTES: usize = 100 * 1024;

const NOTE_CACHE_CAPACITY: usize = 64;

/// How many parent directories an `![[embed]]` name is searched through,
/// nearest first, approximating Obsidian's vault-wide lookup by name.
const FILE_SEARCH_DEPTH: usize = 8;

/// The part of a note one embed shows, which is what two embeds have to share
/// for one load of it to serve both.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct NoteKey {
    path: ProjectPath,
    subpath: Option<Subpath>,
}

/// Everything a block depends on. Blocks are kept while their range and key are
/// unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum EmbedKey {
    Image {
        source: String,
        size: Option<EmbedSize>,
    },
    Note(NoteKey),
    File {
        kind: EmbedKind,
        name: String,
        path: PathBuf,
    },
    Missing(String),
}

/// A block standing in for an embed line.
pub(crate) struct EmbedBlock {
    pub range: Range<usize>,
    pub key: EmbedKey,
    pub id: CustomBlockId,
}

/// What a note embed shows.
pub(crate) enum NoteState {
    Loading,
    Failed(SharedString),
    Ready {
        markdown: Entity<Markdown>,
        /// Where relative paths in the note, such as its images, are from.
        directory: Option<PathBuf>,
        truncated: bool,
    },
}

/// One note, heading or block's load and result, shared by every block showing it.
pub(crate) struct NoteEntry {
    state: ArcSwap<NoteState>,
    /// The editors showing this, to repaint when it changes.
    waiters: Mutex<Vec<WeakEntity<Editor>>>,
    load: Mutex<Option<Task<()>>>,
}

impl NoteEntry {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: ArcSwap::from_pointee(NoteState::Loading),
            waiters: Mutex::new(Vec::new()),
            load: Mutex::new(None),
        })
    }

    pub(crate) fn state(&self) -> Arc<NoteState> {
        self.state.load_full()
    }

    fn add_waiter(&self, editor: &WeakEntity<Editor>) {
        let mut waiters = self.waiters.lock();
        waiters.retain(|waiter| waiter.upgrade().is_some());
        if !waiters
            .iter()
            .any(|waiter| waiter.entity_id() == editor.entity_id())
        {
            waiters.push(editor.clone());
        }
    }

    fn finish(&self, state: NoteState, cx: &mut App) {
        self.state.store(Arc::new(state));
        let waiters = self.waiters.lock().clone();
        for waiter in waiters {
            waiter.update(cx, |_, cx| cx.notify()).log_err();
        }
    }

    /// Reads the part of `buffer` this entry shows and makes it the state.
    fn show(
        &self,
        buffer: &Entity<Buffer>,
        subpath: Option<&Subpath>,
        language_registry: Option<Arc<LanguageRegistry>>,
        cx: &mut App,
    ) {
        let text = buffer.read(cx).text();
        let directory = buffer
            .read(cx)
            .file()
            .and_then(|file| file.as_local())
            .and_then(|file| file.abs_path(cx).parent().map(Path::to_path_buf));
        let Some(section) = note_contents::section(&text, subpath) else {
            let what = match subpath {
                Some(Subpath::Heading(name)) => format!("Heading not found: {name}"),
                Some(Subpath::Block(id)) => format!("Block not found: ^{id}"),
                None => "Note not found".to_string(),
            };
            self.finish(NoteState::Failed(what.into()), cx);
            return;
        };
        let (source, truncated) = note_contents::preview_markdown(&section, MAX_NOTE_BYTES);

        let existing = match &*self.state() {
            NoteState::Ready { markdown, .. } => Some(markdown.clone()),
            _ => None,
        };
        let markdown = match existing {
            Some(markdown) => {
                markdown.update(cx, |markdown, cx| markdown.replace(source, cx));
                markdown
            }
            None => cx.new(|cx| Markdown::new(source.into(), language_registry, None, cx)),
        };
        self.finish(
            NoteState::Ready {
                markdown,
                directory,
                truncated,
            },
            cx,
        );
    }
}

struct CacheItem {
    entry: Arc<NoteEntry>,
    /// Held so the buffer stays open while the entry is cached, and so the
    /// entry hears about its edits.
    _buffer: Option<Entity<Buffer>>,
    _subscription: Option<Subscription>,
}

/// The notes any editor is showing in embeds, the most recently used 64 of them.
#[derive(Default)]
pub(crate) struct EmbedCache {
    items: HashMap<NoteKey, CacheItem>,
    /// Keys from least to most recently used.
    recency: VecDeque<NoteKey>,
}

impl Global for EmbedCache {}

impl EmbedCache {
    /// The entry for `key`, and whether this call created it, in which case the
    /// caller must start loading it.
    fn entry(&mut self, key: &NoteKey) -> (Arc<NoteEntry>, bool) {
        if let Some(item) = self.items.get(key) {
            let entry = item.entry.clone();
            if let Some(position) = self.recency.iter().position(|recent| recent == key)
                && let Some(recent) = self.recency.remove(position)
            {
                self.recency.push_back(recent);
            }
            return (entry, false);
        }

        let entry = NoteEntry::new();
        self.items.insert(
            key.clone(),
            CacheItem {
                entry: entry.clone(),
                _buffer: None,
                _subscription: None,
            },
        );
        self.recency.push_back(key.clone());
        while self.items.len() > NOTE_CACHE_CAPACITY {
            let Some(oldest) = self.recency.pop_front() else {
                break;
            };
            self.items.remove(&oldest);
        }
        (entry, true)
    }

    fn attach(&mut self, key: &NoteKey, buffer: Entity<Buffer>, subscription: Subscription) {
        if let Some(item) = self.items.get_mut(key) {
            item._buffer = Some(buffer);
            item._subscription = Some(subscription);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }
}

/// What a block draws.
#[derive(Clone)]
enum Content {
    Image {
        source: String,
        size: Option<EmbedSize>,
    },
    Note {
        entry: Arc<NoteEntry>,
        title: SharedString,
        path: ProjectPath,
    },
    File {
        kind: EmbedKind,
        name: SharedString,
        path: PathBuf,
    },
    Missing(SharedString),
}

struct Wanted {
    range: Range<usize>,
    key: EmbedKey,
    content: Content,
}

/// Where the note being edited is, for resolving what its embeds name.
struct Surroundings {
    /// The folder of the note on disk, when it is a local file.
    directory: Option<PathBuf>,
    location: Option<ProjectPath>,
    project: Option<Entity<Project>>,
    index: Option<Entity<NoteIndex>>,
}

impl Surroundings {
    fn of(editor: &Editor, cx: &App) -> Self {
        let buffer = editor.buffer().read(cx).as_singleton();
        let directory = buffer
            .as_ref()
            .and_then(|buffer| {
                let local_file = buffer.read(cx).file()?.as_local()?;
                Some(local_file.abs_path(cx))
            })
            .and_then(|path| path.parent().map(Path::to_path_buf));
        let location = buffer
            .as_ref()
            .and_then(|buffer| notes::location_of(buffer.read(cx), cx));
        let index = editor
            .addon::<VisualMdAddon>()
            .and_then(|addon| addon.note_index.as_ref())
            .map(|(index, _)| index.clone());
        Self {
            directory,
            location,
            project: editor.project().cloned(),
            index,
        }
    }
}

/// Turns the target of a file embed into a path that can be opened, or `None` if
/// it can't be resolved at all. A relative path is joined onto the note's own
/// directory; an unsaved buffer has no directory to join onto. A `![[name]]` is
/// also looked for in the folders above the note.
fn resolve_file(
    embed: &EmbedInfo,
    note_directory: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> Option<String> {
    let target = embed.target.as_str();
    if target.starts_with("http://") || target.starts_with("https://") {
        return Some(target.to_string());
    }
    let target_path = Path::new(target);
    if target_path.is_absolute() {
        return Some(target.to_string());
    }
    let note_directory = note_directory?;
    if embed.is_wikilink {
        let found = note_directory
            .ancestors()
            .take(FILE_SEARCH_DEPTH)
            .map(|directory| directory.join(target_path))
            .find(|candidate| exists(candidate));
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

/// Whether a file is there: looked up in the project's worktree when the path is
/// inside one, which needs no disk access, and on disk otherwise.
fn file_exists(project: Option<&Entity<Project>>, path: &Path, cx: &App) -> bool {
    match project.and_then(|project| project.read(cx).find_worktree(path, cx)) {
        Some((worktree, relative)) => worktree.read(cx).entry_for_path(&relative).is_some(),
        None => path.exists(),
    }
}

fn file_name(target: &str) -> String {
    target
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(target)
        .to_string()
}

fn wanted_embed(
    embed: &EmbedInfo,
    surroundings: &Surroundings,
    editor: &WeakEntity<Editor>,
    cx: &mut App,
) -> Option<Wanted> {
    let range = embed.range.clone();
    match embed.kind {
        EmbedKind::Image => {
            let project = surroundings.project.as_ref();
            let source = resolve_file(embed, surroundings.directory.as_deref(), |path| {
                file_exists(project, path, cx)
            })?;
            Some(Wanted {
                range,
                key: EmbedKey::Image {
                    source: source.clone(),
                    size: embed.size,
                },
                content: Content::Image {
                    source,
                    size: embed.size,
                },
            })
        }
        EmbedKind::Audio | EmbedKind::Video | EmbedKind::Pdf | EmbedKind::Other => {
            let name = file_name(&embed.target);
            let project = surroundings.project.as_ref();
            let source = resolve_file(embed, surroundings.directory.as_deref(), |path| {
                file_exists(project, path, cx)
            })?;
            let path = PathBuf::from(&source);
            if source.starts_with("http://") || source.starts_with("https://") {
                return Some(Wanted {
                    range,
                    key: EmbedKey::File {
                        kind: embed.kind,
                        name: name.clone(),
                        path: path.clone(),
                    },
                    content: Content::File {
                        kind: embed.kind,
                        name: name.into(),
                        path,
                    },
                });
            }
            if !file_exists(project, &path, cx) {
                let message = format!("File not found: {name}");
                return Some(Wanted {
                    range,
                    key: EmbedKey::Missing(message.clone()),
                    content: Content::Missing(message.into()),
                });
            }
            Some(Wanted {
                range,
                key: EmbedKey::File {
                    kind: embed.kind,
                    name: name.clone(),
                    path: path.clone(),
                },
                content: Content::File {
                    kind: embed.kind,
                    name: name.into(),
                    path,
                },
            })
        }
        EmbedKind::Note => {
            let project = surroundings.project.as_ref()?;
            let path = if embed.target.is_empty() {
                surroundings.location.clone()?
            } else {
                let index = surroundings.index.as_ref()?;
                match index
                    .read(cx)
                    .resolve(&embed.target, surroundings.location.as_ref())
                {
                    Resolution::Found(file) => file.project_path(),
                    Resolution::Missing => {
                        let message = format!("Note not found: {}", embed.target);
                        return Some(Wanted {
                            range,
                            key: EmbedKey::Missing(message.clone()),
                            content: Content::Missing(message.into()),
                        });
                    }
                    Resolution::Unknown => return None,
                }
            };

            // A name with another extension is a file of the project, not a note.
            let extension = path.path.extension().unwrap_or_default().to_string();
            if EmbedKind::for_extension(&extension) != Some(EmbedKind::Note)
                && !extension.is_empty()
            {
                let absolute = project.read(cx).absolute_path(&path, cx)?;
                let kind = EmbedKind::for_extension(&extension).unwrap_or(EmbedKind::Other);
                let name = file_name(path.path.as_unix_str());
                return Some(Wanted {
                    range,
                    key: EmbedKey::File {
                        kind,
                        name: name.clone(),
                        path: absolute.clone(),
                    },
                    content: Content::File {
                        kind,
                        name: name.into(),
                        path: absolute,
                    },
                });
            }

            let key = NoteKey {
                path: path.clone(),
                subpath: embed.subpath.clone(),
            };
            let (entry, created) = cx.default_global::<EmbedCache>().entry(&key);
            entry.add_waiter(editor);
            if created {
                start_loading(&entry, project, key.clone(), cx);
            }
            let stem = path
                .path
                .file_name()
                .map(|name| name.rsplit_once('.').map_or(name, |(stem, _)| stem))
                .unwrap_or_default()
                .to_string();
            let title = match &embed.subpath {
                Some(Subpath::Heading(heading)) => format!("{stem} > {heading}"),
                Some(Subpath::Block(id)) => format!("{stem} > ^{id}"),
                None => stem,
            };
            Some(Wanted {
                range,
                key: EmbedKey::Note(key),
                content: Content::Note {
                    entry,
                    title: title.into(),
                    path,
                },
            })
        }
    }
}

/// Opens the note and shows it in `entry`, then keeps showing it as it changes.
fn start_loading(entry: &Arc<NoteEntry>, project: &Entity<Project>, key: NoteKey, cx: &mut App) {
    let language_registry = project.read(cx).languages().clone();
    let open = project.update(cx, |project, cx| project.open_buffer(key.path.clone(), cx));
    let task = cx.spawn({
        let entry = entry.clone();
        async move |cx| match open.await {
            Ok(buffer) => {
                cx.update(|cx| {
                    entry.show(
                        &buffer,
                        key.subpath.as_ref(),
                        Some(language_registry.clone()),
                        cx,
                    );
                    let subscription = cx.subscribe(&buffer, {
                        let entry = entry.clone();
                        let subpath = key.subpath.clone();
                        let buffer = buffer.clone();
                        move |_, event: &BufferEvent, cx| {
                            if matches!(event, BufferEvent::Edited { .. }) {
                                entry.show(
                                    &buffer,
                                    subpath.as_ref(),
                                    Some(language_registry.clone()),
                                    cx,
                                );
                            }
                        }
                    });
                    cx.default_global::<EmbedCache>()
                        .attach(&key, buffer.clone(), subscription);
                });
            }
            Err(error) => {
                log::warn!("could not open {:?} to embed it: {error:#}", key.path);
                cx.update(|cx| {
                    entry.finish(
                        NoteState::Failed(format!("Could not open the note: {error:#}").into()),
                        cx,
                    )
                });
            }
        }
    });
    *entry.load.lock() = Some(task);
}

/// Diffs `computed.embeds` against the blocks inserted by the previous refresh,
/// keyed on what each block draws as well as on its range.
pub(crate) fn apply_embeds(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    computed: &Plan,
    cx: &mut Context<Editor>,
) {
    let surroundings = Surroundings::of(editor, cx);
    let editor_handle = cx.weak_entity();
    let wanted: Vec<Wanted> = computed
        .embeds
        .iter()
        .filter_map(|embed| wanted_embed(embed, &surroundings, &editor_handle, cx))
        .collect();

    let previous = editor
        .addon_mut::<VisualMdAddon>()
        .map(|addon| std::mem::take(&mut addon.embed_blocks))
        .unwrap_or_default();

    let wanted_keys: std::collections::HashSet<(&Range<usize>, &EmbedKey)> = wanted
        .iter()
        .map(|embed| (&embed.range, &embed.key))
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

    let new_embeds: Vec<&Wanted> = wanted
        .iter()
        .filter(|embed| {
            !kept
                .iter()
                .any(|block| block.range == embed.range && block.key == embed.key)
        })
        .collect();
    if !new_embeds.is_empty() {
        let properties: Vec<BlockProperties<Anchor>> = new_embeds
            .iter()
            .map(|embed| {
                let anchors = crate::to_anchor_range(snapshot, &embed.range);
                let start = anchors.start;
                let content = embed.content.clone();
                let editor_handle = editor_handle.clone();
                BlockProperties {
                    placement: BlockPlacement::Replace(anchors.start..=anchors.end),
                    height: Some(match &embed.content {
                        Content::Image { .. } => IMAGE_BLOCK_ROWS,
                        Content::Note { .. } => NOTE_BLOCK_ROWS,
                        Content::File { .. } | Content::Missing(_) => CHIP_BLOCK_ROWS,
                    }),
                    style: BlockStyle::Fixed,
                    render: Arc::new(move |cx: &mut BlockContext| {
                        render_embed(&content, &editor_handle, start, cx)
                    }),
                    priority: 0,
                }
            })
            .collect();
        let ids = editor.insert_blocks(properties, None, cx);
        kept.extend(
            new_embeds
                .into_iter()
                .zip(ids)
                .map(|(embed, id)| EmbedBlock {
                    range: embed.range.clone(),
                    key: embed.key.clone(),
                    id,
                }),
        );
    }

    if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
        addon.embed_blocks = kept;
    }
}

fn render_embed(
    content: &Content,
    editor: &WeakEntity<Editor>,
    start: Anchor,
    cx: &mut BlockContext,
) -> AnyElement {
    match content {
        // An image has always been revealed by the editor's own handling of a
        // click in a block, and keeps that.
        Content::Image { source, size } => render_image(source, *size, cx),
        Content::Note { entry, title, path } => {
            let body = render_note(entry, title, path, editor, cx);
            revealing(body, editor, start, cx)
        }
        Content::File { kind, name, path } => {
            let body = render_file(*kind, name, path, cx);
            revealing(body, editor, start, cx)
        }
        Content::Missing(message) => {
            let body = message_box(message.clone(), cx);
            revealing(body, editor, start, cx)
        }
    }
}

/// Wraps `body` so that clicking it puts the cursor in the embed's line, which
/// shows its source instead of the block.
fn revealing(
    body: AnyElement,
    editor: &WeakEntity<Editor>,
    start: Anchor,
    cx: &mut BlockContext,
) -> AnyElement {
    let editor = editor.clone();
    div()
        .id(ElementId::from(cx.block_id))
        .w(cx.max_width)
        .on_click(move |_, window, cx| {
            if let Some(editor) = editor.upgrade() {
                editor.update(cx, |editor, cx| reveal_source(editor, start, window, cx));
            }
        })
        .child(body)
        .into_any_element()
}

fn message_box(message: SharedString, cx: &mut BlockContext) -> AnyElement {
    let (border, muted) = {
        let colors = cx.app.theme().colors();
        (colors.border, colors.text_muted)
    };
    div()
        .px_2()
        .py_1()
        .border_1()
        .border_color(border)
        .rounded_md()
        .text_color(muted)
        .child(message)
        .into_any_element()
}

fn render_image(source: &str, size: Option<EmbedSize>, cx: &mut BlockContext) -> AnyElement {
    let (border, muted) = {
        let colors = cx.app.theme().colors();
        (colors.border, colors.text_muted)
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
    let loading = move || {
        div()
            .h(line_height * 2.)
            .text_color(muted)
            .child(SharedString::from("Loading image…"))
            .into_any_element()
    };
    let image_source: ImageSource =
        if source.starts_with("http://") || source.starts_with("https://") {
            ImageSource::from(SharedString::from(source.to_string()))
        } else {
            ImageSource::from(PathBuf::from(source))
        };
    let mut image = img(image_source)
        .max_w_full()
        .max_h(line_height * MAX_IMAGE_ROWS)
        .object_fit(ObjectFit::Contain)
        .with_fallback(unavailable)
        .with_loading(loading);
    if let Some(size) = size {
        image = image.w(px(size.width as f32));
        if let Some(height) = size.height {
            image = image.h(px(height as f32));
        }
    }
    div().w(cx.max_width).flex().child(image).into_any_element()
}

fn render_note(
    entry: &NoteEntry,
    title: &SharedString,
    path: &ProjectPath,
    editor: &WeakEntity<Editor>,
    cx: &mut BlockContext,
) -> AnyElement {
    let (border, muted, background) = {
        let colors = cx.app.theme().colors();
        (colors.border, colors.text_muted, colors.surface_background)
    };
    let line_height = cx.line_height;

    let body = match &*entry.state() {
        NoteState::Loading => div()
            .text_color(muted)
            .child(SharedString::from("Loading…"))
            .into_any_element(),
        NoteState::Failed(message) => div()
            .text_color(muted)
            .child(message.clone())
            .into_any_element(),
        NoteState::Ready {
            markdown,
            directory,
            truncated,
        } => {
            let directory = directory.clone();
            let element = MarkdownElement::new(
                markdown.clone(),
                MarkdownStyle::themed(MarkdownFont::Preview, cx.window, cx.app),
            )
            .image_resolver(move |url, _| {
                if url.starts_with("http://") || url.starts_with("https://") {
                    return Some(ImageSource::from(SharedString::from(url.to_string())));
                }
                let target = Path::new(url);
                if target.is_absolute() {
                    return Some(ImageSource::from(target.to_path_buf()));
                }
                directory
                    .as_ref()
                    .map(|directory| ImageSource::from(directory.join(target)))
            });
            let mut column = div().flex().flex_col().child(element);
            if *truncated {
                column = column.child(
                    div()
                        .text_color(muted)
                        .child(SharedString::from("… open the note to see all of it")),
                );
            }
            column.into_any_element()
        }
    };

    let open = {
        let editor = editor.clone();
        let path = path.clone();
        div()
            .id(ElementId::from((ElementId::from(cx.block_id), "open")))
            .cursor_pointer()
            .text_color(muted)
            .child(SharedString::from("Open"))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                if let Some(editor) = editor.upgrade() {
                    open_note(&editor, path.clone(), window, cx);
                }
            })
    };

    div()
        .w(cx.max_width)
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .border_1()
        .border_color(border)
        .rounded_md()
        .bg(background)
        .child(
            div()
                .flex()
                .justify_between()
                .text_color(muted)
                .child(title.clone())
                .child(open),
        )
        .child(
            div()
                .max_h(line_height * MAX_NOTE_ROWS)
                .overflow_hidden()
                .child(body),
        )
        .into_any_element()
}

fn render_file(
    kind: EmbedKind,
    name: &SharedString,
    path: &Path,
    cx: &mut BlockContext,
) -> AnyElement {
    let (border, muted, background) = {
        let colors = cx.app.theme().colors();
        (colors.border, colors.text_muted, colors.surface_background)
    };
    let label = match kind {
        EmbedKind::Audio => "Audio",
        EmbedKind::Video => "Video",
        EmbedKind::Pdf => "PDF",
        _ => "File",
    };
    let path = path.to_path_buf();
    let open = div()
        .id(ElementId::from((ElementId::from(cx.block_id), "open")))
        .cursor_pointer()
        .text_color(muted)
        .child(SharedString::from("Open"))
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            cx.open_with_system(&path);
        });
    div()
        .w(cx.max_width)
        .flex()
        .justify_between()
        .px_2()
        .py_1()
        .border_1()
        .border_color(border)
        .rounded_md()
        .bg(background)
        .child(
            div()
                .flex()
                .gap_2()
                .child(div().text_color(muted).child(SharedString::from(label)))
                .child(name.clone()),
        )
        .child(open)
        .into_any_element()
}

/// Opens `path` in the workspace the editor is in.
fn open_note(editor: &Entity<Editor>, path: ProjectPath, window: &mut Window, cx: &mut App) {
    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        workspace
            .open_path(path, None, true, window, cx)
            .detach_and_log_err(cx);
    });
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, WindowHandle};
    use serde_json::json;

    use crate::integration_tests::{editor_in_project, init_test};

    use super::*;

    fn keys(window: &WindowHandle<Editor>, cx: &mut TestAppContext) -> Vec<EmbedKey> {
        window
            .read_with(cx, |editor, _| {
                editor
                    .addon::<VisualMdAddon>()
                    .map(|addon| addon.embed_blocks.iter().map(|b| b.key.clone()).collect())
                    .unwrap_or_default()
            })
            .expect("the window is open")
    }

    /// The text each cached note currently shows, or its failure message.
    fn shown(cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            let entries: Vec<Arc<NoteEntry>> = cx
                .try_global::<EmbedCache>()
                .map(|cache| {
                    cache
                        .items
                        .values()
                        .map(|item| item.entry.clone())
                        .collect()
                })
                .unwrap_or_default();
            entries
                .iter()
                .map(|entry| match &*entry.state() {
                    NoteState::Loading => "loading".to_string(),
                    NoteState::Failed(message) => format!("failed: {message}"),
                    NoteState::Ready { markdown, .. } => markdown.read(cx).source().to_string(),
                })
                .collect()
        })
    }

    async fn open(
        cx: &mut TestAppContext,
        files: serde_json::Value,
        path: &str,
    ) -> WindowHandle<Editor> {
        init_test(cx);
        let window = editor_in_project(cx, files, path).await;
        cx.run_until_parked();
        window
    }

    #[gpui::test]
    async fn test_an_image_embed_keeps_its_size_and_a_chip_its_name(cx: &mut TestAppContext) {
        let window = open(
            cx,
            json!({
                "a.md": "![cat|300x200](pics/cat.png)\n\n![[song.mp3]]\n\n![[Gone.pdf]]\n\nafter",
                "song.mp3": "",
            }),
            "/dir/a.md",
        )
        .await;

        let keys = keys(&window, cx);
        assert_eq!(keys.len(), 3, "{keys:?}");
        assert!(keys.contains(&EmbedKey::Image {
            source: "/dir/pics/cat.png".to_string(),
            size: Some(EmbedSize {
                width: 300,
                height: Some(200)
            }),
        }));
        assert!(keys.contains(&EmbedKey::File {
            kind: EmbedKind::Audio,
            name: "song.mp3".to_string(),
            path: PathBuf::from("/dir/song.mp3"),
        }));
        assert!(keys.contains(&EmbedKey::Missing("File not found: Gone.pdf".to_string())));
    }

    #[gpui::test]
    async fn test_a_note_embed_shows_the_note_and_follows_its_edits(cx: &mut TestAppContext) {
        let window = open(
            cx,
            json!({
                "a.md": "![[Other]]\n\nafter",
                "Other.md": "---\ntitle: x\n---\n# Other\nsome text\n",
            }),
            "/dir/a.md",
        )
        .await;

        assert!(matches!(keys(&window, cx).as_slice(), [EmbedKey::Note(_)]));
        assert_eq!(shown(cx), ["# Other\nsome text"]);

        let project = window
            .update(cx, |editor, _, _| editor.project().cloned())
            .ok()
            .flatten()
            .expect("the editor has a project");
        let buffer = project
            .update(cx, |project, cx| {
                let path = ProjectPath {
                    worktree_id: project
                        .visible_worktrees(cx)
                        .next()
                        .expect("a worktree")
                        .read(cx)
                        .id(),
                    path: util::rel_path::rel_path("Other.md").into(),
                };
                project.open_buffer(path, cx)
            })
            .await
            .expect("the note opens");
        buffer.update(cx, |buffer, cx| {
            let end = buffer.len();
            buffer.edit([(end..end, "more text\n")], None, cx);
        });
        cx.run_until_parked();

        assert_eq!(shown(cx), ["# Other\nsome text\nmore text"]);
    }

    #[gpui::test]
    async fn test_a_heading_or_a_block_is_embedded_alone(cx: &mut TestAppContext) {
        let window = open(
            cx,
            json!({
                "a.md": "![[Other#Part]]\n\n![[Other#^one]]\n\n![[Other#Nope]]\n\nafter",
                "Other.md": "# Top\nintro\n\n## Part\nin part\n\n## Next\nelsewhere ^one\n",
            }),
            "/dir/a.md",
        )
        .await;

        assert_eq!(keys(&window, cx).len(), 3);
        let mut shown = shown(cx);
        shown.sort();
        assert_eq!(
            shown,
            [
                "## Part\nin part",
                "elsewhere",
                "failed: Heading not found: Nope"
            ]
        );
    }

    #[gpui::test]
    async fn test_a_note_that_embeds_itself_shows_its_own_embed_as_a_name(cx: &mut TestAppContext) {
        let window = open(
            cx,
            json!({ "a.md": "# Loop\n\n![[a]]\n\nafter" }),
            "/dir/a.md",
        )
        .await;

        assert_eq!(keys(&window, cx).len(), 1);
        assert_eq!(shown(cx), ["# Loop\n\n*↳ a*\n\nafter"]);
    }

    #[gpui::test]
    async fn test_two_embeds_of_the_same_part_share_one_load(cx: &mut TestAppContext) {
        let window = open(
            cx,
            json!({
                "a.md": "![[Other]]\n\n![[Other]]\n\nafter",
                "Other.md": "text",
            }),
            "/dir/a.md",
        )
        .await;

        assert_eq!(keys(&window, cx).len(), 2);
        assert_eq!(cx.update(|cx| cx.global::<EmbedCache>().len()), 1);
    }

    #[gpui::test]
    async fn test_a_missing_note_says_so_and_a_long_one_is_cut(cx: &mut TestAppContext) {
        let long = format!("{}\n", "a line of text\n".repeat(MAX_NOTE_BYTES / 10));
        let window = open(
            cx,
            json!({ "a.md": "![[Nothing]]\n\n![[Long]]\n\nafter", "Long.md": long }),
            "/dir/a.md",
        )
        .await;

        let keys = keys(&window, cx);
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&EmbedKey::Missing("Note not found: Nothing".to_string())));
        let truncated = cx.update(|cx| {
            let cache = cx.global::<EmbedCache>();
            cache.items.values().all(|item| match &*item.entry.state() {
                NoteState::Ready {
                    truncated,
                    markdown,
                    ..
                } => *truncated && markdown.read(cx).source().len() <= MAX_NOTE_BYTES,
                _ => false,
            })
        });
        assert!(truncated);
    }

    #[gpui::test]
    async fn test_a_touched_embed_line_shows_its_source_not_a_block(cx: &mut TestAppContext) {
        let window = open(
            cx,
            json!({ "a.md": "![[Other]]\n\nafter", "Other.md": "text" }),
            "/dir/a.md",
        )
        .await;
        assert_eq!(keys(&window, cx).len(), 1);

        window
            .update(cx, |editor, window, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([
                        editor::MultiBufferOffset(3)..editor::MultiBufferOffset(3)
                    ]);
                });
                crate::refresh(editor, window, cx);
            })
            .expect("the window is open");
        cx.run_until_parked();

        assert!(keys(&window, cx).is_empty());
    }

    /// The height in rows of every block, in document order.
    fn block_heights(window: &WindowHandle<Editor>, cx: &mut TestAppContext) -> Vec<u32> {
        window
            .update(cx, |editor, window, cx| {
                let snapshot = editor.snapshot(window, cx);
                snapshot
                    .blocks_in_range(
                        editor::display_map::DisplayRow(0)..editor::display_map::DisplayRow(500),
                    )
                    .map(|(_, block)| block.height())
                    .collect()
            })
            .expect("the window is open")
    }

    #[gpui::test]
    async fn test_a_note_embed_block_is_as_tall_as_the_note_and_no_taller_than_the_cap(
        cx: &mut TestAppContext,
    ) {
        let long = "a line of text\n\n".repeat(200);
        let window = open(
            cx,
            json!({
                "a.md": "![[Short]]\n\n![[Long]]\n\nafter",
                "Short.md": "one line",
                "Long.md": long,
            }),
            "/dir/a.md",
        )
        .await;

        let heights = block_heights(&window, cx);
        assert_eq!(heights.len(), 2, "{heights:?}");
        let (short, long) = (heights[0].min(heights[1]), heights[0].max(heights[1]));
        assert_ne!(short, NOTE_BLOCK_ROWS, "the estimate was never replaced");
        assert!(short < long, "{heights:?}");
        assert!(
            long <= MAX_NOTE_ROWS as u32 + 4,
            "a cap of {MAX_NOTE_ROWS} rows and a header: {heights:?}"
        );
    }

    #[gpui::test]
    async fn test_an_image_block_is_measured_and_not_a_fixed_ten_rows(cx: &mut TestAppContext) {
        let window = open(
            cx,
            json!({ "a.md": "![](missing.png)\n\nafter" }),
            "/dir/a.md",
        )
        .await;

        let heights = block_heights(&window, cx);
        assert_eq!(heights.len(), 1);
        assert_ne!(
            heights[0], IMAGE_BLOCK_ROWS,
            "the estimate was never replaced"
        );
        assert!(
            heights[0] < 6,
            "a message box is a few rows, not a picture's box: {heights:?}"
        );
    }

    #[gpui::test]
    async fn test_a_note_with_another_extension_in_its_name_is_still_a_note(
        cx: &mut TestAppContext,
    ) {
        let window = open(
            cx,
            json!({
                "a.md": "![[Notes 1.2]]\n\n![[data.csv]]\n\nafter",
                "Notes 1.2.md": "versioned",
                "data.csv": "a,b",
            }),
            "/dir/a.md",
        )
        .await;

        let keys = keys(&window, cx);
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert!(matches!(keys[0], EmbedKey::Note(_)));
        assert!(
            matches!(&keys[1], EmbedKey::File { kind: EmbedKind::Other, name, .. } if name == "data.csv"),
            "{:?}",
            keys[1]
        );
    }
}
