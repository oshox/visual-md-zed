//! Images pasted into a note: where the file goes, and how the note links to it.
//!
//! With `visual_md.attachment_folder` set, `Paste` of an image saves it in that
//! folder, creating it if need be, and inserts `![](path)` with the path from the
//! note's folder. With it unset the editor's own paste runs, which saves the
//! image next to the note.

use std::sync::Arc;

use anyhow::Result;
use editor::actions::Paste;
use editor::{Editor, MultiBufferOffset, SelectionEffects};
use gpui::{ClipboardEntry, Context, Image, TaskExt as _, Window};
use util::ResultExt as _;
use util::rel_path::RelPath;

/// The folder an image is saved in, as a path from the root of the worktree,
/// when `setting` is the folder the user chose and `note_folder` is the folder of
/// the note, also from the root. A setting that starts with `/` is from the root,
/// any other is from the note's folder, and `..` goes up. `None` when the setting
/// is empty or goes up out of the worktree.
pub fn attachment_folder(setting: &str, note_folder: &str) -> Option<String> {
    let setting = setting.trim().replace('\\', "/");
    if setting.is_empty() {
        return None;
    }
    let mut components: Vec<&str> = if setting.starts_with('/') {
        Vec::new()
    } else {
        note_folder
            .split('/')
            .filter(|component| !component.is_empty())
            .collect()
    };
    for component in setting.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            name => components.push(name),
        }
    }
    Some(components.join("/"))
}

/// The path to write in a link from a note in `from_folder` to `to_file`, both
/// from the root of the worktree. Spaces are written `%20`, which is how a link
/// destination can hold them.
pub fn relative_link(from_folder: &str, to_file: &str) -> String {
    let from: Vec<&str> = from_folder
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let to: Vec<&str> = to_file
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let shared = from
        .iter()
        .zip(&to)
        .take_while(|(from, to)| from == to)
        .count()
        // The last part of `to_file` is the file, which is never a folder to share.
        .min(to.len().saturating_sub(1));
    let mut parts: Vec<&str> = vec![".."; from.len() - shared];
    parts.extend(&to[shared..]);
    parts.join("/").replace(' ', "%20")
}

/// A name for the image in `folder` that no file has: `image.png`, then
/// `image_1.png` and on, where `exists` says whether a path from the root of the
/// worktree is taken.
pub fn unused_name(folder: &str, extension: &str, exists: impl Fn(&str) -> bool) -> String {
    let path = |name: &str| {
        if folder.is_empty() {
            name.to_string()
        } else {
            format!("{folder}/{name}")
        }
    };
    let mut name = format!("image.{extension}");
    let mut counter = 1u32;
    while exists(&path(&name)) {
        name = format!("image_{counter}.{extension}");
        counter += 1;
    }
    name
}

/// What pasting `image` is going to do.
struct ImagePaste {
    worktree: gpui::Entity<project::Worktree>,
    path: Arc<RelPath>,
    link: String,
}

fn plan_paste(editor: &Editor, image: &Image, cx: &mut Context<Editor>) -> Option<ImagePaste> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let head = {
        let display_snapshot = editor.display_snapshot(cx);
        editor
            .selections
            .newest::<MultiBufferOffset>(&display_snapshot)
            .head()
    };
    let setting = editor
        .buffer()
        .read(cx)
        .language_settings_at(head, cx)
        .visual_md
        .attachment_folder
        .clone()?;

    let file = buffer.read(cx).file()?;
    let worktree_id = file.worktree_id(cx);
    let note_folder = file.path().parent()?.as_unix_str().to_string();
    let worktree = editor
        .project()?
        .read(cx)
        .worktree_for_id(worktree_id, cx)?;

    let folder = attachment_folder(&setting, &note_folder)?;
    let snapshot = worktree.read(cx).snapshot();
    let name = unused_name(&folder, image.format.extension(), |path| {
        RelPath::from_unix_str(path)
            .ok()
            .is_some_and(|path| snapshot.entry_for_path(path).is_some())
    });
    let file_path = if folder.is_empty() {
        name
    } else {
        format!("{folder}/{name}")
    };
    let path: Arc<RelPath> = RelPath::from_unix_str(&file_path).log_err()?.into();
    Some(ImagePaste {
        worktree,
        path,
        link: relative_link(&note_folder, &file_path),
    })
}

