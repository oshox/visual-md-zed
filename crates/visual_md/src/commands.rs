//! Editor commands that extensions provide: running one on the document, and
//! offering them in the command palette.

use std::ops::Range;

use command_palette_hooks::{
    CommandInterceptItem, CommandInterceptResult, GlobalCommandPaletteInterceptor,
};
use editor::{Bias, Editor, MultiBufferOffset, SelectionEffects};
use extension::{VisualMdCommandContext, VisualMdCommandResult, VisualMdTextEdit};
use gpui::{Action, App, Context, Task, TaskExt as _, WeakEntity, Window};
use schemars::JsonSchema;
use serde::Deserialize;
use workspace::{Toast, Workspace, notifications::NotificationId};

use crate::VisualMdAddon;
use crate::extensions::{ExtensionCommand, HookError, VisualMdExtensions};

const PALETTE_INTERCEPTOR_KEY: &str = "visual_md";

/// The most an extension may change in one command, so that a runaway one cannot
/// make the editor allocate without bound.
pub const MAX_EDITS_PER_COMMAND: usize = 100_000;
pub const MAX_INSERTED_BYTES_PER_COMMAND: usize = 16 * 1024 * 1024;

/// Runs an editor command that an extension provides, named `<extension id>.<command id>`.
///
/// Bind it with `["visual_md::RunExtensionCommand", {"id": "my-extension.my-command"}]`.
/// The commands also appear in the command palette while Markdown live preview is
/// showing in the active editor.
#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = visual_md)]
#[serde(deny_unknown_fields)]
pub struct RunExtensionCommand {
    pub id: String,
}

pub(crate) fn init(cx: &mut App) {
    GlobalCommandPaletteInterceptor::register(cx, PALETTE_INTERCEPTOR_KEY, palette_commands);
}

/// Checks an extension's edits against `text` and puts them in order. The edits
/// are all in the coordinates of `text`, so they must not overlap, and each must
/// lie inside it on character boundaries.
pub(crate) fn validate_edits(
    text: &str,
    mut edits: Vec<VisualMdTextEdit>,
) -> Result<Vec<(Range<usize>, String)>, String> {
    if edits.len() > MAX_EDITS_PER_COMMAND {
        return Err(format!(
            "it returned {} edits, over the limit of {MAX_EDITS_PER_COMMAND}",
            edits.len()
        ));
    }
    let inserted_bytes: usize = edits.iter().map(|edit| edit.new_text.len()).sum();
    if inserted_bytes > MAX_INSERTED_BYTES_PER_COMMAND {
        return Err(format!(
            "it inserts {inserted_bytes} bytes, over the limit of {MAX_INSERTED_BYTES_PER_COMMAND}"
        ));
    }

    edits.sort_by_key(|edit| edit.range.start);
    let mut previous_end = 0;
    for edit in &edits {
        let range = &edit.range;
        if range.start > range.end {
            return Err(format!("the range {range:?} ends before it starts"));
        }
        if range.end > text.len() {
            return Err(format!(
                "the range {range:?} is past the end of the {} byte document",
                text.len()
            ));
        }
        if !text.is_char_boundary(range.start) || !text.is_char_boundary(range.end) {
            return Err(format!("the range {range:?} splits a character"));
        }
        if range.start < previous_end {
            return Err(format!("the range {range:?} overlaps an earlier edit"));
        }
        previous_end = range.end;
    }
    Ok(edits
        .into_iter()
        .map(|edit| (edit.range, edit.new_text))
        .collect())
}

