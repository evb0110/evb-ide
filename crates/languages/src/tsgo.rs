use anyhow::Result;
use async_trait::async_trait;
use collections::HashMap;
use gpui::AsyncApp;
use language::{
    LanguageName, LanguageServerId, LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain,
};
use lsp::{CodeActionKind, LanguageServerBinary, LanguageServerName, Uri};
use node_runtime::{NodeRuntime, VersionStrategy};
use project::{lsp_store::language_server_settings, project_settings::ProjectSettings};
use semver::Version;
use serde_json::{Value, json};
use settings::{Settings, SettingsLocation};
use std::{
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::{ResultExt, merge_json_value_into, rel_path::RelPath};

use crate::vtsls::{
    VtslsLspAdapter, typescript_completion_label, typescript_language_features_config,
};

const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("tsgo");
const PACKAGE_NAME: &str = "typescript";
const PACKAGE_JSON_PATH: &str = "node_modules/typescript/package.json";
const LAUNCHER_PATH: &str = "node_modules/typescript/bin/tsc";

/// The native TypeScript language server shipped by TypeScript 7 and later as `tsc --lsp`.
pub struct TsgoLspAdapter {
    node: NodeRuntime,
}

impl TsgoLspAdapter {
    pub fn new(node: NodeRuntime) -> Self {
        Self { node }
    }
}

fn server_arguments() -> Vec<OsString> {
    vec!["--lsp".into(), "--stdio".into()]
}

/// The platform binary `typescript` would launch, run directly so that stopping the server
/// cannot leave it running behind the Node launcher on Windows.
fn native_binary_path(version_dir: &Path) -> Option<PathBuf> {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        "linux" => "linux",
        "freebsd" => "freebsd",
        "openbsd" => "openbsd",
        "netbsd" => "netbsd",
        _ => return None,
    };
    let architecture = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "arm" => "arm",
        "loongarch64" => "loong64",
        "riscv64" => "riscv64",
        _ => return None,
    };
    let binary_name = if cfg!(windows) { "tsc.exe" } else { "tsc" };
    let path = version_dir
        .join("node_modules/@typescript")
        .join(format!("typescript-{platform}-{architecture}"))
        .join("lib")
        .join(binary_name);
    path.is_file().then_some(path)
}

async fn server_binary(node: &NodeRuntime, version_dir: &Path) -> Result<LanguageServerBinary> {
    if let Some(path) = native_binary_path(version_dir) {
        return Ok(LanguageServerBinary {
            path,
            env: None,
            arguments: server_arguments(),
        });
    }
    let mut arguments = vec![version_dir.join(LAUNCHER_PATH).into_os_string()];
    arguments.extend(server_arguments());
    Ok(LanguageServerBinary {
        path: node.binary_path().await?,
        env: None,
        arguments,
    })
}

/// tsgo reports diagnostics for files it reads itself, such as `tsconfig.json`, under lowercased
/// paths on case-insensitive file systems. Those paths match no worktree, so the diagnostics would
/// be dropped; resolving the path restores the casing the file system stores.
fn uri_with_file_system_case(uri: &Uri) -> Option<Uri> {
    let path = uri.to_file_path().ok()?;
    let resolved_path = std::fs::canonicalize(&path).ok()?;
    // Resolving also follows symlinks, which must not move paths away from the worktree.
    let differs_only_in_case = resolved_path != path
        && resolved_path
            .to_string_lossy()
            .eq_ignore_ascii_case(&path.to_string_lossy());
    if !differs_only_in_case {
        return None;
    }
    Uri::from_file_path(resolved_path).ok()
}

impl LspInstaller for TsgoLspAdapter {
    type BinaryVersion = Version;

    async fn fetch_latest_server_version(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        cx: &mut AsyncApp,
    ) -> Result<Self::BinaryVersion> {
        let location = SettingsLocation {
            worktree_id: delegate.worktree_id(),
            path: RelPath::empty(),
        };
        let requirement = cx.update(|cx| {
            ProjectSettings::get(Some(location), cx)
                .typescript
                .version
                .native_requirement()
        });
        self.node
            .npm_package_latest_version_with_requirement(PACKAGE_NAME, Some(&requirement))
            .await
    }

