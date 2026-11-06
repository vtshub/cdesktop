pub mod queue;
pub mod review;

use axum::{
    Extension, Json, Router,
    extract::{Query, State},
    http::HeaderMap,
    middleware::from_fn_with_state,
    response::Json as ResponseJson,
    routing::{get, post},
};
use db::models::{
    coding_agent_turn::{CodingAgentTurn, TurnSelection},
    execution_process::{ExecutionProcess, ExecutionProcessRunReason},
    provider::{AgentInjection, Provider},
    requests::UpdateSession,
    scratch::{Scratch, ScratchType},
    session::{CreateSession, Session, SessionError},
    workspace::{Workspace, WorkspaceError},
    workspace_repo::WorkspaceRepo,
};
use deployment::Deployment;
use executors::{
    actions::{
        ExecutorAction, ExecutorActionType, coding_agent_follow_up::CodingAgentFollowUpRequest,
    },
    profile::ExecutorConfig,
};
use serde::Deserialize;
use services::services::container::ContainerService;
use ts_rs::TS;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::{
    DeploymentImpl,
    error::ApiError,
    middleware::load_session_middleware,
    routes::{teammates::spawn_via_session, workspaces::execution::RunScriptError},
};

#[derive(Debug, Deserialize)]
pub struct SessionQuery {
    pub workspace_id: Uuid,
}

#[derive(Debug, Deserialize, TS)]
pub struct CreateSessionRequest {
    pub workspace_id: Uuid,
    pub executor: Option<String>,
    pub name: Option<String>,
}

pub async fn get_sessions(
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<SessionQuery>,
) -> Result<ResponseJson<ApiResponse<Vec<Session>>>, ApiError> {
    let pool = &deployment.db().pool;
    let sessions = Session::find_by_workspace_id(pool, query.workspace_id).await?;
    Ok(ResponseJson(ApiResponse::success(sessions)))
}

pub async fn get_session(
    Extension(session): Extension<Session>,
) -> Result<ResponseJson<ApiResponse<Session>>, ApiError> {
    Ok(ResponseJson(ApiResponse::success(session)))
}

pub async fn create_session(
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<CreateSessionRequest>,
) -> Result<ResponseJson<ApiResponse<Session>>, ApiError> {
    let pool = &deployment.db().pool;

    // Verify workspace exists
    let _workspace = Workspace::find_by_id(pool, payload.workspace_id)
        .await?
        .ok_or(ApiError::Workspace(WorkspaceError::ValidationError(
            "Workspace not found".to_string(),
        )))?;

    let session = Session::create(
        pool,
        &CreateSession {
            executor: payload.executor,
            name: payload.name,
        },
        Uuid::new_v4(),
        payload.workspace_id,
    )
    .await?;

    Ok(ResponseJson(ApiResponse::success(session)))
}

pub async fn update_session(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<UpdateSession>,
) -> Result<ResponseJson<ApiResponse<Session>>, ApiError> {
    let pool = &deployment.db().pool;

    Session::update(pool, session.id, request.name.as_deref()).await?;

    let updated = Session::find_by_id(pool, session.id)
        .await?
        .ok_or(ApiError::Session(SessionError::NotFound))?;

    Ok(ResponseJson(ApiResponse::success(updated)))
}

pub async fn delete_session(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<()>>, ApiError> {
    let pool = &deployment.db().pool;
    Session::delete(pool, session.id).await?;
    Ok(ResponseJson(ApiResponse::success(())))
}

#[derive(Debug, Deserialize, TS)]
pub struct CreateFollowUpAttempt {
    pub prompt: String,
    /// Executor + model + permission + reasoning config for this turn.
    /// Optional — when omitted the server inherits the recipient session's
    /// last config via `ExecutionProcess::latest_executor_config_for_session`.
    /// Frontend callers still send a full config; the `cdesktop team send`
    /// CLI omits it so peers don't accidentally swap a teammate's model
    /// mid-stream.
    #[serde(default)]
    #[ts(optional)]
    pub executor_config: Option<ExecutorConfig>,
    pub retry_process_id: Option<Uuid>,
    pub force_when_dirty: Option<bool>,
    pub perform_git_reset: Option<bool>,
    /// Optional branch to check out in worktree-disabled mode before spawning.
    #[serde(default)]
    #[ts(optional)]
    pub branch: Option<String>,
    /// When `true` with `branch`, runs `git checkout -b <branch>` (create new branch).
    #[serde(default)]
    #[ts(optional)]
    pub create_new_branch: Option<bool>,
    /// Provider to route this message through. `None` = inherit from the
    /// recipient's last execution (same fallback as `executor_config`).
    #[serde(default)]
    #[ts(optional)]
    pub selected_provider_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, TS)]
