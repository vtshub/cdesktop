use std::path::{Path, PathBuf};

use axum::{
    Extension, Json, Router,
    extract::{Query, State},
    response::Json as ResponseJson,
    routing::{get, post},
};
use db::models::{workspace::Workspace, workspace_repo::WorkspaceRepo};
use deployment::Deployment;
use executors::{
    executors::{CodingAgent, ExecutorError},
    profile::{ExecutorConfigs, ExecutorProfileId},
};
use serde::{Deserialize, Serialize};
use services::services::container::ContainerService;
use ts_rs::TS;
use utils::response::ApiResponse;

use super::{codex_setup, cursor_setup, gh_cli_setup::GhCliSetupError};
use crate::{DeploymentImpl, error::ApiError};

#[derive(Debug, Deserialize, Serialize, TS)]
pub struct RunAgentSetupRequest {
    pub executor_profile_id: ExecutorProfileId,
}

#[derive(Debug, Serialize, TS)]
pub struct RunAgentSetupResponse {}

#[derive(Deserialize, TS)]
pub struct OpenEditorRequest {
    editor_type: Option<String>,
    file_path: Option<String>,
}

#[derive(Debug, Serialize, TS)]
pub struct OpenEditorResponse {
    pub url: Option<String>,
}

#[derive(Debug, Serialize, TS)]
pub struct OpenEditorPathResponse {
    pub workspace_path: String,
}

#[derive(Debug, Deserialize)]
pub struct OpenEditorPathQuery {
    file_path: Option<String>,
}

pub fn router() -> Router<DeploymentImpl> {
    Router::new()
        .route("/editor/path", get(get_workspace_editor_path))
        .route("/editor/open", post(open_workspace_in_editor))
        .route("/files/open", post(open_workspace_path))
        .route("/files/check", post(check_workspace_paths))
        .route("/agent/setup", post(run_agent_setup))
        .route("/github/cli/setup", post(gh_cli_setup_handler))
}

#[axum::debug_handler]
pub async fn run_agent_setup(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<RunAgentSetupRequest>,
) -> Result<ResponseJson<ApiResponse<RunAgentSetupResponse>>, ApiError> {
    // Worktree-disabled workspaces intentionally skip setup scripts; respond
    // with a no-op success rather than spawning a process.
    if !workspace.use_worktree {
        return Ok(ResponseJson(ApiResponse::success(RunAgentSetupResponse {})));
    }

    let executor_profile_id = payload.executor_profile_id;
    let config = ExecutorConfigs::get_cached();
    let coding_agent = config.get_coding_agent_or_default(&executor_profile_id);
    match coding_agent {
        CodingAgent::CursorAgent(_) => {
            cursor_setup::run_cursor_setup(&deployment, &workspace).await?;
        }
        CodingAgent::Codex(codex) => {
            codex_setup::run_codex_setup(&deployment, &workspace, &codex).await?;
        }
        _ => return Err(ApiError::Executor(ExecutorError::SetupHelperNotSupported)),
    }

    deployment
        .track_if_analytics_allowed(
            "agent_setup_script_executed",
            serde_json::json!({
                "executor_profile_id": executor_profile_id.to_string(),
                "workspace_id": workspace.id.to_string(),
            }),
        )
        .await;

    Ok(ResponseJson(ApiResponse::success(RunAgentSetupResponse {})))
}

pub async fn open_workspace_in_editor(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<OpenEditorRequest>,
) -> Result<ResponseJson<ApiResponse<OpenEditorResponse>>, ApiError> {
    let path = resolve_workspace_editor_path(&deployment, &workspace, payload.file_path.as_deref())
        .await?;

    let editor_config = {
        let config = deployment.config().read().await;
        let editor_type_str = payload.editor_type.as_deref();
        config.editor.with_override(editor_type_str)
    };

    match editor_config.open_file(path.as_path()).await {
        Ok(url) => {
            tracing::info!(
                "Opened editor for workspace {} at path: {}{}",
                workspace.id,
                path.display(),
                if url.is_some() { " (remote mode)" } else { "" }
            );

            deployment
                .track_if_analytics_allowed(
                    "task_attempt_editor_opened",
                    serde_json::json!({
                        "workspace_id": workspace.id.to_string(),
                        "editor_type": payload.editor_type.as_ref(),
                        "remote_mode": url.is_some(),
                    }),
                )
                .await;

            Ok(ResponseJson(ApiResponse::success(OpenEditorResponse {
                url,
            })))
        }
        Err(e) => {
            tracing::error!(
                "Failed to open editor for attempt {}: {:?}",
                workspace.id,
                e
            );
            Err(ApiError::EditorOpen(e))
        }
    }
}

