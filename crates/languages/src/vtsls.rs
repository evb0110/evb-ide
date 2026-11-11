use anyhow::{Context as _, Result};
use async_trait::async_trait;
use collections::{HashMap, HashSet};
use futures::{StreamExt as _, lock::Mutex};
use gpui::{App, AsyncApp};
use language::{
    LanguageName, LspAdapter, LspAdapterDelegate, LspInstaller, PromptResponseContext, Toolchain,
};
use lsp::{CodeActionKind, LanguageServerBinary, LanguageServerName, Uri};
use node_runtime::{NodeRuntime, VersionStrategy};
use project::{
    Fs,
    lsp_store::language_server_settings,
    project_settings::{ProjectSettings, TypeScriptVersion},
};
use regex::Regex;
use semver::{Version, VersionReq};
use serde_json::Value;
use serde_json::json;
use settings::{Settings, SettingsLocation, WorktreeId, update_settings_file};
use std::{
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use util::{ResultExt, maybe, merge_json_value_into, rel_path::RelPath};

const ACTION_ALWAYS: &str = "Always";
const ACTION_NEVER: &str = "Never";
const UPDATE_IMPORTS_MESSAGE_PATTERN: &str = "Update imports for";
const VTSLS_SERVER_NAME: &str = "vtsls";

fn typescript_server_binary_arguments(server_path: &Path) -> Vec<OsString> {
    vec![server_path.into(), "--stdio".into()]
}

pub struct VtslsLspAdapter {
    node: NodeRuntime,
    fs: Arc<dyn Fs>,
    /// TypeScript version requirements already checked against npm in this session, so each
    /// one looks for a newer release once instead of on every configuration refresh.
    resolved_typescript_requirements: Mutex<HashSet<String>>,
}

impl VtslsLspAdapter {
    const PACKAGE_NAME: &'static str = "@vtsls/language-server";
    const SERVER_PATH: &'static str = "node_modules/@vtsls/language-server/bin/vtsls.js";

    const TYPESCRIPT_PACKAGE_NAME: &'static str = "typescript";
    const TYPESCRIPT_TSDK_PATH: &'static str = "node_modules/typescript/lib";
    const MANAGED_TYPESCRIPT_DIR: &'static str = "typescript";
    const TYPESCRIPT_YARN_TSDK_PATH: &'static str = ".yarn/sdks/typescript/lib";

    pub fn new(node: NodeRuntime, fs: Arc<dyn Fs>) -> Self {
        VtslsLspAdapter {
            node,
            fs,
            resolved_typescript_requirements: Mutex::default(),
        }
    }

    async fn tsdk_path(&self, adapter: &Arc<dyn LspAdapterDelegate>) -> Option<&'static str> {
        let yarn_sdk = adapter
            .worktree_root_path()
            .join(Self::TYPESCRIPT_YARN_TSDK_PATH);

        let tsdk_path = if self.fs.is_dir(&yarn_sdk).await {
            Self::TYPESCRIPT_YARN_TSDK_PATH
        } else {
            Self::TYPESCRIPT_TSDK_PATH
        };

        // vtsls doesn't support TypeScript 7+, which no longer ships `tsserver.js`.
        if self
            .fs
            .is_file(
                &adapter
                    .worktree_root_path()
                    .join(tsdk_path)
                    .join("tsserver.js"),
            )
            .await
        {
            Some(tsdk_path)
        } else {
            None
        }
    }

    /// Installs the newest TypeScript matching `requirement` next to the server, because the
    /// TypeScript vtsls bundles is pinned to an older release.
    async fn managed_tsdk_path(
        &self,
        requirement: &VersionReq,
        delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> Result<PathBuf> {
        let installs_dir = delegate
            .language_server_download_dir(&SERVER_NAME)
            .await
            .context("missing language server download directory")?
            .join(Self::MANAGED_TYPESCRIPT_DIR);
        let mut resolved_requirements = self.resolved_typescript_requirements.lock().await;
        let installed_version = self
            .installed_typescript_versions(&installs_dir)
            .await
            .into_iter()
            .filter(|version| requirement.matches(version))
            .max();
        let requirement_key = requirement.to_string();

        let version = match installed_version {
            Some(installed_version) if resolved_requirements.contains(&requirement_key) => {
                installed_version
            }
            installed_version => {
                let latest_version = self
                    .node
                    .npm_package_latest_version_with_requirement(
                        Self::TYPESCRIPT_PACKAGE_NAME,
                        Some(requirement),
                    )
                    .await;
                match (latest_version, installed_version) {
                    (Ok(latest_version), installed_version) => {
                        if installed_version.as_ref() != Some(&latest_version) {
                            let version_dir = installs_dir.join(latest_version.to_string());
                            self.fs.create_dir(&version_dir).await?;
                            self.node
                                .npm_install_packages(
                                    &version_dir,
                                    &[(
                                        Self::TYPESCRIPT_PACKAGE_NAME,
                                        latest_version.to_string().as_str(),
                                    )],
                                )
                                .await
                                .with_context(|| {
                                    format!("installing TypeScript {latest_version}")
                                })?;
                        }
                        latest_version
                    }
                    (Err(error), Some(installed_version)) => {
                        log::warn!(
                            "Using installed TypeScript {installed_version}, failed to check for a newer version matching {requirement}: {error:#}"
                        );
                        installed_version
                    }
                    (Err(error), None) => {
                        return Err(error.context(format!(
                            "resolving a TypeScript version matching {requirement}"
                        )));
                    }
                }
            }
        };
        resolved_requirements.insert(requirement_key);

        let tsdk_path = installs_dir
            .join(version.to_string())
            .join(Self::TYPESCRIPT_TSDK_PATH);
        anyhow::ensure!(
            self.fs.is_file(&tsdk_path.join("tsserver.js")).await,
            "TypeScript {version} has no tsserver.js"
        );
        Ok(tsdk_path)
    }

    async fn installed_typescript_versions(&self, installs_dir: &Path) -> Vec<Version> {
        let Ok(mut entries) = self.fs.read_dir(installs_dir).await else {
            return Vec::new();
        };
        let mut versions = Vec::new();
        while let Some(entry) = entries.next().await {
            let Some(entry) = entry.log_err() else {
                continue;
            };
            let Some(version) = entry
                .file_name()
                .and_then(|file_name| file_name.to_str())
                .and_then(|file_name| Version::parse(file_name).ok())
            else {
                continue;
            };
            if self
                .fs
                .is_file(&entry.join(Self::TYPESCRIPT_TSDK_PATH).join("tsserver.js"))
                .await
            {
                versions.push(version);
            }
        }
        versions
    }

    pub fn enhance_diagnostic_message(message: &str) -> Option<String> {
        static SINGLE_WORD_REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"'([^\s']*)'").expect("Failed to create REGEX"));

        static MULTI_WORD_REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"'([^']+\s+[^']*)'").expect("Failed to create REGEX"));

        let first = SINGLE_WORD_REGEX.replace_all(message, "`$1`").to_string();
        let second = MULTI_WORD_REGEX
            .replace_all(&first, "\n```typescript\n$1\n```\n")
            .to_string();
        Some(second)
    }
}

