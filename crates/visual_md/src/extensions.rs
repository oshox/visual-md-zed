//! The `visual_md` hooks of extensions: which extensions are registered, what
//! they claim, and the one guarded way every call into one of them is made.
//!
//! Nothing here runs inside `refresh`. Callers read what is registered and
//! start a guarded call for whatever is missing, then refresh again when it
//! finishes, the way `ensure_code_languages_loaded` does for grammars.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use extension::{
    Extension, ExtensionHostProxy, ExtensionManifest, ExtensionVisualMdProxy,
    VisualMdCommandContext, VisualMdCommandResult, VisualMdFenceRequest, VisualMdFenceResult,
    VisualMdManifestEntry,
};
use futures::FutureExt as _;
use futures::future::{BoxFuture, Either, select};
use gpui::{App, AppContext as _, BorrowAppContext as _, Global, Task};

pub const FENCE_TIMEOUT: Duration = Duration::from_secs(5);
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

const MAX_IN_FLIGHT_CALLS_PER_EXTENSION: usize = 4;
const FAILURES_BEFORE_DISABLING: usize = 3;

/// What an extension can be asked to do. Extensions are called through this
/// instead of `extension::Extension` so tests can stand in for wasm.
pub trait VisualMdHooks: Send + Sync + 'static {
    fn render_fence(
        &self,
        renderer: String,
        request: VisualMdFenceRequest,
    ) -> BoxFuture<'static, Result<VisualMdFenceResult>>;

    fn run_command(
        &self,
        command: String,
        context: VisualMdCommandContext,
    ) -> BoxFuture<'static, Result<VisualMdCommandResult>>;
}

struct ExtensionHooks(Arc<dyn Extension>);

impl VisualMdHooks for ExtensionHooks {
    fn render_fence(
        &self,
        renderer: String,
        request: VisualMdFenceRequest,
    ) -> BoxFuture<'static, Result<VisualMdFenceResult>> {
        let extension = self.0.clone();
        async move { extension.visual_md_render_fence(renderer, request).await }.boxed()
    }

    fn run_command(
        &self,
        command: String,
        context: VisualMdCommandContext,
    ) -> BoxFuture<'static, Result<VisualMdCommandResult>> {
        let extension = self.0.clone();
        async move { extension.visual_md_run_command(command, context).await }.boxed()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HookError {
    #[error("extension {0} is not registered")]
    NotRegistered(Arc<str>),
    #[error("extension {0} has no code to run")]
    NoHooks(Arc<str>),
    #[error("extension {0} was disabled after repeated failures")]
    Disabled(Arc<str>),
    #[error("extension {0} already has too many requests in flight")]
    Busy(Arc<str>),
    #[error("extension {0} took too long to respond")]
    TimedOut(Arc<str>),
    #[error("{0}")]
    Failed(String),
}

/// Counts an extension's calls in flight and its failures in a row, shared by
/// the calls themselves so they can update it from the background.
struct Health {
    extension_id: Arc<str>,
    in_flight: AtomicUsize,
    consecutive_failures: AtomicUsize,
    disabled: AtomicBool,
}

impl Health {
    fn new(extension_id: Arc<str>) -> Self {
        Self {
            extension_id,
            in_flight: AtomicUsize::new(0),
            consecutive_failures: AtomicUsize::new(0),
            disabled: AtomicBool::new(false),
        }
    }

    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// A failed call and a timed out one count the same: either way the
    /// extension is not answering usefully.
    fn record_failure(&self) {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= FAILURES_BEFORE_DISABLING && !self.disabled.swap(true, Ordering::Relaxed) {
            log::error!(
                "disabling the Markdown live preview hooks of extension {} until Zed restarts: \
                {failures} calls in a row failed or timed out",
                self.extension_id
            );
        }
    }
}

struct InFlightCall(Arc<Health>);

