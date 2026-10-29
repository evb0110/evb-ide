use std::{sync::Arc, time::Duration};

use gpui::{AppContext, AsyncApp, WeakEntity};
use lsp::{LanguageServer, LanguageServerId, LanguageServerName};
use serde_json::Value;
use util::ResultExt as _;

use crate::{LspStore, ProjectSettings};
use settings::Settings;

struct VueServerRequest;
struct TypescriptServerResponse;

impl lsp::notification::Notification for VueServerRequest {
    type Params = Vec<(u64, String, serde_json::Value)>;

    const METHOD: &'static str = "tsserver/request";
}

impl lsp::notification::Notification for TypescriptServerResponse {
    type Params = Vec<(u64, serde_json::Value)>;

    const METHOD: &'static str = "tsserver/response";
}

const VUE_SERVER_NAME: LanguageServerName = LanguageServerName::new_static("vue-language-server");
const VTSLS: LanguageServerName = LanguageServerName::new_static("vtsls");
const TS_LS: LanguageServerName = LanguageServerName::new_static("typescript-language-server");

/// How long Vue requests wait for a TypeScript server to finish starting. Vue
/// caches a null answer as "this file has no TypeScript project", so answering
/// before the TypeScript server runs (as on a cold start, when both servers
/// launch together) loses type information until the servers restart.
const TYPESCRIPT_SERVER_STARTUP_WAIT: Duration = Duration::from_secs(60);
const TYPESCRIPT_SERVER_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// The running TypeScript server that should answer the given Vue server: one
/// from the Vue server's own worktree when that worktree has one, otherwise any.
/// Returns `None` while the chosen worktree's TypeScript server is still starting.
fn typescript_server_for_vue_server(
    lsp_store: &LspStore,
    vue_server_id: LanguageServerId,
) -> Option<Arc<LanguageServer>> {
    let local = lsp_store.as_local()?;
    let vue_worktree = local
        .language_server_ids
        .iter()
        .find_map(|(seed, server)| (server.id == vue_server_id).then_some(seed.worktree_id));
    let typescript_seeds = || {
        local
            .language_server_ids
            .iter()
            .filter(|(seed, _)| [VTSLS, TS_LS].contains(&seed.name))
    };
    let has_worktree_server =
        typescript_seeds().any(|(seed, _)| Some(seed.worktree_id) == vue_worktree);
    typescript_seeds()
        .filter(|(seed, _)| !has_worktree_server || Some(seed.worktree_id) == vue_worktree)
        .find_map(|(_, server)| local.language_server_for_id(server.id))
}

pub fn register_requests(lsp_store: WeakEntity<LspStore>, language_server: &LanguageServer) {
    let language_server_name = language_server.name();
    if language_server_name != VUE_SERVER_NAME {
        return;
    }

    let vue_server_id = language_server.server_id();
    language_server
        .on_notification::<VueServerRequest, _>({
            move |params, cx| {
                let lsp_store = lsp_store.clone();
                let Ok(Some(vue_server)) =
                    lsp_store.read_with(cx, |this, _| this.language_server_for_id(vue_server_id))
                else {
                    return;
                };

                let requests = params;
                cx.spawn(async move |cx| {
                    let mut waited = Duration::ZERO;
                    let target_server = loop {
                        match lsp_store.read_with(cx, |this, _| {
                            typescript_server_for_vue_server(this, vue_server_id)
                        }) {
                            Ok(Some(server)) => break server,
                            Ok(None) if waited < TYPESCRIPT_SERVER_STARTUP_WAIT => {
                                cx.background_executor()
                                    .timer(TYPESCRIPT_SERVER_POLL_INTERVAL)
                                    .await;
                                waited += TYPESCRIPT_SERVER_POLL_INTERVAL;
                            }
                            other => {
                                log::warn!(
                                    "vue-language-server forwarding skipped after {waited:?}: \
                                        no running TypeScript server ({other:?}). \
                                        Returning null tsserver responses"
                                );
                                if !requests.is_empty() {
                                    let null_responses = requests
                                        .into_iter()
                                        .map(|(id, _, _)| (id, Value::Null))
                                        .collect::<Vec<_>>();
                                    vue_server
                                        .notify::<TypescriptServerResponse>(null_responses)
                                        .log_err();
                                }
                                return;
                            }
                        }
                    };
                    forward_requests(requests, target_server, vue_server, cx);
                })
                .detach();
            }
        })
        .detach();
}

fn forward_requests(
    requests: Vec<(u64, String, Value)>,
    target_server: Arc<LanguageServer>,
    vue_server: Arc<LanguageServer>,
    cx: &mut AsyncApp,
) {
    let cx = cx.clone();
    let request_timeout = cx.update(|app| {
        ProjectSettings::get_global(app)
            .global_lsp_settings
            .get_request_timeout()
    });

    for (request_id, command, payload) in requests.into_iter() {
        let target_server = target_server.clone();
        let vue_server = vue_server.clone();
        cx.background_spawn(async move {
                        let response = target_server
                            .request::<lsp::request::ExecuteCommand>(
                                lsp::ExecuteCommandParams {
                                    command: "typescript.tsserverRequest".to_owned(),
                                    arguments: vec![Value::String(command), payload],
                                    ..Default::default()
                                }, request_timeout
                            )
                            .await;

                        let response_body = match response {
                            util::ConnectionResult::Result(Ok(result)) => match result {
                                Some(Value::Object(mut map)) => map
                                    .remove("body")
                                    .unwrap_or(Value::Null),
                                Some(_) => Value::Null,
                                None => Value::Null,
                            },
                            util::ConnectionResult::Result(Err(error)) => {
                                log::warn!(
                                    "typescript.tsserverRequest failed: {error:?} for request {request_id}"
                                );
                                Value::Null
                            }
                            other => {
                                log::warn!(
                                    "typescript.tsserverRequest did not return a response: {other:?} for request {request_id}"
                                );
                                Value::Null
                            }
                        };

                        if let Err(err) = vue_server
                            .notify::<TypescriptServerResponse>(vec![(request_id, response_body)])
                        {
                            log::warn!(
                                "Failed to notify vue-language-server of tsserver response: {err:?}"
                            );
                        }
                    })
                    .detach();
    }
}
