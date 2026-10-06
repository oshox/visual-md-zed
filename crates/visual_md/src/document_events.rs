//! Telling extensions what happens to a document: that it was opened, saved or
//! changed, together with its outline.
//!
//! An extension hears only about the events it subscribed to in
//! `[visual_md.events]`, and only for documents Zed MD is showing with live
//! preview. Nothing here waits for an extension: the work is a task of its own,
//! and an extension that is slow or fails costs the editor nothing.

use std::sync::Arc;

use editor::Editor;
use extension::{
    VisualMdDocumentEvent, VisualMdDocumentEventKind, VisualMdEventsManifestEntry, VisualMdOutline,
};
use gpui::{AppContext as _, Context, Task};
use util::ResultExt as _;

use crate::VisualMdAddon;
use crate::extensions::{HookError, VisualMdExtensions, when_not_busy};
use crate::outline::{self, MAX_DOCUMENT_BYTES};
use crate::plan::parse_blocks;

/// The extensions subscribed to an event, as chosen by `wants`.
fn subscribers(
    cx: &Context<Editor>,
    wants: impl Fn(&VisualMdEventsManifestEntry) -> bool,
) -> Vec<(Arc<str>, VisualMdEventsManifestEntry)> {
    cx.try_global::<VisualMdExtensions>()
        .map(|registry| {
            registry
                .event_subscribers()
                .into_iter()
                .filter(|(_, events)| wants(events))
                .collect()
        })
        .unwrap_or_default()
}

/// The document was shown with live preview for the first time.
pub(crate) fn send_opened(editor: &Editor, cx: &mut Context<Editor>) {
    let extension_ids = subscribers(cx, |events| events.opened)
        .into_iter()
        .map(|(extension_id, _)| extension_id)
        .collect();
    send(editor, VisualMdDocumentEventKind::Opened, extension_ids, cx);
}

pub(crate) fn send_saved(editor: &Editor, cx: &mut Context<Editor>) {
    let extension_ids = subscribers(cx, |events| events.saved)
        .into_iter()
        .map(|(extension_id, _)| extension_id)
        .collect();
    send(editor, VisualMdDocumentEventKind::Saved, extension_ids, cx);
}

/// Starts the wait before each subscribed extension is told that the document
/// changed. Another edit starts the wait again, so an extension hears of a burst
/// of typing once, when it stops, after the time it asked for.
pub(crate) fn schedule_changed(editor: &mut Editor, cx: &mut Context<Editor>) {
    let subscribers = subscribers(cx, |events| events.changed);
    let Some(addon) = editor.addon_mut::<VisualMdAddon>() else {
        return;
    };
    addon
        .changed_events
        .retain(|extension_id, _| subscribers.iter().any(|(id, _)| id == extension_id));
    if !addon.active {
        addon.changed_events.clear();
        return;
    }
    for (extension_id, events) in subscribers {
        let debounce = events.changed_debounce();
        let task = cx.spawn({
            let extension_id = extension_id.clone();
            async move |editor, cx| {
                cx.background_executor().timer(debounce).await;
                editor
                    .update(cx, |editor, cx| {
                        send(
                            editor,
                            VisualMdDocumentEventKind::Changed,
                            vec![extension_id],
                            cx,
                        )
                    })
                    .log_err();
            }
        });
        if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
            addon.changed_events.insert(extension_id, task);
        }
    }
}

