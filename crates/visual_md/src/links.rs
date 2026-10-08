//! Links in Markdown text: finding the one under the pointer, and deciding
//! where it leads, which is for extensions to say when they declared the link's
//! scheme or `[[wikilinks]]`.
//!
//! A link written `[text](destination)` shows only its text, so the editor's own
//! detection of URLs, which looks at the text under the pointer, never sees its
//! destination. Resolving the link here is what makes it clickable.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use editor::hover_links::{HoverAction, HoverLink, ResolvedFileTarget};
use extension::{VisualMdLinkRequest, VisualMdLinkStyle, VisualMdLinkTarget, VisualMdOutline};
use gpui::{App, AppContext as _, AsyncApp, Entity, Task};
use language::language_settings::LanguageSettings;
use language::{Anchor, Buffer, Point, ToOffset as _};
use project::{Project, ProjectPath, ResolvedPath};
use util::ResultExt as _;
use workspace::DeploySearch;

use crate::embeds::MAX_NOTE_BYTES;
use crate::extensions::{HookError, LinkResolvers, VisualMdExtensions, when_not_busy};
use crate::note_contents;
use crate::notes::{self, NoteIndex};
use crate::outline;
use crate::plan::{self, EmbedKind, parse_blocks};

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

/// The reference-style link on `line` that covers byte `offset`, when
/// `definitions` has the label it names, as a link to the destination that label
/// was defined with.
pub fn reference_link_in_line(
    line: &str,
    offset: usize,
    definitions: &HashMap<String, String>,
) -> Option<LinkInLine> {
    if definitions.is_empty() {
        return None;
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_md::INLINE_LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(line, None)?;
    let mut node = tree.root_node().descendant_for_byte_range(offset, offset)?;
    while !matches!(
        node.kind(),
        "full_reference_link" | "collapsed_reference_link" | "shortcut_link"
    ) {
        node = node.parent()?;
    }
    let destination =
        definitions.get(&plan::normalize_label(plan::reference_label(node, line)?))?;
    Some(LinkInLine {
        range: node.byte_range(),
        style: VisualMdLinkStyle::Inline,
        target: destination.clone(),
        is_wikilink: false,
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

/// The line `position` is on, where it starts in the buffer, and where in the
/// line `position` is, in bytes.
fn line_at(
    snapshot: &language::BufferSnapshot,
    position: Anchor,
) -> Option<(String, usize, usize)> {
    let offset = position.to_offset(snapshot);
    let row = snapshot.offset_to_point(offset).row;
    let line_start = snapshot.point_to_offset(Point::new(row, 0));
    let line_end = snapshot.point_to_offset(Point::new(row, snapshot.line_len(row)));
    let line: String = snapshot.text_for_range(line_start..line_end).collect();
    let relative_offset = offset.checked_sub(line_start)?;
    Some((line, line_start, relative_offset))
}

/// The link at `position` for the editor's hover and click handling, when live
/// preview is `active` in the editor.
pub(crate) fn link_at(
    active: bool,
    buffer: &Entity<Buffer>,
    position: Anchor,
    project: Option<&Entity<Project>>,
    note_index: Option<Entity<NoteIndex>>,
    definitions: Option<Arc<HashMap<String, String>>>,
    cx: &mut App,
) -> Option<Task<Option<(Range<Anchor>, HoverLink)>>> {
    if !active {
        return None;
    }
    let snapshot = buffer.read(cx).snapshot();
    let (line, line_start, relative_offset) = line_at(&snapshot, position)?;
    let link = link_in_line(&line, relative_offset).or_else(|| {
        definitions
            .as_deref()
            .and_then(|definitions| reference_link_in_line(&line, relative_offset, definitions))
    });
    let Some(link) = link else {
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

/// What the hover popover shows for the `[[wikilink]]` under `position`: the
/// note, heading or block it names, as Markdown. Nothing when the link leads
/// anywhere else than a note of the project, such as to a target an extension
/// resolves, or when `visual_md.page_preview` is off.
pub(crate) fn preview_at(
    active: bool,
    buffer: &Entity<Buffer>,
    position: Anchor,
    project: Option<&Entity<Project>>,
    note_index: Option<Entity<NoteIndex>>,
    cx: &mut App,
) -> Option<Task<Option<(Range<Anchor>, String)>>> {
    if !active
        || !LanguageSettings::for_buffer(buffer.read(cx), cx)
            .visual_md
            .is_page_preview_enabled()
    {
        return None;
    }
    let snapshot = buffer.read(cx).snapshot();
    let (line, line_start, relative_offset) = line_at(&snapshot, position)?;
    let link = link_in_line(&line, relative_offset).filter(|link| link.is_wikilink)?;
    let range = snapshot.anchor_before(line_start + link.range.start)
        ..snapshot.anchor_after(line_start + link.range.end);

    let resolvers = cx
        .try_global::<VisualMdExtensions>()
        .map(|registry| registry.link_resolvers())
        .unwrap_or_default();
    if !resolvers.wikilinks.is_empty() {
        return None;
    }

    let (name, subpath) = plan::split_subpath(&link.target);
    let opening = if name.is_empty() {
        None
    } else {
        let from = notes::location_of(buffer.read(cx), cx);
        let notes::Resolution::Found(file) = note_index?.read(cx).resolve(name, from.as_ref())
        else {
            return None;
        };
        let path = file.project_path();
        let extension = path.path.extension().unwrap_or_default();
        if !extension.is_empty() && EmbedKind::for_extension(extension) != Some(EmbedKind::Note) {
            return None;
        }
        let project = project?;
        Some(project.update(cx, |project, cx| project.open_buffer(path, cx)))
    };

    let current = buffer.clone();
    Some(cx.spawn(async move |cx| {
        let note = match opening {
            Some(opening) => opening.await.log_err()?,
            None => current,
        };
        let text = cx.update(|cx| note.read(cx).text());
        let preview = cx
            .background_spawn(async move {
                let section = note_contents::section(&text, subpath.as_ref())?;
                let (preview, _) = note_contents::preview_markdown(&section, MAX_NOTE_BYTES);
                (!preview.trim().is_empty()).then_some(preview)
            })
            .await?;
        Some((range, preview))
    }))
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

    fn definitions(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(label, destination)| (plan::normalize_label(label), destination.to_string()))
            .collect()
    }

    #[test]
    fn test_a_reference_link_leads_where_its_label_was_defined() {
        let defined = definitions(&[("ref", "https://a.example"), ("Text", "b.md")]);
        for (line, offset, range, target) in [
            ("see [the docs][REF] now", 8, 4..19, "https://a.example"),
            ("see [text][] now", 6, 4..12, "b.md"),
            ("see [ref] now", 5, 4..9, "https://a.example"),
        ] {
            let link = reference_link_in_line(line, offset, &defined).expect(line);
            assert_eq!(link.range, range, "for {line:?}");
            assert_eq!(link.target, target, "for {line:?}");
            assert!(!link.is_wikilink);
        }
    }

    #[test]
    fn test_a_reference_link_is_nothing_without_its_definition_or_off_the_link() {
        let defined = definitions(&[("ref", "https://a.example")]);
        assert_eq!(
            reference_link_in_line("see [text][nope]", 6, &defined),
            None
        );
        assert_eq!(reference_link_in_line("see [ref] now", 11, &defined), None);
        assert_eq!(
            reference_link_in_line("see [ref] now", 6, &HashMap::new()),
            None
        );
        assert_eq!(reference_link_in_line("see `[ref]` now", 7, &defined), None);
        assert_eq!(reference_link_in_line("see [[ref]] now", 7, &defined), None);
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
    use gpui::{Modifiers, TestAppContext, WindowHandle};
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
    async fn test_a_reference_link_opens_the_destination_of_its_definition(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let text = "see [the docs][docs] and [other] now\n\n[docs]: https://example.com/docs\n[other]: other.md\n";
        let document = open(
            cx,
            json!({ "a.md": text, "other.md": "o" }),
            "/dir/a.md",
            true,
        )
        .await;

        let (range, link) = link_for(cx, &document, text.find("docs]").unwrap_or(0))
            .await
            .expect("a link");
        assert_eq!(text_of(text, range), "[the docs][docs]");
        assert_eq!(link, "url https://example.com/docs");

        let (range, link) = link_for(cx, &document, text.find("other]").unwrap_or(0))
            .await
            .expect("a link");
        assert_eq!(text_of(text, range), "[other]");
        assert_eq!(link, "file other.md");

        assert!(
            link_for(cx, &document, text.find("now").unwrap_or(0))
                .await
                .is_none()
        );
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

    async fn preview_for(
        cx: &mut TestAppContext,
        document: &Document,
        offset: usize,
    ) -> Option<(Range<usize>, String)> {
        let buffer = document.buffer.clone();
        let project = document.project.clone();
        let task = document
            .window
            .update(cx, |editor, _window, cx| {
                let position = buffer.read(cx).anchor_before(offset);
                editor
                    .addon::<VisualMdAddon>()
                    .and_then(|addon| addon.hover_at(&buffer, position, Some(&project), cx))
            })
            .ok()
            .flatten()?;
        let (range, preview) = task.await?;
        let snapshot = document.buffer.read_with(cx, |buffer, _| buffer.snapshot());
        Some((
            range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot),
            preview,
        ))
    }

    const OTHER: &str = "---\ntitle: Other\n---\n# First\nfirst text\n\n## Nested\nnested text\n\n# Second\nsecond [[x|shown]] text ^second-id\n\nloose paragraph ^loose\n";

    fn notes() -> serde_json::Value {
        json!({ "other.md": OTHER, "empty.md": "", "pic.png": "x", "sub": { "deep.md": "deep text" } })
    }

    #[gpui::test]
    async fn test_a_wikilink_previews_the_note_without_its_front_matter(cx: &mut TestAppContext) {
        init_test(cx);
        let text = "go [[Other]] now\n";
        let mut files = notes();
        files["a.md"] = json!(text);
        let document = open(cx, files, "/dir/a.md", true).await;

        let (range, preview) = preview_for(cx, &document, 6).await.expect("a preview");

        assert_eq!(text_of(text, range), "[[Other]]");
        assert!(preview.starts_with("# First"), "{preview}");
        assert!(!preview.contains("title: Other"), "{preview}");
        assert!(preview.contains("second shown text"), "{preview}");
        assert!(!preview.contains("^second-id"), "{preview}");
    }

    #[gpui::test]
    async fn test_a_wikilink_to_a_heading_previews_that_section(cx: &mut TestAppContext) {
        init_test(cx);
        let text = "go [[Other#First]] and [[Other#Nested]]\n";
        let mut files = notes();
        files["a.md"] = json!(text);
        let document = open(cx, files, "/dir/a.md", true).await;

        let (_, first) = preview_for(cx, &document, 6).await.expect("a preview");
        assert_eq!(first, "# First\nfirst text\n\n## Nested\nnested text");

        let nested = text.find("Nested").expect("the text has it");
        let (_, nested) = preview_for(cx, &document, nested).await.expect("a preview");
        assert_eq!(nested, "## Nested\nnested text");
    }

    #[gpui::test]
    async fn test_a_wikilink_to_a_block_previews_the_block_without_its_id(cx: &mut TestAppContext) {
        init_test(cx);
        let text = "go [[Other#^loose]]\n";
        let mut files = notes();
        files["a.md"] = json!(text);
        let document = open(cx, files, "/dir/a.md", true).await;

        let (_, preview) = preview_for(cx, &document, 6).await.expect("a preview");

        assert_eq!(preview, "loose paragraph");
    }

    #[gpui::test]
    async fn test_a_wikilink_to_a_heading_of_this_note_previews_it_from_the_buffer(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let text = "# Intro\nintro text\n\n# Later\nsee [[#Intro]]\n";
        let document = open(cx, json!({ "a.md": text }), "/dir/a.md", true).await;
        let offset = text.find("[[#Intro]]").expect("the text has it") + 3;

        let (_, preview) = preview_for(cx, &document, offset).await.expect("a preview");

        assert_eq!(preview, "# Intro\nintro text");
    }

    #[gpui::test]
    async fn test_a_preview_shows_edits_that_are_not_saved(cx: &mut TestAppContext) {
        init_test(cx);
        let text = "go [[Other]] now\n";
        let mut files = notes();
        files["a.md"] = json!(text);
        let document = open(cx, files, "/dir/a.md", true).await;
        let other = document
            .project
            .update(cx, |project, cx| {
                project.open_local_buffer("/dir/other.md", cx)
            })
            .await
            .expect("the file opens");
        other.update(cx, |buffer, cx| buffer.edit([(0..0, "edited ")], None, cx));

        let (_, preview) = preview_for(cx, &document, 6).await.expect("a preview");

        assert!(preview.starts_with("edited ---"), "{preview}");
    }

    #[gpui::test]
    async fn test_a_wikilink_in_a_folder_previews_the_note_by_its_name(cx: &mut TestAppContext) {
        init_test(cx);
        let text = "go [[Deep]] now\n";
        let mut files = notes();
        files["a.md"] = json!(text);
        let document = open(cx, files, "/dir/a.md", true).await;

        let (_, preview) = preview_for(cx, &document, 6).await.expect("a preview");

        assert_eq!(preview, "deep text");
    }

    #[gpui::test]
    async fn test_nothing_is_previewed_that_is_not_a_note_with_text(cx: &mut TestAppContext) {
        init_test(cx);
        let text = "[[Nobody]] [[pic.png]] [[Empty]] [[Other#Nowhere]] [[Other#^nowhere]] [a link](other.md) plain\n";
        let mut files = notes();
        files["a.md"] = json!(text);
        let document = open(cx, files, "/dir/a.md", true).await;

        for needle in [
            "Nobody",
            "pic.png",
            "Empty",
            "Nowhere]]",
            "^nowhere",
            "a link",
            "plain",
        ] {
            let offset = text.find(needle).expect("the text has it");
            assert_eq!(
                preview_for(cx, &document, offset).await,
                None,
                "{needle} has no preview"
            );
        }
    }

    #[gpui::test]
    async fn test_the_setting_turns_previews_off(cx: &mut TestAppContext) {
        init_test(cx);
        let text = "go [[Other]] now\n";
        let mut files = notes();
        files["a.md"] = json!(text);
        let document = open(cx, files, "/dir/a.md", true).await;
        assert!(preview_for(cx, &document, 6).await.is_some());

        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content
                    .project
                    .all_languages
                    .defaults
                    .visual_md
                    .get_or_insert_default()
                    .page_preview = Some(false);
            });
        });
        cx.run_until_parked();

        assert_eq!(preview_for(cx, &document, 6).await, None);
    }

    #[gpui::test]
    async fn test_a_wikilink_an_extension_resolves_is_not_previewed(cx: &mut TestAppContext) {
        init_test(cx);
        let extension =
            extension_with_links(cx, "a-first", "wikilinks = true\n", Behavior::Succeed);
        extension.set_link_responder(|_| None);
        let text = "go [[Other]] now\n";
        let mut files = notes();
        files["a.md"] = json!(text);
        let document = open(cx, files, "/dir/a.md", true).await;

        assert_eq!(preview_for(cx, &document, 6).await, None);
    }

    #[gpui::test]
    async fn test_a_preview_is_cut_at_the_limit(cx: &mut TestAppContext) {
        init_test(cx);
        let long = "a line of text\n".repeat(20_000);
        let text = "go [[Long]] now\n";
        let document = open(
            cx,
            json!({ "a.md": text, "long.md": long }),
            "/dir/a.md",
            true,
        )
        .await;

        let (_, preview) = preview_for(cx, &document, 6).await.expect("a preview");

        assert!(preview.len() <= MAX_NOTE_BYTES, "{}", preview.len());
        assert!(preview.len() > MAX_NOTE_BYTES / 2, "{}", preview.len());
    }

    const NOTED: &str =
        "see[^1] and[^2] there\n\n[^1]: The **note**\n    continues [[Other]].\n[^2]: Second.\n";

    #[gpui::test]
    async fn test_a_footnote_reference_previews_its_definition(cx: &mut TestAppContext) {
        init_test(cx);
        let document = open(cx, json!({ "a.md": NOTED }), "/dir/a.md", true).await;
        let offset = NOTED.find("[^1]").expect("the text has it") + 2;

        let (range, preview) = preview_for(cx, &document, offset).await.expect("a preview");

        assert_eq!(text_of(NOTED, range), "[^1]");
        assert_eq!(preview, "The **note**\ncontinues Other.");
        assert_eq!(
            preview_for(cx, &document, NOTED.find("there").unwrap_or(0)).await,
            None
        );
    }

    #[gpui::test]
    async fn test_a_footnote_preview_does_not_depend_on_the_page_preview_setting(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let document = open(cx, json!({ "a.md": NOTED }), "/dir/a.md", true).await;
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content
                    .project
                    .all_languages
                    .defaults
                    .visual_md
                    .get_or_insert_default()
                    .page_preview = Some(false);
            });
        });
        cx.run_until_parked();

        let offset = NOTED.find("[^2]").expect("the text has it");
        let (_, preview) = preview_for(cx, &document, offset).await.expect("a preview");

        assert_eq!(preview, "Second.");
    }

    #[gpui::test]
    async fn test_a_footnote_preview_follows_the_text_as_it_is_edited(cx: &mut TestAppContext) {
        init_test(cx);
        let document = open(cx, json!({ "a.md": NOTED }), "/dir/a.md", true).await;
        let offset = NOTED.find("[^1]").expect("the text has it") + 2;
        let added = "A longer start of the line, ";
        document
            .buffer
            .update(cx, |buffer, cx| buffer.edit([(0..0, added)], None, cx));
        cx.run_until_parked();

        assert_eq!(
            preview_for(cx, &document, offset).await,
            None,
            "that is now in the added text"
        );
        let (_, preview) = preview_for(cx, &document, offset + added.len())
            .await
            .expect("a preview");
        assert_eq!(preview, "The **note**\ncontinues Other.");
    }

    #[gpui::test]
    async fn test_clicking_a_footnote_reference_moves_the_cursor_to_its_definition(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let document = open(cx, json!({ "a.md": NOTED }), "/dir/a.md", true).await;
        let offset = NOTED.find("[^1]").expect("the text has it") + 1;

        let (range, link) = raw_link_for(cx, &document, offset).await.expect("a link");
        assert_eq!(text_of(NOTED, range), "[^1]");
        let HoverLink::Action(go_to_definition) = link else {
            panic!("the link should be an action");
        };
        // Run where the editor navigation runs it, outside an update of the editor.
        cx.update_window(document.window.into(), |_, window, cx| {
            go_to_definition.run(window, cx)
        })
        .expect("the window is open");
        cx.run_until_parked();

        let head = document
            .window
            .update(cx, |editor, _window, cx| {
                let display_snapshot = editor.display_snapshot(cx);
                editor
                    .selections
                    .newest::<editor::MultiBufferOffset>(&display_snapshot)
                    .head()
                    .0
            })
            .expect("the window is open");
        assert_eq!(
            head,
            NOTED.find("The **note**").expect("the text has it"),
            "the cursor is at the start of the definition's text"
        );
    }

    #[gpui::test]
    async fn test_the_mouse_over_a_footnote_number_hovers_and_follows_the_reference(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("see it now[^1] and more text\n\n[^1]: The definition.\n\nendˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));
        cx.run_until_parked();
        assert_eq!(
            cx.display_text(),
            "see it now1 and more text\n\n1. The definition.\n\nend\n"
        );

        let on_number =
            cx.pixel_position("see it now[ˇ^1] and more text\n\n[^1]: The definition.\n\nend\n");
        cx.simulate_mouse_move(on_number, None, Modifiers::secondary_key());
        cx.assert_editor_text_highlights(
            HighlightKey::HoveredLinkState,
            "see it now«[^1]ˇ» and more text\n\n[^1]: The definition.\n\nend\n",
        );
        cx.simulate_click(on_number, Modifiers::secondary_key());
        cx.run_until_parked();
        cx.assert_editor_state("see it now[^1] and more text\n\n[^1]: ˇThe definition.\n\nend\n");
    }

    #[gpui::test]
    async fn test_resting_the_mouse_on_a_footnote_number_shows_the_definition(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("see it now[^1] and more text\n\n[^1]: The **definition**.\n\nendˇ\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));
        cx.run_until_parked();

        let on_number = cx
            .pixel_position("see it now[ˇ^1] and more text\n\n[^1]: The **definition**.\n\nend\n");
        cx.simulate_mouse_move(on_number, None, Modifiers::none());
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(1000));
        cx.run_until_parked();

        let shown = cx.editor(|editor, _, cx| {
            editor
                .hover_state
                .info_popovers
                .iter()
                .filter_map(|popover| popover.parsed_content.as_ref())
                .map(|markdown| markdown.read(cx).source().to_string())
                .collect::<Vec<_>>()
        });
        assert_eq!(shown, vec!["The **definition**.".to_string()]);

        let elsewhere = cx
            .pixel_position("see it now[^1] and moˇre text\n\n[^1]: The **definition**.\n\nend\n");
        cx.simulate_mouse_move(elsewhere, None, Modifiers::none());
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(1000));
        cx.run_until_parked();
        cx.editor(|editor, _, _| assert!(!editor.hover_state.visible()));
    }
}