pub struct ResetProcessRequest {
    pub process_id: Uuid,
    pub force_when_dirty: Option<bool>,
    pub perform_git_reset: Option<bool>,
}

pub async fn follow_up(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
    headers: HeaderMap,
    Json(mut payload): Json<CreateFollowUpAttempt>,
) -> Result<ResponseJson<ApiResponse<ExecutionProcess>>, ApiError> {
    let pool = &deployment.db().pool;

    // `cdesktop team send` sets this header so the server can attribute the
    // peer message in telemetry. The UI omits the header (its sends are
    // not part of the team-coordination flow).
    let team_from_session: Option<Uuid> = headers
        .get("x-cdesktop-from-session")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());

    // Load workspace from session
    let mut workspace = Workspace::find_by_id(pool, session.workspace_id)
        .await?
        .ok_or(ApiError::Workspace(WorkspaceError::ValidationError(
            "Workspace not found".to_string(),
        )))?;

    tracing::info!("{:?}", workspace);

    // Worktree-disabled mode: optionally checkout a branch in the real repo,
    // then record the resulting HEAD into workspace.branch. User is trusted
    // with their own working tree — no dirty-tree pre-check.
    if !workspace.use_worktree {
        let repos = WorkspaceRepo::find_repos_for_workspace(pool, workspace.id).await?;
        if let Some(repo) = repos.first()
            && repo.is_git
        {
            let git = deployment.git();
            if let Some(branch) = payload.branch.as_deref() {
                let create_new = payload.create_new_branch.unwrap_or(false);
                // Skip the checkout when the requested branch already matches
                // the current HEAD and we're not creating a new branch —
                // matches the "dirty tree, selected branch == HEAD" scenario.
                let current = git.get_current_branch(&repo.path).ok();
                let already_on_branch = !create_new && current.as_deref() == Some(branch);
                if !already_on_branch {
                    git.checkout_branch(&repo.path, branch, create_new)
                        .map_err(|e| {
                            ApiError::Workspace(WorkspaceError::ValidationError(e.to_string()))
                        })?;
                }
            }
            if let Ok(current_branch) = git.get_current_branch(&repo.path)
                && current_branch != workspace.branch
            {
                Workspace::update_branch_name(pool, workspace.id, &current_branch).await?;
                workspace.branch = current_branch;
            }
        }
    }

    deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;

    // Inherit executor_config + provider from the recipient's last execution
    // when the caller omits them — used by the `cdesktop team send` CLI so a
    // peer cannot accidentally swap a teammate's model mid-stream. Frontend
    // callers always send a config, so the inheritance path is a no-op for
    // them.
    let inherited_config_provider =
        if payload.executor_config.is_none() || payload.selected_provider_id.is_none() {
            ExecutionProcess::latest_executor_config_for_session(pool, session.id).await?
        } else {
            None
        };

    if payload.executor_config.is_none() {
        payload.executor_config = inherited_config_provider
            .as_ref()
            .map(|(cfg, _)| cfg.clone());
    }
    if payload.selected_provider_id.is_none() {
        payload.selected_provider_id = inherited_config_provider
            .as_ref()
            .and_then(|(_, pid)| pid.as_deref().and_then(|s| s.parse().ok()));
    }

    // A chat stays on the provider it started with. Each provider can point
    // the CLI at its own config dir and endpoint (e.g. a local model with
    // CLAUDE_CONFIG_DIR), so resuming a session under another provider fails
    // with "No conversation found" or sends the chat to the wrong account.
    // A browser that never picked a model for this chat shows the fallback
    // (Opus), so the picker alone cannot be trusted here.
    if let Some((prev_cfg, Some(prev_pid))) =
        ExecutionProcess::latest_executor_config_for_session(pool, session.id).await?
        && let Ok(prev_provider) = prev_pid.parse::<Uuid>()
        && payload.selected_provider_id != Some(prev_provider)
    {
        tracing::warn!(
            session_id = %session.id,
            requested = ?payload.selected_provider_id,
            kept = %prev_provider,
            "follow-up asked for another provider; keeping the chat's provider"
        );
        payload.selected_provider_id = Some(prev_provider);
        if let Some(cfg) = payload.executor_config.as_mut() {
            cfg.model_id = prev_cfg.model_id.clone();
        }
    }

    let mut executor_config = payload.executor_config.ok_or_else(|| {
        ApiError::BadRequest(
            "executor_config required: session has no prior execution to inherit from".into(),
        )
    })?;
    let executor_profile_id = executor_config.profile_id();

    // Validate executor matches session if session has prior executions
    let expected_executor: Option<String> =
        ExecutionProcess::latest_executor_profile_for_session(pool, session.id)
            .await?
            .map(|profile| profile.executor.to_string())
            .or_else(|| session.executor.clone());

    if let Some(expected) = expected_executor {
        let actual = executor_profile_id.executor.to_string();
        if expected != actual {
            return Err(ApiError::Session(SessionError::ExecutorMismatch {
                expected,
                actual,
            }));
        }
    }

    if session.executor.is_none() {
        Session::update_executor(pool, session.id, &executor_profile_id.executor.to_string())
            .await?;
    }

    if let Some(proc_id) = payload.retry_process_id {
        let force_when_dirty = payload.force_when_dirty.unwrap_or(false);
        let perform_git_reset = payload.perform_git_reset.unwrap_or(true);
        deployment
            .container()
            .reset_session_to_process(session.id, proc_id, perform_git_reset, force_when_dirty)
            .await?;
    }

    let latest_session_info = CodingAgentTurn::find_latest_session_info(pool, session.id).await?;

    let prompt = payload.prompt;
    let prompt_byte_count = prompt.len();

    let working_dir = session
        .agent_working_dir
        .as_ref()
        .filter(|dir| !dir.is_empty())
        .cloned();

    // Resolve the provider up front so we can both prefix the OpenCode model
    // id (see `Provider::prefix_opencode_model_id`) before the action_type is
    // built AND reuse the loaded record to build the spawn injection.
    // TODO(phase-G): map ProviderError variants to a structured ApiError code
    // (e.g. PROVIDER_MISSING_API_KEY) so the picker can render a "configure
    // API key for this provider" CTA instead of a generic 400.
    let resolved_provider = if let Some(provider_id) = payload.selected_provider_id {
        let provider = Provider::find_by_id(pool, provider_id)
            .await
            .map_err(|_| ApiError::BadRequest(format!("Provider '{provider_id}' not found")))?;

        if !provider.enabled {
            return Err(ApiError::BadRequest(format!(
                "Provider '{}' is disabled",
                provider.name
            )));
        }

        if let Some(m) = executor_config.model_id.as_deref() {
            executor_config.model_id =
                Some(provider.prefix_opencode_model_id(executor_config.executor, m));
        }

        Some(provider)
    } else {
        None
    };

    let action_type = if let Some(info) = latest_session_info {
        let is_reset = payload.retry_process_id.is_some();
        ExecutorActionType::CodingAgentFollowUpRequest(CodingAgentFollowUpRequest {
            prompt: prompt.clone(),
            session_id: info.session_id,
            reset_to_message_id: if is_reset { info.message_id } else { None },
            executor_config: executor_config.clone(),
            working_dir: working_dir.clone(),
        })
    } else {
        ExecutorActionType::CodingAgentInitialRequest(
            executors::actions::coding_agent_initial::CodingAgentInitialRequest {
                prompt,
                executor_config: executor_config.clone(),
                working_dir,
            },
        )
    };

    // Build the spawn-time provider injection if a provider was selected for
    // this message. `Provider::build_agent_injection` dispatches per agent —
    // Codex emits env + ThreadStartParams overrides; every other agent uses
    // env-only.
    let injection = if let Some(provider) = resolved_provider {
        let model_id = executor_config.model_id.as_deref().unwrap_or("");
        provider
            .build_agent_injection(executor_config.executor, model_id)
            .map_err(|e| ApiError::BadRequest(e.to_string()))?
    } else {
        AgentInjection::default()
    };

    let selected_provider_id_str = payload.selected_provider_id.map(|id| id.to_string());
    let selected_model_id_str = executor_config.model_id.clone();

    let action = {
        let mut a = ExecutorAction::new(action_type, None);
        if let Some(env) = injection.env {
            a = a.with_provider_env(env);
        }
        if let Some(codex) = injection.codex {
            a = a.with_provider_codex(codex);
        }
        a.with_provider_selection(selected_provider_id_str, selected_model_id_str)
    };

    let execution_process = deployment
        .container()
        .start_execution(
            &workspace,
            &session,
            &action,
            &ExecutionProcessRunReason::CodingAgent,
        )
        .await?;

    // Clear the draft follow-up scratch on successful spawn
    // This ensures the scratch is wiped even if the user navigates away quickly
    if let Err(e) = Scratch::delete(pool, session.id, &ScratchType::DraftFollowUp).await {
        // Log but don't fail the request - scratch deletion is best-effort
        tracing::debug!(
            "Failed to delete draft follow-up scratch for session {}: {}",
            session.id,
            e
        );
    }

    // Peer-send telemetry. Only emitted when the `cdesktop team send` CLI
    // tagged the request with the X-Cdesktop-From-Session header — UI
    // follow-ups go untracked here (in MVP, the UI never sends to a peer;
    // it switches the active pill and the user types into that session).
    if let Some(from_id) = team_from_session {
        deployment
            .track_if_analytics_allowed(
                "team_message_sent",
                serde_json::json!({
                    "workspace_id": session.workspace_id.to_string(),
                    "source": "cli",
                    "from_session": from_id.to_string(),
                    "to_session": session.id.to_string(),
                    "byte_count": prompt_byte_count,
                }),
            )
            .await;
    }

    Ok(ResponseJson(ApiResponse::success(execution_process)))
}

