use std::borrow::Cow;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use cloud_api_types::ExtensionProvides;
use collections::{BTreeMap, BTreeSet, HashMap};
use fs::Fs;
use language::LanguageName;
use lsp::LanguageServerName;
use semver::Version;
use serde::{Deserialize, Serialize};
use util::paths::PathStyle;
use util::rel_path::{RelPath, RelPathBuf};

use crate::{ExtensionCapability, VISUAL_MD_RULE_NODE_KINDS, VisualMdSpanStyle, is_hex_color};

/// This is the old version of the extension manifest, from when it was `extension.json`.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct OldExtensionManifest {
    pub name: String,
    pub version: Arc<str>,

    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub authors: Vec<String>,

    #[serde(default)]
    pub themes: BTreeMap<Arc<str>, RelPathBuf>,
    #[serde(default)]
    pub languages: BTreeMap<Arc<str>, RelPathBuf>,
    #[serde(default)]
    pub grammars: BTreeMap<Arc<str>, RelPathBuf>,
}

/// The schema version of the [`ExtensionManifest`].
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy, Serialize, Deserialize)]
pub struct SchemaVersion(pub i32);

impl fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl SchemaVersion {
    pub const ZERO: Self = Self(0);

    pub fn is_v0(&self) -> bool {
        self == &Self::ZERO
    }
}

// TODO: We should change this to just always be a Vec<PathBuf> once we bump the
// extension.toml schema version to 2
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExtensionSnippets {
    Single(PathBuf),
    Multiple(Vec<PathBuf>),
}

impl ExtensionSnippets {
    pub fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        match self {
            ExtensionSnippets::Single(path) => std::slice::from_ref(path).iter(),
            ExtensionSnippets::Multiple(paths) => paths.iter(),
        }
    }
}

