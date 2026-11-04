use std::sync::Arc;
use std::{
    fmt::Debug,
    path::{Path, PathBuf},
};

use anyhow::Result;
use collections::{HashMap, HashSet};
use gpui::{App, AssetSource, Global, SharedString};
use parking_lot::RwLock;
use thiserror::Error;

use crate::{
    Appearance, AppearanceContent, ChevronIcons, DEFAULT_ICON_THEME_NAME, DirectoryIcons,
    IconDefinition, IconTheme, Theme, ThemeFamily, default_icon_theme, deserialize_icon_theme,
};

/// The metadata for a theme.
#[derive(Debug, Clone)]
pub struct ThemeMeta {
    /// The name of the theme.
    pub name: SharedString,
    /// The appearance of the theme.
    pub appearance: Appearance,
}

/// An error indicating that the theme with the given name was not found.
#[derive(Debug, Error, Clone)]
#[error("theme not found: {0}")]
pub struct ThemeNotFoundError(pub SharedString);

/// An error indicating that the icon theme with the given name was not found.
#[derive(Debug, Error, Clone)]
#[error("icon theme not found: {0}")]
pub struct IconThemeNotFoundError(pub SharedString);

/// The global [`ThemeRegistry`].
///
/// This newtype exists for obtaining a unique [`TypeId`](std::any::TypeId) when
/// inserting the [`ThemeRegistry`] into the context as a global.
///
/// This should not be exposed outside of this module.
#[derive(Default)]
struct GlobalThemeRegistry(Arc<ThemeRegistry>);

impl std::ops::DerefMut for GlobalThemeRegistry {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl std::ops::Deref for GlobalThemeRegistry {
    type Target = Arc<ThemeRegistry>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Global for GlobalThemeRegistry {}

struct ThemeRegistryState {
    themes: HashMap<SharedString, Arc<Theme>>,
    icon_themes: HashMap<SharedString, Arc<IconTheme>>,
    bundled_icon_theme_names: HashSet<SharedString>,
    loaded_icon_theme: Option<LoadedIconTheme>,
    /// Whether the extensions have been loaded yet.
    extensions_loaded: bool,
}

/// The registry for themes.
pub struct ThemeRegistry {
    state: RwLock<ThemeRegistryState>,
    assets: Box<dyn AssetSource>,
}

struct LoadedIconTheme {
    path: PathBuf,
    icons_root_dir: PathBuf,
    contents: Vec<u8>,
    bundled: bool,
}

impl ThemeRegistry {
    /// Returns the global [`ThemeRegistry`].
    pub fn global(cx: &App) -> Arc<Self> {
        cx.global::<GlobalThemeRegistry>().0.clone()
    }

    /// Returns the global [`ThemeRegistry`].
    ///
    /// Inserts a default [`ThemeRegistry`] if one does not yet exist.
    pub fn default_global(cx: &mut App) -> Arc<Self> {
        cx.default_global::<GlobalThemeRegistry>().0.clone()
    }

    /// Returns the global [`ThemeRegistry`] if it exists.
    pub fn try_global(cx: &mut App) -> Option<Arc<Self>> {
        cx.try_global::<GlobalThemeRegistry>().map(|t| t.0.clone())
    }

    /// Sets the global [`ThemeRegistry`].
    pub(crate) fn set_global(assets: Box<dyn AssetSource>, cx: &mut App) {
        cx.set_global(GlobalThemeRegistry(Arc::new(ThemeRegistry::new(assets))));
    }

    /// Returns the asset source used by this registry.
    pub fn assets(&self) -> &dyn AssetSource {
        self.assets.as_ref()
    }

    /// Creates a new [`ThemeRegistry`] with the given [`AssetSource`].
    pub fn new(assets: Box<dyn AssetSource>) -> Self {
        let registry = Self {
            state: RwLock::new(ThemeRegistryState {
                themes: HashMap::default(),
                icon_themes: HashMap::default(),
                bundled_icon_theme_names: HashSet::default(),
                loaded_icon_theme: None,
                extensions_loaded: false,
            }),
            assets,
        };

        // We're loading the Zed default theme, as we need a theme to be loaded
        // for tests.
        registry.insert_theme_families([crate::fallback_themes::zed_default_themes()]);

        let default_icon_theme = crate::default_icon_theme();
        registry
            .state
            .write()
            .icon_themes
            .insert(default_icon_theme.name.clone(), default_icon_theme);

        registry
    }

    /// Returns whether the extensions have been loaded.
    pub fn extensions_loaded(&self) -> bool {
        self.state.read().extensions_loaded
    }