const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("vtsls");

impl LspInstaller for VtslsLspAdapter {
    type BinaryVersion = Version;

    async fn fetch_latest_server_version(
        &self,
        _: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut AsyncApp,
    ) -> Result<Self::BinaryVersion> {
        self.node
            .npm_package_latest_version(Self::PACKAGE_NAME)
            .await
    }

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        let env = delegate.shell_env().await;
        let path = delegate.which(SERVER_NAME.as_ref()).await?;
        Some(LanguageServerBinary {
            path: path.clone(),
            arguments: typescript_server_binary_arguments(&path),
            env: Some(env),
        })
    }

    fn fetch_server_binary(
        &self,
        _latest_version: Self::BinaryVersion,
        container_dir: PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let node = self.node.clone();

        async move {
            let server_path = container_dir.join(Self::SERVER_PATH);

            node.npm_install_latest_packages(&container_dir, &[Self::PACKAGE_NAME])
                .await?;

            Ok(LanguageServerBinary {
                path: node.binary_path().await?,
                env: None,
                arguments: typescript_server_binary_arguments(&server_path),
            })
        }
    }

    fn check_if_version_installed(
        &self,
        version: &Self::BinaryVersion,
        container_dir: &PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Option<LanguageServerBinary>> + use<> {
        let node = self.node.clone();
        let server_version = version.clone();
        let container_dir = container_dir.clone();

        async move {
            let server_path = container_dir.join(Self::SERVER_PATH);

            if node
                .should_install_npm_package(
                    Self::PACKAGE_NAME,
                    &server_path,
                    &container_dir,
                    VersionStrategy::Latest(&server_version),
                )
                .await
            {
                return None;
            }

            Some(LanguageServerBinary {
                path: node.binary_path().await.ok()?,
                env: None,
                arguments: typescript_server_binary_arguments(&server_path),
            })
        }
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        get_cached_ts_server_binary(container_dir, &self.node).await
    }
}

#[async_trait(?Send)]
impl LspAdapter for VtslsLspAdapter {
    fn name(&self) -> LanguageServerName {
        SERVER_NAME
    }

