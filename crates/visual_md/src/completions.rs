//! Completions Zed MD adds to Markdown editors on top of the editor's own:
//! the commands of extensions when `/` is typed at the start of a line, and
//! what extensions suggest after `[[`.
//!
//! The provider wraps whichever one the editor had, forwards everything it does
//! not handle itself, and hands the original back when live preview stops
//! showing.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use editor::{CompletionContext, CompletionProvider, Editor};
use extension::{VisualMdCompletionItem, VisualMdCompletionRequest};
use gpui::{App, Context, Entity, Task, Window};
use language::{Anchor, Buffer, CodeLabel, Point, ToOffset as _};
use project::{
    Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource, Project,
    lsp_store::CompletionDocumentation,
};

use crate::VisualMdAddon;
use crate::commands::RunExtensionCommand;
use crate::extensions::{ExtensionCommand, HookError, VisualMdExtensions, when_not_busy};

/// The most Markdown files of the project that go to an extension with a
/// completion request.
pub const MAX_FILES_PER_REQUEST: usize = 2_000;

/// What the text before the cursor asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// `/` at the start of a line, then what was typed after it.
    Slash { query: String },
    /// `[[` or `![[`, then what was typed after it.
    Wikilink { query: String },
}

/// What `prefix`, the text of a line up to the cursor, asks completions for.
pub fn trigger_in(prefix: &str) -> Option<Trigger> {
    let command = prefix.trim_start().strip_prefix('/');
    if let Some(query) = command
        && query
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Some(Trigger::Slash {
            query: query.to_string(),
        });
    }

    let open = prefix.rfind("[[")?;
    let query = prefix.get(open + 2..)?;
    (!query.contains(['[', ']', '|'])).then(|| Trigger::Wikilink {
        query: query.to_string(),
    })
}

/// The project's Markdown files, relative to their worktrees' roots, at most
/// [`MAX_FILES_PER_REQUEST`] of them.
pub fn markdown_files(project: &Entity<Project>, cx: &App) -> Vec<String> {
    let mut files = Vec::new();
    for worktree in project.read(cx).visible_worktrees(cx) {
        let snapshot = worktree.read(cx).snapshot();
        for entry in snapshot.files(false, 0) {
            let is_markdown = entry
                .path
                .extension()
                .is_some_and(|extension| matches!(extension, "md" | "markdown"));
            if is_markdown {
                files.push(entry.path.as_unix_str().to_string());
                if files.len() >= MAX_FILES_PER_REQUEST {
                    return files;
                }
            }
        }
    }
    files
}

pub(crate) struct VisualMdCompletionProvider {
    /// The provider the editor had before, which still serves everything Zed MD
    /// does not.
    inner: Option<Rc<dyn CompletionProvider>>,
    project: Option<Entity<Project>>,
}

impl VisualMdCompletionProvider {
    /// The text of the line up to `position`.
    fn line_prefix(buffer: &Entity<Buffer>, position: Anchor, cx: &App) -> String {
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let row = snapshot.offset_to_point(offset).row;
        let line_start = snapshot.point_to_offset(Point::new(row, 0));
        snapshot.text_for_range(line_start..offset).collect()
    }