impl From<&str> for ExtensionSnippets {
    fn from(value: &str) -> Self {
        ExtensionSnippets::Single(value.into())
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct ExtensionManifest {
    pub id: Arc<str>,
    pub name: String,
    pub version: Arc<str>,
    pub schema_version: SchemaVersion,

    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub lib: LibManifestEntry,

    #[serde(default)]
    pub themes: Vec<RelPathBuf>,
    #[serde(default)]
    pub icon_themes: Vec<RelPathBuf>,
    #[serde(default)]
    pub languages: Vec<RelPathBuf>,
    #[serde(default)]
    pub grammars: BTreeMap<Arc<str>, GrammarManifestEntry>,
    #[serde(default)]
    pub language_servers: BTreeMap<LanguageServerName, LanguageServerManifestEntry>,
    #[serde(default)]
    pub context_servers: BTreeMap<Arc<str>, ContextServerManifestEntry>,
    #[serde(default)]
    pub slash_commands: BTreeMap<Arc<str>, SlashCommandManifestEntry>,
    #[serde(default)]
    pub snippets: Option<ExtensionSnippets>,
    #[serde(default)]
    pub capabilities: Vec<ExtensionCapability>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub debug_adapters: BTreeMap<Arc<str>, DebugAdapterManifestEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub debug_locators: BTreeMap<Arc<str>, DebugLocatorManifestEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub language_model_providers: BTreeMap<Arc<str>, LanguageModelProviderManifestEntry>,
    /// Hooks into Zed MD's Markdown live preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual_md: Option<VisualMdManifestEntry>,
}

/// The `[visual_md]` section of `extension.toml`: what an extension hooks into
/// in Zed MD's Markdown live preview.
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct VisualMdManifestEntry {
    /// The language tags of the fenced code blocks this extension renders, such
    /// as `mermaid`. Matching ignores case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fence_renderers: Vec<String>,
    /// The editor commands this extension provides, keyed by command id. A
    /// command is run as `<extension id>.<command id>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<Arc<str>, VisualMdCommandManifestEntry>,
    /// Rules that style, hide or replace text matching a pattern or a syntax
    /// node, declared with `[[visual_md.syntax_rules]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub syntax_rules: Vec<VisualMdSyntaxRuleManifestEntry>,
    /// Callout types this extension adds, keyed by the name used in
    /// `> [!name]`. Matching ignores case.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub callouts: BTreeMap<Arc<str>, VisualMdCalloutManifestEntry>,
}

/// A syntax rule provided through `[[visual_md.syntax_rules]]`. It has exactly
/// one of `pattern` and `node`.
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct VisualMdSyntaxRuleManifestEntry {
    /// Names the rule, for the extension's `visual-md-apply-rule`.
    pub id: Arc<str>,
    /// A regular expression, in the syntax of Rust's `regex` crate, matched
    /// against the text of each paragraph, heading, list item and table cell,
    /// outside inline code. A match never spans a line break.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// A kind of syntax node to match instead, one of
    /// [`VISUAL_MD_RULE_NODE_KINDS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// How to style every match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<VisualMdSpanStyle>,
    /// The capture groups of `pattern` to hide while the cursor is away from
    /// the match. Group 0 is the whole match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hide: Vec<usize>,
    /// Whether the extension is asked what to do with each match, through
    /// `visual-md-apply-rule`, on top of `style` and `hide`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dynamic: bool,
}

/// A callout type provided through `[visual_md.callouts.<name>]`.
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct VisualMdCalloutManifestEntry {
    /// The title shown instead of the capitalized name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The built-in callout type whose look this one starts from, such as
    /// `warning`. Without it the callout starts from the generic look.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The icon: the name of one of Zed's icons, such as `star`, or the path of
    /// an `.svg` file inside the extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The accent color, as a hex string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    /// The background color, as a hex string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
}

/// An editor command provided through `[visual_md.commands.<id>]`.
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct VisualMdCommandManifestEntry {
    /// The name shown in the command palette.
    pub title: String,
    /// What the command does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl VisualMdManifestEntry {
    /// The paths, relative to the extension's directory, of the `.svg` files
    /// its callouts use as icons. They have to ship with the extension.
    pub fn callout_icon_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .callouts
            .values()
            .filter_map(|callout| callout.icon.as_deref())
            .filter(|icon| icon.ends_with(".svg"))
            .map(PathBuf::from)
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// Checks the entry, returning one message for each problem found. The
    /// extension's hooks should not be registered when there are any.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for language in &self.fence_renderers {
            let is_valid = !language.is_empty()
                && language
                    .chars()
                    .all(|character| !character.is_whitespace() && character != '`');
            if !is_valid {
                problems.push(format!(
                    "fence renderer {language:?} is not a valid language tag"
                ));
            }
        }
        for (id, command) in &self.commands {
            let is_valid = !id.is_empty()
                && id
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character));
            if !is_valid {
                problems.push(format!(
                    "command id {id:?} may only contain letters, digits, `-` and `_`"
                ));
            }
            if command.title.trim().is_empty() {
                problems.push(format!("command {id:?} has no title"));
            }
        }
        self.validate_syntax_rules(&mut problems);
        self.validate_callouts(&mut problems);
        problems
    }

    fn validate_syntax_rules(&self, problems: &mut Vec<String>) {
        let mut seen_ids = BTreeSet::new();
        for rule in &self.syntax_rules {
            let id = &rule.id;
            let is_valid_id = !id.is_empty()
                && id
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character));
            if !is_valid_id {
                problems.push(format!(
                    "syntax rule id {id:?} may only contain letters, digits, `-` and `_`"
                ));
            }
            if !seen_ids.insert(id.clone()) {
                problems.push(format!("syntax rule id {id:?} is used more than once"));
            }

            match (&rule.pattern, &rule.node) {
                (Some(pattern), None) if pattern.is_empty() => {
                    problems.push(format!("syntax rule {id:?} has an empty pattern"));
                }
                (Some(_), None) => {}
                (None, Some(node)) => {
                    if !VISUAL_MD_RULE_NODE_KINDS.contains(&node.as_str()) {
                        problems.push(format!(
                            "syntax rule {id:?} matches `{node}`, which is not one of: {}",
                            VISUAL_MD_RULE_NODE_KINDS.join(", ")
                        ));
                    }
                    if !rule.hide.is_empty() {
                        problems.push(format!(
                            "syntax rule {id:?} hides capture groups, which only a `pattern` has"
                        ));
                    }
                }
                _ => problems.push(format!(
                    "syntax rule {id:?} must have exactly one of `pattern` and `node`"
                )),
            }

            if rule.style.is_none() && rule.hide.is_empty() && !rule.dynamic {
                problems.push(format!(
                    "syntax rule {id:?} does nothing: it needs a `style`, `hide` or `dynamic = true`"
                ));
            }
            if let Some(style) = &rule.style {
                validate_span_style(&format!("syntax rule {id:?}"), style, problems);
            }
        }
    }

    fn validate_callouts(&self, problems: &mut Vec<String>) {
        for (name, callout) in &self.callouts {
            let is_valid_name = !name.is_empty()
                && name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character));
            if !is_valid_name {
                problems.push(format!(
                    "callout name {name:?} may only contain letters, digits, `-` and `_`"
                ));
            }
            let context = format!("callout {name:?}");
            for (field, value) in [("title", &callout.title), ("kind", &callout.kind)] {
                if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
                    problems.push(format!("{context} has an empty {field}"));
                }
            }
            for (field, value) in [
                ("accent", &callout.accent),
                ("background", &callout.background),
            ] {
                if let Some(value) = value
                    && !is_hex_color(value)
                {
                    problems.push(format!(
                        "{context} has {field} {value:?}, which is not a hex color"
                    ));
                }
            }
            if let Some(icon) = &callout.icon {
                if let Some(path) = icon.strip_suffix(".svg") {
                    let path = std::path::Path::new(path);
                    let stays_inside = !path.as_os_str().is_empty()
                        && path
                            .components()
                            .all(|component| matches!(component, std::path::Component::Normal(_)));
                    if !stays_inside {
                        problems.push(format!(
                            "{context} has icon {icon:?}, which is not a path inside the extension"
                        ));
                    }
                } else if icon.is_empty()
                    || !icon
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '_')
                {
                    problems.push(format!(
                        "{context} has icon {icon:?}, which is neither an icon name nor an `.svg` path"
                    ));
                }
            }
        }
    }
}