    /// Sets the flag indicating that the extensions have loaded.
    pub fn set_extensions_loaded(&self) {
        let mut state = self.state.write();
        state.extensions_loaded = true;
        state.loaded_icon_theme = None;
    }

    /// Inserts the given theme families into the registry.
    pub fn insert_theme_families(&self, families: impl IntoIterator<Item = ThemeFamily>) {
        for family in families.into_iter() {
            self.insert_themes(family.themes);
        }
    }

    /// Registers theme families for use in tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn register_test_themes(&self, families: impl IntoIterator<Item = ThemeFamily>) {
        self.insert_theme_families(families);
    }

    /// Registers icon themes for use in tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn register_test_icon_themes(&self, icon_themes: impl IntoIterator<Item = IconTheme>) {
        let mut state = self.state.write();
        state.loaded_icon_theme = None;
        for icon_theme in icon_themes {
            state
                .icon_themes
                .insert(icon_theme.name.clone(), Arc::new(icon_theme));
        }
    }

    /// Inserts the given themes into the registry.
    pub fn insert_themes(&self, themes: impl IntoIterator<Item = Theme>) {
        let mut state = self.state.write();
        for theme in themes.into_iter() {
            state.themes.insert(theme.name.clone(), Arc::new(theme));
        }
    }

    /// Removes the themes with the given names from the registry.
    pub fn remove_user_themes(&self, themes_to_remove: &[SharedString]) {
        self.state
            .write()
            .themes
            .retain(|name, _| !themes_to_remove.contains(name))
    }

    /// Removes all themes from the registry.
    pub fn clear(&self) {
        self.state.write().themes.clear();
    }

    /// Returns the names of all themes in the registry.
    pub fn list_names(&self) -> Vec<SharedString> {
        let mut names = self.state.read().themes.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }

    /// Returns the metadata of all themes in the registry.
    pub fn list(&self) -> Vec<ThemeMeta> {
        self.state
            .read()
            .themes
            .values()
            .map(|theme| ThemeMeta {
                name: theme.name.clone(),
                appearance: theme.appearance(),
            })
            .collect()
    }

    /// Returns the theme with the given name.
    pub fn get(&self, name: &str) -> Result<Arc<Theme>, ThemeNotFoundError> {
        self.state
            .read()
            .themes
            .get(name)
            .ok_or_else(|| ThemeNotFoundError(name.to_string().into()))
            .cloned()
    }

    /// Returns the default icon theme.
    pub fn default_icon_theme(&self) -> Result<Arc<IconTheme>, IconThemeNotFoundError> {
        self.get_icon_theme(DEFAULT_ICON_THEME_NAME)
    }

    /// Returns the metadata of all icon themes in the registry.
    pub fn list_icon_themes(&self) -> Vec<ThemeMeta> {
        self.state
            .read()
            .icon_themes
            .values()
            .map(|theme| ThemeMeta {
                name: theme.name.clone(),
                appearance: theme.appearance,
            })
            .collect()
    }

    /// Returns the icon theme with the specified name.
    pub fn get_icon_theme(&self, name: &str) -> Result<Arc<IconTheme>, IconThemeNotFoundError> {
        // The built-in icon theme's upstream name, which existing settings may still use.
        let name = if name == "Zed (Default)" {
            DEFAULT_ICON_THEME_NAME
        } else {
            name
        };
        self.state
            .read()
            .icon_themes
            .get(name)
            .ok_or_else(|| IconThemeNotFoundError(name.to_string().into()))
            .cloned()
    }

    /// Removes the icon themes with the given names from the registry.
    pub fn remove_icon_themes(&self, icon_themes_to_remove: &[SharedString]) {
        let mut state = self.state.write();
        if !icon_themes_to_remove.is_empty() {
            state.loaded_icon_theme = None;
        }
        for name in icon_themes_to_remove {
            if !state.bundled_icon_theme_names.contains(name) {
                state.icon_themes.remove(name);
            }
        }
    }

    /// Bundle-provided themes must survive loading or removing an older installed extension.
    pub fn load_bundled_icon_theme(
        &self,
        path: &Path,
        icons_root_dir: &Path,
        contents: Vec<u8>,
    ) -> Result<()> {
        self.load_icon_theme_inner(path, icons_root_dir, contents, true)
    }

    /// Loads the icon theme from the icon theme family and adds it to the registry.
    ///
    /// The `icons_root_dir` parameter indicates the root directory from which
    /// the relative paths to icons in the theme should be resolved against.
    pub fn load_icon_theme(
        &self,
        path: &Path,
        icons_root_dir: &Path,
        contents: Vec<u8>,
    ) -> Result<()> {
        self.load_icon_theme_inner(path, icons_root_dir, contents, false)
    }