impl Drop for InFlightCall {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

struct RegisteredExtension {
    name: String,
    entry: VisualMdManifestEntry,
    hooks: Option<Arc<dyn VisualMdHooks>>,
    health: Arc<Health>,
    generation: u64,
}

/// A renderer an extension registered for a fenced code block language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FenceRenderer {
    pub extension_id: Arc<str>,
    /// Changes whenever the extension is registered again, so anything cached
    /// from an older build of it can be told apart.
    pub generation: u64,
    /// The language tag as the extension declared it.
    pub renderer: String,
}

/// An editor command an extension declared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionCommand {
    pub extension_id: Arc<str>,
    pub extension_name: String,
    /// The id as declared in the manifest.
    pub command: Arc<str>,
    pub title: String,
    pub description: Option<String>,
}

impl ExtensionCommand {
    /// The id the command is run by, `<extension id>.<command id>`.
    pub fn qualified_id(&self) -> String {
        format!("{}.{}", self.extension_id, self.command)
    }
}

/// Every extension that registered `visual_md` hooks. Changing it notifies
/// global observers, which is how editors learn to render again.
#[derive(Default)]
pub struct VisualMdExtensions {
    extensions: BTreeMap<Arc<str>, RegisteredExtension>,
    registrations: u64,
}

impl Global for VisualMdExtensions {}

pub(crate) fn init(cx: &mut App) {
    if cx.try_global::<VisualMdExtensions>().is_none() {
        cx.set_global(VisualMdExtensions::default());
    }
    ExtensionHostProxy::default_global(cx).register_visual_md_proxy(VisualMdProxy);
}

struct VisualMdProxy;

impl ExtensionVisualMdProxy for VisualMdProxy {
    fn register_visual_md_extension(
        &self,
        manifest: Arc<ExtensionManifest>,
        extension: Option<Arc<dyn Extension>>,
        cx: &mut App,
    ) {
        let hooks = extension.map(|extension| Arc::new(ExtensionHooks(extension)) as _);
        VisualMdExtensions::register(&manifest, hooks, cx);
    }

    fn unregister_visual_md_extension(&self, extension_id: Arc<str>, cx: &mut App) {
        VisualMdExtensions::unregister(&extension_id, cx);
    }
}

impl VisualMdExtensions {
    /// Registers an extension, replacing any earlier registration with the same
    /// id. An extension whose `[visual_md]` section is invalid is not
    /// registered at all.
    pub fn register(
        manifest: &ExtensionManifest,
        hooks: Option<Arc<dyn VisualMdHooks>>,
        cx: &mut App,
    ) {
        let Some(entry) = manifest.visual_md.clone() else {
            return;
        };
        let problems = entry.validate();
        if !problems.is_empty() {
            log::error!(
                "not registering the Markdown live preview hooks of extension {}: {}",
                manifest.id,
                problems.join("; ")
            );
            return;
        }

        let extension_id = manifest.id.clone();
        let name = manifest.name.clone();
        cx.update_global::<Self, _>(|registry, _| {
            registry.registrations += 1;
            for language in &entry.fence_renderers {
                let language = language.to_lowercase();
                let claimed_by = registry.extensions.iter().find(|(id, other)| {
                    **id != extension_id
                        && other
                            .entry
                            .fence_renderers
                            .iter()
                            .any(|claimed| claimed.to_lowercase() == language)
                });
                if let Some((owner, _)) = claimed_by {
                    log::warn!(
                        "extension {extension_id} renders `{language}` fenced code blocks, \
                        but extension {owner} already does, and it wins as it sorts first"
                    );
                }
            }
            let health = Arc::new(Health::new(extension_id.clone()));
            registry.extensions.insert(
                extension_id,
                RegisteredExtension {
                    name,
                    entry,
                    hooks,
                    health,
                    generation: registry.registrations,
                },
            );
        });
    }

    pub fn unregister(extension_id: &str, cx: &mut App) {
        cx.update_global::<Self, _>(|registry, _| {
            registry.extensions.remove(extension_id);
        });
    }

    pub fn is_empty(&self) -> bool {
        self.extensions.is_empty()
    }

