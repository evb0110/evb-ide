mod base_keymap_setting;
mod content_into_gpui;
mod editable_setting_control;
mod editorconfig_store;
mod granted_write_path;
mod keymap_file;
mod settings_file;
mod settings_store;
mod vscode_import;

pub use settings_macros::RegisterSetting;

pub mod settings_content {
    pub use ::settings_content::*;
}

pub mod fallible_options {
    pub use ::settings_content::{FallibleOption, parse_json};
}

#[doc(hidden)]
pub mod private {
    pub use crate::settings_store::{RegisteredSetting, SettingValue};
    pub use inventory;
}

use gpui::{App, Global};

use crate as settings;
use std::env;
use std::{borrow::Cow, fmt, str};
use util::asset_str;

pub use ::settings_content::*;
pub use base_keymap_setting::*;
pub use content_into_gpui::IntoGpui;
pub use editable_setting_control::*;
pub use editorconfig_store::{
    Editorconfig, EditorconfigEvent, EditorconfigProperties, EditorconfigStore,
};
pub use granted_write_path::GrantedWritePath;
pub use keymap_file::{
    KeyBindingValidator, KeyBindingValidatorRegistration, KeybindSource, KeybindUpdateOperation,
    KeybindUpdateTarget, KeymapFile, KeymapFileLoadResult,
};
pub use settings_file::*;
pub use settings_json::*;
pub use settings_store::{
    DefaultSemanticTokenRules, InvalidSettingsError, LSP_SETTINGS_SCHEMA_URL_PREFIX,
    LocalSettingsKind, LocalSettingsPath, MigrationStatus, Settings, SettingsFile,
    SettingsJsonSchemaParams, SettingsKey, SettingsLocation, SettingsParseResult, SettingsStore,
};

pub use vscode_import::{VsCodeSettings, VsCodeSettingsSource};

pub use keymap_file::ActionSequence;

#[derive(Clone, Debug, Default, RegisterSetting)]
pub struct CopilotSettings {
    pub enterprise_uri: Option<String>,
}

impl Settings for CopilotSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            enterprise_uri: content
                .copilot
                .as_ref()
                .and_then(|copilot| copilot.enterprise_uri.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActiveSettingsProfileName(pub String);

impl Global for ActiveSettingsProfileName {}

pub trait UserSettingsContentExt {
    fn for_profile(&self, cx: &App) -> Option<&SettingsProfile>;
    fn for_release_channel(&self) -> Option<&SettingsContent>;
    fn for_os(&self) -> Option<&SettingsContent>;
}

impl UserSettingsContentExt for UserSettingsContent {
    fn for_profile(&self, cx: &App) -> Option<&SettingsProfile> {
        let Some(active_profile) = cx.try_global::<ActiveSettingsProfileName>() else {
            return None;
        };
        self.profiles.get(&active_profile.0)
    }

    fn for_release_channel(&self) -> Option<&SettingsContent> {
        self.release_channel_overrides
            .get_by_key(release_channel::RELEASE_CHANNEL.dev_name())
    }

    fn for_os(&self) -> Option<&SettingsContent> {
        self.platform_overrides.get_by_key(env::consts::OS)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct WorktreeId(usize);

impl From<WorktreeId> for usize {
    fn from(value: WorktreeId) -> Self {
        value.0
    }
}

impl WorktreeId {
    pub fn from_usize(handle_id: usize) -> Self {
        Self(handle_id)
    }

    pub fn from_proto(id: u64) -> Self {
        Self(id as usize)
    }

    pub fn to_proto(self) -> u64 {
        self.0 as u64
    }

    pub fn to_usize(self) -> usize {
        self.0
    }
}

impl fmt::Display for WorktreeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

// Dev builds read the checkout's files at runtime instead of embedding them;
// see the `assets` crate for the rationale.
util::fs_embed! {
    pub struct SettingsAssets,
    crate_relative = "../../assets",
    root_relative = "assets",
    include = ["settings/*", "keymaps/*"],
    exclude = ["*.DS_Store"],
}

pub fn init(cx: &mut App) {
    let settings = SettingsStore::new(cx, &app_default_settings());
    cx.set_global(settings);
    SettingsStore::observe_active_settings_profile_name(cx).detach();
}

pub fn default_settings() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/default.json")
}

/// EVB IDE keeps its defaults in a separate `settings/evb.json` layer instead of
/// editing `default.json`, so upstream changes to `default.json` rebase cleanly.
pub fn evb_default_settings() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/evb.json")
}

/// `default.json` with the `evb.json` layer merged over it.
pub fn app_default_settings() -> Cow<'static, str> {
    let defaults = default_settings();
    let merged = parse_json_with_comments::<serde_json::Value>(&defaults).and_then(|mut value| {
        let overrides = parse_json_with_comments::<serde_json::Value>(&evb_default_settings())?;
        util::merge_non_null_json_value_into(overrides, &mut value);
        Ok(serde_json::to_string(&value)?)
    });
    match merged {
        Ok(merged) => Cow::Owned(merged),
        Err(error) => {
            log::error!("failed to apply settings/evb.json defaults: {error:#}");
            defaults
        }
    }
}