/// Handles `Paste`. An image is saved where `visual_md.attachment_folder` says
/// and linked from the note; anything else is for the editor to paste.
pub(crate) fn intercept_paste(
    editor: &mut Editor,
    _: &Paste,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    if !crate::live_preview_enabled(editor, cx) || editor.read_only(cx) {
        cx.propagate();
        return;
    }
    let image = cx.read_from_clipboard().and_then(|item| {
        item.entries().iter().find_map(|entry| match entry {
            ClipboardEntry::Image(image) if !image.bytes.is_empty() => Some(image.clone()),
            _ => None,
        })
    });
    let Some(image) = image else {
        cx.propagate();
        return;
    };
    let Some(paste) = plan_paste(editor, &image, cx) else {
        cx.propagate();
        return;
    };

    let ImagePaste {
        worktree,
        path,
        link,
    } = paste;
    let created = worktree.update(cx, |worktree, cx| {
        worktree.create_entry(path, false, Some(image.bytes.clone()), cx)
    });
    cx.spawn_in(window, async move |editor, cx| -> Result<()> {
        created.await?;
        editor.update_in(cx, |editor, window, cx| {
            insert_image_link(editor, &link, window, cx)
        })?;
        Ok(())
    })
    .detach_and_log_err(cx);
}

/// Writes `![](link)` in place of each selection, with the cursor between the
/// brackets for the alternative text.
fn insert_image_link(
    editor: &mut Editor,
    link: &str,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let text = format!("![]({link})");
    let selections: Vec<_> = {
        let display_snapshot = editor.display_snapshot(cx);
        editor
            .selections
            .all::<MultiBufferOffset>(&display_snapshot)
            .into_iter()
            .map(|selection| selection.start.0..selection.end.0)
            .collect()
    };
    let mut cursors = Vec::with_capacity(selections.len());
    let mut shift = 0isize;
    for range in &selections {
        let start = (range.start as isize + shift) as usize;
        cursors.push(MultiBufferOffset(start + 2)..MultiBufferOffset(start + 2));
        shift += text.len() as isize - range.len() as isize;
    }
    editor.transact(window, cx, |editor, window, cx| {
        editor.edit(
            selections.into_iter().map(|range| {
                (
                    MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                    text.clone(),
                )
            }),
            cx,
        );
        editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
            selections.select_ranges(cursors);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{ClipboardItem, ImageFormat, TestAppContext, WindowHandle};
    use serde_json::json;

    use crate::integration_tests::{editor_in_project, init_test};

    #[test]
    fn test_a_folder_from_the_note_is_taken_from_the_notes_folder() {
        assert_eq!(
            attachment_folder("attachments", "notes/2026").as_deref(),
            Some("notes/2026/attachments")
        );
        assert_eq!(
            attachment_folder("./img", "notes").as_deref(),
            Some("notes/img")
        );
        assert_eq!(attachment_folder("img", "").as_deref(), Some("img"));
        assert_eq!(attachment_folder("a/b", "n").as_deref(), Some("n/a/b"));
    }

    #[test]
    fn test_a_folder_starting_with_a_slash_is_from_the_root() {
        assert_eq!(
            attachment_folder("/assets", "notes/2026").as_deref(),
            Some("assets")
        );
        assert_eq!(attachment_folder("/", "notes").as_deref(), Some(""));
        assert_eq!(
            attachment_folder("/a/./b/", "notes").as_deref(),
            Some("a/b")
        );
    }

    #[test]
    fn test_dots_go_up_a_folder_but_not_out_of_the_worktree() {
        assert_eq!(
            attachment_folder("../images", "notes/2026").as_deref(),
            Some("notes/images")
        );
        assert_eq!(
            attachment_folder("../../images", "notes/2026").as_deref(),
            Some("images")
        );
        assert_eq!(attachment_folder("../../../images", "notes/2026"), None);
        assert_eq!(attachment_folder("/../images", "notes"), None);
    }

    #[test]
    fn test_an_empty_folder_setting_means_none() {
        assert_eq!(attachment_folder("", "notes"), None);
        assert_eq!(attachment_folder("  ", "notes"), None);
    }

    #[test]
    fn test_windows_separators_are_taken_for_slashes() {
        assert_eq!(
            attachment_folder("assets\\img", "notes").as_deref(),
            Some("notes/assets/img")
        );
    }

    #[test]
    fn test_a_link_goes_down_up_or_sideways_from_the_notes_folder() {
        assert_eq!(
            relative_link("notes", "notes/attachments/image.png"),
            "attachments/image.png"
        );
        assert_eq!(
            relative_link("notes/2026", "assets/image.png"),
            "../../assets/image.png"
        );
        assert_eq!(
            relative_link("notes/2026", "notes/images/image.png"),
            "../images/image.png"
        );
        assert_eq!(relative_link("", "image.png"), "image.png");
        assert_eq!(relative_link("notes", "image.png"), "../image.png");
        assert_eq!(relative_link("notes", "notes/image.png"), "image.png");
    }

    #[test]
    fn test_spaces_in_a_link_are_escaped() {
        assert_eq!(
            relative_link("my notes", "my notes/my images/image.png"),
            "my%20images/image.png"
        );
    }

    #[test]
    fn test_a_name_is_numbered_until_nothing_has_it() {
        let taken = ["img/image.png", "img/image_1.png"];
        assert_eq!(
            unused_name("img", "png", |path| taken.contains(&path)),
            "image_2.png"
        );
        assert_eq!(
            unused_name("img", "jpg", |path| taken.contains(&path)),
            "image.jpg"
        );
        assert_eq!(
            unused_name("", "png", |path| path == "image.png"),
            "image_1.png"
        );
    }

    fn png() -> Image {
        Image::from_bytes(ImageFormat::Png, vec![137, 80, 78, 71, 1, 2, 3])
    }

    fn set_attachment_folder(cx: &mut TestAppContext, folder: Option<&str>) {
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content
                    .project
                    .all_languages
                    .defaults
                    .visual_md
                    .get_or_insert_default()
                    .attachment_folder = folder.map(str::to_string);
            });
        });
        cx.run_until_parked();
    }

    fn paste_image(cx: &mut TestAppContext, window: &WindowHandle<Editor>) {
        cx.write_to_clipboard(ClipboardItem::new_image(&png()));
        window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(Paste), cx)
            })
            .expect("the window is open");
        cx.run_until_parked();
    }

    fn text(cx: &mut TestAppContext, window: &WindowHandle<Editor>) -> String {
        window
            .update(cx, |editor, _, cx| editor.text(cx))
            .expect("the window is open")
    }

    async fn file_exists(
        window: &WindowHandle<Editor>,
        cx: &mut TestAppContext,
        path: &str,
    ) -> bool {
        let project = window
            .update(cx, |editor, _, _| editor.project().cloned())
            .ok()
            .flatten()
            .expect("the editor has a project");
        let fs = project.read_with(cx, |project, _| project.fs().clone());
        fs.is_file(std::path::Path::new(&format!("/dir/{path}")))
            .await
    }

    #[gpui::test]
    async fn test_a_pasted_image_goes_in_the_attachment_folder_and_is_linked(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let window = editor_in_project(
            cx,
            json!({ "notes": { "a.md": "text " } }),
            "/dir/notes/a.md",
        )
        .await;
        set_attachment_folder(cx, Some("attachments"));

        paste_image(cx, &window);

        assert_eq!(text(cx, &window), "text ![](attachments/image.png)");
        assert!(file_exists(&window, cx, "notes/attachments/image.png").await);
    }

    #[gpui::test]
    async fn test_a_folder_from_the_root_is_linked_from_the_notes_folder(cx: &mut TestAppContext) {
        init_test(cx);
        set_attachment_folder(cx, Some("/assets/img"));
        let window =
            editor_in_project(cx, json!({ "notes": { "a.md": "" } }), "/dir/notes/a.md").await;

        paste_image(cx, &window);

        assert_eq!(text(cx, &window), "![](../assets/img/image.png)");
        assert!(file_exists(&window, cx, "assets/img/image.png").await);
    }

    #[gpui::test]
    async fn test_a_second_image_does_not_replace_the_first(cx: &mut TestAppContext) {
        init_test(cx);
        let window = editor_in_project(cx, json!({ "a.md": "" }), "/dir/a.md").await;
        set_attachment_folder(cx, Some("pics"));

        paste_image(cx, &window);
        window
            .update(cx, |editor, window, cx| {
                let end = editor.buffer().read(cx).len(cx);
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([end..end]);
                });
            })
            .expect("the window is open");
        paste_image(cx, &window);

        assert_eq!(
            text(cx, &window),
            "![](pics/image.png)![](pics/image_1.png)"
        );
    }

    #[gpui::test]
    async fn test_the_cursor_waits_between_the_brackets_for_alternative_text(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let window = editor_in_project(cx, json!({ "a.md": "" }), "/dir/a.md").await;
        set_attachment_folder(cx, Some("pics"));

        paste_image(cx, &window);
        window
            .update(cx, |editor, window, cx| {
                editor.handle_input("alt", window, cx)
            })
            .expect("the window is open");

        assert_eq!(text(cx, &window), "![alt](pics/image.png)");
    }

    #[gpui::test]
    async fn test_without_a_folder_setting_the_editor_pastes_the_image_beside_the_note(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        set_attachment_folder(cx, None);
        let window =
            editor_in_project(cx, json!({ "notes": { "a.md": "" } }), "/dir/notes/a.md").await;

        paste_image(cx, &window);

        assert!(file_exists(&window, cx, "notes/image.png").await);
        assert!(
            text(cx, &window).starts_with("![]("),
            "{}",
            text(cx, &window)
        );
    }

    #[gpui::test]
    async fn test_a_folder_that_leaves_the_worktree_is_left_to_the_editor(cx: &mut TestAppContext) {
        init_test(cx);
        set_attachment_folder(cx, Some("../../out"));
        let window =
            editor_in_project(cx, json!({ "notes": { "a.md": "" } }), "/dir/notes/a.md").await;

        paste_image(cx, &window);

        assert!(file_exists(&window, cx, "notes/image.png").await);
    }

    #[gpui::test]
    async fn test_with_live_preview_off_the_editor_pastes_the_image_beside_the_note(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let window =
            editor_in_project(cx, json!({ "notes": { "a.md": "" } }), "/dir/notes/a.md").await;
        set_attachment_folder(cx, Some("pics"));
        window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(crate::ToggleLivePreview), cx)
            })
            .expect("the window is open");

        paste_image(cx, &window);

        assert!(file_exists(&window, cx, "notes/image.png").await);
        assert!(!file_exists(&window, cx, "notes/pics/image.png").await);
    }

    #[gpui::test]
    async fn test_pasted_text_is_not_touched(cx: &mut TestAppContext) {
        init_test(cx);
        let window = editor_in_project(cx, json!({ "a.md": "" }), "/dir/a.md").await;
        set_attachment_folder(cx, Some("pics"));

        cx.write_to_clipboard(ClipboardItem::new_string("plain".to_string()));
        window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(Paste), cx)
            })
            .expect("the window is open");

        assert_eq!(text(cx, &window), "plain");
        assert!(!file_exists(&window, cx, "pics/image.png").await);
    }
}
