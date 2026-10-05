//! Provides hooks for customizing the behavior of the command palette.

#![deny(missing_docs)]

use std::{any::TypeId, rc::Rc};

use collections::{HashSet, TypeIdHashSet};
use derive_more::{Deref, DerefMut};
use gpui::{Action, App, AppContext as _, BorrowAppContext, Global, Task, WeakEntity};
use workspace::Workspace;

/// Initializes the command palette hooks.
pub fn init(cx: &mut App) {
    cx.set_global(GlobalCommandPaletteFilter::default());
}

/// A filter for the command palette.
#[derive(Default)]
pub struct CommandPaletteFilter {
    hidden_namespaces: HashSet<&'static str>,
    hidden_action_types: TypeIdHashSet,
    /// Actions that have explicitly been shown. These should be shown even if
    /// they are in a hidden namespace.
    shown_action_types: TypeIdHashSet,
}

#[derive(Deref, DerefMut, Default)]
struct GlobalCommandPaletteFilter(CommandPaletteFilter);

impl Global for GlobalCommandPaletteFilter {}

impl CommandPaletteFilter {
    /// Returns the global [`CommandPaletteFilter`], if one is set.
    pub fn try_global(cx: &App) -> Option<&CommandPaletteFilter> {
        cx.try_global::<GlobalCommandPaletteFilter>()
            .map(|filter| &filter.0)
    }

    /// Returns a mutable reference to the global [`CommandPaletteFilter`].
    pub fn global_mut(cx: &mut App) -> &mut Self {
        cx.global_mut::<GlobalCommandPaletteFilter>()
    }

    /// Updates the global [`CommandPaletteFilter`] using the given closure.
    pub fn update_global<F>(cx: &mut App, update: F)
    where
        F: FnOnce(&mut Self, &mut App),
    {
        if cx.has_global::<GlobalCommandPaletteFilter>() {
            cx.update_global(|this: &mut GlobalCommandPaletteFilter, cx| update(&mut this.0, cx))
        }
    }

    /// Returns whether the given [`Action`] is hidden by the filter.
    pub fn is_hidden(&self, action: &dyn Action) -> bool {
        let name = action.name();
        let namespace = name.split("::").next().unwrap_or("malformed action name");

        // If this action has specifically been shown then it should be visible.
        if self.shown_action_types.contains(&action.type_id()) {
            return false;
        }

        self.hidden_namespaces.contains(namespace)
            || self.hidden_action_types.contains(&action.type_id())
    }

    /// Hides all actions in the given namespace.
    pub fn hide_namespace(&mut self, namespace: &'static str) {
        self.hidden_namespaces.insert(namespace);
    }

    /// Shows all actions in the given namespace.
    pub fn show_namespace(&mut self, namespace: &'static str) {
        self.hidden_namespaces.remove(namespace);
    }

    /// Hides all actions with the given types.
    pub fn hide_action_types<'a>(&mut self, action_types: impl IntoIterator<Item = &'a TypeId>) {
        for action_type in action_types {
            self.hidden_action_types.insert(*action_type);
            self.shown_action_types.remove(action_type);
        }
    }

    /// Shows all actions with the given types.
    pub fn show_action_types<'a>(&mut self, action_types: impl IntoIterator<Item = &'a TypeId>) {
        for action_type in action_types {
            self.shown_action_types.insert(*action_type);
            self.hidden_action_types.remove(action_type);
        }
    }
}

/// The result of intercepting a command palette command.
#[derive(Debug)]
pub struct CommandInterceptItem {
    /// The action produced as a result of the interception.
    pub action: Box<dyn Action>,
    /// The display string to show in the command palette for this result.
    pub string: String,
    /// The character positions in the string that match the query.
    /// Used for highlighting matched characters in the command palette UI.
    pub positions: Vec<usize>,
}

/// The result of intercepting a command palette command.
#[derive(Default, Debug)]
pub struct CommandInterceptResult {
    /// The items
    pub results: Vec<CommandInterceptItem>,
    /// Whether or not to continue to show the normal matches
    pub exclusive: bool,
}

type Interceptor =
    Rc<dyn Fn(&str, WeakEntity<Workspace>, &mut App) -> Task<CommandInterceptResult>>;