pub(crate) fn run_extension_command(
    editor: &mut Editor,
    action: &RunExtensionCommand,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    if !crate::live_preview_enabled(editor, cx) {
        cx.propagate();
        return;
    }
    let Some(command) = cx
        .try_global::<VisualMdExtensions>()
        .and_then(|registry| registry.command(&action.id))
    else {
        cx.propagate();
        return;
    };
    if editor.read_only(cx) {
        notify(
            editor,
            format!("{} cannot change a read-only document", command.title),
            cx,
        );
        return;
    }

    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let edit_count = snapshot.edit_count();
    let display_snapshot = editor.display_snapshot(cx);
    let selections = editor
        .selections
        .all::<MultiBufferOffset>(&display_snapshot)
        .into_iter()
        .map(|selection| {
            let range = selection.range();
            range.start.0..range.end.0
        })
        .collect();
    let context = VisualMdCommandContext {
        text: snapshot.text(),
        selections,
        path: crate::fence_render::note_path(editor, cx),
    };

    let call = cx.try_global::<VisualMdExtensions>().map(|registry| {
        registry.run_command(
            &command.extension_id,
            command.command.to_string(),
            context,
            cx,
        )
    });
    let Some(call) = call else {
        return;
    };
    cx.spawn_in(window, async move |editor, cx| {
        let result = call.await;
        editor.update_in(cx, |editor, window, cx| {
            finish_command(editor, &command, edit_count, result, window, cx)
        })
    })
    .detach_and_log_err(cx);
}

fn finish_command(
    editor: &mut Editor,
    command: &ExtensionCommand,
    edit_count: usize,
    result: Result<VisualMdCommandResult, HookError>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            log::error!(
                "extension command {} failed: {error}",
                command.qualified_id()
            );
            notify(editor, format!("{} failed: {error}", command.title), cx);
            return;
        }
    };

    let snapshot = editor.buffer().read(cx).snapshot(cx);
    if snapshot.edit_count() != edit_count {
        log::info!(
            "dropping the result of extension command {}: the document changed while it ran",
            command.qualified_id()
        );
        notify(
            editor,
            format!(
                "{} was not applied because the text changed while it ran",
                command.title
            ),
            cx,
        );
        return;
    }

    let edits = match validate_edits(&snapshot.text(), result.edits) {
        Ok(edits) => edits,
        Err(problem) => {
            log::error!(
                "extension command {} returned invalid edits: {problem}",
                command.qualified_id()
            );
            notify(
                editor,
                format!(
                    "{} returned edits that cannot be applied: {problem}",
                    command.title
                ),
                cx,
            );
            return;
        }
    };

    if !edits.is_empty() || result.selections.is_some() {
        let selections = result.selections;
        editor.transact(window, cx, |editor, window, cx| {
            editor.edit(
                edits.into_iter().map(|(range, new_text)| {
                    (
                        MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                        new_text,
                    )
                }),
                cx,
            );

            let Some(selections) = selections else {
                return;
            };
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let is_valid_offset = |offset: usize| {
                snapshot
                    .clip_offset(MultiBufferOffset(offset), Bias::Left)
                    .0
                    == offset
            };
            let ranges: Vec<_> = selections
                .into_iter()
                .filter(|range| {
                    range.start <= range.end
                        && is_valid_offset(range.start)
                        && is_valid_offset(range.end)
                })
                .map(|range| MultiBufferOffset(range.start)..MultiBufferOffset(range.end))
                .collect();
            if ranges.is_empty() {
                log::warn!(
                    "extension command {} returned no usable selections, keeping the editor's",
                    command.qualified_id()
                );
                return;
            }
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections.select_ranges(ranges);
            });
        });
    }

    if let Some(message) = result.message {
        notify(editor, message, cx);
    }
}

fn notify(editor: &Editor, message: String, cx: &mut Context<Editor>) {
    let Some(workspace) = editor.workspace() else {
        log::info!("{message}");
        return;
    };
    workspace.update(cx, |workspace, cx| {
        workspace.show_toast(
            Toast::new(NotificationId::unique::<RunExtensionCommand>(), message),
            cx,
        )
    });
}

