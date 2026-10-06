//! Provides access to Zed settings.

#[path = "../wit/since_v0.9.0/settings.rs"]
mod types;

use crate::{Project, Result, SettingsLocation, Worktree, wit};
use serde_json;
pub use types::*;

impl LanguageSettings {
    /// Returns the [`LanguageSettings`] for the given language.
    pub fn for_worktree(language: Option<&str>, worktree: &Worktree) -> Result<Self> {
        get_settings("language", language, Some(worktree.id()))
    }
}

impl LspSettings {
    /// Returns the [`LspSettings`] for the given language server.
    pub fn for_worktree(language_server_name: &str, worktree: &Worktree) -> Result<Self> {
        get_settings("lsp", Some(language_server_name), Some(worktree.id()))
    }
}

impl ContextServerSettings {
    /// Returns the [`ContextServerSettings`] for the given context server.
    pub fn for_project(context_server_id: &str, project: &Project) -> Result<Self> {
        let global_setting: Self = get_settings("context_servers", Some(context_server_id), None)?;

        for worktree_id in project.worktree_ids() {
            let settings = get_settings(
                "context_servers",
                Some(context_server_id),
                Some(worktree_id),
            )?;
            if settings != global_setting {
                return Ok(settings);
            }
        }

        Ok(global_setting)
    }
}

/// Reads what the user has set for the calling extension under
/// `visual_md.extensions.<extension id>`, or, given a key, that key of it.
///
/// The settings are free-form JSON, and an extension is only ever handed its
/// own. They are `null` when nothing is set, so ask for an `Option` to tell
/// that apart from a value that does not fit `T`.
pub fn visual_md_extension_settings<T: serde::de::DeserializeOwned>(
    key: Option<&str>,
) -> Result<T> {
    get_settings("visual_md", key, None)
}

fn get_settings<T: serde::de::DeserializeOwned>(
    settings_type: &str,
    settings_name: Option<&str>,
    worktree_id: Option<u64>,
) -> Result<T> {
    let location = worktree_id.map(|worktree_id| SettingsLocation {
        worktree_id,
        path: String::new(),
    });
    let settings_json = wit::get_settings(location.as_ref(), settings_type, settings_name)?;
    let settings: T = serde_json::from_str(&settings_json).map_err(|err| err.to_string())?;
    Ok(settings)
}