pub async fn get_turn_selections(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<Vec<TurnSelection>>>, ApiError> {
    let pool = &deployment.db().pool;
    let selections = CodingAgentTurn::turn_selections_for_session(pool, session.id)
        .await
        .map_err(ApiError::Database)?;
    Ok(ResponseJson(ApiResponse::success(selections)))
}

pub async fn reset_process(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<ResetProcessRequest>,
) -> Result<ResponseJson<ApiResponse<()>>, ApiError> {
    let force_when_dirty = payload.force_when_dirty.unwrap_or(false);
    let perform_git_reset = payload.perform_git_reset.unwrap_or(true);

    deployment
        .container()
        .reset_session_to_process(
            session.id,
            payload.process_id,
            perform_git_reset,
            force_when_dirty,
        )
        .await?;

    Ok(ResponseJson(ApiResponse::success(())))
}

pub async fn run_setup_script(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<ExecutionProcess, RunScriptError>>, ApiError> {
    let pool = &deployment.db().pool;

    let workspace = Workspace::find_by_id(pool, session.workspace_id)
        .await?
        .ok_or(ApiError::Workspace(WorkspaceError::ValidationError(
            "Workspace not found".to_string(),
        )))?;

    // Worktree-disabled workspaces run in the user's real repo, which already
    // has its environment set up. Skip setup-script execution entirely.
    if !workspace.use_worktree {
        return Ok(ResponseJson(ApiResponse::error_with_data(
            RunScriptError::NoScriptConfigured,
        )));
    }

    if ExecutionProcess::has_running_non_dev_server_processes_for_workspace(pool, workspace.id)
        .await?
    {
        return Ok(ResponseJson(ApiResponse::error_with_data(
            RunScriptError::ProcessAlreadyRunning,
        )));
    }

    deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;

    let repos = WorkspaceRepo::find_repos_for_workspace(pool, workspace.id).await?;
    let executor_action = match deployment.container().setup_actions_for_repos(&repos) {
        Some(action) => action,
        None => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                RunScriptError::NoScriptConfigured,
            )));
        }
    };

    let execution_process = deployment
        .container()
        .start_execution(
            &workspace,
            &session,
            &executor_action,
            &ExecutionProcessRunReason::SetupScript,
        )
        .await?;

    deployment
        .track_if_analytics_allowed(
            "setup_script_executed",
            serde_json::json!({
                "workspace_id": workspace.id.to_string(),
            }),
        )
        .await;

    Ok(ResponseJson(ApiResponse::success(execution_process)))
}

pub fn router(deployment: &DeploymentImpl) -> Router<DeploymentImpl> {
    let session_id_router = Router::new()
        .route(
            "/",
            get(get_session).put(update_session).delete(delete_session),
        )
        .route("/follow-up", post(follow_up))
        .route("/turn-selections", get(get_turn_selections))
        .route("/reset", post(reset_process))
        .route("/setup", post(run_setup_script))
        .route("/review", post(review::start_review))
        .route("/teammates", post(spawn_via_session))
        .layer(from_fn_with_state(
            deployment.clone(),
            load_session_middleware,
        ));

    let sessions_router = Router::new()
        .route("/", get(get_sessions).post(create_session))
        .nest("/{session_id}", session_id_router)
        .nest("/{session_id}/queue", queue::router(deployment));

    Router::new().nest("/sessions", sessions_router)
}