    fn load_icon_theme_inner(
        &self,
        path: &Path,
        icons_root_dir: &Path,
        contents: Vec<u8>,
        bundled: bool,
    ) -> Result<()> {
        if self
            .state
            .read()
            .loaded_icon_theme
            .as_ref()
            .is_some_and(|loaded| {
                loaded.path == path
                    && loaded.icons_root_dir == icons_root_dir
                    && loaded.contents == contents
                    && loaded.bundled == bundled
            })
        {
            return Ok(());
        }

        let icon_theme_family = deserialize_icon_theme(&contents)?;
        let resolve_icon_path = |path: SharedString| {
            if bundled {
                // A bundled theme's icons are embedded assets, whose keys use `/` on every
                // platform; `Path::join` would put a `\` after the root on Windows.
                format!("{}/{path}", icons_root_dir.to_string_lossy()).into()
            } else {
                icons_root_dir
                    .join(path.as_ref())
                    .to_string_lossy()
                    .to_string()
                    .into()
            }
        };

        let default_icon_theme = default_icon_theme();

        let mut state = self.state.write();
        for icon_theme in icon_theme_family.themes {
            if !bundled
                && state
                    .bundled_icon_theme_names
                    .contains(icon_theme.name.as_str())
            {
                continue;
            }
            let mut file_stems = default_icon_theme.file_stems.clone();
            file_stems.extend(icon_theme.file_stems);

            let mut file_suffixes = default_icon_theme.file_suffixes.clone();
            file_suffixes.extend(icon_theme.file_suffixes);

            let mut named_directory_icons = default_icon_theme.named_directory_icons.clone();
            named_directory_icons.extend(icon_theme.named_directory_icons.into_iter().map(
                |(key, value)| {
                    (
                        key,
                        DirectoryIcons {
                            collapsed: value.collapsed.map(resolve_icon_path),
                            expanded: value.expanded.map(resolve_icon_path),
                        },
                    )
                },
            ));

            let icon_theme = IconTheme {
                id: uuid::Uuid::new_v4().to_string(),
                name: icon_theme.name.into(),
                appearance: match icon_theme.appearance {
                    AppearanceContent::Light => Appearance::Light,
                    AppearanceContent::Dark => Appearance::Dark,
                },
                directory_icons: DirectoryIcons {
                    collapsed: icon_theme.directory_icons.collapsed.map(resolve_icon_path),
                    expanded: icon_theme.directory_icons.expanded.map(resolve_icon_path),
                },
                named_directory_icons,
                chevron_icons: ChevronIcons {
                    collapsed: icon_theme.chevron_icons.collapsed.map(resolve_icon_path),
                    expanded: icon_theme.chevron_icons.expanded.map(resolve_icon_path),
                },
                file_stems,
                file_suffixes,
                file_icons: icon_theme
                    .file_icons
                    .into_iter()
                    .map(|(key, icon)| {
                        (
                            key,
                            IconDefinition {
                                path: resolve_icon_path(icon.path),
                            },
                        )
                    })
                    .collect(),
            };

            if bundled {
                state
                    .bundled_icon_theme_names
                    .insert(icon_theme.name.clone());
            }
            state
                .icon_themes
                .insert(icon_theme.name.clone(), Arc::new(icon_theme));
        }
        state.loaded_icon_theme = (!state.extensions_loaded).then(|| LoadedIconTheme {
            path: path.to_path_buf(),
            icons_root_dir: icons_root_dir.to_path_buf(),
            contents,
            bundled,
        });

        Ok(())
    }
}

