//! Asking extensions what to do with matches of their `dynamic` syntax rules,
//! and keeping what they answered.
//!
//! `refresh` only reads the answers kept here and lists the matches that have
//! none; a task per batch of them does the asking, and editors refresh again
//! when answers land.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use editor::Editor;
use extension::{VisualMdRuleMatch, VisualMdRuleOutput};
use gpui::{App, BorrowAppContext as _, Context, Global, Task};
use parking_lot::Mutex;

use crate::extensions::{HookError, VisualMdExtensions, when_not_busy};
use crate::plan::MissingRuleInput;
use crate::rules::{DynamicKey, DynamicResult, validate_output};

/// The most matches that go to an extension in one call.
pub const MAX_MATCHES_PER_CALL: usize = 256;

/// The most answers kept. When there would be more, they are all dropped, and
/// the ones still wanted are asked for again.
const MAX_KEPT_ANSWERS: usize = 8192;

type PendingKeys = Arc<Mutex<HashSet<DynamicKey>>>;

/// What extensions have answered for matches of their dynamic rules. Changing
/// it, which only a finished request does, notifies global observers: that is
/// how editors learn to apply the answers.
#[derive(Default)]
pub struct DynamicRuleResults {
    answers: Arc<HashMap<DynamicKey, DynamicResult>>,
    /// The matches being asked about right now. Changing it notifies nobody,
    /// which is why it is kept apart from the answers.
    pending: PendingKeys,
}

impl Global for DynamicRuleResults {}

impl DynamicRuleResults {
    pub fn answers(&self) -> Arc<HashMap<DynamicKey, DynamicResult>> {
        self.answers.clone()
    }

    fn insert(&mut self, entries: Vec<(DynamicKey, DynamicResult)>) {
        if self.answers.len() + entries.len() > MAX_KEPT_ANSWERS {
            self.answers = Arc::default();
        }
        Arc::make_mut(&mut self.answers).extend(entries);
    }
}

pub(crate) fn init(cx: &mut App) {
    if cx.try_global::<DynamicRuleResults>().is_none() {
        cx.set_global(DynamicRuleResults::default());
    }
}

/// One call to one extension about one of its rules.
#[derive(Debug, PartialEq, Eq)]
pub struct Batch {
    pub extension_id: Arc<str>,
    pub rule: Arc<str>,
    pub inputs: Vec<MissingRuleInput>,
}