#[derive(Deserialize, TS)]
pub struct OpenPathRequest {
    /// A path from the chat: absolute, or relative to the workspace or one of its repos.
    path: String,
    /// Show it in the file manager instead of opening it.
    #[serde(default)]
    reveal: bool,
}

#[derive(Debug, Serialize, TS)]
pub struct OpenPathResponse {
    pub path: String,
    pub revealed: bool,
}

/// Opens a file named in the chat with its default program, or shows a folder (or a file that could run code) in
/// the file manager. Relative paths are looked up in the workspace and its repos; the path must exist.
pub async fn open_workspace_path(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<OpenPathRequest>,
) -> Result<ResponseJson<ApiResponse<OpenPathResponse>>, ApiError> {
    let bases = path_bases(&deployment, &workspace).await?;
    let Some(found) = resolve_chat_path(&bases, &payload.path) else {
        return Err(ApiError::BadRequest(format!("no such file or folder: {}", payload.path)));
    };
    let found = dunce_like(found);
    let reveal = payload.reveal || found.is_dir() || utils::browser::is_runnable_path(&found);
    let result = if payload.reveal {
        utils::browser::reveal_path(&found).await
    } else {
        utils::browser::open_path(&found).await
    };
    result.map_err(|e| ApiError::BadRequest(format!("could not open {}: {e}", found.display())))?;
    tracing::info!(workspace_id = %workspace.id, path = %found.display(), reveal, "opened a path from the chat");
    Ok(ResponseJson(ApiResponse::success(OpenPathResponse {
        path: found.to_string_lossy().into_owned(),
        revealed: reveal,
    })))
}

#[derive(Deserialize, TS)]
pub struct CheckPathsRequest {
    paths: Vec<String>,
}

#[derive(Debug, Serialize, TS)]
pub struct CheckPathsResponse {
    /// The asked strings that name an existing file or folder.
    pub existing: Vec<String>,
}

/// Which of the paths named in the chat exist (so only those become clickable).
pub async fn check_workspace_paths(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<CheckPathsRequest>,
) -> Result<ResponseJson<ApiResponse<CheckPathsResponse>>, ApiError> {
    let bases = path_bases(&deployment, &workspace).await?;
    let existing = payload
        .paths
        .into_iter()
        .take(500)
        .filter(|p| p.len() <= 1024 && resolve_chat_path(&bases, p).is_some())
        .collect();
    Ok(ResponseJson(ApiResponse::success(CheckPathsResponse { existing })))
}

/// Folders a relative path from the chat may be relative to: each repo in the workspace, the workspace, each repo
/// folder itself (a workspace without a worktree runs there).
async fn path_bases(
    deployment: &DeploymentImpl,
    workspace: &Workspace,
) -> Result<Vec<PathBuf>, ApiError> {
    let container_ref = deployment
        .container()
        .ensure_container_exists(workspace)
        .await?;
    let base = PathBuf::from(&container_ref);
    let repos =
        WorkspaceRepo::find_repos_for_workspace(&deployment.db().pool, workspace.id).await?;
    let mut bases: Vec<PathBuf> = repos.iter().map(|r| base.join(&r.name)).collect();
    bases.push(base);
    bases.extend(repos.iter().map(|r| PathBuf::from(&r.path)));
    Ok(bases)
}