fn validate_span_style(context: &str, style: &VisualMdSpanStyle, problems: &mut Vec<String>) {
    for (field, value) in [
        ("color", &style.color),
        ("background_color", &style.background_color),
    ] {
        if let Some(value) = value
            && !is_hex_color(value)
        {
            problems.push(format!(
                "{context} has {field} {value:?}, which is not a hex color"
            ));
        }
    }
    if style
        .font_weight
        .is_some_and(|weight| !(100..=900).contains(&weight))
    {
        problems.push(format!("{context} has a font_weight outside 100 to 900"));
    }
    if style
        .theme_token
        .as_ref()
        .is_some_and(|token| token.trim().is_empty())
    {
        problems.push(format!("{context} has an empty theme_token"));
    }
}

impl ExtensionManifest {
    /// Returns the set of features provided by the extension.
    pub fn provides(&self) -> BTreeSet<ExtensionProvides> {
        let mut provides = BTreeSet::default();
        if !self.themes.is_empty() {
            provides.insert(ExtensionProvides::Themes);
        }

        if !self.icon_themes.is_empty() {
            provides.insert(ExtensionProvides::IconThemes);
        }

        if !self.languages.is_empty() {
            provides.insert(ExtensionProvides::Languages);
        }

        if !self.grammars.is_empty() {
            provides.insert(ExtensionProvides::Grammars);
        }

        if !self.language_servers.is_empty() {
            provides.insert(ExtensionProvides::LanguageServers);
        }

        if !self.context_servers.is_empty() {
            provides.insert(ExtensionProvides::ContextServers);
        }

        if self.snippets.is_some() {
            provides.insert(ExtensionProvides::Snippets);
        }

        if !self.debug_adapters.is_empty() {
            provides.insert(ExtensionProvides::DebugAdapters);
        }

        if self.visual_md.is_some() {
            provides.insert(ExtensionProvides::VisualMd);
        }

        provides
    }

    pub fn allow_exec(
        &self,
        desired_command: &str,
        desired_args: &[impl AsRef<str> + std::fmt::Debug],
    ) -> Result<()> {
        let is_allowed = self.capabilities.iter().any(|capability| match capability {
            ExtensionCapability::ProcessExec(capability) => {
                capability.allows(desired_command, desired_args)
            }
            _ => false,
        });

        if !is_allowed {
            bail!(
                "capability for process:exec {desired_command} {desired_args:?} was not listed in the extension manifest",
            );
        }

        Ok(())
    }

    pub fn allow_remote_load(&self) -> bool {
        self.remote_load().is_some()
    }

    pub fn remote_load(&self) -> Option<RemoteLoad<'_>> {
        (!self.language_servers.is_empty()
            || !self.debug_adapters.is_empty()
            || !self.debug_locators.is_empty())
        .then_some(RemoteLoad { manifest: self })
    }
}

pub struct RemoteLoad<'a> {
    manifest: &'a ExtensionManifest,
}

impl RemoteLoad<'_> {
    pub fn language_dependencies(&self) -> impl Iterator<Item = LanguageName> + '_ {
        self.manifest
            .language_servers
            .values()
            .flat_map(|language_server_config| language_server_config.languages())
    }
}

pub fn build_debug_adapter_schema_path(
    adapter_name: &Arc<str>,
    meta: &DebugAdapterManifestEntry,
) -> anyhow::Result<RelPathBuf> {
    match &meta.schema_path {
        Some(path) => Ok(path.clone()),
        None => RelPath::new(
            &Path::new("debug_adapter_schemas")
                .join(Path::new(adapter_name.as_ref()).with_extension("json")),
            PathStyle::local(),
        )
        .map(Cow::into_owned),
    }
}

#[derive(Clone, Default, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct LibManifestEntry {
    pub kind: Option<ExtensionLibraryKind>,
    pub version: Option<Version>,
}