    fn slash_completions(
        query: &str,
        replace_range: Range<Anchor>,
        match_start: Anchor,
        cx: &App,
    ) -> CompletionResponse {
        let commands: Vec<ExtensionCommand> = cx
            .try_global::<VisualMdExtensions>()
            .map(|registry| registry.commands())
            .unwrap_or_default();
        let completions = commands
            .into_iter()
            .filter(|command| command.slash)
            .map(|command| {
                let id = command.qualified_id();
                Completion {
                    replace_range: replace_range.clone(),
                    // The command replaces what was typed, and the text goes
                    // before the command runs, so that it is not in what the
                    // command is given.
                    new_text: String::new(),
                    label: CodeLabel::plain(command.title.clone(), None),
                    documentation: command
                        .description
                        .map(|description| CompletionDocumentation::SingleLine(description.into())),
                    source: CompletionSource::Custom,
                    icon_path: None,
                    icon_color: None,
                    match_start: Some(match_start),
                    snippet_deduplication_key: None,
                    insert_text_mode: None,
                    confirm: Some(Arc::new(move |_, window, cx| {
                        window
                            .dispatch_action(Box::new(RunExtensionCommand { id: id.clone() }), cx);
                        false
                    })),
                    group: None,
                }
            })
            .collect::<Vec<_>>();
        log::debug!(
            "offering {} slash commands for `/{query}`",
            completions.len()
        );
        CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions::default(),
            is_incomplete: false,
        }
    }

    /// What the extensions suggest for `query` after `[[`.
    fn wikilink_completions(
        &self,
        query: String,
        document_path: Option<String>,
        replace_range: Range<Anchor>,
        close_brackets: bool,
        cx: &mut App,
    ) -> Task<CompletionResponse> {
        let completers = cx
            .try_global::<VisualMdExtensions>()
            .map(|registry| registry.wikilink_completers())
            .unwrap_or_default();
        let files = self
            .project
            .as_ref()
            .map(|project| markdown_files(project, cx))
            .unwrap_or_default();
        let request = VisualMdCompletionRequest {
            query,
            path: document_path,
            files,
        };
        cx.spawn(async move |cx| {
            let mut completions = Vec::new();
            for extension_id in completers {
                let answer = when_not_busy(cx, |cx| match cx.try_global::<VisualMdExtensions>() {
                    Some(registry) => registry.complete(&extension_id, request.clone(), cx),
                    None => Task::ready(Err(HookError::NotRegistered(extension_id.clone()))),
                })
                .await;
                match answer {
                    Ok(items) => {
                        completions.extend(items.into_iter().map(|item| {
                            completion_for(item, replace_range.clone(), close_brackets)
                        }))
                    }
                    Err(error) => log::warn!(
                        "extension {extension_id} could not complete a wikilink: {error}"
                    ),
                }
            }
            CompletionResponse {
                completions,
                display_options: CompletionDisplayOptions::default(),
                is_incomplete: true,
            }
        })
    }
}

fn completion_for(
    item: VisualMdCompletionItem,
    replace_range: Range<Anchor>,
    close_brackets: bool,
) -> Completion {
    let mut new_text = item.insert_text;
    if close_brackets {
        new_text.push_str("]]");
    }
    Completion {
        replace_range: replace_range.clone(),
        new_text,
        label: CodeLabel::plain(item.label, None),
        documentation: item
            .detail
            .map(|detail| CompletionDocumentation::SingleLine(detail.into())),
        source: CompletionSource::Custom,
        icon_path: None,
        icon_color: None,
        match_start: Some(replace_range.start),
        snippet_deduplication_key: None,
        insert_text_mode: None,
        confirm: None,
        group: None,
    }
}