/// The existing file or folder a chat string names: absolute, or relative to one of `bases`. Quotes and a trailing
/// `:line` are dropped.
fn resolve_chat_path(bases: &[PathBuf], text: &str) -> Option<PathBuf> {
    let raw = text.trim().trim_matches(|c| c == '"' || c == '\'' || c == '`');
    if raw.is_empty() || raw.contains('\n') {
        return None;
    }
    // "file.ts:42" or "file.ts:42:7" -> "file.ts"
    let mut raw = raw;
    for _ in 0..2 {
        if let Some((head, tail)) = raw.rsplit_once(':')
            && !tail.is_empty()
            && tail.chars().all(|c| c.is_ascii_digit())
            && head.len() > 2
        {
            raw = head;
        }
    }
    let wanted = Path::new(raw);
    if wanted.is_absolute() {
        return wanted.exists().then(|| wanted.to_path_buf());
    }
    bases.iter().map(|b| b.join(wanted)).find(|p| p.exists())
}

/// Canonical form without the Windows `\\?\` prefix, which Explorer does not take.
fn dunce_like(path: PathBuf) -> PathBuf {
    match std::fs::canonicalize(&path) {
        Ok(p) => {
            let s = p.to_string_lossy();
            match s.strip_prefix(r"\\?\") {
                Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
                _ => p,
            }
        }
        Err(_) => path,
    }
}

pub async fn get_workspace_editor_path(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<OpenEditorPathQuery>,
) -> Result<ResponseJson<ApiResponse<OpenEditorPathResponse>>, ApiError> {
    let path =
        resolve_workspace_editor_path(&deployment, &workspace, query.file_path.as_deref()).await?;

    Ok(ResponseJson(ApiResponse::success(OpenEditorPathResponse {
        workspace_path: path.to_string_lossy().into_owned(),
    })))
}

async fn resolve_workspace_editor_path(
    deployment: &DeploymentImpl,
    workspace: &Workspace,
    file_path: Option<&str>,
) -> Result<PathBuf, ApiError> {
    let container_ref = deployment
        .container()
        .ensure_container_exists(workspace)
        .await?;
    deployment.container().touch(workspace).await?;

    let workspace_path = Path::new(&container_ref);
    let workspace_repos =
        WorkspaceRepo::find_repos_for_workspace(&deployment.db().pool, workspace.id).await?;
    let workspace_path = if workspace_repos.len() == 1 && file_path.is_none() {
        workspace_path.join(&workspace_repos[0].name)
    } else {
        workspace_path.to_path_buf()
    };

    Ok(if let Some(file_path) = file_path {
        workspace_path.join(file_path)
    } else {
        workspace_path
    })
}

#[axum::debug_handler]
pub async fn gh_cli_setup_handler(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<
    ResponseJson<ApiResponse<db::models::execution_process::ExecutionProcess, GhCliSetupError>>,
    ApiError,
> {
    // Worktree-disabled workspaces skip setup scripts; report as unsupported
    // rather than spawning a process.
    if !workspace.use_worktree {
        return Ok(ResponseJson(ApiResponse::error_with_data(
            GhCliSetupError::SetupHelperNotSupported,
        )));
    }

    match super::gh_cli_setup::run_gh_cli_setup(&deployment, &workspace).await {
        Ok(execution_process) => {
            deployment
                .track_if_analytics_allowed(
                    "gh_cli_setup_executed",
                    serde_json::json!({
                        "workspace_id": workspace.id.to_string(),
                    }),
                )
                .await;

            Ok(ResponseJson(ApiResponse::success(execution_process)))
        }
        Err(ApiError::Executor(executors::executors::ExecutorError::ExecutableNotFound {
            program,
        })) if program == "brew" => Ok(ResponseJson(ApiResponse::error_with_data(
            GhCliSetupError::BrewMissing,
        ))),
        Err(ApiError::Executor(ExecutorError::SetupHelperNotSupported)) => Ok(ResponseJson(
            ApiResponse::error_with_data(GhCliSetupError::SetupHelperNotSupported),
        )),
        Err(ApiError::Executor(err)) => Ok(ResponseJson(ApiResponse::error_with_data(
            GhCliSetupError::Other {
                message: err.to_string(),
            },
        ))),
        Err(err) => Err(err),
    }
}