#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct AgentServerManifestEntry {
    /// Display name for the agent (shown in menus).
    pub name: String,
    /// Environment variables to set when launching the agent server.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Optional icon path (relative to extension root, e.g., "ai.svg").
    /// Should be a small SVG icon for display in menus.
    #[serde(default)]
    pub icon: Option<String>,
    /// Per-target configuration for archive-based installation.
    /// The key format is "{os}-{arch}" where:
    /// - os: "darwin" (macOS), "linux", "windows"
    /// - arch: "aarch64" (arm64), "x86_64"
    ///
    /// Example:
    /// ```toml
    /// [agent_servers.myagent.targets.darwin-aarch64]
    /// archive = "https://example.com/myagent-darwin-arm64.zip"
    /// cmd = "./myagent"
    /// args = ["--serve"]
    /// sha256 = "abc123..."  # optional
    /// ```
    ///
    /// For Node.js-based agents, you can use "node" as the cmd to automatically
    /// use Zed's managed Node.js runtime instead of relying on the user's PATH:
    /// ```toml
    /// [agent_servers.nodeagent.targets.darwin-aarch64]
    /// archive = "https://example.com/nodeagent.zip"
    /// cmd = "node"
    /// args = ["index.js", "--port", "3000"]
    /// ```
    ///
    /// Note: All commands are executed with the archive extraction directory as the
    /// working directory, so relative paths in args (like "index.js") will resolve
    /// relative to the extracted archive contents.
    pub targets: HashMap<String, TargetConfig>,
}

#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct TargetConfig {
    /// URL to download the archive from (e.g., "https://github.com/owner/repo/releases/download/v1.0.0/myagent-darwin-arm64.zip")
    pub archive: String,
    /// Command to run (e.g., "./myagent" or "./myagent.exe")
    pub cmd: String,
    /// Command-line arguments to pass to the agent server.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional SHA-256 hash of the archive for verification.
    /// If not provided and the URL is a GitHub release, we'll attempt to fetch it from GitHub.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Environment variables to set when launching the agent server.
    /// These target-specific env vars will override any env vars set at the agent level.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl TargetConfig {
    pub fn from_proto(proto: proto::ExternalExtensionAgentTarget) -> Self {
        Self {
            archive: proto.archive,
            cmd: proto.cmd,
            args: proto.args,
            sha256: proto.sha256,
            env: proto.env.into_iter().collect(),
        }
    }

    pub fn to_proto(&self) -> proto::ExternalExtensionAgentTarget {
        proto::ExternalExtensionAgentTarget {
            archive: self.archive.clone(),
            cmd: self.cmd.clone(),
            args: self.args.clone(),
            sha256: self.sha256.clone(),
            env: self
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub enum ExtensionLibraryKind {
    Rust,
}

#[derive(Clone, Default, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct GrammarManifestEntry {
    pub repository: String,
    #[serde(alias = "commit")]
    pub rev: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Clone, Default, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct LanguageServerManifestEntry {
    /// Deprecated in favor of `languages`.
    #[serde(default)]
    language: Option<LanguageName>,
    /// The list of languages this language server should work with.
    #[serde(default)]
    languages: Vec<LanguageName>,
    #[serde(default)]
    pub language_ids: HashMap<LanguageName, String>,
    #[serde(default)]
    pub code_action_kinds: Option<Vec<lsp::CodeActionKind>>,
}

impl LanguageServerManifestEntry {
    /// Returns the list of languages for the language server.
    ///
    /// Prefer this over accessing the `language` or `languages` fields directly,
    /// as we currently support both.
    ///
    /// We can replace this with just field access for the `languages` field once
    /// we have removed `language`.
    pub fn languages(&self) -> impl IntoIterator<Item = LanguageName> + '_ {
        let language = if self.languages.is_empty() {
            self.language.clone()
        } else {
            None
        };
        self.languages.iter().cloned().chain(language)
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct ContextServerManifestEntry {}

#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct SlashCommandManifestEntry {
    pub description: String,
    pub requires_argument: bool,
}

#[derive(Clone, Default, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct DebugAdapterManifestEntry {
    pub schema_path: Option<RelPathBuf>,
}

#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct DebugLocatorManifestEntry {}

/// Manifest entry for a language model provider.
#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct LanguageModelProviderManifestEntry {
    /// Display name for the provider.
    pub name: String,
    /// Path to an SVG icon file relative to the extension root (e.g., "icons/provider.svg").
    #[serde(default)]
    pub icon: Option<String>,
}

impl ExtensionManifest {
    pub async fn load(fs: Arc<dyn Fs>, extension_dir: &Path) -> Result<Self> {
        let extension_name = extension_dir
            .file_name()
            .and_then(OsStr::to_str)
            .context("invalid extension name")?;

        let extension_manifest_path = extension_dir.join("extension.toml");
        if fs.is_file(&extension_manifest_path).await {
            let manifest_content = fs.load(&extension_manifest_path).await.with_context(|| {
                format!("loading {extension_name} extension.toml, {extension_manifest_path:?}")
            })?;
            toml::from_str(&manifest_content).map_err(|err| {
                anyhow!("Invalid extension.toml for extension {extension_name}:\n{err}")
            })
        } else if let extension_manifest_path = extension_manifest_path.with_extension("json")
            && fs.is_file(&extension_manifest_path).await
        {
            let manifest_content = fs.load(&extension_manifest_path).await.with_context(|| {
                format!("loading {extension_name} extension.json, {extension_manifest_path:?}")
            })?;

            serde_json::from_str::<OldExtensionManifest>(&manifest_content)
                .with_context(|| format!("invalid extension.json for extension {extension_name}"))
                .map(|manifest_json| manifest_from_old_manifest(manifest_json, extension_name))
        } else {
            anyhow::bail!("No extension manifest found for extension {extension_name}")
        }
    }
}

