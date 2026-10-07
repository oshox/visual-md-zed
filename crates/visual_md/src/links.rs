//! Links in Markdown text: finding the one under the pointer, and deciding
//! where it leads, which is for extensions to say when they declared the link's
//! scheme or `[[wikilinks]]`.
//!
//! A link written `[text](destination)` shows only its text, so the editor's own
//! detection of URLs, which looks at the text under the pointer, never sees its
//! destination. Resolving the link here is what makes it clickable.

use std::ops::Range;
use std::sync::Arc;

use editor::hover_links::{HoverAction, HoverLink, ResolvedFileTarget};
use extension::{VisualMdLinkRequest, VisualMdLinkStyle, VisualMdLinkTarget, VisualMdOutline};
use gpui::{App, AsyncApp, Entity, Task};
use language::{Anchor, Buffer, Point, ToOffset as _};
use project::{Project, ProjectPath, ResolvedPath};
use workspace::DeploySearch;

use crate::extensions::{HookError, LinkResolvers, VisualMdExtensions, when_not_busy};
use crate::notes::{self, NoteIndex};
use crate::outline;
use crate::plan::parse_blocks;

/// A link on one line of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkInLine {
    /// Where the link is on the line, in bytes.
    pub range: Range<usize>,
    pub style: VisualMdLinkStyle,
    /// The destination of an inline link as written, or the name inside the
    /// brackets of a wikilink.
    pub target: String,
    /// Whether it is written `[[name]]` or `![[name]]`, as opposed to
    /// `[text](destination)` or `![alt](destination)`.
    pub is_wikilink: bool,
}

/// What is on a single line of text, as extensions are told about a document.
fn outline_of_line(line: &str) -> Option<VisualMdOutline> {
    let tree = parse_blocks(line)?;
    Some(outline::outline(line, &tree))
}

/// A `#tag` on one line of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagInLine {
    /// Where it is on the line, in bytes, `#` included.
    pub range: Range<usize>,
    /// Its name, without the `#`.
    pub name: String,
}

/// The tag on `line` that covers byte `offset`, if there is one. Tags in inline
/// code and in links are not tags.
pub fn tag_in_line(line: &str, offset: usize) -> Option<TagInLine> {
    outline_of_line(line)?
        .tags
        .into_iter()
        .find(|tag| tag.range.contains(&offset))
        .map(|tag| TagInLine {
            range: tag.range,
            name: tag.name,
        })
}

/// The link on `line` that covers byte `offset`, if there is one. Links in
/// inline code are not links.
pub fn link_in_line(line: &str, offset: usize) -> Option<LinkInLine> {
    outline_of_line(line)?
        .links
        .into_iter()
        .find(|link| link.range.contains(&offset))
        .map(|link| LinkInLine {
            is_wikilink: line
                .get(link.range.clone())
                .is_some_and(|written| written.starts_with("[[") || written.starts_with("![[")),
            range: link.range,
            style: link.style,
            target: link.target,
        })
}

/// The scheme of a link destination, lowercased: letters, digits, `+`, `-` and
/// `.` starting with a letter, followed by `:`. A single letter is a drive
/// (`C:\notes`), not a scheme.
pub fn scheme_of(destination: &str) -> Option<String> {
    let (scheme, _) = destination.split_once(':')?;
    let mut characters = scheme.chars();
    let is_scheme = scheme.len() >= 2
        && characters
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        });
    is_scheme.then(|| scheme.to_ascii_lowercase())
}

/// The path a relative destination names, without its `#fragment`, and `None`
/// for a destination that is only an anchor or empty.
pub fn local_path(destination: &str) -> Option<String> {
    let path = destination
        .split_once('#')
        .map_or(destination, |(path, _)| path)
        .trim()
        .replace("%20", " ");
    (!path.is_empty()).then_some(path)
}

/// The note a wikilink target names: what is before a `#heading` or `#^block`.
pub fn note_of(target: &str) -> String {
    target
        .split('#')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// What to do about a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Ask these extensions, in order, until one knows the target.
    Extensions {
        extensions: Vec<Arc<str>>,
        request: VisualMdLinkRequest,
    },
    /// Open an address.
    Url(String),
    /// Open a file, relative to the document unless the path is absolute.
    File(String),
    /// Open the note a wikilink names, if the project has it.
    Note(String),
    /// Nothing, because nothing resolves it.
    Nothing,
}