/// The commands whose palette entry, `<extension name>: <title>`, contains
/// `query` ignoring case, with the character positions that matched.
pub(crate) fn matching_commands(
    query: &str,
    commands: Vec<ExtensionCommand>,
) -> Vec<(ExtensionCommand, String, Vec<usize>)> {
    let query: Vec<char> = query.trim().to_lowercase().chars().collect();
    if query.is_empty() {
        return Vec::new();
    }
    commands
        .into_iter()
        .filter_map(|command| {
            let label = format!("{}: {}", command.extension_name, command.title);
            let lowered: Vec<char> = label.to_lowercase().chars().collect();
            // Lowercasing can change a character's length, which would
            // misplace the highlights, so such labels are only matched, not
            // highlighted.
            let same_length = lowered.len() == label.chars().count();
            let start = lowered
                .windows(query.len())
                .position(|window| window == query.as_slice())?;
            let positions = if same_length {
                (start..start + query.len()).collect()
            } else {
                Vec::new()
            };
            Some((command, label, positions))
        })
        .collect()
}

fn palette_commands(
    query: &str,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) -> Task<CommandInterceptResult> {
    let nothing = || Task::ready(CommandInterceptResult::default());
    let Some(workspace) = workspace.upgrade() else {
        return nothing();
    };
    let Some(editor) = workspace.read(cx).active_item_as::<Editor>(cx) else {
        return nothing();
    };
    let is_live_preview_active = editor
        .read(cx)
        .addon::<VisualMdAddon>()
        .is_some_and(|addon| addon.active);
    if !is_live_preview_active {
        return nothing();
    }
    let Some(registry) = cx.try_global::<VisualMdExtensions>() else {
        return nothing();
    };

    let results = matching_commands(query, registry.commands())
        .into_iter()
        .map(|(command, label, positions)| CommandInterceptItem {
            action: Box::new(RunExtensionCommand {
                id: command.qualified_id(),
            }),
            string: label,
            positions,
        })
        .collect();
    Task::ready(CommandInterceptResult {
        results,
        exclusive: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(range: Range<usize>, new_text: &str) -> VisualMdTextEdit {
        VisualMdTextEdit {
            range,
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn test_edits_are_ordered_and_kept_in_original_coordinates() {
        let edits = validate_edits(
            "- one\n- two\n",
            vec![edit(8..11, "TWO"), edit(2..5, "ONE!!")],
        );

        assert_eq!(
            edits,
            Ok(vec![
                (2..5, "ONE!!".to_string()),
                (8..11, "TWO".to_string())
            ])
        );
    }

    #[test]
    fn test_adjacent_and_zero_width_edits_are_allowed() {
        assert!(validate_edits("abcd", vec![edit(0..2, "x"), edit(2..4, "y")]).is_ok());
        assert!(validate_edits("abcd", vec![edit(2..2, "x"), edit(2..2, "y")]).is_ok());
        assert!(validate_edits("abcd", vec![edit(0..4, ""), edit(4..4, "tail")]).is_ok());
        assert_eq!(validate_edits("abcd", Vec::new()), Ok(Vec::new()));
    }

    #[test]
    fn test_invalid_edits_are_rejected() {
        let reversed = Range { start: 3, end: 1 };
        // `é` is two bytes, so 1..2 splits it.
        let cases: Vec<(&str, Vec<VisualMdTextEdit>)> = vec![
            ("abcd", vec![edit(0..5, "x")]),
            ("abcd", vec![edit(5..5, "x")]),
            ("abcd", vec![edit(reversed, "x")]),
            ("abcd", vec![edit(0..3, "x"), edit(2..4, "y")]),
            ("abcd", vec![edit(0..4, "x"), edit(1..2, "y")]),
            ("aéb", vec![edit(1..2, "x")]),
            ("aéb", vec![edit(2..3, "x")]),
        ];
        for (text, edits) in cases {
            assert!(
                validate_edits(text, edits.clone()).is_err(),
                "{edits:?} on {text:?} should be rejected"
            );
        }
    }

    #[test]
    fn test_too_many_edits_or_too_much_text_is_rejected() {
        let many = (0..=MAX_EDITS_PER_COMMAND)
            .map(|_| edit(0..0, ""))
            .collect::<Vec<_>>();
        assert!(validate_edits("abc", many).is_err());

        let large = "x".repeat(MAX_INSERTED_BYTES_PER_COMMAND + 1);
        assert!(validate_edits("abc", vec![edit(0..0, &large)]).is_err());
    }

    fn command(extension_name: &str, title: &str) -> ExtensionCommand {
        ExtensionCommand {
            extension_id: "notes".into(),
            extension_name: extension_name.to_string(),
            command: "uppercase".into(),
            title: title.to_string(),
            description: None,
        }
    }

    #[test]
    fn test_palette_matching_ignores_case_and_reports_positions() {
        let matches = matching_commands(
            "UPPER",
            vec![
                command("Notes", "Uppercase Selection"),
                command("Notes", "Sort"),
            ],
        );

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].1, "Notes: Uppercase Selection");
        assert_eq!(matches[0].2, vec![7, 8, 9, 10, 11]);
    }

    #[test]
    fn test_palette_matching_includes_the_extension_name() {
        let matches = matching_commands("notes: up", vec![command("Notes", "Uppercase")]);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].2, (0..9).collect::<Vec<_>>());
    }

    #[test]
    fn test_an_empty_query_matches_nothing() {
        assert!(matching_commands("  ", vec![command("Notes", "Uppercase")]).is_empty());
    }
}