/// The interceptors of the command palette. Each is registered under a key, so
/// independent features can each have one without replacing another's.
#[derive(Clone, Default)]
pub struct GlobalCommandPaletteInterceptor(Vec<(&'static str, Interceptor)>);

impl Global for GlobalCommandPaletteInterceptor {}

impl GlobalCommandPaletteInterceptor {
    /// Registers an interceptor under `key`, replacing the one previously
    /// registered under the same key, if any.
    pub fn register(
        cx: &mut App,
        key: &'static str,
        interceptor: impl Fn(&str, WeakEntity<Workspace>, &mut App) -> Task<CommandInterceptResult>
        + 'static,
    ) {
        let interceptors = &mut cx.default_global::<Self>().0;
        let interceptor: Interceptor = Rc::new(interceptor);
        match interceptors
            .iter_mut()
            .find(|(existing_key, _)| *existing_key == key)
        {
            Some((_, existing)) => *existing = interceptor,
            None => interceptors.push((key, interceptor)),
        }
    }

    /// Removes the interceptor registered under `key`.
    pub fn unregister(cx: &mut App, key: &'static str) {
        if let Some(interceptors) = cx.try_global::<Self>() {
            let mut interceptors = interceptors.clone();
            interceptors
                .0
                .retain(|(existing_key, _)| *existing_key != key);
            cx.set_global(interceptors);
        }
    }

    /// Intercepts the given query from the command palette with every
    /// registered interceptor. Their results are concatenated in registration
    /// order, and the combined result is exclusive if any of them is.
    pub fn intercept(
        query: &str,
        workspace: WeakEntity<Workspace>,
        cx: &mut App,
    ) -> Option<Task<CommandInterceptResult>> {
        let handlers: Vec<Interceptor> = cx
            .try_global::<Self>()?
            .0
            .iter()
            .map(|(_, handler)| handler.clone())
            .collect();
        let mut tasks: Vec<Task<CommandInterceptResult>> = handlers
            .iter()
            .map(|handler| handler(query, workspace.clone(), cx))
            .collect();
        if tasks.len() <= 1 {
            return tasks.pop();
        }
        Some(cx.background_spawn(async move {
            let mut merged = CommandInterceptResult::default();
            for task in tasks {
                let result = task.await;
                merged.results.extend(result.results);
                merged.exclusive |= result.exclusive;
            }
            merged
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, actions};

    actions!(command_palette_hooks_test, [First, Second]);

    fn register_result(
        cx: &mut TestAppContext,
        key: &'static str,
        label: &'static str,
        exclusive: bool,
    ) {
        cx.update(|cx| {
            GlobalCommandPaletteInterceptor::register(cx, key, move |_, _, _| {
                Task::ready(CommandInterceptResult {
                    results: vec![CommandInterceptItem {
                        action: First.boxed_clone(),
                        string: label.to_string(),
                        positions: Vec::new(),
                    }],
                    exclusive,
                })
            });
        });
    }

    async fn intercepted(cx: &mut TestAppContext) -> Option<(Vec<String>, bool)> {
        let task = cx.update(|cx| {
            GlobalCommandPaletteInterceptor::intercept("query", WeakEntity::new_invalid(), cx)
        })?;
        let result = task.await;
        Some((
            result.results.into_iter().map(|item| item.string).collect(),
            result.exclusive,
        ))
    }

    #[gpui::test]
    async fn nothing_is_intercepted_without_interceptors(cx: &mut TestAppContext) {
        assert!(intercepted(cx).await.is_none());
    }

    #[gpui::test]
    async fn a_single_interceptor_behaves_as_before(cx: &mut TestAppContext) {
        register_result(cx, "one", "first", true);

        assert_eq!(
            intercepted(cx).await,
            Some((vec!["first".to_string()], true))
        );
    }

    #[gpui::test]
    async fn interceptors_are_merged_in_registration_order(cx: &mut TestAppContext) {
        register_result(cx, "one", "first", false);
        register_result(cx, "two", "second", false);

        assert_eq!(
            intercepted(cx).await,
            Some((vec!["first".to_string(), "second".to_string()], false))
        );
    }

    #[gpui::test]
    async fn the_merged_result_is_exclusive_if_any_interceptor_is(cx: &mut TestAppContext) {
        register_result(cx, "one", "first", false);
        register_result(cx, "two", "second", true);

        assert_eq!(
            intercepted(cx).await.map(|(_, exclusive)| exclusive),
            Some(true)
        );
    }

    #[gpui::test]
    async fn registering_a_key_again_replaces_its_interceptor(cx: &mut TestAppContext) {
        register_result(cx, "one", "first", false);
        register_result(cx, "two", "second", false);
        register_result(cx, "one", "replacement", false);

        assert_eq!(
            intercepted(cx).await,
            Some((vec!["replacement".to_string(), "second".to_string()], false))
        );
    }

    #[gpui::test]
    async fn unregistering_removes_only_that_key(cx: &mut TestAppContext) {
        register_result(cx, "one", "first", false);
        register_result(cx, "two", "second", false);

        cx.update(|cx| GlobalCommandPaletteInterceptor::unregister(cx, "one"));
        assert_eq!(
            intercepted(cx).await,
            Some((vec!["second".to_string()], false))
        );

        cx.update(|cx| GlobalCommandPaletteInterceptor::unregister(cx, "two"));
        assert!(intercepted(cx).await.is_none());
    }
}