/// Decides what to do about `link` given which extensions resolve what.
pub fn resolve(
    link: &LinkInLine,
    resolvers: &LinkResolvers,
    document_path: Option<&str>,
) -> Resolution {
    let request = |scheme: Option<String>| VisualMdLinkRequest {
        scheme,
        target: link.target.clone(),
        wikilink: link.is_wikilink,
        path: document_path.map(str::to_string),
    };
    if link.is_wikilink {
        let note = note_of(&link.target);
        return if resolvers.wikilinks.is_empty() {
            if note.is_empty() {
                Resolution::Nothing
            } else {
                Resolution::Note(note)
            }
        } else {
            Resolution::Extensions {
                extensions: resolvers.wikilinks.clone(),
                request: request(None),
            }
        };
    }

    match scheme_of(&link.target) {
        Some(scheme) => match resolvers.schemes.get(&scheme) {
            Some(extension) => Resolution::Extensions {
                extensions: vec![extension.clone()],
                request: request(Some(scheme)),
            },
            None if matches!(scheme.as_str(), "http" | "https" | "mailto") => {
                Resolution::Url(link.target.clone())
            }
            None => Resolution::Nothing,
        },
        None => match local_path(&link.target) {
            Some(path) => Resolution::File(path),
            None => Resolution::Nothing,
        },
    }
}