    /// The lowercased language tags some extension renders.
    pub fn fence_languages(&self) -> BTreeSet<String> {
        self.extensions
            .values()
            .flat_map(|extension| &extension.entry.fence_renderers)
            .map(|language| language.to_lowercase())
            .collect()
    }

    /// The renderer for a fenced code block language, ignoring case. When more
    /// than one extension claims it, the one whose id sorts first wins.
    pub fn renderer_for_language(&self, language: &str) -> Option<FenceRenderer> {
        let language = language.to_lowercase();
        self.extensions
            .iter()
            .find_map(|(extension_id, extension)| {
                let renderer = extension
                    .entry
                    .fence_renderers
                    .iter()
                    .find(|claimed| claimed.to_lowercase() == language)?;
                Some(FenceRenderer {
                    extension_id: extension_id.clone(),
                    generation: extension.generation,
                    renderer: renderer.clone(),
                })
            })
    }

    pub fn commands(&self) -> Vec<ExtensionCommand> {
        self.extensions
            .iter()
            .flat_map(|(extension_id, extension)| {
                extension
                    .entry
                    .commands
                    .iter()
                    .map(|(command, entry)| ExtensionCommand {
                        extension_id: extension_id.clone(),
                        extension_name: extension.name.clone(),
                        command: command.clone(),
                        title: entry.title.clone(),
                        description: entry.description.clone(),
                    })
            })
            .collect()
    }

    /// Finds a command by its qualified id, `<extension id>.<command id>`.
    /// Extension ids may themselves contain dots, so the id is compared whole
    /// instead of being split.
    pub fn command(&self, qualified_id: &str) -> Option<ExtensionCommand> {
        self.commands()
            .into_iter()
            .find(|command| command.qualified_id() == qualified_id)
    }

    pub fn render_fence(
        &self,
        extension_id: &str,
        renderer: String,
        request: VisualMdFenceRequest,
        cx: &App,
    ) -> Task<Result<VisualMdFenceResult, HookError>> {
        self.call(extension_id, FENCE_TIMEOUT, cx, move |hooks| {
            hooks.render_fence(renderer, request)
        })
    }

    pub fn run_command(
        &self,
        extension_id: &str,
        command: String,
        context: VisualMdCommandContext,
        cx: &App,
    ) -> Task<Result<VisualMdCommandResult, HookError>> {
        self.call(extension_id, COMMAND_TIMEOUT, cx, move |hooks| {
            hooks.run_command(command, context)
        })
    }