impl CompletionProvider for VisualMdCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: Anchor,
        context: CompletionContext,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let inner = match &self.inner {
            Some(inner) => inner.completions(buffer, buffer_position, context, window, cx),
            None => Task::ready(Ok(Vec::new())),
        };
        let trigger = trigger_in(&Self::line_prefix(buffer, buffer_position, cx));
        let Some(trigger) = trigger else {
            return inner;
        };

        let snapshot = buffer.read(cx).snapshot();
        let cursor = buffer_position.to_offset(&snapshot);
        let query_length = match &trigger {
            Trigger::Slash { query } | Trigger::Wikilink { query } => query.len(),
        };
        let query_start = cursor.saturating_sub(query_length);
        let extra = match &trigger {
            Trigger::Slash { .. } => 1,
            Trigger::Wikilink { .. } => 0,
        };
        let replace_range = snapshot.anchor_before(query_start.saturating_sub(extra))
            ..snapshot.anchor_after(cursor);
        let match_start = snapshot.anchor_before(query_start);

        let ours: Task<CompletionResponse> = match trigger {
            Trigger::Slash { query } => Task::ready(Self::slash_completions(
                &query,
                replace_range,
                match_start,
                cx,
            )),
            Trigger::Wikilink { query } => {
                let after_cursor: String = snapshot
                    .text_for_range(cursor..(cursor + 2).min(snapshot.len()))
                    .collect();
                self.wikilink_completions(
                    query,
                    crate::fence_render::buffer_path(buffer, cx),
                    snapshot.anchor_before(query_start)..snapshot.anchor_after(cursor),
                    after_cursor != "]]",
                    cx,
                )
            }
        };

        cx.spawn(async move |_, _| {
            let mut responses = match inner.await {
                Ok(responses) => responses,
                Err(error) => {
                    log::warn!("the editor's own completions failed: {error:#}");
                    Vec::new()
                }
            };
            responses.push(ours.await);
            Ok(responses)
        })
    }

    fn resolve_completions(
        &self,
        buffer: Entity<Buffer>,
        completion_indices: Vec<usize>,
        completions: Rc<RefCell<Box<[Completion]>>>,
        cx: &mut Context<Editor>,
    ) -> Task<Result<bool>> {
        match &self.inner {
            Some(inner) => inner.resolve_completions(buffer, completion_indices, completions, cx),
            None => Task::ready(Ok(false)),
        }
    }

    fn apply_additional_edits_for_completion(
        &self,
        buffer: Entity<Buffer>,
        completions: Rc<RefCell<Box<[Completion]>>>,
        completion_index: usize,
        push_to_history: bool,
        all_commit_ranges: Vec<Range<Anchor>>,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Option<language::Transaction>>> {
        match &self.inner {
            Some(inner) => inner.apply_additional_edits_for_completion(
                buffer,
                completions,
                completion_index,
                push_to_history,
                all_commit_ranges,
                cx,
            ),
            None => Task::ready(Ok(None)),
        }
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: Anchor,
        text: &str,
        trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        let starts_menu = match text {
            "/" => matches!(
                trigger_in(&Self::line_prefix(buffer, position, cx)),
                Some(Trigger::Slash { query }) if query.is_empty()
            ),
            "[" => matches!(
                trigger_in(&Self::line_prefix(buffer, position, cx)),
                Some(Trigger::Wikilink { query }) if query.is_empty()
            ),
            _ => false,
        };
        starts_menu
            || self.inner.as_ref().is_some_and(|inner| {
                inner.is_completion_trigger(buffer, position, text, trigger_in_words, cx)
            })
    }

    fn selection_changed(
        &self,
        mat: Option<&fuzzy::StringMatch>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(inner) = &self.inner {
            inner.selection_changed(mat, window, cx);
        }
    }

    fn sort_completions(&self) -> bool {
        self.inner
            .as_ref()
            .is_none_or(|inner| inner.sort_completions())
    }

    fn filter_completions(&self) -> bool {
        self.inner
            .as_ref()
            .is_none_or(|inner| inner.filter_completions())
    }

    fn show_snippets(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.show_snippets())
    }
}

/// Makes the editor ask Zed MD for completions as well as whatever it asked
/// before.
pub(crate) fn install(editor: &mut Editor) {
    let provider = Rc::new(VisualMdCompletionProvider {
        inner: editor.completion_provider(),
        project: editor.project().cloned(),
    });
    editor.set_completion_provider(Some(provider.clone()));
    if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
        addon.completion_provider = Some(provider);
    }
}