#[cfg(test)]
mod integration_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use editor::test::editor_test_context::EditorTestContext;
    use extension::VisualMdCommandResult;
    use fs::FakeFs;
    use gpui::{AppContext as _, Entity, TestAppContext, VisualContext as _, VisualTestContext};
    use project::Project;
    use workspace::MultiWorkspace;

    use crate::extensions::test_support::{Behavior, FakeHooks, register};
    use crate::integration_tests::{init_test, markdown_language};

    use super::*;

    const COMMANDS: &str = "[visual_md.commands.uppercase]\ntitle = \"Uppercase Selection\"\n";

    fn setup(cx: &mut TestAppContext, behavior: Behavior) -> Arc<FakeHooks> {
        init_test(cx);
        let hooks = FakeHooks::new(&cx.executor(), behavior);
        register(cx, "notes", COMMANDS, Some(hooks.clone()));
        hooks
    }

    async fn editor_showing(cx: &mut TestAppContext, text: &str) -> EditorTestContext {
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state(text);
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));
        cx
    }

    fn run(cx: &mut EditorTestContext, id: &str) {
        cx.dispatch_action(RunExtensionCommand { id: id.to_string() });
        cx.run_until_parked();
    }

    fn buffer_text(cx: &mut EditorTestContext) -> String {
        cx.update_editor(|editor, _, cx| editor.buffer().read(cx).snapshot(cx).text())
    }

    fn edit(range: Range<usize>, new_text: &str) -> VisualMdTextEdit {
        VisualMdTextEdit {
            range,
            new_text: new_text.to_string(),
        }
    }

    #[gpui::test]
    fn test_the_action_needs_an_id_so_the_palette_does_not_list_it(cx: &mut TestAppContext) {
        init_test(cx);

        cx.update(|cx| {
            assert!(
                cx.build_action("visual_md::RunExtensionCommand", None)
                    .is_err(),
                "the palette builds actions without data, and only lists those that build"
            );
            let action = cx
                .build_action(
                    "visual_md::RunExtensionCommand",
                    Some(serde_json::json!({ "id": "notes.uppercase" })),
                )
                .expect("an action with an id builds");
            assert!(action.partial_eq(&RunExtensionCommand {
                id: "notes.uppercase".to_string()
            }));
            assert!(
                cx.build_action(
                    "visual_md::RunExtensionCommand",
                    Some(serde_json::json!({ "id": "a.b", "extra": 1 })),
                )
                .is_err()
            );
        });
    }

    #[gpui::test]
    async fn test_a_command_receives_the_text_the_selections_and_the_path(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, "- «oneˇ»\n- twoˇ\n").await;

        run(&mut cx, "notes.uppercase");

        let contexts = hooks.command_contexts();
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].text, "- one\n- two\n");
        assert_eq!(contexts[0].selections, vec![2..5, 11..11]);
        assert_eq!(contexts[0].path.as_deref(), Some("/root/file"));
    }

    #[gpui::test]
    async fn test_edits_are_applied_in_the_coordinates_of_the_original_text(
        cx: &mut TestAppContext,
    ) {
        let hooks = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, "ˇ- one\n- two\n").await;
        hooks.set_command_result(VisualMdCommandResult {
            // The first edit makes the text longer, which must not move the second.
            edits: vec![edit(8..11, "TWO"), edit(2..5, "ONE!!")],
            selections: Some(vec![7..7, 13..13]),
            message: None,
        });

        run(&mut cx, "notes.uppercase");

        cx.assert_editor_state("- ONE!!ˇ\n- TWOˇ\n");
    }

    #[gpui::test]
    async fn test_a_command_is_one_undo_step(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, "ˇ- one\n- two\n").await;
        hooks.set_command_result(VisualMdCommandResult {
            edits: vec![edit(2..5, "ONE"), edit(8..11, "TWO")],
            ..Default::default()
        });

        run(&mut cx, "notes.uppercase");
        assert_eq!(buffer_text(&mut cx), "- ONE\n- TWO\n");
        cx.update_editor(|editor, window, cx| editor.undo(&Default::default(), window, cx));

        assert_eq!(buffer_text(&mut cx), "- one\n- two\n");
    }

    #[gpui::test]
    async fn test_selections_that_do_not_fit_are_ignored(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, "- onˇe\n").await;
        hooks.set_command_result(VisualMdCommandResult {
            edits: vec![edit(2..5, "ONE")],
            selections: Some(vec![0..999]),
            message: None,
        });

        run(&mut cx, "notes.uppercase");

        cx.assert_editor_state("- ONEˇ\n");
    }

    #[gpui::test]
    async fn test_invalid_edits_change_nothing(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, "ˇ- one\n").await;
        hooks.set_command_result(VisualMdCommandResult {
            edits: vec![edit(2..5, "ONE"), edit(4..99, "oops")],
            ..Default::default()
        });

        run(&mut cx, "notes.uppercase");

        assert_eq!(buffer_text(&mut cx), "- one\n");
    }

    #[gpui::test]
    async fn test_a_failing_command_changes_nothing(cx: &mut TestAppContext) {
        setup(cx, Behavior::Fail);
        let mut cx = editor_showing(cx, "ˇ- one\n").await;

        run(&mut cx, "notes.uppercase");

        assert_eq!(buffer_text(&mut cx), "- one\n");
    }

    #[gpui::test]
    async fn test_a_result_for_text_that_changed_meanwhile_is_dropped(cx: &mut TestAppContext) {
        let delay = Duration::from_secs(1);
        let hooks = setup(cx, Behavior::TakeLongerThan(delay));
        let mut cx = editor_showing(cx, "ˇ- one\n").await;
        hooks.set_command_result(VisualMdCommandResult {
            edits: vec![edit(2..5, "ONE")],
            ..Default::default()
        });

        run(&mut cx, "notes.uppercase");
        cx.update_buffer(|buffer, cx| buffer.edit([(0..0, "typed ")], None, cx));
        cx.executor().advance_clock(delay);
        cx.run_until_parked();

        assert_eq!(buffer_text(&mut cx), "typed - one\n");
    }

    #[gpui::test]
    async fn test_an_unknown_command_runs_nothing(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, "ˇ- one\n").await;

        run(&mut cx, "notes.missing");
        run(&mut cx, "missing.uppercase");

        assert_eq!(hooks.calls(), 0);
        assert_eq!(buffer_text(&mut cx), "- one\n");
    }

    #[gpui::test]
    async fn test_commands_do_nothing_while_live_preview_is_off(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇ- one\n");
        // No Markdown language, so live preview is not decorating this buffer.
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));

        run(&mut cx, "notes.uppercase");

        assert_eq!(hooks.calls(), 0);
    }

    /// An editor in a workspace, showing `text` as Markdown.
    async fn workspace_with_markdown_editor(
        cx: &mut TestAppContext,
        text: &str,
        language: Option<Arc<language::Language>>,
    ) -> (WeakEntity<Workspace>, Entity<Editor>, VisualTestContext) {
        init_test(cx);
        cx.update(|cx| theme_settings::init(theme::LoadThemes::JustBase, cx));
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window has a workspace");
        let mut cx = VisualTestContext::from_window(*window, cx);

        let buffer = project
            .update(&mut cx, |project, cx| {
                project.create_buffer(None, false, cx)
            })
            .await
            .expect("an empty buffer");
        buffer.update(&mut cx, |buffer, cx| {
            buffer.set_text(text, cx);
            buffer.set_language(language, cx);
        });
        let multi_buffer = cx.new(|cx| editor::MultiBuffer::singleton(buffer, cx));
        let editor = cx.new_window_entity(|window, cx| {
            Editor::new(
                editor::EditorMode::full(),
                multi_buffer,
                Some(project),
                window,
                cx,
            )
        });
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        });
        cx.run_until_parked();
        (workspace.downgrade(), editor, cx)
    }

    async fn palette_labels(
        cx: &mut VisualTestContext,
        workspace: &WeakEntity<Workspace>,
        query: &str,
    ) -> Vec<String> {
        let task = cx.update(|_, cx| {
            GlobalCommandPaletteInterceptor::intercept(query, workspace.clone(), cx)
        });
        let Some(task) = task else {
            return Vec::new();
        };
        task.await
            .results
            .into_iter()
            .map(|item| item.string)
            .collect()
    }

    #[gpui::test]
    async fn test_the_palette_lists_extension_commands_only_while_live_preview_shows(
        cx: &mut TestAppContext,
    ) {
        setup(cx, Behavior::Succeed);
        let (workspace, _editor, mut cx) =
            workspace_with_markdown_editor(cx, "- one\n", Some(markdown_language())).await;
        cx.update(|_, cx| init(cx));

        assert_eq!(
            palette_labels(&mut cx, &workspace, "upper").await,
            vec!["Extension notes: Uppercase Selection".to_string()]
        );
        assert!(
            palette_labels(&mut cx, &workspace, "nothing like it")
                .await
                .is_empty()
        );
        assert!(palette_labels(&mut cx, &workspace, "").await.is_empty());
    }

    #[gpui::test]
    async fn test_the_palette_lists_nothing_for_a_buffer_that_is_not_markdown(
        cx: &mut TestAppContext,
    ) {
        setup(cx, Behavior::Succeed);
        let (workspace, _editor, mut cx) =
            workspace_with_markdown_editor(cx, "- one\n", None).await;
        cx.update(|_, cx| init(cx));

        assert!(
            palette_labels(&mut cx, &workspace, "upper")
                .await
                .is_empty()
        );
    }

    #[gpui::test]
    async fn test_the_palette_offers_commands_alongside_other_interceptors(
        cx: &mut TestAppContext,
    ) {
        setup(cx, Behavior::Succeed);
        let (workspace, _editor, mut cx) =
            workspace_with_markdown_editor(cx, "- one\n", Some(markdown_language())).await;
        cx.update(|_, cx| {
            init(cx);
            GlobalCommandPaletteInterceptor::register(cx, "other", |_, _, _| {
                Task::ready(CommandInterceptResult {
                    results: vec![CommandInterceptItem {
                        action: Box::new(editor::actions::Newline),
                        string: "Other".to_string(),
                        positions: Vec::new(),
                    }],
                    exclusive: false,
                })
            });
        });

        let labels = palette_labels(&mut cx, &workspace, "upper").await;

        assert!(labels.contains(&"Other".to_string()));
        assert!(labels.contains(&"Extension notes: Uppercase Selection".to_string()));
    }
}