    fn check_if_version_installed(
        &self,
        version: &Self::BinaryVersion,
        container_dir: &PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Option<LanguageServerBinary>> + use<> {
        let node = self.node.clone();
        let version = version.clone();
        let version_dir = container_dir.join(version.to_string());

        async move {
            if node
                .should_install_npm_package(
                    PACKAGE_NAME,
                    &version_dir.join(PACKAGE_JSON_PATH),
                    &version_dir,
                    VersionStrategy::Pin(&version),
                )
                .await
            {
                return None;
            }
            server_binary(&node, &version_dir).await.log_err()
        }
    }

    fn fetch_server_binary(
        &self,
        latest_version: Self::BinaryVersion,
        container_dir: PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let node = self.node.clone();

        async move {
            // Each version gets its own directory so projects pinned to different TypeScript 7
            // releases do not reinstall over each other.
            let version = latest_version.to_string();
            let version_dir = container_dir.join(&version);
            smol::fs::create_dir_all(&version_dir).await?;
            node.npm_install_packages(&version_dir, &[(PACKAGE_NAME, version.as_str())])
                .await?;
            server_binary(&node, &version_dir).await
        }
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        let mut entries = smol::fs::read_dir(&container_dir).await.log_err()?;
        let mut newest_version: Option<Version> = None;
        while let Some(entry) = futures::StreamExt::next(&mut entries).await {
            let Some(entry) = entry.log_err() else {
                continue;
            };
            let Some(version) = entry
                .file_name()
                .to_str()
                .and_then(|file_name| Version::parse(file_name).ok())
            else {
                continue;
            };
            if entry.path().join(PACKAGE_JSON_PATH).is_file()
                && newest_version
                    .as_ref()
                    .is_none_or(|newest_version| &version > newest_version)
            {
                newest_version = Some(version);
            }
        }
        let version_dir = container_dir.join(newest_version?.to_string());
        server_binary(&self.node, &version_dir).await.log_err()
    }
}

#[async_trait(?Send)]
impl LspAdapter for TsgoLspAdapter {
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
        let config = typescript_language_features_config();
        let mut workspace_configuration = json!({
            "typescript": config,
            "javascript": config,
        });
        let override_options = cx.update(|cx| {
            language_server_settings(delegate.as_ref(), &SERVER_NAME, cx)
                .and_then(|settings| settings.settings.clone())
        });
        if let Some(override_options) = override_options {
            merge_json_value_into(override_options, &mut workspace_configuration);
        }
        Ok(workspace_configuration)
    }

    fn process_diagnostics(&self, params: &mut lsp::PublishDiagnosticsParams, _: LanguageServerId) {
        if let Some(uri) = uri_with_file_system_case(&params.uri) {
            params.uri = uri;
        }
    }

    fn diagnostic_message_to_markdown(&self, message: &str) -> Option<String> {
        VtslsLspAdapter::enhance_diagnostic_message(message)
    }

    fn language_ids(&self) -> HashMap<LanguageName, String> {
        HashMap::from_iter([
            (LanguageName::new_static("TypeScript"), "typescript".into()),
            (LanguageName::new_static("JavaScript"), "javascript".into()),
            (LanguageName::new_static("TSX"), "typescriptreact".into()),
        ])
    }

    /// Started only when `typescript.version` selects TypeScript 7+, or when listed in a
    /// language's `language_servers`.
    fn is_opt_in_for(&self, _: &LanguageName) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::uri_with_file_system_case;
    use lsp::Uri;

    #[test]
    fn test_uri_with_file_system_case() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let directory_path = std::fs::canonicalize(directory.path())?;
        let config_path = directory_path.join("Project").join("tsconfig.json");
        std::fs::create_dir_all(
            config_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("no parent"))?,
        )?;
        std::fs::write(&config_path, "{}")?;

        let exact_uri =
            Uri::from_file_path(&config_path).map_err(|()| anyhow::anyhow!("bad path"))?;
        assert_eq!(uri_with_file_system_case(&exact_uri), None);

        let lowercased_path = directory_path.join("project").join("tsconfig.json");
        let lowercased_uri =
            Uri::from_file_path(&lowercased_path).map_err(|()| anyhow::anyhow!("bad path"))?;
        let file_system_is_case_insensitive = lowercased_path.exists();
        let restored_uri = uri_with_file_system_case(&lowercased_uri);
        if file_system_is_case_insensitive {
            assert_eq!(restored_uri, Some(exact_uri));
        } else {
            assert_eq!(restored_uri, None);
        }

        #[cfg(unix)]
        {
            let link_path = directory_path.join("linked-project");
            std::os::unix::fs::symlink(directory_path.join("Project"), &link_path)?;
            let linked_uri = Uri::from_file_path(link_path.join("tsconfig.json"))
                .map_err(|()| anyhow::anyhow!("bad path"))?;
            assert_eq!(uri_with_file_system_case(&linked_uri), None);
        }
        Ok(())
    }
}