fn send(
    editor: &Editor,
    kind: VisualMdDocumentEventKind,
    extension_ids: Vec<Arc<str>>,
    cx: &mut Context<Editor>,
) {
    if extension_ids.is_empty() || !crate::is_decorating(editor) {
        return;
    }
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    if snapshot.len().0 > MAX_DOCUMENT_BYTES {
        log::info!(
            "not telling extensions about a {} byte document: the limit is {MAX_DOCUMENT_BYTES}",
            snapshot.len().0
        );
        return;
    }
    let text = snapshot.text();
    let path = crate::fence_render::note_path(editor, cx);

    cx.spawn(async move |_, cx| {
        let outline = cx
            .background_spawn(async move {
                match parse_blocks(&text) {
                    Some(tree) => outline::outline(&text, &tree),
                    None => VisualMdOutline::default(),
                }
            })
            .await;
        let calls = extension_ids.into_iter().map(|extension_id| {
            let event = VisualMdDocumentEvent {
                kind,
                path: path.clone(),
                outline: outline.clone(),
            };
            let mut cx = cx.clone();
            async move {
                let result =
                    when_not_busy(&mut cx, |cx| match cx.try_global::<VisualMdExtensions>() {
                        Some(registry) => registry.document_event(&extension_id, event.clone(), cx),
                        None => Task::ready(Err(HookError::NotRegistered(extension_id.clone()))),
                    })
                    .await;
                if let Err(error) = result {
                    log::warn!(
                        "extension {extension_id} could not be told about a {kind:?} event: {error}"
                    );
                }
            }
        });
        futures::future::join_all(calls).await;
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use editor::test::editor_test_context::EditorTestContext;
    use extension::VisualMdLinkStyle;
    use gpui::TestAppContext;

    use crate::extensions::test_support::{Behavior, FakeHooks, register};
    use crate::integration_tests::{init_test, markdown_language};

    use super::*;

    const DOCUMENT: &str = "ˇ# Title\n\nsee [[Note]] #tag\n\n- [ ] todo\n";

    fn events_section(events: &str) -> String {
        format!("[visual_md.events]\n{events}")
    }

    async fn editor_showing(cx: &mut TestAppContext, text: &str) -> EditorTestContext {
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state(text);
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));
        cx.run_until_parked();
        cx
    }

    fn kinds(hooks: &FakeHooks) -> Vec<VisualMdDocumentEventKind> {
        hooks
            .document_events()
            .iter()
            .map(|event| event.kind)
            .collect()
    }

    /// Lets every pending `changed` wait run out, so a test starts from a
    /// document nothing is about to be reported about.
    fn settle(cx: &mut EditorTestContext) {
        cx.executor().advance_clock(Duration::from_secs(11));
        cx.run_until_parked();
    }

    fn setup(cx: &mut TestAppContext, events: &str) -> Arc<FakeHooks> {
        init_test(cx);
        let hooks = FakeHooks::new(&cx.executor(), Behavior::Succeed);
        register(cx, "notes", &events_section(events), Some(hooks.clone()));
        hooks
    }

    #[gpui::test]
    async fn test_opening_a_document_tells_a_subscriber_once_with_its_outline(
        cx: &mut TestAppContext,
    ) {
        let hooks = setup(cx, "opened = true\n");
        let mut cx = editor_showing(cx, DOCUMENT).await;
        settle(&mut cx);

        let events = hooks.document_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, VisualMdDocumentEventKind::Opened);
        assert_eq!(events[0].path.as_deref(), Some("/root/file"));
        let outline = &events[0].outline;
        assert_eq!(outline.headings.len(), 1);
        assert_eq!(outline.headings[0].text, "Title");
        assert_eq!(outline.links.len(), 1);
        assert_eq!(outline.links[0].style, VisualMdLinkStyle::Wikilink);
        assert_eq!(outline.tags[0].name, "tag");
        assert_eq!(outline.tasks.len(), 1);

        cx.update_editor(|editor, window, cx| crate::force_refresh(editor, window, cx));
        settle(&mut cx);
        assert_eq!(hooks.document_events().len(), 1, "opening is reported once");

        // Turning live preview off and on again shows the document anew, but
        // does not open it.
        cx.dispatch_action(crate::ToggleLivePreview);
        cx.dispatch_action(crate::ToggleLivePreview);
        settle(&mut cx);
        assert_eq!(hooks.document_events().len(), 1);
    }

    #[gpui::test]
    fn test_only_extensions_with_code_and_a_subscription_are_subscribers(cx: &mut TestAppContext) {
        init_test(cx);
        let hooks = FakeHooks::new(&cx.executor(), Behavior::Succeed);
        register(
            cx,
            "listens",
            &events_section("saved = true\n"),
            Some(hooks.clone()),
        );
        register(
            cx,
            "no-flags",
            &events_section("changed_debounce_ms = 500\n"),
            Some(hooks.clone()),
        );
        register(cx, "no-code", &events_section("opened = true\n"), None);
        register(
            cx,
            "no-section",
            "fence_renderers = [\"flow\"]\n",
            Some(hooks),
        );

        cx.update(|cx| {
            let subscribers = cx.global::<VisualMdExtensions>().event_subscribers();
            assert_eq!(
                subscribers
                    .iter()
                    .map(|(extension_id, _)| extension_id.as_ref())
                    .collect::<Vec<_>>(),
                vec!["listens"]
            );
            assert!(subscribers[0].1.saved);
        });
    }

    #[gpui::test]
    async fn test_an_extension_hears_only_of_what_it_subscribed_to(cx: &mut TestAppContext) {
        let hooks = setup(cx, "saved = true\n");
        let mut cx = editor_showing(cx, DOCUMENT).await;
        settle(&mut cx);
        assert!(hooks.document_events().is_empty(), "no opened, no changed");

        cx.update_editor(|_, _, cx| cx.emit(editor::EditorEvent::Saved));
        cx.run_until_parked();

        assert_eq!(kinds(&hooks), vec![VisualMdDocumentEventKind::Saved]);
    }

    #[gpui::test]
    async fn test_changes_are_reported_once_typing_stops(cx: &mut TestAppContext) {
        let hooks = setup(cx, "changed = true\nchanged_debounce_ms = 500\n");
        let mut cx = editor_showing(cx, DOCUMENT).await;
        settle(&mut cx);
        let before = hooks.document_events().len();

        for _ in 0..3 {
            cx.update_buffer(|buffer, cx| buffer.edit([(2..2, "x")], None, cx));
            cx.executor().advance_clock(Duration::from_millis(300));
            cx.run_until_parked();
        }
        assert_eq!(
            hooks.document_events().len(),
            before,
            "each edit starts the wait again"
        );

        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();

        let events = hooks.document_events();
        assert_eq!(events.len(), before + 1);
        let last = events.last().expect("an event was sent");
        assert_eq!(last.kind, VisualMdDocumentEventKind::Changed);
        assert_eq!(
            last.outline.headings[0].text, "xxxTitle",
            "the outline is of the document as it is now"
        );
    }

    #[gpui::test]
    async fn test_the_wait_is_each_extensions_own(cx: &mut TestAppContext) {
        init_test(cx);
        let quick = FakeHooks::new(&cx.executor(), Behavior::Succeed);
        let slow = FakeHooks::new(&cx.executor(), Behavior::Succeed);
        register(
            cx,
            "quick",
            &events_section("changed = true\nchanged_debounce_ms = 300\n"),
            Some(quick.clone()),
        );
        register(
            cx,
            "slow",
            &events_section("changed = true\nchanged_debounce_ms = 3000\n"),
            Some(slow.clone()),
        );
        let mut cx = editor_showing(cx, DOCUMENT).await;
        settle(&mut cx);
        let (quick_before, slow_before) =
            (quick.document_events().len(), slow.document_events().len());

        cx.update_buffer(|buffer, cx| buffer.edit([(0..0, "x")], None, cx));
        cx.executor().advance_clock(Duration::from_millis(400));
        cx.run_until_parked();
        assert_eq!(quick.document_events().len(), quick_before + 1);
        assert_eq!(slow.document_events().len(), slow_before);

        cx.executor().advance_clock(Duration::from_secs(3));
        cx.run_until_parked();
        assert_eq!(slow.document_events().len(), slow_before + 1);
    }

    #[gpui::test]
    async fn test_the_wait_is_kept_within_its_bounds(cx: &mut TestAppContext) {
        let hooks = setup(cx, "changed = true\nchanged_debounce_ms = 1\n");
        let mut cx = editor_showing(cx, DOCUMENT).await;
        settle(&mut cx);
        let before = hooks.document_events().len();

        cx.update_buffer(|buffer, cx| buffer.edit([(0..0, "x")], None, cx));
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        assert_eq!(hooks.document_events().len(), before, "250 ms at the least");

        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        assert_eq!(hooks.document_events().len(), before + 1);
    }

    #[gpui::test]
    async fn test_an_extension_with_no_events_section_hears_nothing(cx: &mut TestAppContext) {
        init_test(cx);
        let hooks = FakeHooks::new(&cx.executor(), Behavior::Succeed);
        register(
            cx,
            "notes",
            "fence_renderers = [\"flow\"]\n",
            Some(hooks.clone()),
        );
        let mut cx = editor_showing(cx, DOCUMENT).await;
        cx.update_editor(|_, _, cx| cx.emit(editor::EditorEvent::Saved));
        settle(&mut cx);

        assert!(hooks.document_events().is_empty());
    }

    #[gpui::test]
    async fn test_a_document_that_is_not_markdown_reports_nothing(cx: &mut TestAppContext) {
        let hooks = setup(cx, "opened = true\nsaved = true\nchanged = true\n");
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state(DOCUMENT);
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));
        cx.update_editor(|_, _, cx| cx.emit(editor::EditorEvent::Saved));
        settle(&mut cx);

        assert!(hooks.document_events().is_empty());
    }

    #[gpui::test]
    async fn test_a_huge_document_is_skipped(cx: &mut TestAppContext) {
        let hooks = setup(cx, "opened = true\n");
        let huge = format!("ˇ{}\n", "word ".repeat(MAX_DOCUMENT_BYTES / 5 + 10));
        let mut cx = editor_showing(cx, &huge).await;
        settle(&mut cx);

        assert!(hooks.document_events().is_empty());
    }

    #[gpui::test]
    async fn test_a_failing_extension_does_not_get_in_the_way(cx: &mut TestAppContext) {
        init_test(cx);
        let hooks = FakeHooks::new(&cx.executor(), Behavior::Fail);
        register(
            cx,
            "notes",
            &events_section("opened = true\nsaved = true\n"),
            Some(hooks.clone()),
        );
        let mut cx = editor_showing(cx, DOCUMENT).await;
        cx.update_editor(|_, _, cx| cx.emit(editor::EditorEvent::Saved));
        settle(&mut cx);

        assert_eq!(hooks.document_events().len(), 2, "both were attempted");
        cx.set_state("ˇstill editable\n");
        cx.assert_editor_state("ˇstill editable\n");
    }

    #[gpui::test]
    async fn test_an_extension_registered_later_is_not_told_of_a_document_already_open(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let mut cx = editor_showing(cx, DOCUMENT).await;
        let hooks = FakeHooks::new(&cx.executor(), Behavior::Succeed);

        register(
            &mut cx,
            "notes",
            &events_section("opened = true\n"),
            Some(hooks.clone()),
        );
        cx.run_until_parked();
        settle(&mut cx);

        assert!(hooks.document_events().is_empty());
    }

    #[gpui::test]
    async fn test_unregistering_stops_the_events(cx: &mut TestAppContext) {
        let hooks = setup(cx, "changed = true\n");
        let mut cx = editor_showing(cx, DOCUMENT).await;
        settle(&mut cx);
        let before = hooks.document_events().len();

        cx.update_buffer(|buffer, cx| buffer.edit([(0..0, "x")], None, cx));
        cx.update(|_, cx| VisualMdExtensions::unregister("notes", cx));
        settle(&mut cx);

        assert_eq!(hooks.document_events().len(), before);
    }
}