impl Default for ThemeRegistry {
    fn default() -> Self {
        Self::new(Box::new(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;

    const BUNDLED_PATH: &str = "evb-jetbrains-icons/icon_themes/jetbrains-icons.json";
    const BUNDLED_ROOT: &str = "evb-jetbrains-icons";
    const BUNDLED_CONTENTS: &[u8] =
        include_bytes!("../../../extensions/evb-jetbrains-icons/icon_themes/jetbrains-icons.json");

    fn load_legacy_extension(registry: &ThemeRegistry) -> Result<()> {
        let mut legacy = deserialize_icon_theme(BUNDLED_CONTENTS)?;
        for theme in &mut legacy.themes {
            theme.directory_icons.expanded = theme.directory_icons.collapsed.clone();
            for icons in theme.named_directory_icons.values_mut() {
                icons.expanded = icons.collapsed.clone();
            }
        }
        registry.load_icon_theme(
            Path::new("/extensions/installed/evb-jetbrains-icons/icon_themes/jetbrains-icons.json"),
            Path::new("/extensions/installed/evb-jetbrains-icons"),
            serde_json::to_vec(&legacy)?,
        )
    }

    fn assert_bundled_folder_icons(registry: &ThemeRegistry) -> Result<()> {
        let bundled = deserialize_icon_theme(BUNDLED_CONTENTS)?;
        for expected in bundled.themes {
            let actual = registry.get_icon_theme(&expected.name)?;
            let assert_icons =
                |actual: &DirectoryIcons, expected: &crate::DirectoryIconsContent| {
                    let resolve = |path: &Option<SharedString>| {
                        // Bundled icons are embedded assets, keyed with `/` on every platform.
                        path.as_ref().map(|path| format!("{BUNDLED_ROOT}/{path}"))
                    };
                    assert_eq!(
                        actual.expanded.as_deref(),
                        resolve(&expected.expanded).as_deref()
                    );
                    assert_eq!(
                        actual.collapsed.as_deref(),
                        resolve(&expected.collapsed).as_deref()
                    );
                    assert_ne!(actual.collapsed, actual.expanded);
                };
            assert_icons(&actual.directory_icons, &expected.directory_icons);
            for (name, expected) in &expected.named_directory_icons {
                assert_icons(
                    actual
                        .named_directory_icons
                        .get(name)
                        .context("named folder icon")?,
                    expected,
                );
            }
        }
        Ok(())
    }

    #[test]
    fn bundled_icon_themes_survive_legacy_extension_load_and_removal() -> Result<()> {
        let registry = ThemeRegistry::default();
        registry.load_bundled_icon_theme(
            Path::new(BUNDLED_PATH),
            Path::new(BUNDLED_ROOT),
            BUNDLED_CONTENTS.to_vec(),
        )?;
        load_legacy_extension(&registry)?;
        assert_bundled_folder_icons(&registry)?;
        registry.set_extensions_loaded();
        load_legacy_extension(&registry)?;
        assert_bundled_folder_icons(&registry)?;
        registry.remove_icon_themes(&[
            "JetBrains Icons Light".into(),
            "JetBrains Icons Dark".into(),
        ]);
        assert_bundled_folder_icons(&registry)
    }

    #[test]
    fn bundled_icon_themes_replace_previously_loaded_extensions() -> Result<()> {
        let registry = ThemeRegistry::default();
        load_legacy_extension(&registry)?;
        registry.load_bundled_icon_theme(
            Path::new(BUNDLED_PATH),
            Path::new(BUNDLED_ROOT),
            BUNDLED_CONTENTS.to_vec(),
        )?;
        assert_bundled_folder_icons(&registry)
    }

    #[test]
    fn cached_icon_theme_can_be_registered_as_bundled() -> Result<()> {
        let registry = ThemeRegistry::default();
        registry.load_icon_theme(
            Path::new(BUNDLED_PATH),
            Path::new(BUNDLED_ROOT),
            BUNDLED_CONTENTS.to_vec(),
        )?;
        registry.load_bundled_icon_theme(
            Path::new(BUNDLED_PATH),
            Path::new(BUNDLED_ROOT),
            BUNDLED_CONTENTS.to_vec(),
        )?;
        load_legacy_extension(&registry)?;
        assert_bundled_folder_icons(&registry)
    }

    #[test]
    fn extension_icon_themes_can_still_be_reloaded_and_removed() -> Result<()> {
        let registry = ThemeRegistry::default();
        let mut contents = deserialize_icon_theme(BUNDLED_CONTENTS)?;
        for theme in &mut contents.themes {
            theme.name = format!("Custom {}", theme.name);
        }
        let bytes = serde_json::to_vec(&contents)?;
        registry.load_icon_theme(Path::new("custom.json"), Path::new("first"), bytes.clone())?;
        registry.load_icon_theme(Path::new("custom.json"), Path::new("second"), bytes)?;
        let theme = registry.get_icon_theme("Custom JetBrains Icons Dark")?;
        assert!(
            theme
                .directory_icons
                .expanded
                .as_deref()
                .context("expanded icon")?
                .starts_with("second/")
        );
        registry.remove_icon_themes(&["Custom JetBrains Icons Dark".into()]);
        assert!(
            registry
                .get_icon_theme("Custom JetBrains Icons Dark")
                .is_err()
        );
        Ok(())
    }
}