/// The link at `position` for the editor's hover and click handling, when live
/// preview is `active` in the editor.
pub(crate) fn link_at(
    active: bool,
    buffer: &Entity<Buffer>,
    position: Anchor,
    project: Option<&Entity<Project>>,
    note_index: Option<Entity<NoteIndex>>,
    cx: &mut App,
) -> Option<Task<Option<(Range<Anchor>, HoverLink)>>> {
    if !active {
        return None;
    }
    let snapshot = buffer.read(cx).snapshot();
    let offset = position.to_offset(&snapshot);
    let row = snapshot.offset_to_point(offset).row;
    let line_start = snapshot.point_to_offset(Point::new(row, 0));
    let line_end = snapshot.point_to_offset(Point::new(row, snapshot.line_len(row)));
    let line: String = snapshot.text_for_range(line_start..line_end).collect();
    let relative_offset = offset.checked_sub(line_start)?;
    let Some(link) = link_in_line(&line, relative_offset) else {
        return tag_search_link(&line, relative_offset, line_start, &snapshot);
    };
    let range = snapshot.anchor_before(line_start + link.range.start)
        ..snapshot.anchor_after(line_start + link.range.end);

    let resolvers = cx
        .try_global::<VisualMdExtensions>()
        .map(|registry| registry.link_resolvers())
        .unwrap_or_default();
    let document_path = crate::fence_render::buffer_path(buffer, cx);
    let resolution = resolve(&link, &resolvers, document_path.as_deref());
    let from = notes::location_of(buffer.read(cx), cx);
    let buffer = buffer.clone();
    let project = project.cloned();

    match resolution {
        Resolution::Nothing => None,
        Resolution::Note(note) => {
            let link = note_link(&note, note_index.as_ref(), from.as_ref(), cx)?;
            Some(Task::ready(Some((range, link))))
        }
        Resolution::Url(url) => Some(Task::ready(Some((range, HoverLink::Url(url))))),
        Resolution::File(path) => Some(cx.spawn(async move |cx| {
            let link = file_link(&path, &buffer, project.as_ref(), cx).await?;
            Some((range, link))
        })),
        Resolution::Extensions {
            extensions,
            request,
        } => Some(cx.spawn(async move |cx| {
            for extension_id in extensions {
                let answer = when_not_busy(cx, |cx| match cx.try_global::<VisualMdExtensions>() {
                    Some(registry) => registry.resolve_link(&extension_id, request.clone(), cx),
                    None => Task::ready(Err(HookError::NotRegistered(extension_id.clone()))),
                })
                .await;
                match answer {
                    Ok(Some(VisualMdLinkTarget::Url(url))) => {
                        return Some((range, HoverLink::Url(url)));
                    }
                    Ok(Some(VisualMdLinkTarget::File(path))) => {
                        if let Some(link) = file_link(&path, &buffer, project.as_ref(), cx).await {
                            return Some((range, link));
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        log::warn!("extension {extension_id} could not resolve a link: {error}");
                    }
                }
            }
            // No extension knew where a wikilink goes, so it names a note of the project.
            if request.wikilink {
                let note = note_of(&request.target);
                let link =
                    cx.update(|cx| note_link(&note, note_index.as_ref(), from.as_ref(), cx))?;
                return Some((range, link));
            }
            None
        })),
    }
}

/// A link from the tag under the pointer that searches the project for it.
fn tag_search_link(
    line: &str,
    relative_offset: usize,
    line_start: usize,
    snapshot: &language::BufferSnapshot,
) -> Option<Task<Option<(Range<Anchor>, HoverLink)>>> {
    let tag = tag_in_line(line, relative_offset)?;
    let range = snapshot.anchor_before(line_start + tag.range.start)
        ..snapshot.anchor_after(line_start + tag.range.end);
    let query = format!("#{}", tag.name);
    let search = HoverAction::new(move |window, cx| {
        window.dispatch_action(
            Box::new(DeploySearch {
                query: Some(query.clone()),
                ..Default::default()
            }),
            cx,
        );
    });
    Some(Task::ready(Some((range, HoverLink::Action(search)))))
}

/// A link to the note `note` names in the project, if it has one.
fn note_link(
    note: &str,
    note_index: Option<&Entity<NoteIndex>>,
    from: Option<&ProjectPath>,
    cx: &App,
) -> Option<HoverLink> {
    match note_index?.read(cx).resolve(note, from) {
        notes::Resolution::Found(file) => Some(HoverLink::File(ResolvedFileTarget {
            resolved_path: ResolvedPath::ProjectPath {
                project_path: file.project_path(),
                is_dir: false,
            },
            row: None,
            column: None,
        })),
        notes::Resolution::Missing | notes::Resolution::Unknown => None,
    }
}

/// A link to the file at `path`, if the project has it. A relative path is
/// taken from the document's own directory.
async fn file_link(
    path: &str,
    buffer: &Entity<Buffer>,
    project: Option<&Entity<Project>>,
    cx: &mut AsyncApp,
) -> Option<HoverLink> {
    let project = project?;
    let resolved_path = project
        .update(cx, |project, cx| {
            project.resolve_path_in_buffer(path, buffer, cx)
        })
        .await
        .filter(|resolved| resolved.is_file())?;
    Some(HoverLink::File(ResolvedFileTarget {
        resolved_path,
        row: None,
        column: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(target: &str, is_wikilink: bool) -> LinkInLine {
        LinkInLine {
            range: 0..1,
            style: if is_wikilink {
                VisualMdLinkStyle::Wikilink
            } else {
                VisualMdLinkStyle::Inline
            },
            target: target.to_string(),
            is_wikilink,
        }
    }

    #[test]
    fn test_the_link_under_an_offset_is_found() {
        let line = "see [the docs](https://x.org/d) or [[Note|alias]] here";

        let inline = link_in_line(line, 6).expect("offset 6 is in the first link");
        assert_eq!(&line[inline.range.clone()], "[the docs](https://x.org/d)");
        assert_eq!(inline.target, "https://x.org/d");
        assert!(!inline.is_wikilink);
        assert_eq!(inline.style, VisualMdLinkStyle::Inline);

        let wiki = link_in_line(line, 40).expect("offset 40 is in the wikilink");
        assert_eq!(&line[wiki.range.clone()], "[[Note|alias]]");
        assert_eq!(wiki.target, "Note");
        assert!(wiki.is_wikilink);
    }

    #[test]
    fn test_the_tag_under_an_offset_is_found() {
        let line = "a #idea, `#code` and [x](#anchor) #a/b";

        let idea = tag_in_line(line, 4).expect("offset 4 is in #idea");
        assert_eq!(&line[idea.range.clone()], "#idea");
        assert_eq!(idea.name, "idea");
        assert!(tag_in_line(line, 2).is_some(), "the hash is part of it");
        assert!(tag_in_line(line, 7).is_none(), "the comma is not");
        assert!(tag_in_line(line, 11).is_none(), "code is not a tag");
        assert!(tag_in_line(line, 25).is_none(), "an anchor is not a tag");
        assert_eq!(tag_in_line(line, 36).expect("a nested tag").name, "a/b");
    }

    #[test]
    fn test_offsets_outside_links_find_nothing() {
        let line = "see [x](y) now";

        assert!(link_in_line(line, 0).is_none());
        assert!(link_in_line(line, 3).is_none());
        assert!(link_in_line(line, 10).is_none(), "one past the end");
        assert!(link_in_line(line, 12).is_none());
        assert!(link_in_line(line, 99).is_none());
    }

    #[test]
    fn test_the_edges_of_a_link_are_in_it() {
        let line = "a [x](y) b";

        assert!(link_in_line(line, 2).is_some(), "the opening bracket");
        assert!(link_in_line(line, 7).is_some(), "the closing parenthesis");
        assert!(link_in_line(line, 8).is_none());
    }

    #[test]
    fn test_an_embed_is_a_wikilink_but_an_image_is_not() {
        let embed = link_in_line("![[pic.png]]", 4).expect("an embed");
        assert!(embed.is_wikilink);
        assert_eq!(embed.style, VisualMdLinkStyle::Embed);

        let image = link_in_line("![alt](pic.png)", 4).expect("an image");
        assert!(!image.is_wikilink);
        assert_eq!(image.style, VisualMdLinkStyle::Embed);
        assert_eq!(image.target, "pic.png");
    }

    #[test]
    fn test_links_in_code_are_not_found() {
        assert!(link_in_line("`[x](y)` and `[[z]]`", 3).is_none());
        assert!(link_in_line("`[x](y)` and `[[z]]`", 15).is_none());
    }

    #[test]
    fn test_links_are_found_in_lists_and_quotes_and_with_multibyte_text() {
        assert!(link_in_line("- item [x](y)", 9).is_some());
        assert!(link_in_line("> quote [x](y)", 10).is_some());
        let line = "é [[Café]] ü";
        let found = link_in_line(line, 5).expect("inside the wikilink");
        assert_eq!(&line[found.range], "[[Café]]");
    }

    #[test]
    fn test_schemes() {
        assert_eq!(scheme_of("https://x.org").as_deref(), Some("https"));
        assert_eq!(scheme_of("Notes://a").as_deref(), Some("notes"));
        assert_eq!(scheme_of("wiki+v2:page").as_deref(), Some("wiki+v2"));
        assert_eq!(scheme_of("mailto:a@b.c").as_deref(), Some("mailto"));
        assert_eq!(scheme_of("C:\\notes"), None, "a drive");
        assert_eq!(scheme_of("docs/page.md"), None);
        assert_eq!(scheme_of("./a:b.md"), None);
        assert_eq!(scheme_of("1abc://x"), None);
        assert_eq!(scheme_of(":x"), None);
        assert_eq!(scheme_of(""), None);
    }

    #[test]
    fn test_local_paths() {
        assert_eq!(local_path("docs/page.md").as_deref(), Some("docs/page.md"));
        assert_eq!(local_path("page.md#section").as_deref(), Some("page.md"));
        assert_eq!(local_path("my%20notes.md").as_deref(), Some("my notes.md"));
        assert_eq!(local_path("#section"), None);
        assert_eq!(local_path("  "), None);
        assert_eq!(local_path(""), None);
    }

    fn resolvers() -> LinkResolvers {
        LinkResolvers {
            schemes: [("notes".to_string(), Arc::from("notes-ext"))].into(),
            wikilinks: vec![Arc::from("a"), Arc::from("b")],
        }
    }

    #[test]
    fn test_web_links_open_as_addresses_without_any_extension() {
        for target in ["https://x.org/a", "http://x.org", "mailto:a@b.c"] {
            assert_eq!(
                resolve(&link(target, false), &LinkResolvers::default(), None),
                Resolution::Url(target.to_string())
            );
        }
    }

    #[test]
    fn test_relative_paths_open_as_files() {
        assert_eq!(
            resolve(
                &link("docs/a.md#top", false),
                &LinkResolvers::default(),
                None
            ),
            Resolution::File("docs/a.md".to_string())
        );
        assert_eq!(
            resolve(&link("#top", false), &LinkResolvers::default(), None),
            Resolution::Nothing
        );
    }

    #[test]
    fn test_a_declared_scheme_goes_to_its_extension() {
        let resolution = resolve(&link("notes://page", false), &resolvers(), Some("/n/a.md"));

        assert_eq!(
            resolution,
            Resolution::Extensions {
                extensions: vec![Arc::from("notes-ext")],
                request: VisualMdLinkRequest {
                    scheme: Some("notes".to_string()),
                    target: "notes://page".to_string(),
                    wikilink: false,
                    path: Some("/n/a.md".to_string()),
                },
            }
        );
    }

    #[test]
    fn test_other_schemes_are_nobodys() {
        assert_eq!(
            resolve(&link("ftp://x.org", false), &resolvers(), None),
            Resolution::Nothing
        );
    }

    #[test]
    fn test_wikilinks_go_to_the_extensions_that_declared_them_in_order() {
        let resolution = resolve(&link("Note", true), &resolvers(), None);

        assert_eq!(
            resolution,
            Resolution::Extensions {
                extensions: vec![Arc::from("a"), Arc::from("b")],
                request: VisualMdLinkRequest {
                    scheme: None,
                    target: "Note".to_string(),
                    wikilink: true,
                    path: None,
                },
            }
        );
        let nobody = LinkResolvers::default();
        assert_eq!(
            resolve(&link("Note", true), &nobody, None),
            Resolution::Note("Note".to_string()),
            "without an extension a wikilink names a note of the project"
        );
        assert_eq!(
            resolve(&link("Note#Heading", true), &nobody, None),
            Resolution::Note("Note".to_string())
        );
        assert_eq!(
            resolve(&link("#Heading", true), &nobody, None),
            Resolution::Nothing,
            "a heading of the same note has no file to open"
        );
    }
}

#[cfg(test)]
mod integration_tests {
    use editor::test::editor_test_context::EditorTestContext;
    use editor::{Addon as _, Editor, EditorMode, HighlightKey, MultiBuffer};
    use fs::FakeFs;
    use gpui::{AppContext as _, Modifiers, TestAppContext, WindowHandle};
    use project::ResolvedPath;
    use serde_json::json;

    use crate::VisualMdAddon;
    use crate::extensions::LINK_TIMEOUT;
    use crate::extensions::test_support::{Behavior, FakeHooks, register};
    use crate::integration_tests::{init_test, markdown_language};

    use super::*;

    struct Document {
        window: WindowHandle<Editor>,
        buffer: Entity<Buffer>,
        project: Entity<Project>,
    }

    async fn open(
        cx: &mut TestAppContext,
        files: serde_json::Value,
        path: &str,
        markdown: bool,
    ) -> Document {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/dir", files).await;
        let project = Project::test(fs, ["/dir".as_ref()], cx).await;
        let buffer = project
            .update(cx, |project, cx| project.open_local_buffer(path, cx))
            .await
            .expect("the file opens");
        if markdown {
            buffer.update(cx, |buffer, cx| {
                buffer.set_language(Some(markdown_language()), cx)
            });
        }
        let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer.clone(), cx));
        let window = cx.add_window({
            let project = project.clone();
            |window, cx| Editor::new(EditorMode::full(), multi_buffer, Some(project), window, cx)
        });
        window
            .update(cx, |editor, window, cx| crate::refresh(editor, window, cx))
            .expect("the window is open");
        cx.run_until_parked();
        Document {
            window,
            buffer,
            project,
        }
    }

    fn summarize(link: HoverLink) -> String {
        match link {
            HoverLink::Url(url) => format!("url {url}"),
            HoverLink::File(target) => match target.resolved_path {
                ResolvedPath::ProjectPath { project_path, .. } => {
                    format!("file {}", project_path.path.as_unix_str())
                }
                ResolvedPath::AbsPath { path, .. } => format!("file {path}"),
            },
            _ => "other".to_string(),
        }
    }

    /// The link at byte `offset` of the document, as the range it covers and a
    /// description of where it leads.
    async fn link_for(
        cx: &mut TestAppContext,
        document: &Document,
        offset: usize,
    ) -> Option<(Range<usize>, String)> {
        let (range, link) = raw_link_for(cx, document, offset).await?;
        Some((range, summarize(link)))
    }

    async fn raw_link_for(
        cx: &mut TestAppContext,
        document: &Document,
        offset: usize,
    ) -> Option<(Range<usize>, HoverLink)> {
        let buffer = document.buffer.clone();
        let project = document.project.clone();
        let task = document
            .window
            .update(cx, |editor, _window, cx| {
                let position = buffer.read(cx).anchor_before(offset);
                editor
                    .addon::<VisualMdAddon>()
                    .and_then(|addon| addon.link_at(&buffer, position, Some(&project), cx))
            })
            .ok()
            .flatten()?;
        let (range, link) = task.await?;
        let snapshot = document.buffer.read_with(cx, |buffer, _| buffer.snapshot());
        Some((
            range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot),
            link,
        ))
    }

    fn text_of(text: &str, range: Range<usize>) -> &str {
        text.get(range).unwrap_or_default()
    }

    const LINKS: &str = "see [the docs](https://example.com/docs) and [other](other.md#top) and [missing](nope.md) and [anchor](#top) and [ftp](ftp://x.org) and [mail](mailto:a@b.c)\n";

    fn files() -> serde_json::Value {
        json!({ "a.md": LINKS, "other.md": "other", "sub": { "b.md": "sub" } })
    }

    #[gpui::test]
    async fn test_a_web_link_shows_its_range_and_opens_its_address(cx: &mut TestAppContext) {
        init_test(cx);
        let document = open(cx, files(), "/dir/a.md", true).await;
        let offset = LINKS.find("docs").expect("the text has it");

        let (range, link) = link_for(cx, &document, offset).await.expect("a link");

        assert_eq!(
            text_of(LINKS, range),
            "[the docs](https://example.com/docs)"
        );
        assert_eq!(link, "url https://example.com/docs");
    }

    #[gpui::test]
    async fn test_a_relative_link_opens_the_file_it_names(cx: &mut TestAppContext) {
        init_test(cx);
        let document = open(cx, files(), "/dir/a.md", true).await;

        let (range, link) = link_for(cx, &document, LINKS.find("other").unwrap_or(0))
            .await
            .expect("a link");

        assert_eq!(text_of(LINKS, range), "[other](other.md#top)");
        assert_eq!(link, "file other.md");
    }

    #[gpui::test]
    async fn test_a_relative_link_is_taken_from_the_documents_directory(cx: &mut TestAppContext) {
        init_test(cx);
        let document = open(
            cx,
            json!({ "sub": { "b.md": "[up](../other.md) [down](c.md)\n", "c.md": "c" }, "other.md": "o" }),
            "/dir/sub/b.md",
            true,
        )
        .await;

        assert_eq!(
            link_for(cx, &document, 2).await.map(|(_, link)| link),
            Some("file other.md".to_string())
        );
        assert_eq!(
            link_for(cx, &document, 20).await.map(|(_, link)| link),
            Some("file sub/c.md".to_string())
        );
    }

    #[gpui::test]
    async fn test_links_that_lead_nowhere_are_not_links(cx: &mut TestAppContext) {
        init_test(cx);
        let document = open(cx, files(), "/dir/a.md", true).await;

        for name in ["missing", "anchor", "ftp"] {
            let offset = LINKS.find(&format!("[{name}]")).expect("the text has it") + 2;
            assert_eq!(
                link_for(cx, &document, offset).await,
                None,
                "the {name} link"
            );
        }
        let mail = LINKS.find("[mail]").expect("the text has it") + 2;
        assert_eq!(
            link_for(cx, &document, mail).await.map(|(_, link)| link),
            Some("url mailto:a@b.c".to_string())
        );
    }

    #[gpui::test]
    async fn test_text_outside_a_link_and_links_in_code_are_not_links(cx: &mut TestAppContext) {
        init_test(cx);
        let document = open(
            cx,
            json!({ "a.md": "plain `[x](https://y.org)` text\n" }),
            "/dir/a.md",
            true,
        )
        .await;

        assert_eq!(link_for(cx, &document, 2).await, None);
        assert_eq!(link_for(cx, &document, 10).await, None);
    }

    #[gpui::test]
    async fn test_nothing_is_a_link_where_live_preview_is_not_showing(cx: &mut TestAppContext) {
        init_test(cx);
        let document = open(cx, files(), "/dir/a.md", false).await;

        assert_eq!(link_for(cx, &document, 8).await, None);
    }

    fn extension_with_links(
        cx: &mut TestAppContext,
        id: &str,
        links: &str,
        behavior: Behavior,
    ) -> Arc<FakeHooks> {
        let hooks = FakeHooks::new(&cx.executor(), behavior);
        register(
            cx,
            id,
            &format!("[visual_md.links]\n{links}"),
            Some(hooks.clone()),
        );
        hooks
    }

    #[gpui::test]
    async fn test_a_declared_scheme_is_resolved_by_its_extension(cx: &mut TestAppContext) {
        init_test(cx);
        let hooks = extension_with_links(cx, "notes", "schemes = [\"notes\"]\n", Behavior::Succeed);
        hooks.set_link_responder(|request| {
            Some(VisualMdLinkTarget::Url(format!(
                "https://example.com/{}",
                request.target.trim_start_matches("notes://")
            )))
        });
        let text = "see [page](notes://alpha) and [web](https://w.org)\n";
        let document = open(cx, json!({ "a.md": text }), "/dir/a.md", true).await;

        let found = link_for(cx, &document, 6).await.expect("a link");
        assert_eq!(found.1, "url https://example.com/alpha");
        assert_eq!(text_of(text, found.0), "[page](notes://alpha)");

        let requests = hooks.link_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].scheme.as_deref(), Some("notes"));
        assert_eq!(requests[0].target, "notes://alpha");
        assert!(!requests[0].wikilink);
        assert_eq!(requests[0].path.as_deref(), Some("/dir/a.md"));

        let web = text.find("web").expect("the text has it");
        assert_eq!(
            link_for(cx, &document, web).await.map(|(_, link)| link),
            Some("url https://w.org".to_string())
        );
        assert_eq!(
            hooks.link_requests().len(),
            1,
            "web links are not asked about"
        );
    }

    #[gpui::test]
    async fn test_an_extension_may_resolve_a_scheme_to_a_file(cx: &mut TestAppContext) {
        init_test(cx);
        let hooks = extension_with_links(cx, "notes", "schemes = [\"notes\"]\n", Behavior::Succeed);
        hooks.set_link_responder(|_| Some(VisualMdLinkTarget::File("other.md".to_string())));
        let document = open(
            cx,
            json!({ "a.md": "[page](notes://alpha)\n", "other.md": "o" }),
            "/dir/a.md",
            true,
        )
        .await;

        assert_eq!(
            link_for(cx, &document, 2).await.map(|(_, link)| link),
            Some("file other.md".to_string())
        );
    }

    #[gpui::test]
    async fn test_a_scheme_the_extension_does_not_know_is_no_link(cx: &mut TestAppContext) {
        init_test(cx);
        let hooks = extension_with_links(cx, "notes", "schemes = [\"notes\"]\n", Behavior::Succeed);
        let document = open(
            cx,
            json!({ "a.md": "[page](notes://alpha)\n" }),
            "/dir/a.md",
            true,
        )
        .await;

        assert_eq!(link_for(cx, &document, 2).await, None);
        assert_eq!(hooks.link_requests().len(), 1);
    }

    #[gpui::test]
    async fn test_a_failing_extension_leaves_no_link_and_no_panic(cx: &mut TestAppContext) {
        init_test(cx);
        extension_with_links(cx, "notes", "schemes = [\"notes\"]\n", Behavior::Fail);
        let document = open(
            cx,
            json!({ "a.md": "[page](notes://alpha)\n" }),
            "/dir/a.md",
            true,
        )
        .await;

        assert_eq!(link_for(cx, &document, 2).await, None);
    }

    #[gpui::test]
    async fn test_a_slow_extension_is_given_up_on(cx: &mut TestAppContext) {
        init_test(cx);
        extension_with_links(
            cx,
            "notes",
            "schemes = [\"notes\"]\n",
            Behavior::TakeLongerThan(LINK_TIMEOUT * 2),
        );
        let document = open(
            cx,
            json!({ "a.md": "[page](notes://alpha)\n" }),
            "/dir/a.md",
            true,
        )
        .await;
        let buffer = document.buffer.clone();
        let project = document.project.clone();
        let task = document
            .window
            .update(cx, |editor, _window, cx| {
                let position = buffer.read(cx).anchor_before(2);
                editor
                    .addon::<VisualMdAddon>()
                    .and_then(|addon| addon.link_at(&buffer, position, Some(&project), cx))
            })
            .ok()
            .flatten()
            .expect("the scheme is declared, so there is something to wait for");

        cx.executor().advance_clock(LINK_TIMEOUT);

        assert!(task.await.is_none());
    }

    #[gpui::test]
    async fn test_wikilinks_are_asked_of_each_extension_until_one_knows(cx: &mut TestAppContext) {
        init_test(cx);
        let first = extension_with_links(cx, "a-first", "wikilinks = true\n", Behavior::Succeed);
        first.set_link_responder(|_| None);
        let second = extension_with_links(cx, "b-second", "wikilinks = true\n", Behavior::Succeed);
        second.set_link_responder(|request| {
            (request.target == "Other").then(|| VisualMdLinkTarget::File("other.md".to_string()))
        });
        let text = "go [[Other]] or [[Unknown]]\n";
        let document = open(
            cx,
            json!({ "a.md": text, "other.md": "o" }),
            "/dir/a.md",
            true,
        )
        .await;

        let found = link_for(cx, &document, 5).await.expect("a link");
        assert_eq!(found.1, "file other.md");
        assert_eq!(text_of(text, found.0), "[[Other]]");
        assert_eq!(first.link_requests().len(), 1);
        assert!(first.link_requests()[0].wikilink);
        assert_eq!(first.link_requests()[0].scheme, None);
        assert_eq!(second.link_requests().len(), 1);

        let unknown = text.find("Unknown").expect("the text has it");
        assert_eq!(link_for(cx, &document, unknown).await, None);
    }

    #[gpui::test]
    async fn test_clicking_a_tag_searches_the_project_for_it(cx: &mut TestAppContext) {
        init_test(cx);
        let text = "a #idea and `#code` and [x](#anchor) and #nested/tag\n";
        let document = open(cx, json!({ "a.md": text }), "/dir/a.md", true).await;
        let searches = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        cx.update(|cx| {
            let searches = searches.clone();
            cx.on_action(move |search: &DeploySearch, _| {
                searches.borrow_mut().push(search.query.clone());
            });
        });

        let offset = text.find("idea").expect("the text has it");
        let (range, link) = raw_link_for(cx, &document, offset)
            .await
            .expect("a tag is a link");
        assert_eq!(text_of(text, range), "#idea");
        let HoverLink::Action(search) = link else {
            panic!("expected an action, got {link:?}");
        };
        document
            .window
            .update(cx, |_, window, cx| search.run(window, cx))
            .expect("the window is there");
        assert_eq!(*searches.borrow(), vec![Some("#idea".to_string())]);

        let nested = text.find("nested").expect("the text has it");
        let (range, _) = raw_link_for(cx, &document, nested)
            .await
            .expect("a nested tag is a link");
        assert_eq!(text_of(text, range), "#nested/tag");

        let in_code = text.find("code").expect("the text has it");
        assert!(raw_link_for(cx, &document, in_code).await.is_none());
        let in_link = text.find("anchor").expect("the text has it");
        assert!(
            raw_link_for(cx, &document, in_link).await.is_none(),
            "the anchor of a link is not a tag"
        );
    }

    #[gpui::test]
    async fn test_a_wikilink_no_extension_knows_still_opens_the_note(cx: &mut TestAppContext) {
        init_test(cx);
        let extension =
            extension_with_links(cx, "a-first", "wikilinks = true\n", Behavior::Succeed);
        extension.set_link_responder(|_| None);
        let text = "go [[Other]]\n";
        let document = open(
            cx,
            json!({ "a.md": text, "other.md": "o" }),
            "/dir/a.md",
            true,
        )
        .await;

        let found = link_for(cx, &document, 6).await.expect("a link");

        assert_eq!(found.1, "file other.md");
        assert_eq!(
            extension.link_requests().len(),
            1,
            "the extension was asked first"
        );
    }

    #[gpui::test]
    async fn test_a_wikilink_without_an_extension_opens_the_note_of_that_name(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let text = "go [[Other]] or [[Nobody]]\n";
        let document = open(
            cx,
            json!({ "a.md": text, "other.md": "o" }),
            "/dir/a.md",
            true,
        )
        .await;

        let found = link_for(cx, &document, 6).await.expect("a link");
        assert_eq!(found.1, "file other.md");
        assert_eq!(text_of(text, found.0), "[[Other]]");

        let nobody = text.find("Nobody").expect("the text has it");
        assert_eq!(
            link_for(cx, &document, nobody).await,
            None,
            "there is no note to open"
        );
    }

    #[gpui::test]
    async fn test_hovering_and_clicking_a_link_with_its_url_hidden(cx: &mut TestAppContext) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("see [the docs](https://example.com/docs) nowˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));
        cx.run_until_parked();

        let on_text = cx.pixel_position("see [the dˇocs](https://example.com/docs) now\n");
        cx.simulate_mouse_move(on_text, None, Modifiers::secondary_key());
        cx.assert_editor_text_highlights(
            HighlightKey::HoveredLinkState,
            "see «[the docs](https://example.com/docs)ˇ» now\n",
        );
        cx.simulate_click(on_text, Modifiers::secondary_key());
        assert_eq!(cx.opened_url(), Some("https://example.com/docs".into()));
    }
}