    fn code_action_kinds(&self) -> Option<Vec<CodeActionKind>> {
        Some(vec![
            CodeActionKind::QUICKFIX,
            CodeActionKind::REFACTOR,
            CodeActionKind::REFACTOR_EXTRACT,
            CodeActionKind::SOURCE,
        ])
    }

    async fn label_for_completion(
        &self,
        item: &lsp::CompletionItem,
        language: &Arc<language::Language>,
    ) -> Option<language::CodeLabel> {
        typescript_completion_label(item, language)
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: Option<Uri>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let location = SettingsLocation {
            worktree_id: delegate.worktree_id(),
            path: RelPath::empty(),
        };
        let typescript_version = cx.update(|cx| {
            ProjectSettings::get(Some(location), cx)
                .typescript
                .version
                .clone()
        });
        let project_tsdk_path = match typescript_version {
            TypeScriptVersion::Auto => self.tsdk_path(delegate).await,
            TypeScriptVersion::Requirement(_) => None,
        };
        let tsdk_path = match project_tsdk_path {
            Some(project_tsdk_path) => Some(project_tsdk_path.to_string()),
            None => {
                let requirement = typescript_version.tsserver_requirement();
                match self.managed_tsdk_path(&requirement, delegate).await {
                    Ok(managed_tsdk_path) => Some(managed_tsdk_path.to_string_lossy().into_owned()),
                    Err(error) => {
                        let message = format!(
                            "Failed to install TypeScript {requirement}, using the TypeScript bundled with vtsls: {error:#}"
                        );
                        log::error!("{message}");
                        // Only a version the user picked is worth interrupting them for; the
                        // automatic choice silently keeps the bundled TypeScript, as before.
                        if matches!(typescript_version, TypeScriptVersion::Requirement(_)) {
                            cx.update(|cx| delegate.show_notification(&message, cx));
                        }
                        None
                    }
                }
            }
        };
        let mut config = typescript_language_features_config();
        config["tsdk"] = json!(tsdk_path);
        config["tsserver"] = json!({ "maxTsServerMemory": 8192 });

        let mut default_workspace_configuration = serde_json::json!({
            "typescript": config,
            "javascript": config,
            "vtsls": {
                "experimental": {
                    "completion": {
                        "enableServerSideFuzzyMatch": true,
                        "entriesLimit": 5000,
                    }
                },
               "autoUseWorkspaceTsdk": true
            }
        });

        let override_options = cx.update(|cx| {
            language_server_settings(delegate.as_ref(), &SERVER_NAME, cx)
                .and_then(|s| s.settings.clone())
        });

        if let Some(override_options) = override_options {
            merge_json_value_into(override_options, &mut default_workspace_configuration)
        }

        Ok(default_workspace_configuration)
    }

    fn diagnostic_message_to_markdown(&self, message: &str) -> Option<String> {
        VtslsLspAdapter::enhance_diagnostic_message(message)
    }

    /// vtsls picks its TypeScript once at startup and ignores later `typescript.tsdk` changes.
    fn launch_fingerprint(&self, worktree_id: WorktreeId, cx: &App) -> Option<String> {
        let location = SettingsLocation {
            worktree_id,
            path: RelPath::empty(),
        };
        match &ProjectSettings::get(Some(location), cx).typescript.version {
            TypeScriptVersion::Auto => None,
            TypeScriptVersion::Requirement(requirement) => Some(requirement.to_string()),
        }
    }

    fn language_ids(&self) -> HashMap<LanguageName, String> {
        HashMap::from_iter([
            (LanguageName::new_static("TypeScript"), "typescript".into()),
            (LanguageName::new_static("JavaScript"), "javascript".into()),
            (LanguageName::new_static("TSX"), "typescriptreact".into()),
        ])
    }

    fn process_prompt_response(&self, context: &PromptResponseContext, cx: &mut AsyncApp) {
        let selected_title = context.selected_action.title.as_str();
        let is_preference_response =
            selected_title == ACTION_ALWAYS || selected_title == ACTION_NEVER;
        if !is_preference_response {
            return;
        }

        if context.message.contains(UPDATE_IMPORTS_MESSAGE_PATTERN) {
            let setting_value = match selected_title {
                ACTION_ALWAYS => "always",
                ACTION_NEVER => "never",
                _ => return,
            };

            let settings = json!({
                "typescript": {
                    "updateImportsOnFileMove": {
                        "enabled": setting_value
                    }
                },
                "javascript": {
                    "updateImportsOnFileMove": {
                        "enabled": setting_value
                    }
                }
            });

            let _ = cx.update(|cx| {
                update_settings_file(self.fs.clone(), cx, move |content, _| {
                    let lsp_settings = content
                        .project
                        .lsp
                        .0
                        .entry(VTSLS_SERVER_NAME.into())
                        .or_default();

                    if let Some(existing) = &mut lsp_settings.settings {
                        merge_json_value_into(settings, existing);
                    } else {
                        lsp_settings.settings = Some(settings);
                    }
                });
            });
        }
    }
}