fn manifest_from_old_manifest(
    manifest_json: OldExtensionManifest,
    extension_id: &str,
) -> ExtensionManifest {
    ExtensionManifest {
        id: extension_id.into(),
        name: manifest_json.name,
        version: manifest_json.version,
        description: manifest_json.description,
        repository: manifest_json.repository,
        authors: manifest_json.authors,
        schema_version: SchemaVersion::ZERO,
        lib: Default::default(),
        themes: {
            let mut themes = manifest_json.themes.into_values().collect::<Vec<_>>();
            themes.sort_unstable();
            themes.dedup();
            themes
        },
        icon_themes: Vec::new(),
        languages: {
            let mut languages = manifest_json.languages.into_values().collect::<Vec<_>>();
            languages.sort_unstable();
            languages.dedup();
            languages
        },
        grammars: manifest_json
            .grammars
            .into_keys()
            .map(|grammar_name| (grammar_name, Default::default()))
            .collect(),
        language_servers: Default::default(),
        context_servers: BTreeMap::default(),
        slash_commands: BTreeMap::default(),
        snippets: None,
        capabilities: Vec::new(),
        debug_adapters: Default::default(),
        debug_locators: Default::default(),
        language_model_providers: Default::default(),
        visual_md: None,
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use util::rel_path::rel_path_buf;

    use crate::ProcessExecCapability;

    use super::*;

    fn extension_manifest() -> ExtensionManifest {
        ExtensionManifest {
            id: "test".into(),
            name: "Test".to_string(),
            version: "1.0.0".into(),
            schema_version: SchemaVersion::ZERO,
            description: None,
            repository: None,
            authors: vec![],
            lib: Default::default(),
            themes: vec![],
            icon_themes: vec![],
            languages: vec![],
            grammars: BTreeMap::default(),
            language_servers: BTreeMap::default(),
            context_servers: BTreeMap::default(),
            slash_commands: BTreeMap::default(),
            snippets: None,
            capabilities: vec![],
            debug_adapters: Default::default(),
            debug_locators: Default::default(),
            language_model_providers: BTreeMap::default(),
            visual_md: None,
        }
    }

    const VISUAL_MD_MANIFEST: &str = r#"
id = "notes"
name = "Notes"
version = "1.0.0"
schema_version = 1

[visual_md]
fence_renderers = ["mermaid", "Flow"]

[visual_md.commands.uppercase]
title = "Uppercase Selection"
description = "Uppercases the selected text."

[visual_md.commands.today]
title = "Insert Today's Date"
"#;

    #[test]
    fn test_visual_md_section_is_parsed() {
        let manifest: ExtensionManifest = toml::from_str(VISUAL_MD_MANIFEST).unwrap();
        let visual_md = manifest.visual_md.as_ref().unwrap();

        assert_eq!(visual_md.fence_renderers, vec!["mermaid", "Flow"]);
        assert_eq!(visual_md.commands.len(), 2);
        assert_eq!(
            visual_md.commands.get("uppercase"),
            Some(&VisualMdCommandManifestEntry {
                title: "Uppercase Selection".to_string(),
                description: Some("Uppercases the selected text.".to_string()),
            })
        );
        assert_eq!(visual_md.commands["today"].description, None);
        assert_eq!(visual_md.validate(), Vec::<String>::new());
    }

    #[test]
    fn test_visual_md_section_round_trips() {
        let manifest: ExtensionManifest = toml::from_str(VISUAL_MD_MANIFEST).unwrap();

        let serialized = toml::to_string(&manifest).unwrap();
        let reparsed: ExtensionManifest = toml::from_str(&serialized).unwrap();

        assert_eq!(reparsed, manifest);
    }

    #[test]
    fn test_manifest_without_a_visual_md_section_has_none() {
        let manifest: ExtensionManifest = toml::from_str(
            "id = \"plain\"\nname = \"Plain\"\nversion = \"1.0.0\"\nschema_version = 1\n",
        )
        .unwrap();

        assert_eq!(manifest.visual_md, None);
        assert!(!manifest.provides().contains(&ExtensionProvides::VisualMd));
    }

    #[test]
    fn test_visual_md_section_is_provided_even_when_empty() {
        let manifest = ExtensionManifest {
            visual_md: Some(VisualMdManifestEntry::default()),
            ..extension_manifest()
        };

        assert!(manifest.provides().contains(&ExtensionProvides::VisualMd));
    }

    #[test]
    fn test_visual_md_validation_reports_each_problem() {
        let entry = VisualMdManifestEntry {
            fence_renderers: vec![
                "mermaid".to_string(),
                String::new(),
                "two words".to_string(),
                "``".to_string(),
            ],
            commands: BTreeMap::from([
                (
                    Arc::from("fine-id_1"),
                    VisualMdCommandManifestEntry {
                        title: "Fine".to_string(),
                        description: None,
                    },
                ),
                (
                    Arc::from("has.dot"),
                    VisualMdCommandManifestEntry {
                        title: "Dotted".to_string(),
                        description: None,
                    },
                ),
                (
                    Arc::from("untitled"),
                    VisualMdCommandManifestEntry {
                        title: "   ".to_string(),
                        description: None,
                    },
                ),
            ]),
            ..Default::default()
        };

        let problems = entry.validate();

        assert_eq!(problems.len(), 5, "{problems:?}");
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("\"two words\""))
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("\"has.dot\""))
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("\"untitled\" has no title"))
        );
    }

    #[test]
    fn test_build_adapter_schema_path_with_schema_path() {
        let adapter_name = Arc::from("my_adapter");
        let entry = DebugAdapterManifestEntry {
            schema_path: Some(rel_path_buf("foo/bar")),
        };

        let path = build_debug_adapter_schema_path(&adapter_name, &entry).unwrap();
        assert_eq!(path, rel_path_buf("foo/bar"));
    }

    #[test]
    fn test_build_adapter_schema_path_without_schema_path() {
        let adapter_name = Arc::from("my_adapter");
        let entry = DebugAdapterManifestEntry::default();

        let path = build_debug_adapter_schema_path(&adapter_name, &entry).unwrap();
        assert_eq!(path, rel_path_buf("debug_adapter_schemas/my_adapter.json"));
    }

    #[test]
    fn test_allow_exec_exact_match() {
        let manifest = ExtensionManifest {
            capabilities: vec![ExtensionCapability::ProcessExec(ProcessExecCapability {
                command: "ls".to_string(),
                args: vec!["-la".to_string()],
            })],
            ..extension_manifest()
        };

        assert!(manifest.allow_exec("ls", &["-la"]).is_ok());
        assert!(manifest.allow_exec("ls", &["-l"]).is_err());
        assert!(manifest.allow_exec("pwd", &[] as &[&str]).is_err());
    }

    #[test]
    fn test_allow_exec_wildcard_arg() {
        let manifest = ExtensionManifest {
            capabilities: vec![ExtensionCapability::ProcessExec(ProcessExecCapability {
                command: "git".to_string(),
                args: vec!["*".to_string()],
            })],
            ..extension_manifest()
        };

        assert!(manifest.allow_exec("git", &["status"]).is_ok());
        assert!(manifest.allow_exec("git", &["commit"]).is_ok());
        assert!(manifest.allow_exec("git", &["status", "-s"]).is_err()); // too many args
        assert!(manifest.allow_exec("npm", &["install"]).is_err()); // wrong command
    }

    #[test]
    fn test_allow_exec_double_wildcard() {
        let manifest = ExtensionManifest {
            capabilities: vec![ExtensionCapability::ProcessExec(ProcessExecCapability {
                command: "cargo".to_string(),
                args: vec!["test".to_string(), "**".to_string()],
            })],
            ..extension_manifest()
        };

        assert!(manifest.allow_exec("cargo", &["test"]).is_ok());
        assert!(manifest.allow_exec("cargo", &["test", "--all"]).is_ok());
        assert!(
            manifest
                .allow_exec("cargo", &["test", "--all", "--no-fail-fast"])
                .is_ok()
        );
        assert!(manifest.allow_exec("cargo", &["build"]).is_err()); // wrong first arg
    }

    #[test]
    fn test_allow_exec_mixed_wildcards() {
        let manifest = ExtensionManifest {
            capabilities: vec![ExtensionCapability::ProcessExec(ProcessExecCapability {
                command: "docker".to_string(),
                args: vec!["run".to_string(), "*".to_string(), "**".to_string()],
            })],
            ..extension_manifest()
        };

        assert!(manifest.allow_exec("docker", &["run", "nginx"]).is_ok());
        assert!(manifest.allow_exec("docker", &["run"]).is_err());
        assert!(
            manifest
                .allow_exec("docker", &["run", "ubuntu", "bash"])
                .is_ok()
        );
        assert!(
            manifest
                .allow_exec("docker", &["run", "alpine", "sh", "-c", "echo hello"])
                .is_ok()
        );
        assert!(manifest.allow_exec("docker", &["ps"]).is_err()); // wrong first arg
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn test_deserialize_manifest_with_windows_separators() {
        use indoc::indoc;

        let content = indoc! {r#"
            id = "test-manifest"
            name = "Test Manifest"
            version = "0.0.1"
            schema_version = 0
            languages = ["foo\\bar"]
        "#};
        let manifest: ExtensionManifest = toml::from_str(&content).expect("manifest should parse");
        assert_eq!(manifest.languages, vec![rel_path_buf("foo/bar")]);
    }

    const VISUAL_MD_RULES_MANIFEST: &str = r##"
id = "notes"
name = "Notes"
version = "1.0.0"
schema_version = 1

[visual_md]

[[visual_md.syntax_rules]]
id = "mention"
pattern = '@(\w+)'
style = { color = "#3b82f6", font_weight = 600 }

[[visual_md.syntax_rules]]
id = "emoji"
pattern = '(:)(\w+)(:)'
hide = [1, 3]
dynamic = true

[[visual_md.syntax_rules]]
id = "comment"
node = "html_tag"
style = { theme_token = "comment", italic = true }

[visual_md.callouts.sample]
title = "Sample"
kind = "tip"
icon = "star"
accent = "#a855f7"
background = "#a855f71a"

[visual_md.callouts.drawn]
icon = "icons/drawn.svg"
"##;

    #[test]
    fn test_visual_md_rules_and_callouts_are_parsed() {
        let manifest: ExtensionManifest = toml::from_str(VISUAL_MD_RULES_MANIFEST).unwrap();
        let visual_md = manifest.visual_md.as_ref().unwrap();

        assert_eq!(visual_md.syntax_rules.len(), 3);
        let mention = &visual_md.syntax_rules[0];
        assert_eq!(mention.id.as_ref(), "mention");
        assert_eq!(mention.pattern.as_deref(), Some(r"@(\w+)"));
        assert_eq!(mention.node, None);
        assert_eq!(mention.hide, Vec::<usize>::new());
        assert!(!mention.dynamic);
        assert_eq!(
            mention.style,
            Some(VisualMdSpanStyle {
                color: Some("#3b82f6".to_string()),
                font_weight: Some(600),
                ..Default::default()
            })
        );

        let emoji = &visual_md.syntax_rules[1];
        assert_eq!(emoji.hide, vec![1, 3]);
        assert!(emoji.dynamic);
        assert_eq!(emoji.style, None);

        let comment = &visual_md.syntax_rules[2];
        assert_eq!(comment.node.as_deref(), Some("html_tag"));
        assert_eq!(
            comment.style.as_ref().and_then(|style| style.italic),
            Some(true)
        );

        assert_eq!(visual_md.callouts.len(), 2);
        assert_eq!(
            visual_md.callouts.get("sample"),
            Some(&VisualMdCalloutManifestEntry {
                title: Some("Sample".to_string()),
                kind: Some("tip".to_string()),
                icon: Some("star".to_string()),
                accent: Some("#a855f7".to_string()),
                background: Some("#a855f71a".to_string()),
            })
        );
        assert_eq!(visual_md.validate(), Vec::<String>::new());
    }

    #[test]
    fn test_visual_md_rules_and_callouts_round_trip() {
        let manifest: ExtensionManifest = toml::from_str(VISUAL_MD_RULES_MANIFEST).unwrap();
        let serialized = toml::to_string(&manifest).unwrap();
        let reparsed: ExtensionManifest = toml::from_str(&serialized).unwrap();

        assert_eq!(reparsed.visual_md, manifest.visual_md);
    }

    fn rule(id: &str) -> VisualMdSyntaxRuleManifestEntry {
        VisualMdSyntaxRuleManifestEntry {
            id: id.into(),
            pattern: Some("x".to_string()),
            style: Some(VisualMdSpanStyle {
                italic: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn problems_with(rules: Vec<VisualMdSyntaxRuleManifestEntry>) -> Vec<String> {
        VisualMdManifestEntry {
            syntax_rules: rules,
            ..Default::default()
        }
        .validate()
    }

    #[test]
    fn test_a_valid_rule_has_no_problems() {
        assert_eq!(problems_with(vec![rule("fine-id_1")]), Vec::<String>::new());
    }

    #[test]
    fn test_rule_ids_must_be_valid_and_unique() {
        let problems = problems_with(vec![rule("a"), rule("a"), rule("has space"), rule("")]);

        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("more than once"))
        );
    }

    #[test]
    fn test_a_rule_needs_exactly_one_of_pattern_and_node() {
        let neither = VisualMdSyntaxRuleManifestEntry {
            pattern: None,
            ..rule("neither")
        };
        let both = VisualMdSyntaxRuleManifestEntry {
            node: Some("html_tag".to_string()),
            ..rule("both")
        };
        let empty_pattern = VisualMdSyntaxRuleManifestEntry {
            pattern: Some(String::new()),
            ..rule("empty")
        };

        let problems = problems_with(vec![neither, both, empty_pattern]);

        assert_eq!(problems.len(), 3, "{problems:?}");
    }

    #[test]
    fn test_a_node_rule_must_name_an_allowed_kind_and_cannot_hide_groups() {
        let node_rule = |node: &str, hide: Vec<usize>| VisualMdSyntaxRuleManifestEntry {
            pattern: None,
            node: Some(node.to_string()),
            hide,
            ..rule("node")
        };

        assert_eq!(
            problems_with(vec![node_rule("html_tag", vec![])]),
            Vec::<String>::new()
        );
        assert_eq!(problems_with(vec![node_rule("heading", vec![])]).len(), 1);
        assert_eq!(problems_with(vec![node_rule("html_tag", vec![0])]).len(), 1);
    }

    #[test]
    fn test_a_rule_must_do_something() {
        let idle = VisualMdSyntaxRuleManifestEntry {
            style: None,
            ..rule("idle")
        };
        let hides = VisualMdSyntaxRuleManifestEntry {
            style: None,
            hide: vec![0],
            ..rule("hides")
        };
        let dynamic = VisualMdSyntaxRuleManifestEntry {
            style: None,
            dynamic: true,
            ..rule("dynamic")
        };

        let problems = problems_with(vec![idle, hides, dynamic]);

        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("does nothing"));
    }

    #[test]
    fn test_rule_styles_are_checked() {
        let styled = |style: VisualMdSpanStyle| VisualMdSyntaxRuleManifestEntry {
            style: Some(style),
            ..rule("styled")
        };
        let valid_color = styled(VisualMdSpanStyle {
            color: Some("#abc".to_string()),
            background_color: Some("#aabbccdd".to_string()),
            font_weight: Some(900),
            ..Default::default()
        });
        let bad_color = styled(VisualMdSpanStyle {
            color: Some("red".to_string()),
            background_color: Some("#12345".to_string()),
            ..Default::default()
        });
        let bad_weight = styled(VisualMdSpanStyle {
            font_weight: Some(50),
            ..Default::default()
        });
        let bad_token = styled(VisualMdSpanStyle {
            theme_token: Some(" ".to_string()),
            ..Default::default()
        });

        assert_eq!(problems_with(vec![valid_color]), Vec::<String>::new());
        assert_eq!(problems_with(vec![bad_color]).len(), 2);
        assert_eq!(problems_with(vec![bad_weight]).len(), 1);
        assert_eq!(problems_with(vec![bad_token]).len(), 1);
    }

    #[test]
    fn test_callouts_are_checked() {
        let callout = |name: &str, entry: VisualMdCalloutManifestEntry| VisualMdManifestEntry {
            callouts: BTreeMap::from([(Arc::from(name), entry)]),
            ..Default::default()
        };
        let problems =
            |name: &str, entry: VisualMdCalloutManifestEntry| callout(name, entry).validate();
        let with_icon = |icon: &str| VisualMdCalloutManifestEntry {
            icon: Some(icon.to_string()),
            ..Default::default()
        };

        assert_eq!(problems("fine", with_icon("star")), Vec::<String>::new());
        assert_eq!(
            problems("fine", with_icon("icons/star.svg")),
            Vec::<String>::new()
        );
        assert_eq!(problems("two words", Default::default()).len(), 1);
        for bad_icon in [
            "../star.svg",
            "/etc/star.svg",
            ".svg",
            "a/../../b.svg",
            "star icon",
            "",
        ] {
            assert_eq!(
                problems("fine", with_icon(bad_icon)).len(),
                1,
                "icon {bad_icon:?} should be refused"
            );
        }
        let bad_colors = VisualMdCalloutManifestEntry {
            accent: Some("purple".to_string()),
            background: Some("#zzz".to_string()),
            title: Some(" ".to_string()),
            kind: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(problems("fine", bad_colors).len(), 4);
    }

    #[test]
    fn test_hex_colors() {
        for color in ["#fff", "#ffff", "#ffffff", "#ffffffff", "#AbC123"] {
            assert!(is_hex_color(color), "{color}");
        }
        for color in ["fff", "#ff", "#fffff", "#fffffffff", "#ggg", "red", "", "#"] {
            assert!(!is_hex_color(color), "{color}");
        }
    }

    #[test]
    fn test_callout_icon_paths_lists_each_svg_once() {
        let entry = VisualMdManifestEntry {
            callouts: BTreeMap::from([
                (
                    Arc::from("a"),
                    VisualMdCalloutManifestEntry {
                        icon: Some("icons/b.svg".to_string()),
                        ..Default::default()
                    },
                ),
                (
                    Arc::from("b"),
                    VisualMdCalloutManifestEntry {
                        icon: Some("icons/b.svg".to_string()),
                        ..Default::default()
                    },
                ),
                (
                    Arc::from("c"),
                    VisualMdCalloutManifestEntry {
                        icon: Some("icons/a.svg".to_string()),
                        ..Default::default()
                    },
                ),
                (
                    Arc::from("d"),
                    VisualMdCalloutManifestEntry {
                        icon: Some("star".to_string()),
                        ..Default::default()
                    },
                ),
                (Arc::from("e"), VisualMdCalloutManifestEntry::default()),
            ]),
            ..Default::default()
        };

        assert_eq!(
            entry.callout_icon_paths(),
            vec![PathBuf::from("icons/a.svg"), PathBuf::from("icons/b.svg")]
        );
    }
}