/// Groups `inputs` into calls: one rule of one build of one extension each, and
/// no more than [`MAX_MATCHES_PER_CALL`] matches each.
pub fn batches(inputs: &[MissingRuleInput]) -> Vec<Batch> {
    let mut groups: Vec<(Arc<str>, u64, Arc<str>, Vec<MissingRuleInput>)> = Vec::new();
    for input in inputs {
        let key = &input.key;
        match groups
            .iter_mut()
            .find(|(extension_id, generation, rule, _)| {
                *extension_id == key.extension_id
                    && *generation == key.generation
                    && *rule == key.rule
            }) {
            Some((_, _, _, group)) => group.push(input.clone()),
            None => groups.push((
                key.extension_id.clone(),
                key.generation,
                key.rule.clone(),
                vec![input.clone()],
            )),
        }
    }
    groups
        .into_iter()
        .flat_map(|(extension_id, _, rule, group)| {
            group
                .chunks(MAX_MATCHES_PER_CALL)
                .map(|chunk| Batch {
                    extension_id: extension_id.clone(),
                    rule: rule.clone(),
                    inputs: chunk.to_vec(),
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// What to keep for each input of `batch` given how the call went. A call that
/// failed, or answered with the wrong number of outputs, leaves every input
/// failed, so that none is asked again until the extension is registered anew.
pub(crate) fn answers_for(
    batch: &Batch,
    outcome: Result<Vec<VisualMdRuleOutput>, HookError>,
) -> Vec<(DynamicKey, DynamicResult)> {
    let failed = || {
        batch
            .inputs
            .iter()
            .map(|input| (input.key.clone(), DynamicResult::Failed))
            .collect()
    };
    match outcome {
        Ok(outputs) if outputs.len() == batch.inputs.len() => batch
            .inputs
            .iter()
            .zip(outputs)
            .map(|(input, output)| {
                let effects = validate_output(&input.key.text, output);
                (input.key.clone(), DynamicResult::Ready(Arc::new(effects)))
            })
            .collect(),
        Ok(outputs) => {
            log::warn!(
                "extension {} answered {} matches of rule {} with {} outputs",
                batch.extension_id,
                batch.inputs.len(),
                batch.rule,
                outputs.len()
            );
            failed()
        }
        Err(error) => {
            log::warn!(
                "extension {} could not apply rule {}: {error}",
                batch.extension_id,
                batch.rule
            );
            failed()
        }
    }
}

/// Asks the extensions about `inputs`, leaving out the matches already being
/// asked about. The answers arrive later, through [`DynamicRuleResults`].
pub(crate) fn request_missing(inputs: Vec<MissingRuleInput>, cx: &mut Context<Editor>) {
    if inputs.is_empty() {
        return;
    }
    // Not `default_global` or `global_mut`: both notify the global's observers,
    // and these observe this one, which would make every refresh start another.
    let Some(pending) = cx
        .try_global::<DynamicRuleResults>()
        .map(|results| results.pending.clone())
    else {
        return;
    };
    let fresh: Vec<MissingRuleInput> = {
        let mut pending = pending.lock();
        inputs
            .into_iter()
            .filter(|input| pending.insert(input.key.clone()))
            .collect()
    };
    for batch in batches(&fresh) {
        cx.spawn({
            let pending = pending.clone();
            async move |_, cx| {
                let matches: Vec<VisualMdRuleMatch> = batch
                    .inputs
                    .iter()
                    .map(|input| VisualMdRuleMatch {
                        text: input.key.text.clone(),
                        captures: input.captures.clone(),
                    })
                    .collect();
                let outcome = when_not_busy(cx, |cx| match cx.try_global::<VisualMdExtensions>() {
                    Some(registry) => registry.apply_rule(
                        &batch.extension_id,
                        batch.rule.to_string(),
                        matches.clone(),
                        cx,
                    ),
                    None => Task::ready(Err(HookError::NotRegistered(batch.extension_id.clone()))),
                })
                .await;
                let answers = answers_for(&batch, outcome);
                cx.update(|cx| {
                    {
                        let mut pending = pending.lock();
                        for (key, _) in &answers {
                            pending.remove(key);
                        }
                    }
                    cx.update_global::<DynamicRuleResults, _>(|results, _| results.insert(answers));
                });
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use extension::{VisualMdReplacement, VisualMdSpanStyle, VisualMdStyledSpan};

    use super::*;
    use crate::rules::RuleEffects;

    fn input(extension: &str, generation: u64, rule: &str, text: &str) -> MissingRuleInput {
        MissingRuleInput {
            key: DynamicKey {
                extension_id: extension.into(),
                generation,
                rule: rule.into(),
                text: text.to_string(),
            },
            captures: vec![Some(0..text.len())],
        }
    }

    #[test]
    fn test_inputs_are_grouped_by_extension_build_and_rule() {
        let inputs = vec![
            input("a", 1, "r", "x"),
            input("b", 1, "r", "x"),
            input("a", 1, "s", "x"),
            input("a", 2, "r", "x"),
            input("a", 1, "r", "y"),
        ];

        let batches = batches(&inputs);

        assert_eq!(
            batches
                .iter()
                .map(|batch| {
                    (
                        batch.extension_id.as_ref(),
                        batch.rule.as_ref(),
                        batch.inputs.len(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![("a", "r", 2), ("b", "r", 1), ("a", "s", 1), ("a", "r", 1)]
        );
    }

    #[test]
    fn test_a_batch_holds_at_most_256_matches() {
        let inputs: Vec<_> = (0..MAX_MATCHES_PER_CALL * 2 + 1)
            .map(|index| input("a", 1, "r", &format!("m{index}")))
            .collect();

        let batches = batches(&inputs);

        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.inputs.len())
                .collect::<Vec<_>>(),
            vec![MAX_MATCHES_PER_CALL, MAX_MATCHES_PER_CALL, 1]
        );
        assert_eq!(batches[0].inputs[0].key.text, "m0");
        assert_eq!(
            batches[2].inputs[0].key.text,
            format!("m{}", MAX_MATCHES_PER_CALL * 2)
        );
    }

    #[test]
    fn test_no_inputs_make_no_batches() {
        assert!(batches(&[]).is_empty());
    }

    fn batch_of(texts: &[&str]) -> Batch {
        Batch {
            extension_id: "a".into(),
            rule: "r".into(),
            inputs: texts.iter().map(|text| input("a", 1, "r", text)).collect(),
        }
    }

    #[test]
    fn test_answers_are_matched_to_inputs_in_order_and_checked() {
        let batch = batch_of(&["ab", "cd"]);
        let outputs = vec![
            VisualMdRuleOutput {
                hidden: vec![0..1, 5..9],
                ..Default::default()
            },
            VisualMdRuleOutput {
                spans: vec![VisualMdStyledSpan {
                    range: 0..2,
                    style: VisualMdSpanStyle::default(),
                }],
                replacements: vec![VisualMdReplacement {
                    range: 0..1,
                    text: "x".to_string(),
                }],
                ..Default::default()
            },
        ];

        let answers = answers_for(&batch, Ok(outputs));

        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].0.text, "ab");
        assert_eq!(
            answers[0].1,
            DynamicResult::Ready(Arc::new(RuleEffects {
                hidden: vec![0..1],
                ..Default::default()
            })),
            "the out of bounds range is dropped"
        );
        assert_eq!(answers[1].0.text, "cd");
        assert!(matches!(&answers[1].1, DynamicResult::Ready(effects)
            if effects.styled.len() == 1 && effects.replacements == vec![(0..1, "x".to_string())]));
    }

    #[test]
    fn test_a_failed_call_fails_every_input() {
        let batch = batch_of(&["ab", "cd"]);

        for error in [
            HookError::TimedOut("a".into()),
            HookError::Failed("trapped".to_string()),
            HookError::Disabled("a".into()),
        ] {
            let answers = answers_for(&batch, Err(error));

            assert_eq!(answers.len(), 2);
            assert!(
                answers
                    .iter()
                    .all(|(_, answer)| *answer == DynamicResult::Failed)
            );
        }
    }

    #[test]
    fn test_an_answer_with_the_wrong_number_of_outputs_fails_every_input() {
        let batch = batch_of(&["ab", "cd"]);

        let answers = answers_for(&batch, Ok(vec![VisualMdRuleOutput::default()]));

        assert_eq!(answers.len(), 2);
        assert!(
            answers
                .iter()
                .all(|(_, answer)| *answer == DynamicResult::Failed)
        );
        assert!(
            answers_for(&batch, Ok(vec![VisualMdRuleOutput::default(); 3]))
                .iter()
                .all(|(_, answer)| *answer == DynamicResult::Failed)
        );
    }

    fn key(text: &str) -> DynamicKey {
        input("a", 1, "r", text).key
    }

    #[test]
    fn test_answers_accumulate() {
        let mut results = DynamicRuleResults::default();

        results.insert(vec![(key("a"), DynamicResult::Failed)]);
        results.insert(vec![(key("b"), DynamicResult::Failed)]);

        assert_eq!(results.answers().len(), 2);
    }

    #[test]
    fn test_a_snapshot_is_not_changed_by_later_answers() {
        let mut results = DynamicRuleResults::default();
        results.insert(vec![(key("a"), DynamicResult::Failed)]);
        let snapshot = results.answers();

        results.insert(vec![(key("b"), DynamicResult::Failed)]);

        assert_eq!(snapshot.len(), 1);
        assert_eq!(results.answers().len(), 2);
    }

    #[test]
    fn test_too_many_answers_are_dropped_all_at_once() {
        let mut results = DynamicRuleResults::default();
        results.insert(
            (0..MAX_KEPT_ANSWERS)
                .map(|index| (key(&format!("m{index}")), DynamicResult::Failed))
                .collect(),
        );
        assert_eq!(results.answers().len(), MAX_KEPT_ANSWERS);

        results.insert(vec![(key("new"), DynamicResult::Failed)]);

        let answers = results.answers();
        assert_eq!(answers.len(), 1);
        assert!(answers.contains_key(&key("new")));
    }
}

#[cfg(test)]
mod integration_tests {
    use std::ops::Range;
    use std::time::Duration;

    use editor::test::editor_test_context::EditorTestContext;
    use extension::VisualMdReplacement;
    use gpui::TestAppContext;

    use crate::VisualMdAddon;
    use crate::extensions::test_support::{Behavior, FakeHooks, register};
    use crate::extensions::{RULE_TIMEOUT, VisualMdExtensions};
    use crate::integration_tests::{init_test, markdown_language};

    use super::*;

    const EMOJI_RULE: &str =
        "[[visual_md.syntax_rules]]\nid = \"emoji\"\npattern = '(:)(\\w+)(:)'\ndynamic = true\n";

    /// Hides the colons and shows a smiley in place of what is between them.
    fn smiley(rule_match: &VisualMdRuleMatch) -> VisualMdRuleOutput {
        let length = rule_match.text.len();
        VisualMdRuleOutput {
            hidden: vec![0..1, length - 1..length],
            replacements: vec![VisualMdReplacement {
                range: 1..length - 1,
                text: "🙂".to_string(),
            }],
            ..Default::default()
        }
    }

    fn setup(cx: &mut TestAppContext, behavior: Behavior) -> Arc<FakeHooks> {
        init_test(cx);
        let hooks = FakeHooks::new(&cx.executor(), behavior);
        hooks.set_rule_responder(smiley);
        register(cx, "notes", EMOJI_RULE, Some(hooks.clone()));
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

    fn folded(cx: &mut EditorTestContext) -> Vec<(Range<usize>, String)> {
        cx.update_editor(|editor, _, _| {
            editor
                .addon::<VisualMdAddon>()
                .map(|addon| {
                    addon
                        .folded_markers
                        .iter()
                        .map(|(range, key, _)| (range.clone(), key.clone()))
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    fn smiley_folds(start: usize) -> Vec<(Range<usize>, String)> {
        // `:smile:` with its colons hidden and the word replaced.
        vec![
            (start..start + 1, " ".to_string()),
            (start + 1..start + 6, "ext:🙂".to_string()),
            (start + 6..start + 7, " ".to_string()),
        ]
    }

    fn refresh(cx: &mut EditorTestContext) {
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_an_answer_is_applied_once_it_arrives(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, "ˇsay :smile: now\n").await;

        let requests = hooks.rule_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "emoji");
        assert_eq!(requests[0].1.len(), 1);
        assert_eq!(requests[0].1[0].text, ":smile:");
        assert_eq!(
            requests[0].1[0].captures,
            vec![Some(0..7), Some(0..1), Some(1..6), Some(6..7)]
        );
        assert_eq!(folded(&mut cx), smiley_folds(4));
    }

    #[gpui::test]
    async fn test_nothing_changes_until_the_extension_answers(cx: &mut TestAppContext) {
        let delay = Duration::from_millis(500);
        let hooks = setup(cx, Behavior::TakeLongerThan(delay));
        let mut cx = editor_showing(cx, "ˇsay :smile: now\n").await;
        assert!(
            folded(&mut cx).is_empty(),
            "the source shows while it is being asked"
        );
        assert_eq!(hooks.rule_requests().len(), 1);

        cx.executor().advance_clock(delay);
        cx.run_until_parked();

        assert_eq!(folded(&mut cx), smiley_folds(4));
    }

    #[gpui::test]
    async fn test_touching_the_match_shows_its_source(cx: &mut TestAppContext) {
        setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, "ˇsay :smile: now\n").await;
        assert_eq!(folded(&mut cx), smiley_folds(4));

        cx.set_state("say :smiˇle: now\n");
        refresh(&mut cx);
        assert!(folded(&mut cx).is_empty());

        cx.set_state("ˇsay :smile: now\n");
        refresh(&mut cx);
        assert_eq!(folded(&mut cx), smiley_folds(4));
    }

    #[gpui::test]
    async fn test_a_match_that_was_answered_is_not_asked_about_again(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, "ˇsay :smile: now\n").await;

        refresh(&mut cx);
        cx.update_buffer(|buffer, cx| buffer.edit([(0..0, "oh, ")], None, cx));
        cx.run_until_parked();
        refresh(&mut cx);

        assert_eq!(
            hooks.rule_requests().len(),
            1,
            "the same text moved, which is not a new question"
        );
        assert_eq!(folded(&mut cx), smiley_folds(8));
    }

    #[gpui::test]
    async fn test_a_new_text_is_asked_about_alone(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, "ˇ:smile:\n").await;

        cx.update_buffer(|buffer, cx| buffer.edit([(8..8, "\n:grin:")], None, cx));
        cx.run_until_parked();
        refresh(&mut cx);

        let requests = hooks.rule_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].1.len(), 1);
        assert_eq!(requests[1].1[0].text, ":grin:");
    }

    #[gpui::test]
    async fn test_each_distinct_text_is_asked_once_in_one_call(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        editor_showing(cx, "ˇ:a: :b: :a: :c: :b:\n").await;

        let requests = hooks.rule_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]
                .1
                .iter()
                .map(|rule_match| rule_match.text.as_str())
                .collect::<Vec<_>>(),
            vec![":a:", ":b:", ":c:"]
        );
    }

    #[gpui::test]
    async fn test_a_refresh_while_waiting_does_not_ask_again(cx: &mut TestAppContext) {
        let delay = Duration::from_millis(500);
        let hooks = setup(cx, Behavior::TakeLongerThan(delay));
        let mut cx = editor_showing(cx, "ˇ:smile:\n").await;

        refresh(&mut cx);
        cx.update_editor(|editor, window, cx| crate::force_refresh(editor, window, cx));
        cx.run_until_parked();

        assert_eq!(hooks.rule_requests().len(), 1);
        cx.executor().advance_clock(delay);
        cx.run_until_parked();
        assert_eq!(hooks.rule_requests().len(), 1);
    }

    #[gpui::test]
    async fn test_matches_beyond_one_call_go_in_batches(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        let line = (0..400)
            .map(|index| format!(":a{index}:"))
            .collect::<Vec<_>>()
            .join(" ");
        editor_showing(cx, &format!("ˇ{line}\n")).await;

        let sizes: Vec<usize> = hooks
            .rule_requests()
            .iter()
            .map(|(_, matches)| matches.len())
            .collect();
        assert_eq!(sizes.iter().sum::<usize>(), 400);
        assert!(
            sizes.iter().all(|size| *size <= MAX_MATCHES_PER_CALL),
            "{sizes:?}"
        );
        assert_eq!(sizes.len(), 2);
    }

    #[gpui::test]
    async fn test_a_failing_extension_leaves_the_text_alone_and_is_not_asked_again(
        cx: &mut TestAppContext,
    ) {
        let hooks = setup(cx, Behavior::Fail);
        let mut cx = editor_showing(cx, "ˇsay :smile: now\n").await;

        refresh(&mut cx);
        cx.update_editor(|editor, window, cx| crate::force_refresh(editor, window, cx));
        cx.run_until_parked();

        assert!(folded(&mut cx).is_empty());
        assert_eq!(hooks.rule_requests().len(), 1);
    }

    #[gpui::test]
    async fn test_a_timed_out_question_is_not_asked_again(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::TakeLongerThan(RULE_TIMEOUT * 2));
        let mut cx = editor_showing(cx, "ˇsay :smile: now\n").await;

        cx.executor().advance_clock(RULE_TIMEOUT);
        cx.run_until_parked();
        refresh(&mut cx);

        assert!(folded(&mut cx).is_empty());
        assert_eq!(hooks.rule_requests().len(), 1);
    }

    #[gpui::test]
    async fn test_a_short_answer_is_not_applied_to_any_match(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        hooks.drop_rule_outputs(1);
        let mut cx = editor_showing(cx, "ˇ:a: :b:\n").await;

        assert!(folded(&mut cx).is_empty());
    }

    #[gpui::test]
    async fn test_an_answer_that_cannot_be_applied_is_dropped(cx: &mut TestAppContext) {
        let hooks = setup(cx, Behavior::Succeed);
        hooks.set_rule_responder(|_| VisualMdRuleOutput {
            hidden: vec![0..99],
            replacements: vec![VisualMdReplacement {
                range: 1..3,
                text: "two\nlines".to_string(),
            }],
            ..Default::default()
        });
        let mut cx = editor_showing(cx, "ˇsay :smile: now\n").await;

        assert!(folded(&mut cx).is_empty());
    }

    #[gpui::test]
    async fn test_registering_the_extension_again_asks_the_new_build(cx: &mut TestAppContext) {
        let old_build = setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, "ˇsay :smile: now\n").await;
        let new_build = FakeHooks::new(&cx.executor(), Behavior::Succeed);
        new_build.set_rule_responder(|_| VisualMdRuleOutput::default());

        register(&mut cx, "notes", EMOJI_RULE, Some(new_build.clone()));
        cx.run_until_parked();

        assert_eq!(new_build.rule_requests().len(), 1);
        assert_eq!(old_build.rule_requests().len(), 1);
        assert!(
            folded(&mut cx).is_empty(),
            "the new build answered with nothing"
        );
    }

    #[gpui::test]
    async fn test_unregistering_the_extension_removes_what_its_answers_did(
        cx: &mut TestAppContext,
    ) {
        setup(cx, Behavior::Succeed);
        let mut cx = editor_showing(cx, "ˇsay :smile: now\n").await;
        assert_eq!(folded(&mut cx), smiley_folds(4));

        cx.update(|_, cx| VisualMdExtensions::unregister("notes", cx));
        cx.run_until_parked();

        assert!(folded(&mut cx).is_empty());
    }
}