/// VS Code-style JavaScript and TypeScript settings shared by vtsls and the native server.
pub(crate) fn typescript_language_features_config() -> Value {
    json!({
        "suggest": {
            "completeFunctionCalls": true
        },
        "inlayHints": {
            "parameterNames": {
                "enabled": "all",
                "suppressWhenArgumentMatchesName": false
            },
            "parameterTypes": {
                "enabled": true
            },
            "variableTypes": {
                "enabled": true,
                "suppressWhenTypeMatchesName": false
            },
            "propertyDeclarationTypes": {
                "enabled": true
            },
            "functionLikeReturnTypes": {
                "enabled": true
            },
            "enumMemberValues": {
                "enabled": true
            }
        },
        "implementationsCodeLens": {
            "enabled": true,
            "showOnAllClassMethods": true,
            "showOnInterfaceMethods": true
        },
        "referencesCodeLens": {
            "enabled": true,
            "showOnAllFunctions": true
        },
    })
}

pub(crate) fn typescript_completion_label(
    item: &lsp::CompletionItem,
    language: &Arc<language::Language>,
) -> Option<language::CodeLabel> {
    use lsp::CompletionItemKind as Kind;
    let label_len = item.label.len();
    let grammar = language.grammar()?;
    let highlight_id = match item.kind? {
        Kind::CLASS | Kind::INTERFACE | Kind::ENUM => grammar.highlight_id_for_name("type"),
        Kind::CONSTRUCTOR => grammar.highlight_id_for_name("type"),
        Kind::CONSTANT => grammar.highlight_id_for_name("constant"),
        Kind::FUNCTION | Kind::METHOD => grammar.highlight_id_for_name("function"),
        Kind::PROPERTY | Kind::FIELD => grammar.highlight_id_for_name("property"),
        Kind::VARIABLE => grammar.highlight_id_for_name("variable"),
        _ => None,
    }?;

    let text = if let Some(description) = item
        .label_details
        .as_ref()
        .and_then(|label_details| label_details.description.as_ref())
    {
        format!("{} {}", item.label, description)
    } else if let Some(detail) = &item.detail {
        format!("{} {}", item.label, detail)
    } else {
        item.label.clone()
    };
    Some(language::CodeLabel::filtered(
        text,
        label_len,
        item.filter_text.as_deref(),
        vec![(0..label_len, highlight_id)],
    ))
}

async fn get_cached_ts_server_binary(
    container_dir: PathBuf,
    node: &NodeRuntime,
) -> Option<LanguageServerBinary> {
    maybe!(async {
        let server_path = container_dir.join(VtslsLspAdapter::SERVER_PATH);
        anyhow::ensure!(
            server_path.exists(),
            "missing executable in directory {container_dir:?}"
        );
        Ok(LanguageServerBinary {
            path: node.binary_path().await?,
            env: None,
            arguments: typescript_server_binary_arguments(&server_path),
        })
    })
    .await
    .log_err()
}

#[cfg(test)]
mod tests {
    use crate::vtsls::VtslsLspAdapter;

    #[test]
    fn test_diagnostic_message_to_markdown() {
        // Leaves simple messages unchanged
        let message = "The expected type comes from the return type of this signature.";

        let expected = "The expected type comes from the return type of this signature.";

        assert_eq!(
            VtslsLspAdapter::enhance_diagnostic_message(message).expect("Should be some"),
            expected
        );

        // Parses both multi-word and single-word correctly
        let message = "Property 'baz' is missing in type '{ foo: string; bar: string; }' but required in type 'User'.";

        let expected = "Property `baz` is missing in type \n```typescript\n{ foo: string; bar: string; }\n```\n but required in type `User`.";

        assert_eq!(
            VtslsLspAdapter::enhance_diagnostic_message(message).expect("Should be some"),
            expected
        );

        // Parses multi-and-single word in any order, and ignores existing newlines
        let message = "Type '() => { foo: string; bar: string; }' is not assignable to type 'GetUserFunction'.\n  Property 'baz' is missing in type '{ foo: string; bar: string; }' but required in type 'User'.";

        let expected = "Type \n```typescript\n() => { foo: string; bar: string; }\n```\n is not assignable to type `GetUserFunction`.\n  Property `baz` is missing in type \n```typescript\n{ foo: string; bar: string; }\n```\n but required in type `User`.";

        assert_eq!(
            VtslsLspAdapter::enhance_diagnostic_message(message).expect("Should be some"),
            expected
        );
    }
}