pub fn default_semantic_token_rules() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/default_semantic_token_rules.json")
}

#[cfg(target_os = "macos")]
pub const DEFAULT_KEYMAP_PATH: &str = "keymaps/default-macos.json";

#[cfg(target_os = "windows")]
pub const DEFAULT_KEYMAP_PATH: &str = "keymaps/default-windows.json";

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const DEFAULT_KEYMAP_PATH: &str = "keymaps/default-linux.json";

pub fn default_keymap() -> Cow<'static, str> {
    asset_str::<SettingsAssets>(DEFAULT_KEYMAP_PATH)
}

pub const VIM_KEYMAP_PATH: &str = "keymaps/vim.json";

/// EVB IDE bindings, layered over the base keymap.
#[cfg(target_os = "macos")]
pub const EVB_KEYMAP_PATH: &str = "keymaps/evb.json";

/// EVB IDE bindings, layered over the base keymap, with WebStorm's Linux and Windows keys.
#[cfg(not(target_os = "macos"))]
pub const EVB_KEYMAP_PATH: &str = "keymaps/linux/evb.json";

pub fn vim_keymap() -> Cow<'static, str> {
    asset_str::<SettingsAssets>(VIM_KEYMAP_PATH)
}

/// Specific keybinding overrides. Loaded after the base keymap so they win over
/// conflicting base-keymap (and default `Editor`) bindings for the same chords,
/// while still allowing user keymaps (loaded last) to override them. Shared
/// across features - prefer adding a context block here over creating another
/// override keymap file.
#[cfg(target_os = "macos")]
pub const SPECIFIC_OVERRIDES_KEYMAP_PATH: &str = "keymaps/specific-overrides-macos.json";

#[cfg(not(target_os = "macos"))]
pub const SPECIFIC_OVERRIDES_KEYMAP_PATH: &str = "keymaps/specific-overrides.json";

pub fn initial_user_settings_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/initial_user_settings.json")
}

pub fn initial_server_settings_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/initial_server_settings.json")
}

pub fn initial_project_settings_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/initial_local_settings.json")
}

pub fn initial_keymap_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("keymaps/initial.json")
}

pub fn initial_tasks_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/initial_tasks.json")
}

pub fn initial_worktree_setup_tasks_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/initial_worktree_setup_tasks.json")
}

pub fn initial_debug_tasks_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/initial_debug_tasks.json")
}

pub fn initial_local_debug_tasks_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/initial_local_debug_tasks.json")
}

#[cfg(test)]
mod evb_default_settings_tests {
    use super::*;

    #[test]
    fn evb_layer_parses_and_overrides_upstream_defaults() {
        let merged = parse_json_with_comments::<serde_json::Value>(&app_default_settings())
            .expect("merged defaults are valid JSON");
        assert_eq!(merged["base_keymap"], "JetBrains");
        assert_eq!(merged["theme"]["light"], "IntelliJ Light");
        assert_eq!(merged["telemetry"]["metrics"], false);
        assert_eq!(merged["default_open_behavior"], "new_window");
        assert_eq!(merged["cli_default_open_behavior"], "new_window");
        assert_eq!(merged["use_system_window_tabs"], true);
        assert_eq!(merged["credentials_url"], "https://zed.dev/evb-ide");
        // The JSONC list keeps upstream's patterns and adds EVB IDE's config directory.
        let jsonc = &merged["file_types"]["JSONC"];
        let defaults = parse_json_with_comments::<serde_json::Value>(&default_settings())
            .expect("default settings are valid JSON");
        for pattern in defaults["file_types"]["JSONC"]
            .as_array()
            .into_iter()
            .flatten()
        {
            assert!(
                jsonc.as_array().is_some_and(|list| list.contains(pattern)),
                "{pattern}"
            );
        }
        assert!(
            jsonc
                .to_string()
                .contains("**/{evb-ide,EVB-IDE}/{settings,keymap,tasks,debug}.json")
        );
        assert_eq!(merged["project_panel"]["auto_reveal_entries"], false);
        assert_eq!(merged["auto_install_extensions"]["scss"], true);
        // `disable_ai` merges with OR, so a default of true could never be undone.
        assert_eq!(merged["disable_ai"], false);
        // Keys the EVB layer does not mention keep their upstream values.
        assert_eq!(merged["git_panel"]["button"], true);
        assert_eq!(merged["git"]["blame"]["show_avatar"], false);
        assert_eq!(merged["git"]["inline_blame"]["enabled"], true);

        let content = UserSettingsContent::parse_json_with_comments(&app_default_settings());
        assert!(content.is_ok(), "{:?}", content.err());

        // A settings file created on first use must not override the EVB layer.
        let initial =
            parse_json_with_comments::<serde_json::Value>(&initial_user_settings_content())
                .expect("the initial settings file is valid JSON");
        assert_eq!(initial, serde_json::json!({}));
    }
}