/// Gives the editor back the provider it had, unless something else has
/// replaced Zed MD's in the meantime, which is then left in place.
pub(crate) fn uninstall(editor: &mut Editor) {
    let Some(provider) = editor
        .addon_mut::<VisualMdAddon>()
        .and_then(|addon| addon.completion_provider.take())
    else {
        return;
    };
    let is_ours = editor
        .completion_provider()
        .is_some_and(|current| std::ptr::addr_eq(Rc::as_ptr(&current), Rc::as_ptr(&provider)));
    if is_ours {
        editor.set_completion_provider(provider.inner.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slash(query: &str) -> Option<Trigger> {
        Some(Trigger::Slash {
            query: query.to_string(),
        })
    }

    fn wikilink(query: &str) -> Option<Trigger> {
        Some(Trigger::Wikilink {
            query: query.to_string(),
        })
    }

    #[test]
    fn test_a_slash_at_the_start_of_a_line_asks_for_commands() {
        assert_eq!(trigger_in("/"), slash(""));
        assert_eq!(trigger_in("/ins"), slash("ins"));
        assert_eq!(trigger_in("   /insert-date"), slash("insert-date"));
        assert_eq!(trigger_in("\t/a_b"), slash("a_b"));
    }

    #[test]
    fn test_a_slash_elsewhere_asks_for_nothing() {
        assert_eq!(trigger_in("text /"), None);
        assert_eq!(trigger_in("- /ins"), None);
        assert_eq!(trigger_in("/two words"), None);
        assert_eq!(trigger_in("/a/b"), None);
        assert_eq!(trigger_in("// comment"), None);
        assert_eq!(trigger_in(""), None);
        assert_eq!(trigger_in("plain text"), None);
    }

    #[test]
    fn test_double_brackets_ask_for_names() {
        assert_eq!(trigger_in("[["), wikilink(""));
        assert_eq!(trigger_in("see [[No"), wikilink("No"));
        assert_eq!(trigger_in("![[pi"), wikilink("pi"));
        assert_eq!(trigger_in("[[a]] and [[b"), wikilink("b"));
        assert_eq!(trigger_in("[[two words"), wikilink("two words"));
        assert_eq!(trigger_in("- [[é"), wikilink("é"));
    }

    #[test]
    fn test_closed_or_aliased_brackets_ask_for_nothing() {
        assert_eq!(trigger_in("[[a]]"), None);
        assert_eq!(trigger_in("[[a|b"), None);
        assert_eq!(trigger_in("[[a[b"), None);
        assert_eq!(trigger_in("[single"), None);
        assert_eq!(trigger_in("[x](y"), None);
    }
}

#[cfg(test)]
mod integration_tests {
    use editor::actions::ConfirmCompletion;
    use editor::test::editor_test_context::EditorTestContext;
    use extension::{VisualMdCommandResult, VisualMdTextEdit};
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;

    use crate::extensions::test_support::{Behavior, FakeHooks, register};
    use crate::integration_tests::{init_test, markdown_language};

    use super::*;

    const EXTENSION: &str = "[visual_md.commands.insert-date]\ntitle = \"Insert Date\"\ndescription = \"Inserts today.\"\nslash = true\n[visual_md.commands.sort]\ntitle = \"Sort Lines\"\n[visual_md.commands.insert-table]\ntitle = \"Insert Table\"\nslash = true\n[visual_md.links]\nwikilink_completions = true\n";

    fn setup(cx: &mut TestAppContext) -> Arc<FakeHooks> {
        init_test(cx);
        cx.update(|cx| theme_settings::init(theme::LoadThemes::JustBase, cx));
        let hooks = FakeHooks::new(&cx.executor(), Behavior::Succeed);
        hooks.set_completion_responder(|request| {
            ["Alpha", "Beta", "Gamma"]
                .into_iter()
                .filter(|name| {
                    name.to_lowercase()
                        .starts_with(&request.query.to_lowercase())
                })
                .map(|name| VisualMdCompletionItem {
                    label: name.to_string(),
                    detail: Some(format!("{name}.md")),
                    insert_text: name.to_string(),
                })
                .collect()
        });
        register(cx, "notes", EXTENSION, Some(hooks.clone()));
        hooks
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

    fn type_text(cx: &mut EditorTestContext, text: &str) {
        cx.update_editor(|editor, window, cx| editor.handle_input(text, window, cx));
        cx.run_until_parked();
    }

    fn labels(cx: &mut EditorTestContext) -> Option<Vec<String>> {
        cx.update_editor(|editor, _, _| {
            editor.current_completions().map(|completions| {
                completions
                    .iter()
                    .map(|completion| completion.label.text().to_string())
                    .collect()
            })
        })
    }

    async fn confirm(cx: &mut EditorTestContext) {
        let task = cx.update_editor(|editor, window, cx| {
            editor.confirm_completion(&ConfirmCompletion::default(), window, cx)
        });
        if let Some(task) = task {
            task.await.expect("the completion is applied");
        }
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_a_slash_at_the_start_of_a_line_offers_the_slash_commands(
        cx: &mut TestAppContext,
    ) {
        setup(cx);
        let mut cx = editor_showing(cx, "text\nˇ").await;

        type_text(&mut cx, "/");

        let labels = labels(&mut cx).expect("the menu is open");
        assert_eq!(labels, vec!["Insert Date", "Insert Table"]);
    }

    #[gpui::test]
    async fn test_a_slash_in_the_middle_of_a_line_offers_nothing_of_ours(cx: &mut TestAppContext) {
        setup(cx);
        let mut cx = editor_showing(cx, "textˇ").await;

        type_text(&mut cx, " /");

        let offered = labels(&mut cx).unwrap_or_default();
        assert!(!offered.contains(&"Insert Date".to_string()), "{offered:?}");
    }

    #[gpui::test]
    async fn test_typing_after_the_slash_narrows_the_commands(cx: &mut TestAppContext) {
        let hooks = setup(cx);
        let mut cx = editor_showing(cx, "ˇ").await;

        type_text(&mut cx, "/");
        type_text(&mut cx, "t");
        type_text(&mut cx, "a");
        confirm(&mut cx).await;

        assert_eq!(
            hooks.command_ids(),
            vec!["insert-table"],
            "`ta` narrows the menu to Insert Table, which is what is chosen first"
        );
    }

    #[gpui::test]
    async fn test_choosing_a_slash_command_removes_what_was_typed_and_runs_it(
        cx: &mut TestAppContext,
    ) {
        let hooks = setup(cx);
        hooks.set_command_result(VisualMdCommandResult {
            edits: vec![VisualMdTextEdit {
                range: 5..5,
                new_text: "2026-10-04".to_string(),
            }],
            ..Default::default()
        });
        let mut cx = editor_showing(cx, "text\nˇ").await;

        type_text(&mut cx, "/");
        type_text(&mut cx, "date");
        confirm(&mut cx).await;
        cx.run_until_parked();

        let contexts = hooks.command_contexts();
        assert_eq!(contexts.len(), 1);
        assert_eq!(
            contexts[0].text, "text\n",
            "the command is given the text without what was typed to ask for it"
        );
        cx.assert_editor_state("text\n2026-10-04ˇ");
    }

    #[gpui::test]
    async fn test_double_brackets_offer_what_extensions_suggest(cx: &mut TestAppContext) {
        let hooks = setup(cx);
        let mut cx = editor_showing(cx, "see ˇ").await;

        type_text(&mut cx, "[");
        type_text(&mut cx, "[");

        assert_eq!(
            labels(&mut cx),
            Some(vec![
                "Alpha".to_string(),
                "Beta".to_string(),
                "Gamma".to_string()
            ])
        );
        let requests = hooks.completion_requests();
        assert_eq!(
            requests.last().map(|request| request.query.as_str()),
            Some("")
        );
        assert_eq!(
            requests.last().and_then(|request| request.path.as_deref()),
            Some("/root/file")
        );

        type_text(&mut cx, "b");
        assert_eq!(labels(&mut cx), Some(vec!["Beta".to_string()]));
        assert_eq!(
            hooks
                .completion_requests()
                .last()
                .map(|request| request.query.clone()),
            Some("b".to_string()),
            "the extension is asked again as the query grows"
        );
    }

    #[gpui::test]
    async fn test_choosing_a_wikilink_completion_fills_in_the_name_and_closes_the_brackets(
        cx: &mut TestAppContext,
    ) {
        setup(cx);
        let mut cx = editor_showing(cx, "see ˇ").await;

        type_text(&mut cx, "[");
        type_text(&mut cx, "[");
        type_text(&mut cx, "b");
        type_text(&mut cx, "e");
        confirm(&mut cx).await;

        cx.assert_editor_state("see [[Beta]]ˇ");
    }

    #[gpui::test]
    async fn test_already_closed_brackets_are_not_closed_twice(cx: &mut TestAppContext) {
        setup(cx);
        let mut cx = editor_showing(cx, "see [[gaˇ]]").await;

        cx.update_editor(|editor, window, cx| {
            editor.show_completions(&editor::actions::ShowCompletions, window, cx)
        });
        cx.run_until_parked();
        confirm(&mut cx).await;

        assert_eq!(
            cx.update_editor(|editor, _, cx| editor.text(cx)),
            "see [[Gamma]]"
        );
    }

    #[gpui::test]
    async fn test_a_failing_extension_still_leaves_the_rest_of_the_menu(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| theme_settings::init(theme::LoadThemes::JustBase, cx));
        let hooks = FakeHooks::new(&cx.executor(), Behavior::Fail);
        register(cx, "notes", EXTENSION, Some(hooks));
        let mut cx = editor_showing(cx, "ˇ").await;

        type_text(&mut cx, "/");

        assert_eq!(
            labels(&mut cx),
            Some(vec!["Insert Date".to_string(), "Insert Table".to_string()]),
            "the slash commands need no call into the extension"
        );
        type_text(&mut cx, "[");
        type_text(&mut cx, "[");
        assert!(labels(&mut cx).unwrap_or_default().is_empty());
    }

    /// A provider that offers one fixed item, to see what Zed MD forwards.
    struct FixedProvider {
        triggers: Rc<RefCell<Vec<String>>>,
    }

    impl CompletionProvider for FixedProvider {
        fn completions(
            &self,
            _buffer: &Entity<Buffer>,
            buffer_position: Anchor,
            _context: CompletionContext,
            _window: &mut Window,
            _cx: &mut Context<Editor>,
        ) -> Task<Result<Vec<CompletionResponse>>> {
            Task::ready(Ok(vec![CompletionResponse {
                completions: vec![Completion {
                    replace_range: buffer_position..buffer_position,
                    new_text: "inner".to_string(),
                    label: CodeLabel::plain("Inner".to_string(), None),
                    documentation: None,
                    source: CompletionSource::Custom,
                    icon_path: None,
                    icon_color: None,
                    match_start: None,
                    snippet_deduplication_key: None,
                    insert_text_mode: None,
                    confirm: None,
                    group: None,
                }],
                display_options: CompletionDisplayOptions::default(),
                is_incomplete: false,
            }]))
        }

        fn is_completion_trigger(
            &self,
            _buffer: &Entity<Buffer>,
            _position: Anchor,
            text: &str,
            _trigger_in_words: bool,
            _cx: &mut Context<Editor>,
        ) -> bool {
            self.triggers.borrow_mut().push(text.to_string());
            text == "."
        }
    }

    #[gpui::test]
    async fn test_what_the_editor_had_still_serves_everything_else(cx: &mut TestAppContext) {
        setup(cx);
        let triggers = Rc::new(RefCell::new(Vec::new()));
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ");
        cx.update_editor(|editor, _, _| {
            editor.set_completion_provider(Some(Rc::new(FixedProvider {
                triggers: triggers.clone(),
            })));
        });
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));

        type_text(&mut cx, ".");
        assert_eq!(
            labels(&mut cx),
            Some(vec!["Inner".to_string()]),
            "its own trigger still opens its own menu"
        );
        assert!(triggers.borrow().contains(&".".to_string()));

        cx.update_editor(|editor, window, cx| editor.cancel(&Default::default(), window, cx));
        type_text(&mut cx, "x");
        type_text(&mut cx, "\n");
        type_text(&mut cx, "/");
        let offered = labels(&mut cx).unwrap_or_default();
        assert!(
            offered.contains(&"Inner".to_string()) && offered.contains(&"Insert Date".to_string()),
            "both are offered after a slash: {offered:?}"
        );
    }

    #[gpui::test]
    async fn test_leaving_live_preview_gives_the_editor_its_provider_back(cx: &mut TestAppContext) {
        setup(cx);
        let triggers = Rc::new(RefCell::new(Vec::new()));
        let original: Rc<dyn CompletionProvider> = Rc::new(FixedProvider { triggers });
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ");
        cx.update_editor(|editor, _, _| editor.set_completion_provider(Some(original.clone())));
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));
        let is_original = |cx: &mut EditorTestContext, original: &Rc<dyn CompletionProvider>| {
            cx.update_editor(|editor, _, _| {
                editor.completion_provider().is_some_and(|current| {
                    std::ptr::addr_eq(Rc::as_ptr(&current), Rc::as_ptr(original))
                })
            })
        };
        assert!(!is_original(&mut cx, &original), "Zed MD's provider is in");

        cx.dispatch_action(crate::ToggleLivePreview);
        assert!(is_original(&mut cx, &original), "the original is back");

        cx.dispatch_action(crate::ToggleLivePreview);
        assert!(!is_original(&mut cx, &original), "and wrapped again");
    }

    #[gpui::test]
    async fn test_a_provider_someone_else_installed_is_left_in_place(cx: &mut TestAppContext) {
        setup(cx);
        let mut cx = editor_showing(cx, "ˇ").await;
        let theirs: Rc<dyn CompletionProvider> = Rc::new(FixedProvider {
            triggers: Rc::default(),
        });
        cx.update_editor(|editor, _, _| editor.set_completion_provider(Some(theirs.clone())));

        cx.dispatch_action(crate::ToggleLivePreview);

        let kept = cx.update_editor(|editor, _, _| {
            editor
                .completion_provider()
                .is_some_and(|current| std::ptr::addr_eq(Rc::as_ptr(&current), Rc::as_ptr(&theirs)))
        });
        assert!(kept);
    }

    #[gpui::test]
    async fn test_a_buffer_that_is_not_markdown_has_no_slash_menu(cx: &mut TestAppContext) {
        setup(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ");
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));

        type_text(&mut cx, "/");

        let offered = labels(&mut cx).unwrap_or_default();
        assert!(!offered.contains(&"Insert Date".to_string()), "{offered:?}");
    }

    #[gpui::test]
    async fn test_the_projects_markdown_files_are_listed_for_the_extension(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/dir",
            json!({
                "a.md": "", "b.markdown": "", "c.txt": "", "d.MD": "",
                "sub": { "e.md": "", "f.rs": "" },
            }),
        )
        .await;
        let project = Project::test(fs, ["/dir".as_ref()], cx).await;

        let mut files = cx.update(|cx| markdown_files(&project, cx));
        files.sort();

        assert_eq!(files, vec!["a.md", "b.markdown", "sub/e.md"]);
    }

    #[gpui::test]
    async fn test_at_most_2000_files_go_to_an_extension(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let files: serde_json::Map<String, serde_json::Value> = (0..MAX_FILES_PER_REQUEST + 50)
            .map(|index| (format!("note{index}.md"), json!("")))
            .collect();
        fs.insert_tree("/dir", serde_json::Value::Object(files))
            .await;
        let project = Project::test(fs, ["/dir".as_ref()], cx).await;

        let listed = cx.update(|cx| markdown_files(&project, cx));

        assert_eq!(listed.len(), MAX_FILES_PER_REQUEST);
    }
}