    /// Calls into an extension with the limits every hook shares: a timeout, at
    /// most a few requests in flight, and no calls at all once the extension
    /// has failed several times in a row.
    fn call<T: Send + 'static>(
        &self,
        extension_id: &str,
        timeout: Duration,
        cx: &App,
        call: impl FnOnce(Arc<dyn VisualMdHooks>) -> BoxFuture<'static, Result<T>>,
    ) -> Task<Result<T, HookError>> {
        let Some((extension_id, extension)) = self.extensions.get_key_value(extension_id) else {
            return Task::ready(Err(HookError::NotRegistered(extension_id.into())));
        };
        let Some(hooks) = extension.hooks.clone() else {
            return Task::ready(Err(HookError::NoHooks(extension_id.clone())));
        };
        let health = extension.health.clone();
        if health.disabled.load(Ordering::Relaxed) {
            return Task::ready(Err(HookError::Disabled(extension_id.clone())));
        }
        if health.in_flight.fetch_add(1, Ordering::Relaxed) >= MAX_IN_FLIGHT_CALLS_PER_EXTENSION {
            health.in_flight.fetch_sub(1, Ordering::Relaxed);
            return Task::ready(Err(HookError::Busy(extension_id.clone())));
        }
        let in_flight = InFlightCall(health.clone());

        let extension_id = extension_id.clone();
        let timer = cx.background_executor().timer(timeout);
        let call = call(hooks);
        cx.background_spawn(async move {
            let _in_flight = in_flight;
            match select(call, timer).await {
                Either::Left((Ok(value), _)) => {
                    health.record_success();
                    Ok(value)
                }
                Either::Left((Err(error), _)) => {
                    health.record_failure();
                    Err(HookError::Failed(format!("{error:#}")))
                }
                Either::Right(((), _)) => {
                    health.record_failure();
                    Err(HookError::TimedOut(extension_id))
                }
            }
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use extension::VisualMdFenceOutput;
    use gpui::BackgroundExecutor;

    use super::*;

    #[derive(Clone)]
    pub(crate) enum Behavior {
        Succeed,
        Fail,
        TakeLongerThan(Duration),
    }

    /// Stands in for an extension's wasm: answers every call the way it was told
    /// to, and remembers what it was asked.
    pub(crate) struct FakeHooks {
        executor: BackgroundExecutor,
        behavior: Mutex<Behavior>,
        fence_output: Mutex<VisualMdFenceOutput>,
        command_result: Mutex<VisualMdCommandResult>,
        calls: AtomicUsize,
        fence_requests: Mutex<Vec<VisualMdFenceRequest>>,
        command_contexts: Mutex<Vec<VisualMdCommandContext>>,
    }

    impl FakeHooks {
        pub(crate) fn new(executor: &BackgroundExecutor, behavior: Behavior) -> Arc<Self> {
            Arc::new(Self {
                executor: executor.clone(),
                behavior: Mutex::new(behavior),
                fence_output: Mutex::new(VisualMdFenceOutput::Markdown("rendered".into())),
                command_result: Mutex::new(VisualMdCommandResult::default()),
                calls: AtomicUsize::new(0),
                fence_requests: Mutex::new(Vec::new()),
                command_contexts: Mutex::new(Vec::new()),
            })
        }

        pub(crate) fn set_behavior(&self, behavior: Behavior) {
            *self.behavior.lock().expect("the test lock is not poisoned") = behavior;
        }

        pub(crate) fn set_fence_output(&self, output: VisualMdFenceOutput) {
            *self
                .fence_output
                .lock()
                .expect("the test lock is not poisoned") = output;
        }

        pub(crate) fn set_command_result(&self, result: VisualMdCommandResult) {
            *self
                .command_result
                .lock()
                .expect("the test lock is not poisoned") = result;
        }

        pub(crate) fn command_contexts(&self) -> Vec<VisualMdCommandContext> {
            self.command_contexts
                .lock()
                .expect("the test lock is not poisoned")
                .clone()
        }

        pub(crate) fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }

        pub(crate) fn fence_requests(&self) -> Vec<VisualMdFenceRequest> {
            self.fence_requests
                .lock()
                .expect("the test lock is not poisoned")
                .clone()
        }

        fn respond<T: Send + 'static>(&self, value: T) -> BoxFuture<'static, Result<T>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let behavior = self
                .behavior
                .lock()
                .expect("the test lock is not poisoned")
                .clone();
            let executor = self.executor.clone();
            async move {
                match behavior {
                    Behavior::Succeed => Ok(value),
                    Behavior::Fail => anyhow::bail!("the extension trapped"),
                    Behavior::TakeLongerThan(duration) => {
                        executor.timer(duration).await;
                        Ok(value)
                    }
                }
            }
            .boxed()
        }
    }

    impl VisualMdHooks for FakeHooks {
        fn render_fence(
            &self,
            _renderer: String,
            request: VisualMdFenceRequest,
        ) -> BoxFuture<'static, Result<VisualMdFenceResult>> {
            self.fence_requests
                .lock()
                .expect("the test lock is not poisoned")
                .push(request);
            let output = self
                .fence_output
                .lock()
                .expect("the test lock is not poisoned")
                .clone();
            self.respond(VisualMdFenceResult {
                output,
                height_hint: None,
            })
        }

        fn run_command(
            &self,
            _command: String,
            context: VisualMdCommandContext,
        ) -> BoxFuture<'static, Result<VisualMdCommandResult>> {
            self.command_contexts
                .lock()
                .expect("the test lock is not poisoned")
                .push(context);
            let result = self
                .command_result
                .lock()
                .expect("the test lock is not poisoned")
                .clone();
            self.respond(result)
        }
    }

    pub(crate) fn manifest(id: &str, visual_md: &str) -> ExtensionManifest {
        toml::from_str(&format!(
            "id = \"{id}\"\nname = \"Extension {id}\"\nversion = \"1.0.0\"\nschema_version = 1\n\
            [visual_md]\n{visual_md}"
        ))
        .expect("the test manifest should parse")
    }

    pub(crate) fn register(
        cx: &mut gpui::TestAppContext,
        id: &str,
        visual_md: &str,
        hooks: Option<Arc<FakeHooks>>,
    ) {
        let manifest = manifest(id, visual_md);
        cx.update(|cx| {
            VisualMdExtensions::register(
                &manifest,
                hooks.map(|hooks| hooks as Arc<dyn VisualMdHooks>),
                cx,
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use extension::{VisualMdAppearance, VisualMdFenceOutput};
    use gpui::TestAppContext;

    use super::test_support::{Behavior, FakeHooks, manifest, register};
    use super::*;

    fn fence_request() -> VisualMdFenceRequest {
        VisualMdFenceRequest {
            language: "flow".into(),
            info: "flow".into(),
            content: "a -> b".into(),
            appearance: VisualMdAppearance::Dark,
            path: None,
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(init);
    }

    fn render_fence(
        cx: &mut TestAppContext,
        id: &str,
    ) -> Task<Result<VisualMdFenceResult, HookError>> {
        cx.update(|cx| {
            cx.global::<VisualMdExtensions>()
                .render_fence(id, "flow".into(), fence_request(), cx)
        })
    }

    #[gpui::test]
    fn test_registered_languages_and_commands_are_queryable(cx: &mut TestAppContext) {
        init_test(cx);
        register(
            cx,
            "notes",
            "fence_renderers = [\"Flow\", \"graph\"]\n\
            [visual_md.commands.uppercase]\ntitle = \"Uppercase\"\n",
            None,
        );

        cx.update(|cx| {
            let registry = cx.global::<VisualMdExtensions>();
            assert_eq!(
                registry.fence_languages().into_iter().collect::<Vec<_>>(),
                vec!["flow".to_string(), "graph".to_string()]
            );
            assert_eq!(
                registry
                    .renderer_for_language("FLOW")
                    .map(|renderer| (renderer.extension_id, renderer.renderer)),
                Some(("notes".into(), "Flow".to_string()))
            );
            assert_eq!(registry.renderer_for_language("mermaid"), None);

            let command = registry
                .command("notes.uppercase")
                .expect("the declared command is registered");
            assert_eq!(command.title, "Uppercase");
            assert_eq!(command.extension_name, "Extension notes");
            assert!(registry.command("notes.lowercase").is_none());
        });
    }

    #[gpui::test]
    fn test_unregistering_removes_everything_the_extension_claimed(cx: &mut TestAppContext) {
        init_test(cx);
        register(cx, "notes", "fence_renderers = [\"flow\"]", None);
        cx.update(|cx| VisualMdExtensions::unregister("notes", cx));

        cx.update(|cx| {
            let registry = cx.global::<VisualMdExtensions>();
            assert!(registry.is_empty());
            assert_eq!(registry.renderer_for_language("flow"), None);
        });
    }

    #[gpui::test]
    fn test_an_invalid_visual_md_section_is_not_registered(cx: &mut TestAppContext) {
        init_test(cx);
        register(cx, "notes", "fence_renderers = [\"has space\"]", None);

        cx.update(|cx| assert!(cx.global::<VisualMdExtensions>().is_empty()));
    }

    #[gpui::test]
    fn test_the_first_extension_by_id_wins_a_contested_language(cx: &mut TestAppContext) {
        init_test(cx);
        register(cx, "zeta", "fence_renderers = [\"flow\"]", None);
        register(cx, "alpha", "fence_renderers = [\"flow\"]", None);

        cx.update(|cx| {
            let renderer = cx
                .global::<VisualMdExtensions>()
                .renderer_for_language("flow")
                .expect("flow is claimed");
            assert_eq!(renderer.extension_id.as_ref(), "alpha");
        });
    }

    #[gpui::test]
    fn test_registering_again_gives_the_extension_a_new_generation(cx: &mut TestAppContext) {
        init_test(cx);
        let generation = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                cx.global::<VisualMdExtensions>()
                    .renderer_for_language("flow")
                    .map(|renderer| renderer.generation)
            })
        };
        register(cx, "notes", "fence_renderers = [\"flow\"]", None);
        let first = generation(cx);
        register(cx, "other", "fence_renderers = [\"graph\"]", None);
        assert_eq!(generation(cx), first, "other registrations leave it alone");
        register(cx, "notes", "fence_renderers = [\"flow\"]", None);

        assert_ne!(generation(cx), first);
    }

    #[gpui::test]
    fn test_registering_and_unregistering_notify_global_observers(cx: &mut TestAppContext) {
        init_test(cx);
        let notifications = Arc::new(AtomicUsize::new(0));
        let _subscription = cx.update(|cx| {
            let notifications = notifications.clone();
            cx.observe_global::<VisualMdExtensions>(move |_| {
                notifications.fetch_add(1, Ordering::Relaxed);
            })
        });

        register(cx, "notes", "fence_renderers = [\"flow\"]", None);
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        cx.update(|cx| VisualMdExtensions::unregister("notes", cx));
        assert_eq!(notifications.load(Ordering::Relaxed), 2);
    }

    #[gpui::test]
    async fn test_a_call_returns_what_the_extension_returned(cx: &mut TestAppContext) {
        init_test(cx);
        let hooks = FakeHooks::new(&cx.executor(), Behavior::Succeed);
        register(
            cx,
            "notes",
            "fence_renderers = [\"flow\"]",
            Some(hooks.clone()),
        );

        let result = render_fence(cx, "notes").await;

        assert_eq!(
            result,
            Ok(VisualMdFenceResult {
                output: VisualMdFenceOutput::Markdown("rendered".into()),
                height_hint: None,
            })
        );
        assert_eq!(hooks.calls(), 1);
    }

    #[gpui::test]
    async fn test_a_call_to_an_unknown_or_codeless_extension_fails_without_running_anything(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        register(cx, "manifest-only", "fence_renderers = [\"flow\"]", None);

        assert_eq!(
            render_fence(cx, "missing").await,
            Err(HookError::NotRegistered("missing".into()))
        );
        assert_eq!(
            render_fence(cx, "manifest-only").await,
            Err(HookError::NoHooks("manifest-only".into()))
        );
    }

    #[gpui::test]
    async fn test_an_error_from_the_extension_is_reported(cx: &mut TestAppContext) {
        init_test(cx);
        let hooks = FakeHooks::new(&cx.executor(), Behavior::Fail);
        register(cx, "notes", "fence_renderers = [\"flow\"]", Some(hooks));

        assert_eq!(
            render_fence(cx, "notes").await,
            Err(HookError::Failed("the extension trapped".into()))
        );
    }

    #[gpui::test]
    async fn test_a_call_that_takes_too_long_times_out(cx: &mut TestAppContext) {
        init_test(cx);
        let hooks = FakeHooks::new(&cx.executor(), Behavior::TakeLongerThan(FENCE_TIMEOUT * 2));
        register(cx, "notes", "fence_renderers = [\"flow\"]", Some(hooks));

        let task = render_fence(cx, "notes");
        cx.executor().advance_clock(FENCE_TIMEOUT);

        assert_eq!(task.await, Err(HookError::TimedOut("notes".into())));
    }

    #[gpui::test]
    async fn test_an_extension_is_disabled_after_three_failures_in_a_row(cx: &mut TestAppContext) {
        init_test(cx);
        let hooks = FakeHooks::new(&cx.executor(), Behavior::Fail);
        register(
            cx,
            "notes",
            "fence_renderers = [\"flow\"]",
            Some(hooks.clone()),
        );

        for _ in 0..FAILURES_BEFORE_DISABLING {
            assert!(matches!(
                render_fence(cx, "notes").await,
                Err(HookError::Failed(_))
            ));
        }
        hooks.set_behavior(Behavior::Succeed);

        assert_eq!(
            render_fence(cx, "notes").await,
            Err(HookError::Disabled("notes".into()))
        );
        assert_eq!(hooks.calls(), FAILURES_BEFORE_DISABLING);

        register(
            cx,
            "notes",
            "fence_renderers = [\"flow\"]",
            Some(hooks.clone()),
        );
        assert!(
            render_fence(cx, "notes").await.is_ok(),
            "registering the extension again, as a rebuild does, enables it"
        );
    }

    #[gpui::test]
    async fn test_timeouts_count_towards_disabling_an_extension(cx: &mut TestAppContext) {
        init_test(cx);
        let hooks = FakeHooks::new(&cx.executor(), Behavior::TakeLongerThan(FENCE_TIMEOUT * 2));
        register(cx, "notes", "fence_renderers = [\"flow\"]", Some(hooks));

        for _ in 0..FAILURES_BEFORE_DISABLING {
            let task = render_fence(cx, "notes");
            cx.executor().advance_clock(FENCE_TIMEOUT);
            assert_eq!(task.await, Err(HookError::TimedOut("notes".into())));
        }

        assert_eq!(
            render_fence(cx, "notes").await,
            Err(HookError::Disabled("notes".into()))
        );
    }

    #[gpui::test]
    async fn test_a_success_resets_the_count_of_failures_in_a_row(cx: &mut TestAppContext) {
        init_test(cx);
        let hooks = FakeHooks::new(&cx.executor(), Behavior::Fail);
        register(
            cx,
            "notes",
            "fence_renderers = [\"flow\"]",
            Some(hooks.clone()),
        );

        for _ in 0..FAILURES_BEFORE_DISABLING - 1 {
            render_fence(cx, "notes").await.ok();
        }
        hooks.set_behavior(Behavior::Succeed);
        assert!(render_fence(cx, "notes").await.is_ok());
        hooks.set_behavior(Behavior::Fail);
        for _ in 0..FAILURES_BEFORE_DISABLING - 1 {
            render_fence(cx, "notes").await.ok();
        }
        hooks.set_behavior(Behavior::Succeed);

        assert!(render_fence(cx, "notes").await.is_ok());
    }

    #[gpui::test]
    async fn test_at_most_four_calls_are_in_flight_per_extension(cx: &mut TestAppContext) {
        init_test(cx);
        let hooks = FakeHooks::new(&cx.executor(), Behavior::TakeLongerThan(FENCE_TIMEOUT / 2));
        register(
            cx,
            "notes",
            "fence_renderers = [\"flow\"]",
            Some(hooks.clone()),
        );

        let pending = (0..MAX_IN_FLIGHT_CALLS_PER_EXTENSION)
            .map(|_| render_fence(cx, "notes"))
            .collect::<Vec<_>>();
        assert_eq!(
            render_fence(cx, "notes").await,
            Err(HookError::Busy("notes".into()))
        );

        cx.executor().advance_clock(FENCE_TIMEOUT / 2);
        for task in pending {
            assert!(task.await.is_ok());
        }

        hooks.set_behavior(Behavior::Succeed);
        assert!(
            render_fence(cx, "notes").await.is_ok(),
            "capacity frees up once the calls finish"
        );
    }

    #[gpui::test]
    fn test_the_extension_host_proxy_registers_and_unregisters(cx: &mut TestAppContext) {
        init_test(cx);
        let manifest = Arc::new(manifest("notes", "fence_renderers = [\"flow\"]"));

        cx.update(|cx| {
            let proxy = ExtensionHostProxy::default_global(cx);
            proxy.register_visual_md_extension(manifest, None, cx);
            assert!(
                cx.global::<VisualMdExtensions>()
                    .renderer_for_language("flow")
                    .is_some()
            );

            proxy.unregister_visual_md_extension("notes".into(), cx);
            assert!(cx.global::<VisualMdExtensions>().is_empty());
        });
    }
}
