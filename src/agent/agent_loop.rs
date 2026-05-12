//! Main agent loop.
//!
//! Contains the `Agent` struct, `AgentDeps`, and the core event loop (`run`).
//! The heavy lifting is delegated to sibling modules:
//!
//! - `dispatcher` - Tool dispatch (agentic loop, tool execution)
//! - `commands` - System commands and job handlers
//! - `thread_ops` - Thread/session operations (user input, undo, approval, persistence)

use std::collections::HashSet;
use std::sync::Arc;

use base64::Engine as _;
use futures::StreamExt;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::agent::compaction::ContextCompactor;
use crate::agent::context_monitor::ContextMonitor;
use crate::agent::heartbeat::{spawn_heartbeat, spawn_multi_user_heartbeat};
use crate::agent::routine_engine::{RoutineEngine, spawn_cron_ticker};
use crate::agent::self_repair::{DefaultSelfRepair, RepairResult, SelfRepair};
use crate::agent::session::{PendingApproval, Session, ThreadState};
use crate::agent::session_manager::SessionManager;
use crate::agent::submission::{Submission, SubmissionParser, SubmissionResult};
use crate::agent::{HeartbeatConfig as AgentHeartbeatConfig, MessageIntent, Router, Scheduler};
use crate::channels::{ChannelManager, IncomingMessage, OutgoingResponse, StatusUpdate};
use crate::config::{AgentConfig, HeartbeatConfig, MemoryFlushConfig, RoutineConfig};
use crate::context::{ContextManager, JobContext};
use crate::db::Database;
use crate::error::{ChannelError, Error};
use crate::extensions::ExtensionManager;
use crate::generated_images::GeneratedImageSentinel;
use crate::hooks::HookRegistry;
use crate::llm::{ChatMessage, LlmProvider, Reasoning, ReasoningContext, RespondResult};
use crate::safety::SafetyLayer;
use crate::tools::ToolRegistry;
use crate::workspace::Workspace;
use ironclaw_llm::LlmProvider;
use ironclaw_safety::SafetyLayer;
use ironclaw_skills::SkillRegistry;

/// Result of the agentic loop execution.
pub(super) enum AgenticLoopResult {
    /// Completed with a response.
    Response(String),
    /// A tool requires approval before continuing.
    NeedApproval {
        /// The pending approval request to store.
        pending: PendingApproval,
    },
}

/// Channels that represent main/direct sessions (not group chats).
/// Only in these do we include MEMORY.md in the system prompt for privacy.
const MAIN_SESSION_CHANNELS: &[&str] = &["cli", "repl", "web", "gateway", "tui"];

fn is_main_session(channel: &str) -> bool {
    MAIN_SESSION_CHANNELS.contains(&channel)
}

/// Ensure thread.metadata is a JSON object so we can insert keys.
fn ensure_metadata_object(thread: &mut crate::agent::session::Thread) {
    if !thread.metadata.is_object() {
        thread.metadata = serde_json::Value::Object(serde_json::Map::new());
    }
}

/// Collapse a tool output string into a single-line preview for display.
pub(crate) fn truncate_for_preview(output: &str, max_chars: usize) -> String {
    let collapsed: String = output
        .chars()
        .take(max_chars + 50)
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // char_indices gives us byte offsets at char boundaries, so the slice is always valid UTF-8.
    if collapsed.chars().count() > max_chars {
        let byte_offset = collapsed
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(collapsed.len());
        format!("{}...", &collapsed[..byte_offset])
    } else {
        collapsed
    }
}

/// Determine whether a bare-keyword `ApprovalResponse` should be kept as an
/// approval or downgraded to regular user input.
///
/// Returns `true` when the message should be routed as an approval (there IS
/// a pending approval or it's an explicit slash command). Returns `false`
/// when the message should be treated as regular `UserInput`.
///
/// Used by the legacy routing path; the engine_v2 path performs an equivalent
/// check earlier (before the BeforeInbound hook).
fn should_route_as_approval(thread_state: ThreadState, raw_content: &str) -> bool {
    thread_state == ThreadState::AwaitingApproval || raw_content.trim().starts_with('/')
}

#[cfg(test)]
fn resolve_routine_notification_user(metadata: &serde_json::Value) -> Option<String> {
    resolve_owner_scope_notification_user(
        metadata.get("notify_user").and_then(|value| value.as_str()),
        metadata.get("owner_id").and_then(|value| value.as_str()),
    )
}

fn trimmed_option(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn resolve_owner_scope_notification_user(
    explicit_user: Option<&str>,
    owner_fallback: Option<&str>,
) -> Option<String> {
    trimmed_option(explicit_user).or_else(|| trimmed_option(owner_fallback))
}

fn is_single_message_repl(message: &IncomingMessage) -> bool {
    message.channel == "repl"
        && message
            .metadata
            .get("single_message_mode")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
}

fn extension_for_image_media_type(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

fn generated_image_attachment_from_data_url(
    data_url: &str,
    fallback_media_type: Option<&str>,
    index: usize,
) -> Option<OutgoingAttachment> {
    let (metadata, encoded) = data_url.split_once(',')?;
    let header = metadata.strip_prefix("data:")?;
    if !header
        .split(';')
        .any(|part| part.eq_ignore_ascii_case("base64"))
    {
        return None;
    }

    let media_type = header
        .split(';')
        .next()
        .filter(|value| value.starts_with("image/"))
        .or(fallback_media_type)
        .unwrap_or("image/png");
    if !media_type.starts_with("image/") {
        return None;
    }

    let data = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    if data.is_empty() {
        return None;
    }

    Some(OutgoingAttachment {
        filename: format!(
            "generated-image-{}.{}",
            index + 1,
            extension_for_image_media_type(media_type)
        ),
        mime_type: media_type.to_string(),
        data,
    })
}

fn generated_image_attachments_for_turn(
    turn: &crate::agent::session::Turn,
) -> Vec<OutgoingAttachment> {
    let mut seen = HashSet::new();
    let mut attachments = Vec::new();

    for (index, tool_call) in turn.tool_calls.iter().enumerate() {
        let Some(result) = tool_call.result.as_ref() else {
            continue;
        };
        let Some(sentinel) = GeneratedImageSentinel::from_value(result) else {
            continue;
        };
        let Some(data_url) = sentinel
            .data_url()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };

        if !seen.insert(data_url.to_string()) {
            continue;
        }

        match generated_image_attachment_from_data_url(data_url, sentinel.media_type(), index) {
            Some(attachment) => attachments.push(attachment),
            None => tracing::warn!("Generated image data URL could not be decoded for attachment"),
        }
    }

    attachments
}

async fn build_outgoing_response_for_thread(
    session: &Arc<tokio::sync::Mutex<crate::agent::session::Session>>,
    thread_id: Uuid,
    content: impl Into<String>,
) -> OutgoingResponse {
    let mut response = OutgoingResponse::text(content);
    let attachments = {
        let sess = session.lock().await;
        sess.threads
            .get(&thread_id)
            .and_then(|thread| thread.last_turn())
            .map(generated_image_attachments_for_turn)
            .unwrap_or_default()
    };

    if !attachments.is_empty() {
        response = response.with_inline_attachments(attachments);
    }

    response
}

async fn resolve_channel_notification_user(
    extension_manager: Option<&Arc<ExtensionManager>>,
    channel: Option<&str>,
    explicit_user: Option<&str>,
    owner_fallback: Option<&str>,
) -> Option<String> {
    if let Some(user) = trimmed_option(explicit_user) {
        return Some(user);
    }

    if let Some(channel_name) = trimmed_option(channel)
        && let Some(extension_manager) = extension_manager
        && let Some(target) = extension_manager
            .notification_target_for_channel(&channel_name)
            .await
    {
        return Some(target);
    }

    resolve_owner_scope_notification_user(explicit_user, owner_fallback)
}

async fn resolve_routine_notification_target(
    extension_manager: Option<&Arc<ExtensionManager>>,
    metadata: &serde_json::Value,
) -> Option<String> {
    resolve_channel_notification_user(
        extension_manager,
        metadata
            .get("notify_channel")
            .and_then(|value| value.as_str()),
        metadata.get("notify_user").and_then(|value| value.as_str()),
        metadata.get("owner_id").and_then(|value| value.as_str()),
    )
    .await
}

pub(crate) fn chat_tool_execution_metadata(message: &IncomingMessage) -> serde_json::Value {
    serde_json::json!({
        "notify_channel": message.channel,
        "notify_user": message
            .routing_target()
            .unwrap_or_else(|| message.user_id.clone()),
        "notify_thread_id": message.thread_id,
        "notify_metadata": message.metadata,
    })
}

fn should_fallback_routine_notification(error: &ChannelError) -> bool {
    !matches!(error, ChannelError::MissingRoutingTarget { .. })
}

/// Core dependencies for the agent.
///
/// Bundles the shared components to reduce argument count.
pub struct AgentDeps {
    /// Resolved durable owner scope for the instance.
    pub owner_id: String,
    pub store: Option<Arc<dyn Database>>,
    /// Cached settings store. When set, `TenantScope` routes settings reads
    /// through this cache instead of hitting the raw `Database` directly.
    pub settings_store: Option<Arc<dyn crate::db::SettingsStore + Send + Sync>>,
    pub llm: Arc<dyn LlmProvider>,
    /// Cheap/fast LLM for lightweight tasks (heartbeat, routing, evaluation).
    /// Falls back to the main `llm` if None.
    pub cheap_llm: Option<Arc<dyn LlmProvider>>,
    pub safety: Arc<SafetyLayer>,
    pub tools: Arc<ToolRegistry>,
    pub workspace: Option<Arc<Workspace>>,
    pub extension_manager: Option<Arc<ExtensionManager>>,
    /// Learning repository for evidence-backed learnings (Phase 6b).
    pub learning_repo: Option<Arc<crate::workspace::learnings::LearningRepository>>,
    pub hooks: Arc<HookRegistry>,
    pub auth_manager: Option<Arc<crate::auth::extension::AuthManager>>,
    /// Cost enforcement guardrails (daily budget, hourly rate limits).
    pub cost_guard: Arc<crate::agent::cost_guard::CostGuard>,
    /// SSE manager for live job event streaming to the web gateway.
    pub sse_tx: Option<Arc<crate::channels::web::sse::SseManager>>,
    /// HTTP interceptor for trace recording/replay.
    pub http_interceptor: Option<Arc<dyn ironclaw_llm::recording::HttpInterceptor>>,
    /// Audio transcription middleware for voice messages.
    pub transcription: Option<Arc<ironclaw_llm::transcription::TranscriptionMiddleware>>,
    /// Document text extraction middleware for PDF, DOCX, PPTX, etc.
    pub document_extraction: Option<Arc<crate::document_extraction::DocumentExtractionMiddleware>>,
    /// Sandbox readiness state for full-job routine dispatch.
    pub sandbox_readiness: crate::agent::routine_engine::SandboxReadiness,
    /// Software builder for self-repair tool rebuilding.
    pub builder: Option<Arc<dyn crate::tools::SoftwareBuilder>>,
    /// Resolved LLM backend identifier (e.g., "nearai", "openai", "groq").
    /// Used by `/model` persistence to determine which env var to update.
    pub llm_backend: String,
    /// Per-tenant rate limiting registry (lazily creates rate state per user).
    pub tenant_rates: Arc<crate::tenant::TenantRateRegistry>,
}

/// The main agent that coordinates all components.
pub struct Agent {
    pub(super) config: AgentConfig,
    pub(crate) deps: AgentDeps,
    pub(crate) channels: Arc<ChannelManager>,
    pub(super) context_manager: Arc<ContextManager>,
    pub(super) scheduler: Arc<Scheduler>,
    pub(super) router: Router,
    pub(super) session_manager: Arc<SessionManager>,
    pub(super) context_monitor: ContextMonitor,
    pub(super) heartbeat_config: Option<HeartbeatConfig>,
    pub(super) hygiene_config: Option<crate::config::HygieneConfig>,
    pub(super) routine_config: Option<RoutineConfig>,
    /// Shared routine-engine slot used for internal event matching and for exposing
    /// the engine to gateway/manual trigger entry points.
    pub(super) routine_engine_slot:
        Arc<tokio::sync::RwLock<Option<Arc<crate::agent::routine_engine::RoutineEngine>>>>,
    /// Engine v2 mission manager for firing learning missions (set after engine init).
    pub(crate) mission_manager_slot:
        Arc<tokio::sync::RwLock<Option<Arc<ironclaw_engine::MissionManager>>>>,
}

impl Agent {
    pub(super) fn owner_id(&self) -> &str {
        if let Some(workspace) = self.deps.workspace.as_ref() {
            debug_assert_eq!(
                workspace.user_id(),
                self.deps.owner_id,
                "workspace.user_id() must stay aligned with deps.owner_id"
            );
        }

        &self.deps.owner_id
    }

    /// Create a new agent.
    ///
    /// Optionally accepts pre-created `ContextManager` and `SessionManager` for sharing
    /// with external components (job tools, web gateway). Creates new ones if not provided.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: AgentConfig,
        deps: AgentDeps,
        channels: Arc<ChannelManager>,
        heartbeat_config: Option<HeartbeatConfig>,
        hygiene_config: Option<crate::config::HygieneConfig>,
        routine_config: Option<RoutineConfig>,
        context_manager: Option<Arc<ContextManager>>,
        session_manager: Option<Arc<SessionManager>>,
    ) -> Self {
        let context_manager = context_manager
            .unwrap_or_else(|| Arc::new(ContextManager::new(config.max_parallel_jobs)));

        let session_manager = session_manager.unwrap_or_else(|| Arc::new(SessionManager::new()));

        let mut scheduler = Scheduler::new(
            config.clone(),
            context_manager.clone(),
            deps.llm.clone(),
            deps.safety.clone(),
            SchedulerDeps {
                tools: deps.tools.clone(),
                extension_manager: deps.extension_manager.clone(),
                store: deps
                    .store
                    .as_ref()
                    .map(|db| crate::tenant::SystemScope::new(Arc::clone(db))),
                hooks: deps.hooks.clone(),
            },
        );
        if let Some(ref sse) = deps.sse_tx {
            scheduler.set_sse_sender(Arc::clone(sse));
        }
        if let Some(ref interceptor) = deps.http_interceptor {
            scheduler.set_http_interceptor(Arc::clone(interceptor));
        }
        let scheduler = Arc::new(scheduler);

        Self {
            config,
            deps,
            channels,
            context_manager,
            scheduler,
            router: Router::new(),
            session_manager,
            context_monitor: ContextMonitor::new(),
            heartbeat_config,
            hygiene_config,
            routine_config,
            routine_engine_slot: Arc::new(tokio::sync::RwLock::new(None)),
            mission_manager_slot: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    /// Replace the routine-engine slot with a shared one so the gateway and
    /// agent reference the same engine.
    pub fn set_routine_engine_slot(
        &mut self,
        slot: Arc<tokio::sync::RwLock<Option<Arc<crate::agent::routine_engine::RoutineEngine>>>>,
    ) {
        self.routine_engine_slot = slot;
    }

    pub(super) async fn routine_engine(
        &self,
    ) -> Option<Arc<crate::agent::routine_engine::RoutineEngine>> {
        self.routine_engine_slot.read().await.clone()
    }

    /// Set the engine v2 mission manager (called after engine init).
    pub async fn set_mission_manager(&self, mgr: Arc<ironclaw_engine::MissionManager>) {
        *self.mission_manager_slot.write().await = Some(mgr);
    }

    pub(crate) async fn mission_manager(&self) -> Option<Arc<ironclaw_engine::MissionManager>> {
        self.mission_manager_slot.read().await.clone()
    }

    // Convenience accessors

    /// Get the scheduler (for external wiring, e.g. CreateJobTool).
    pub fn scheduler(&self) -> Arc<Scheduler> {
        Arc::clone(&self.scheduler)
    }

    pub(super) fn store(&self) -> Option<&Arc<dyn Database>> {
        self.deps.store.as_ref()
    }

    /// Send a response to the channel, then emit the terminal "Done" status.
    ///
    /// This ordering guarantees that the SSE client receives the assistant
    /// message before the turn-closing event, preventing the web UI from
    /// closing the turn before the message renders (see #2079).
    async fn respond_then_done(
        &self,
        message: &IncomingMessage,
        response: OutgoingResponse,
    ) -> Result<(), ChannelError> {
        let respond_result = self.channels.respond(message, response).await;
        // Always emit Done regardless of whether respond succeeded, so the
        // client knows the turn is over even when the response delivery fails.
        if let Err(e) = self
            .channels
            .send_status(
                &message.channel,
                StatusUpdate::Status("Done".into()),
                &message.metadata,
            )
            .await
        {
            tracing::warn!(
                channel = %message.channel,
                error = %e,
                "Failed to send Done status after response"
            );
        }
        respond_result
    }

    /// Emit the terminal "Done" status without sending a response first.
    ///
    /// Used by code paths that suppress the response (hook-blocked, empty
    /// response) but still need to close the turn for the client.
    async fn send_done(&self, message: &IncomingMessage) {
        if let Err(e) = self
            .channels
            .send_status(
                &message.channel,
                StatusUpdate::Status("Done".into()),
                &message.metadata,
            )
            .await
        {
            tracing::warn!(
                channel = %message.channel,
                error = %e,
                "Failed to send Done status"
            );
        }
    }

    pub(crate) fn llm(&self) -> &Arc<dyn LlmProvider> {
        &self.deps.llm
    }

    pub(crate) fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Get the cheap/fast LLM provider, falling back to the main one.
    pub(crate) fn cheap_llm(&self) -> &Arc<dyn LlmProvider> {
        self.deps.cheap_llm.as_ref().unwrap_or(&self.deps.llm)
    }

    pub(crate) fn safety(&self) -> &Arc<SafetyLayer> {
        &self.deps.safety
    }

    pub(crate) fn tools(&self) -> &Arc<ToolRegistry> {
        &self.deps.tools
    }

    pub(crate) fn workspace(&self) -> Option<&Arc<Workspace>> {
        self.deps.workspace.as_ref()
    }

    pub(crate) fn workspace_for_user(&self, user_id: &str) -> Option<Arc<Workspace>> {
        self.workspace().map(|ws| {
            if ws.user_id() == user_id {
                Arc::clone(ws)
            } else {
                Arc::new(ws.scoped_to_user(user_id))
            }
        })
    }

    pub(crate) fn hooks(&self) -> &Arc<HookRegistry> {
        &self.deps.hooks
    }

    /// Build platform metadata for self-awareness in system prompts.
    pub(crate) async fn platform_info(&self) -> ironclaw_engine::PlatformInfo {
        let active_channels = self.channels.channel_names().await;
        let database_backend = std::env::var("DATABASE_BACKEND")
            .ok()
            .or_else(|| self.deps.store.as_ref().map(|_| "postgres".to_string()));
        ironclaw_engine::PlatformInfo {
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            llm_backend: Some(self.deps.llm_backend.clone()),
            model_name: Some(self.deps.llm.active_model_name()),
            database_backend,
            active_channels,
            owner_id: Some(self.deps.owner_id.clone()),
            repo_url: Some("https://github.com/nearai/ironclaw".to_string()),
        }
    }
    /// Build a tenant-scoped execution context for the given user.
    ///
    /// This is the standard entry point for per-user operations. The returned
    /// [`TenantCtx`] provides a [`TenantScope`] that auto-binds `user_id` on
    /// every database operation and a per-user rate limiter.
    pub(super) async fn tenant_ctx(&self, user_id: &str) -> crate::tenant::TenantCtx {
        use crate::ownership::{UserId, UserRole};
        // Bridge: creates Regular identity from raw string.
        // Will be replaced by OwnershipCache lookup in Task 9.
        let identity = UserId::from_trusted(user_id.to_string(), UserRole::Regular);
        self.tenant_ctx_with_identity(identity).await
    }

    /// Build a tenant-scoped execution context from a resolved [`crate::ownership::UserId`].
    ///
    /// Preferred over [`tenant_ctx`](Self::tenant_ctx) once the call site has a
    /// full `UserId` available.
    pub(super) async fn tenant_ctx_with_identity(
        &self,
        identity: crate::ownership::UserId,
    ) -> crate::tenant::TenantCtx {
        let user_id = identity.as_str();
        let rate = self.deps.tenant_rates.get_or_create(user_id).await;

        let store = self.deps.store.as_ref().map(|db| {
            let scope = crate::tenant::TenantScope::with_identity(identity.clone(), Arc::clone(db));
            match &self.deps.settings_store {
                Some(ss) => scope.with_settings_store(Arc::clone(ss)),
                None => scope,
            }
        });

        // Reuse the owner workspace if user matches, otherwise create per-user.
        // Per-user workspaces are seeded on first creation so they get identity
        // files and BOOTSTRAP.md (which triggers the onboarding greeting).
        let workspace = match &self.deps.workspace {
            Some(ws) if ws.user_id() == user_id => Some(Arc::clone(ws)),
            _ => {
                if let Some(db) = self.deps.store.as_ref() {
                    let ws = Arc::new(Workspace::new_with_db(user_id, Arc::clone(db)));
                    if let Err(e) = ws.seed_if_empty().await {
                        tracing::warn!(
                            user_id = user_id,
                            "Failed to seed per-user workspace: {}",
                            e
                        );
                    }
                    Some(ws)
                } else {
                    None
                }
            }
        };

        crate::tenant::TenantCtx::new(
            identity,
            store,
            workspace,
            Arc::clone(&self.deps.cost_guard),
            rate,
        )
    }

    /// Get a system-scoped database accessor for cross-tenant operations.
    ///
    /// Only for system-level components (heartbeat, routine engine, self-repair,
    /// scheduler). Handler code should use [`tenant_ctx()`](Self::tenant_ctx) instead.
    pub(super) fn system_store(&self) -> Option<crate::tenant::SystemScope> {
        self.deps
            .store
            .as_ref()
            .map(|db| crate::tenant::SystemScope::new(Arc::clone(db)))
    }

    pub(super) fn skill_registry(&self) -> Option<&Arc<std::sync::RwLock<SkillRegistry>>> {
        self.deps.skill_registry.as_ref()
    }

    pub(super) fn skill_catalog(&self) -> Option<&Arc<ironclaw_skills::catalog::SkillCatalog>> {
        self.deps.skill_catalog.as_ref()
    }

    /// Select active skills for a message using deterministic prefiltering.
    /// Select skills for a message. Returns (active skills, rewritten message).
    ///
    /// Skills are selected in two ways:
    /// 1. **Explicit**: `/skill-name` in the message force-activates that skill.
    ///    The `/skill-name` is replaced with the skill's description so the
    ///    sentence reads naturally for the LLM.
    /// 2. **Implicit**: keyword/pattern scoring against the message content.
    ///
    /// One-time setup skills (`*-setup` persona bundles) declare a
    /// `setup_marker` workspace path in their activation frontmatter. Before
    /// scoring, we check the workspace for each distinct marker referenced
    /// by loaded skills and pass the satisfied set to the selector — any
    /// skill whose marker is present is excluded from candidates so it
    /// doesn't keep burning the activation budget after onboarding has
    /// already run. To re-trigger setup, delete the marker file.
    pub(super) async fn select_active_skills(
        &self,
        message_content: &str,
        user_id: &str,
    ) -> (Vec<ironclaw_skills::LoadedSkill>, String, Vec<String>) {
        let Some(registry) = self.skill_registry() else {
            return (vec![], message_content.to_string(), vec![]);
        };
        // Snapshot the skill list + distinct setup markers under the read
        // lock, then drop the guard before any await. The marker checks
        // and the prefilter call don't need the registry lock and we
        // shouldn't hold a poisonable RwLock across an await point.
        let (available, distinct_markers) = match registry.read() {
            Ok(guard) => {
                let skills_clone: Vec<ironclaw_skills::LoadedSkill> = guard.skills().to_vec();
                let mut markers: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                for s in &skills_clone {
                    if let Some(m) = &s.manifest.activation.setup_marker {
                        markers.insert(m.clone());
                    }
                }
                (skills_clone, markers)
            }
            Err(e) => {
                tracing::error!("Skill registry lock poisoned: {}", e);
                return (vec![], message_content.to_string(), vec![]);
            }
        };

        // Resolve which setup markers are satisfied by the current
        // workspace. A marker is "satisfied" iff its path exists.
        // Without a workspace, we conservatively treat all markers as
        // unsatisfied (setup skills can still activate). Errors checking
        // a marker are logged and treated as unsatisfied.
        let mut satisfied: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Some(scoped_ws) = self.workspace_for_user(user_id) {
            for marker in &distinct_markers {
                match scoped_ws.exists(marker).await {
                    Ok(true) => {
                        satisfied.insert(marker.clone());
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::debug!(
                            marker = %marker,
                            "setup-marker existence check failed (treating as unsatisfied): {e}"
                        );
                    }
                }
            }
        }

        // Phase 1: Extract explicit /skill-name mentions
        let (explicit, rewritten) =
            ironclaw_skills::extract_skill_mentions(message_content, &available);

        // Phase 2: Score-based selection on the rewritten message
        let skills_cfg = &self.deps.skills_config;
        let outcome = ironclaw_skills::prefilter_skills(
            &rewritten,
            &available,
            skills_cfg.max_active_skills,
            skills_cfg.max_context_tokens,
            &satisfied,
        );

        // Feedback notes: start with the selector's own notes (chain-load,
        // budget, marker-skipped companions) and prepend a note for each
        // explicit `/mention` force-activation so the UI can explain why
        // a skill loaded even when it didn't score.
        let mut feedback: Vec<String> = explicit
            .iter()
            .map(|s| format!("{}: force-activated via /mention", s.name()))
            .collect();
        feedback.extend(outcome.notes);

        // Merge: explicit mentions first, then scored (dedup by name)
        let mut selected: Vec<ironclaw_skills::LoadedSkill> =
            explicit.into_iter().cloned().collect();
        for skill in outcome.selected {
            if !selected
                .iter()
                .any(|s| s.manifest.name == skill.manifest.name)
            {
                selected.push(skill.clone());
            }
        }

        if !selected.is_empty() {
            tracing::debug!(
                "Selected {} skill(s) for message: {}",
                selected.len(),
                selected
                    .iter()
                    .map(|s| s.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        (selected, rewritten, feedback)
    }

    /// Send initial engine thread list and routines to the TUI channel so
    /// the sidebar is populated before the first user message.
    async fn hydrate_tui_sidebar(&self) {
        let empty_meta = serde_json::Value::Object(serde_json::Map::new());

        // Engine threads
        if self.config.engine_v2
            && let Ok(threads) = crate::bridge::list_engine_threads(None, self.owner_id()).await
        {
            let summaries: Vec<crate::channels::EngineThreadSummary> = threads
                .into_iter()
                .map(|t| crate::channels::EngineThreadSummary {
                    id: t.id,
                    goal: t.goal,
                    thread_type: t.thread_type,
                    state: t.state,
                    step_count: t.step_count,
                    total_tokens: t.total_tokens,
                    created_at: t.created_at,
                    updated_at: t.updated_at,
                })
                .collect();
            let _ = self
                .channels
                .send_status(
                    "tui",
                    StatusUpdate::EngineThreadList { threads: summaries },
                    &empty_meta,
                )
                .await;
        }

        // Routines
        if let Some(system) = self.system_store()
            && let Ok(routines) = system.list_all_routines().await
        {
            for routine in routines {
                let _ = self
                    .channels
                    .send_status(
                        "tui",
                        StatusUpdate::RoutineUpdate {
                            id: routine.id.to_string(),
                            name: routine.name.clone(),
                            trigger_type: format!("{:?}", routine.trigger),
                            enabled: routine.enabled,
                            last_run: routine.last_run_at.map(|t| t.to_rfc3339()),
                            next_fire: routine.next_fire_at.map(|t| t.to_rfc3339()),
                        },
                        &empty_meta,
                    )
                    .await;
            }
        }
    }

    /// Run the agent main loop.
    pub async fn run(self) -> Result<(), Error> {
        // Eagerly initialize engine v2 so gateway API endpoints can serve
        // data (projects, missions, threads) before the first chat message.
        if self.config.engine_v2
            && let Err(e) = crate::bridge::init_engine(&self).await
        {
            tracing::debug!("engine v2: eager init failed: {e}");
        }

        // Start channels
        let mut message_stream = self.channels.start_all().await?;

        // Start self-repair task with notification forwarding
        let mut self_repair = DefaultSelfRepair::new(
            self.context_manager.clone(),
            self.config.stuck_threshold,
            self.config.max_repair_attempts,
        );
        if let Some(system) = self.system_store() {
            self_repair = self_repair.with_store(system);
        }
        if let Some(ref builder) = self.deps.builder {
            self_repair = self_repair.with_builder(Arc::clone(builder), Arc::clone(self.tools()));
        }
        let repair = Arc::new(self_repair);
        let repair_interval = self.config.repair_check_interval;
        let repair_channels = self.channels.clone();
        let repair_owner_id = self.owner_id().to_string();
        let repair_handle = tokio::spawn(async move {
            // Track jobs that have already been escalated to ManualRequired
            // to prevent sending duplicate notifications every repair cycle.
            let mut notified_manual: std::collections::HashSet<uuid::Uuid> =
                std::collections::HashSet::new();

            loop {
                tokio::time::sleep(repair_interval).await;

                // Check stuck jobs
                let stuck_jobs = repair.detect_stuck_jobs().await;
                for job in stuck_jobs {
                    tracing::info!("Attempting to repair stuck job {}", job.job_id);
                    let result = repair.repair_stuck_job(&job).await;
                    let notification = match &result {
                        Ok(RepairResult::Success { message }) => {
                            tracing::info!("Repair succeeded: {}", message);
                            Some(format!(
                                "Job {} was stuck for {}s, recovery succeeded: {}",
                                job.job_id,
                                job.stuck_duration.as_secs(),
                                message
                            ))
                        }
                        Ok(RepairResult::Failed { message }) => {
                            tracing::error!("Repair failed: {}", message);
                            // Dedup: only notify once per job (same pattern as ManualRequired)
                            if notified_manual.insert(job.job_id) {
                                Some(format!(
                                    "Job {} was stuck for {}s, recovery failed permanently: {}",
                                    job.job_id,
                                    job.stuck_duration.as_secs(),
                                    message
                                ))
                            } else {
                                None
                            }
                        }
                        Ok(RepairResult::ManualRequired { message }) => {
                            tracing::warn!("Manual intervention needed: {}", message);
                            // Only notify once per job to prevent notification spam.
                            // The job should have been transitioned to Failed by
                            // repair_stuck_job, but guard against that failing too.
                            if notified_manual.insert(job.job_id) {
                                Some(format!(
                                    "Job {} needs manual intervention: {}",
                                    job.job_id, message
                                ))
                            } else {
                                None
                            }
                        }
                        Ok(RepairResult::Retry { message }) => {
                            tracing::warn!("Repair needs retry: {}", message);
                            None // Don't spam the user on retries
                        }
                        Err(e) => {
                            tracing::error!("Repair error: {}", e);
                            None
                        }
                    };

                    if let Some(msg) = notification {
                        let response = OutgoingResponse::text(format!("Self-Repair: {}", msg));
                        let _ = repair_channels
                            .broadcast_all(&repair_owner_id, response)
                            .await;
                    }
                }

                // Check broken tools
                let broken_tools = repair.detect_broken_tools().await;
                for tool in broken_tools {
                    tracing::info!("Attempting to repair broken tool: {}", tool.name);
                    match repair.repair_broken_tool(&tool).await {
                        Ok(RepairResult::Success { message }) => {
                            let response = OutgoingResponse::text(format!(
                                "Self-Repair: Tool '{}' repaired: {}",
                                tool.name, message
                            ));
                            let _ = repair_channels
                                .broadcast_all(&repair_owner_id, response)
                                .await;
                        }
                        Ok(result) => {
                            tracing::info!("Tool repair result: {:?}", result);
                        }
                        Err(e) => {
                            tracing::error!("Tool repair error: {}", e);
                        }
                    }
                }
            }
        });

        // Spawn session pruning task
        let session_mgr = self.session_manager.clone();
        let session_idle_timeout = self.config.session_idle_timeout;
        let pruning_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(600)); // Every 10 min
            interval.tick().await; // Skip immediate first tick
            loop {
                interval.tick().await;
                session_mgr.prune_stale_sessions(session_idle_timeout).await;
            }
        });

        // Spawn heartbeat if enabled
        let heartbeat_handle = if let Some(ref hb_config) = self.heartbeat_config {
            if hb_config.enabled {
                if let Some(workspace) = self.workspace() {
                    let mut config = AgentHeartbeatConfig::default()
                        .with_interval(std::time::Duration::from_secs(hb_config.interval_secs));
                    config.quiet_hours_start = hb_config.quiet_hours_start;
                    config.quiet_hours_end = hb_config.quiet_hours_end;
                    config.multi_tenant = hb_config.multi_tenant;
                    config.timezone = hb_config
                        .timezone
                        .clone()
                        .or_else(|| Some(self.config.default_timezone.clone()));
                    let heartbeat_notify_user = resolve_owner_scope_notification_user(
                        hb_config.notify_user.as_deref(),
                        Some(self.owner_id()),
                    );
                    if let Some(channel) = &hb_config.notify_channel
                        && let Some(user) = heartbeat_notify_user.as_deref()
                    {
                        config = config.with_notify(user, channel);
                    }

                    // Set up notification channel
                    let (notify_tx, mut notify_rx) =
                        tokio::sync::mpsc::channel::<OutgoingResponse>(16);

                    // Spawn notification forwarder that routes through channel manager
                    let notify_channel = hb_config.notify_channel.clone();
                    let notify_target = resolve_channel_notification_user(
                        self.deps.extension_manager.as_ref(),
                        hb_config.notify_channel.as_deref(),
                        hb_config.notify_user.as_deref(),
                        Some(self.owner_id()),
                    )
                    .await;
                    let notify_user = heartbeat_notify_user;
                    let channels = self.channels.clone();
                    let is_multi_tenant = hb_config.multi_tenant;
                    tokio::spawn(async move {
                        while let Some(response) = notify_rx.recv().await {
                            // In multi-tenant mode, extract the owning user_id from
                            // the response metadata so notifications reach the
                            // correct user rather than the agent's owner.
                            // This intentionally overrides the configured notify_target
                            // because each user's heartbeat should notify that user.
                            let effective_user = if is_multi_tenant {
                                response
                                    .metadata
                                    .get("owner_id")
                                    .and_then(|v| v.as_str())
                                    .map(String::from)
                            } else {
                                None
                            };

                            // Try the configured channel first, fall back to
                            // broadcasting on all channels.
                            let targeted_ok = if let Some(ref channel) = notify_channel {
                                let target = effective_user.as_deref().or(notify_target.as_deref());
                                if let Some(user) = target {
                                    channels
                                        .broadcast(channel, user, response.clone())
                                        .await
                                        .is_ok()
                                } else {
                                    false
                                }
                            } else {
                                false
                            };

                            if !targeted_ok {
                                let fallback = effective_user.as_deref().or(notify_user.as_deref());
                                if let Some(user) = fallback {
                                    let results = channels.broadcast_all(user, response).await;
                                    for (ch, result) in results {
                                        if let Err(e) = result {
                                            tracing::warn!(
                                                "Failed to broadcast heartbeat to {}: {}",
                                                ch,
                                                e
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    });

                    tracing::info!(
                        "Heartbeat enabled with {}s interval",
                        hb_config.interval_secs
                    );
                    Some(spawn_heartbeat(
                        config,
                        workspace.clone(),
                        self.cheap_llm().clone(),
                        Some(notify_tx),
                        None, // Integrity monitor set separately at startup
                    ))
                } else {
                    tracing::warn!("Heartbeat enabled but no workspace available");
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // Spawn routine engine if enabled
        let routine_handle = if let Some(ref rt_config) = self.routine_config {
            if rt_config.enabled {
                if let (Some(store), Some(workspace)) = (self.store(), self.workspace()) {
                    // Set up notification channel (same pattern as heartbeat)
                    let (notify_tx, mut notify_rx) =
                        tokio::sync::mpsc::channel::<OutgoingResponse>(32);

                    let engine = Arc::new(RoutineEngine::new(
                        rt_config.clone(),
                        crate::tenant::SystemScope::new(Arc::clone(store)),
                        self.llm().clone(),
                        Arc::clone(workspace),
                        notify_tx,
                        Some(self.scheduler.clone()),
                        self.deps.extension_manager.clone(),
                        self.tools().clone(),
                        self.safety().clone(),
                        self.deps.sandbox_readiness,
                        self.deps.http_interceptor.clone(),
                    ));

                    // Register routine tools
                    self.deps
                        .tools
                        .register_routine_tools(Arc::clone(store), Arc::clone(&engine));

                    // Load initial event cache
                    engine.refresh_event_cache().await;

                    // Spawn notification forwarder (mirrors heartbeat pattern)
                    let channels = self.channels.clone();
                    let extension_manager = self.deps.extension_manager.clone();
                    tokio::spawn(async move {
                        while let Some(response) = notify_rx.recv().await {
                            let notify_channel = response
                                .metadata
                                .get("notify_channel")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            let fallback_user = resolve_owner_scope_notification_user(
                                response
                                    .metadata
                                    .get("notify_user")
                                    .and_then(|v| v.as_str()),
                                response.metadata.get("owner_id").and_then(|v| v.as_str()),
                            );
                            let Some(user) = resolve_routine_notification_target(
                                extension_manager.as_ref(),
                                &response.metadata,
                            )
                            .await
                            else {
                                tracing::warn!(
                                    notify_channel = ?notify_channel,
                                    "Skipping routine notification with no explicit target or owner scope"
                                );
                                continue;
                            };

                            // Try the configured channel first, fall back to
                            // broadcasting on all channels.
                            let targeted_ok = if let Some(ref channel) = notify_channel {
                                match channels.broadcast(channel, &user, response.clone()).await {
                                    Ok(()) => true,
                                    Err(e) => {
                                        let should_fallback =
                                            should_fallback_routine_notification(&e);
                                        tracing::warn!(
                                            channel = %channel,
                                            user = %user,
                                            error = %e,
                                            should_fallback,
                                            "Failed to send routine notification to configured channel"
                                        );
                                        if !should_fallback {
                                            continue;
                                        }
                                        false
                                    }
                                }
                            } else {
                                false
                            };

                            if !targeted_ok && let Some(user) = fallback_user {
                                let results = channels.broadcast_all(&user, response).await;
                                for (ch, result) in results {
                                    if let Err(e) = result {
                                        tracing::warn!(
                                            "Failed to broadcast routine notification to {}: {}",
                                            ch,
                                            e
                                        );
                                    }
                                }
                            }
                        }
                    });

                    // Spawn cron ticker
                    let cron_interval =
                        std::time::Duration::from_secs(rt_config.cron_check_interval_secs);
                    let cron_handle = spawn_cron_ticker(Arc::clone(&engine), cron_interval);

                    // Store engine reference for event trigger checking
                    // Safety: we're in run() which takes self, no other reference exists
                    let engine_ref = Arc::clone(&engine);
                    // SAFETY: self is consumed by run(), we can smuggle the engine in
                    // via a local to use in the message loop below.

                    // Expose engine to gateway for manual triggering
                    *self.routine_engine_slot.write().await = Some(Arc::clone(&engine));

                    tracing::debug!(
                        "Routines enabled: cron ticker every {}s, max {} concurrent",
                        rt_config.cron_check_interval_secs,
                        rt_config.max_concurrent_routines
                    );

                    Some((cron_handle, engine_ref))
                } else {
                    tracing::warn!("Routines enabled but store/workspace not available");
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // Hydrate TUI sidebar with existing engine threads and routines so the
        // activity panel is populated before the first user message.
        self.hydrate_tui_sidebar().await;

        // Run BOOT.md instructions on startup (Phase 3: OpenClaw gap)
        if self.workspace().is_some() {
            let boot_user_id = "system";
            if let Err(e) = self.run_boot_if_present(boot_user_id).await {
                tracing::warn!("BOOT.md startup failed: {}", e);
            }
        }

        // Main message loop
        tracing::debug!("Agent {} ready and listening", self.config.name);

        loop {
            let message = tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    tracing::debug!("Ctrl+C received, shutting down...");
                    break;
                }
                msg = message_stream.next() => {
                    match msg {
                        Some(m) => m,
                        None => {
                            tracing::debug!("All channel streams ended, shutting down...");
                            break;
                        }
                    }
                }
            };

            // Apply transcription middleware to audio attachments
            let mut message = message;
            if let Some(ref transcription) = self.deps.transcription {
                transcription
                    .process(&mut message.attachments, &mut message.content)
                    .await;
            }

            // Apply document extraction middleware to document attachments
            if let Some(ref doc_extraction) = self.deps.document_extraction {
                doc_extraction.process(&mut message).await;
            }

            // Store successfully extracted document text in workspace for indexing
            self.store_extracted_documents(&message).await;

            match self.handle_message(&message).await {
                Ok(HandleOutcome::Respond(response)) => {
                    // Hook: BeforeOutbound — allow hooks to modify or suppress outbound
                    let event = crate::hooks::HookEvent::Outbound {
                        user_id: message.user_id.clone(),
                        channel: message.channel.clone(),
                        content: response.content.clone(),
                        thread_id: message.thread_id.as_ref().map(|t| t.as_str().to_string()),
                    };
                    match self.hooks().run(&event).await {
                        Err(err) => {
                            tracing::warn!("BeforeOutbound hook blocked response: {}", err);
                            // Still send Done so the client knows the turn is complete
                            // even though the response was suppressed by the hook.
                            self.send_done(&message).await;
                        }
                        Ok(crate::hooks::HookOutcome::Continue {
                            modified: Some(new_content),
                        }) => {
                            let mut response = response;
                            response.content = new_content;
                            if let Err(e) = self.respond_then_done(&message, response).await {
                                tracing::error!(
                                    channel = %message.channel,
                                    error = %e,
                                    "Failed to send response to channel"
                                );
                            }
                        }
                        _ => {
                            if let Err(e) = self.respond_then_done(&message, response).await {
                                tracing::error!(
                                    channel = %message.channel,
                                    error = %e,
                                    "Failed to send response to channel"
                                );
                            }
                        }
                    }
                }
                Ok(HandleOutcome::NoResponse) => {
                    // Empty response (e.g. routine consumed the message, silent reply).
                    // Send Done so the client knows the turn is complete.
                    tracing::debug!(
                        channel = %message.channel,
                        user = %message.user_id,
                        "Suppressed empty response (not sent to channel)"
                    );
                    self.send_done(&message).await;
                }
                Ok(HandleOutcome::Pending) => {
                    // Turn paused awaiting user action (approval, auth, etc).
                    // Do NOT emit Done — the thread is not in a terminal state.
                    // The relevant ApprovalNeeded/AuthRequired status was already
                    // sent by the inner handler before returning.
                    tracing::debug!(
                        channel = %message.channel,
                        user = %message.user_id,
                        "Turn paused (Pending); suppressing Done"
                    );
                }
                Ok(HandleOutcome::Shutdown) => {
                    // Shutdown signal received (/quit, /exit, /shutdown)
                    tracing::debug!("Shutdown command received, exiting...");
                    break;
                }
                Err(e) => {
                    tracing::error!("Error handling message: {}", e);
                    if let Err(send_err) = self
                        .respond_then_done(
                            &message,
                            OutgoingResponse::text(format!("Error: {}", e)),
                        )
                        .await
                    {
                        tracing::error!(
                            channel = %message.channel,
                            error = %send_err,
                            "Failed to send error response to channel"
                        );
                    }
                }
            }

            // Refresh engine v2 thread list in the TUI sidebar after each turn.
            if self.config.engine_v2
                && let Ok(threads) =
                    crate::bridge::list_engine_threads(None, &message.user_id).await
            {
                let summaries: Vec<crate::channels::EngineThreadSummary> = threads
                    .into_iter()
                    .map(|t| crate::channels::EngineThreadSummary {
                        id: t.id,
                        goal: t.goal,
                        thread_type: t.thread_type,
                        state: t.state,
                        step_count: t.step_count,
                        total_tokens: t.total_tokens,
                        created_at: t.created_at,
                        updated_at: t.updated_at,
                    })
                    .collect();
                let _ = self
                    .channels
                    .send_status(
                        &message.channel,
                        StatusUpdate::EngineThreadList { threads: summaries },
                        &message.metadata,
                    )
                    .await;
            }
        }

        // Cleanup
        tracing::debug!("Agent shutting down...");
        repair_handle.abort();
        pruning_handle.abort();
        if let Some(handle) = heartbeat_handle {
            handle.abort();
        }
        if let Some((cron_handle, _)) = routine_handle {
            cron_handle.abort();
        }
        self.scheduler.stop_all().await;
        self.channels.shutdown_all().await?;

        Ok(())
    }

    /// Store extracted document text in workspace memory for future search/recall.
    async fn store_extracted_documents(&self, message: &IncomingMessage) {
        let workspace = match self.workspace_for_user(&message.user_id) {
            Some(ws) => ws,
            None => return,
        };

        for attachment in &message.attachments {
            if attachment.kind != crate::channels::AttachmentKind::Document {
                continue;
            }
            let text = match &attachment.extracted_text {
                Some(t) if !t.starts_with('[') => t, // skip error messages like "[Failed to..."
                _ => continue,
            };

            // Sanitize filename: strip path separators to prevent directory traversal
            let raw_name = attachment.filename.as_deref().unwrap_or("unnamed_document");
            let filename: String = raw_name
                .chars()
                .map(|c| {
                    if c == '/' || c == '\\' || c == '\0' {
                        '_'
                    } else {
                        c
                    }
                })
                .collect();
            let filename = filename.trim_start_matches('.');
            let filename = if filename.is_empty() {
                "unnamed_document"
            } else {
                filename
            };
            let date = chrono::Utc::now().format("%Y-%m-%d");
            let path = format!("documents/{date}/{filename}");

            let header = format!(
                "# {filename}\n\n\
                 > Uploaded by **{}** via **{}** on {date}\n\
                 > MIME: {} | Size: {} bytes\n\n---\n\n",
                message.user_id,
                message.channel,
                attachment.mime_type,
                attachment.size_bytes.unwrap_or(0),
            );
            let content = format!("{header}{text}");

            match workspace.write(&path, &content).await {
                Ok(_) => {
                    tracing::info!(
                        path = %path,
                        text_len = text.len(),
                        "Stored extracted document in workspace memory"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path,
                        error = %e,
                        "Failed to store extracted document in workspace"
                    );
                }
            }
        }
    }

    async fn handle_message(&self, message: &IncomingMessage) -> Result<HandleOutcome, Error> {
        // Log sensitive details at debug level for troubleshooting
        tracing::debug!(
            message_id = %message.id,
            user_id = %message.user_id,
            channel = %message.channel,
            thread_id = ?message.thread_id,
            "Message details"
        );

        // Internal messages (e.g. job-monitor notifications) are already
        // rendered text and should be forwarded directly to the user without
        // entering the normal user-input pipeline (LLM/tool loop).
        // The `is_internal` field and `into_internal()` setter are pub(crate),
        // so external channels cannot spoof this flag.
        if message.is_internal {
            tracing::debug!(
                message_id = %message.id,
                channel = %message.channel,
                "Forwarding internal message"
            );
            return Ok(HandleOutcome::Respond(OutgoingResponse::text(
                message.content.clone(),
            )));
        }

        // Set message tool context for this turn (current channel and target)
        // For Signal, use signal_target from metadata (group:ID or phone number),
        // otherwise fall back to user_id
        let target = message
            .routing_target()
            .unwrap_or_else(|| message.user_id.clone());
        self.tools()
            .set_message_tool_context(Some(message.channel.clone()), Some(target))
            .await;

        // Parse submission type first
        let mut submission = message
            .structured_submission
            .clone()
            .unwrap_or_else(|| SubmissionParser::parse(&message.content));
        tracing::trace!(
            "[agent_loop] Parsed submission: {:?}",
            std::any::type_name_of_val(&submission)
        );

        // Engine V2 early downgrade: bare-keyword ApprovalResponse → UserInput
        // when no approval gate or auth flow is pending. Done before the
        // BeforeInbound hook check so the downgraded message flows through
        // the full UserInput pipeline (hooks, drain loop, etc.).
        // Only applies to engine_v2 because the legacy path needs session/
        // thread state (not yet resolved) to determine AwaitingApproval.
        if self.config.engine_v2
            && matches!(&submission, Submission::ApprovalResponse { .. })
            && !message.content.trim().starts_with('/')
        {
            let has_pending = crate::bridge::has_pending_auth(&message.user_id).await
                || crate::bridge::has_any_pending_gate(
                    &message.user_id,
                    message.conversation_scope(),
                )
                .await;
            if !has_pending {
                submission = Submission::UserInput {
                    content: message.content.clone(),
                };
            }
        }

        // Hook: BeforeInbound — allow hooks to modify or reject user input
        if let Submission::UserInput { ref content } = submission {
            let event = crate::hooks::HookEvent::Inbound {
                user_id: message.user_id.clone(),
                channel: message.channel.clone(),
                content: content.clone(),
                thread_id: message.thread_id.as_ref().map(|t| t.as_str().to_string()),
            };
            match self.hooks().run(&event).await {
                Err(crate::hooks::HookError::Rejected { reason }) => {
                    return Ok(HandleOutcome::Respond(OutgoingResponse::text(format!(
                        "[Message rejected: {}]",
                        reason
                    ))));
                }
                Err(err) => {
                    return Ok(HandleOutcome::Respond(OutgoingResponse::text(format!(
                        "[Message blocked by hook policy: {}]",
                        err
                    ))));
                }
                Ok(crate::hooks::HookOutcome::Continue {
                    modified: Some(new_content),
                }) => {
                    submission = Submission::UserInput {
                        content: new_content,
                    };
                }
                _ => {} // Continue, fail-open errors already logged in registry
            }
        }

        // Engine V2 routing (Strategy C: parallel deployment).
        // Bridge handlers return BridgeOutcome which maps directly to
        // HandleOutcome — gate status is encoded in the return type, not
        // queried post-hoc.
        if self.config.engine_v2 {
            match &submission {
                Submission::UserInput { content } => {
                    return crate::bridge::handle_with_engine(self, message, content)
                        .await
                        .map(HandleOutcome::from);
                }
                Submission::ApprovalResponse { approved, always } => {
                    // Reaching here means the message is a slash command (/approve,
                    // /deny) or has a pending gate/auth — early downgrade above
                    // already handled the bare-keyword-with-no-gate case.
                    if crate::bridge::has_pending_auth(&message.user_id).await {
                        let content = &message.content;
                        return crate::bridge::handle_with_engine(self, message, content)
                            .await
                            .map(HandleOutcome::from);
                    }
                    return crate::bridge::handle_approval(self, message, *approved, *always)
                        .await
                        .map(HandleOutcome::from);
                }
                Submission::ExecApproval {
                    request_id,
                    approved,
                    always,
                } => {
                    return crate::bridge::handle_exec_approval(
                        self,
                        message,
                        *request_id,
                        *approved,
                        *always,
                    )
                    .await
                    .map(HandleOutcome::from);
                }
                Submission::ExternalCallback { request_id } => {
                    return crate::bridge::handle_external_callback(self, message, *request_id)
                        .await
                        .map(HandleOutcome::from);
                }
                Submission::GateAuthResolution {
                    request_id,
                    resolution,
                } => {
                    return crate::bridge::handle_auth_gate_resolution(
                        self,
                        message,
                        *request_id,
                        resolution.clone(),
                    )
                    .await
                    .map(HandleOutcome::from);
                }
                Submission::Interrupt => {
                    return crate::bridge::handle_interrupt(self, message)
                        .await
                        .map(HandleOutcome::from);
                }
                Submission::NewThread => {
                    return crate::bridge::handle_new_thread(self, message)
                        .await
                        .map(HandleOutcome::from);
                }
                Submission::Clear => {
                    return crate::bridge::handle_clear(self, message)
                        .await
                        .map(HandleOutcome::from);
                }
                Submission::Expected { description } => {
                    return crate::bridge::handle_expected(self, message, description)
                        .await
                        .map(HandleOutcome::from);
                }
                Submission::PairingClaim { channel, code } => {
                    return crate::bridge::handle_pairing_claim(self, message, channel, code)
                        .await
                        .map(HandleOutcome::from);
                }
                // Undo/Redo/Resume/SwitchThread: v1-only (engine has no undo;
                // thread switching is implicit via ConversationManager).
                // Compact/Summarize/Suggest: orthogonal to engine (compaction is internal).
                // Heartbeat/SystemCommand/JobStatus/JobCancel/Quit: v1 infrastructure.
                _ => {}
            }
        }

        // V2-only structured submissions must fail before any session/thread
        // resolution on the legacy path. Otherwise a crafted request can
        // switch the active thread via conversation_scope before returning the
        // expected ENGINE_V2 error.
        if !self.config.engine_v2 {
            match submission {
                Submission::ExternalCallback { .. } => {
                    return Ok(HandleOutcome::Respond(OutgoingResponse::text(
                        "Error: External callbacks require ENGINE_V2".to_string(),
                    )));
                }
                Submission::GateAuthResolution { .. } => {
                    return Ok(HandleOutcome::Respond(OutgoingResponse::text(
                        "Error: Auth gate resolution requires ENGINE_V2".to_string(),
                    )));
                }
                _ => {}
            }
        }

        // Hydrate thread from DB if it's a historical thread not in memory
        if let Some(external_thread_id) = message.conversation_scope() {
            tracing::trace!(
                message_id = %message.id,
                thread_id = %external_thread_id,
                "Hydrating thread from DB"
            );
            if let Some(rejection) = self.maybe_hydrate_thread(message, external_thread_id).await {
                return Ok(HandleOutcome::Respond(OutgoingResponse::text(format!(
                    "Error: {}",
                    rejection
                ))));
            }
        }

        // Resolve session and thread. Approval submissions are allowed to
        // target an already-loaded owned thread by UUID across channels so the
        // web approval UI can approve work that originated from HTTP/other
        // owner-scoped channels.
        let approval_thread_uuid = if matches!(
            submission,
            Submission::ExecApproval { .. }
                | Submission::ApprovalResponse { .. }
                | Submission::ExternalCallback { .. }
                | Submission::GateAuthResolution { .. }
        ) {
            message
                .conversation_scope()
                .and_then(|thread_id| Uuid::parse_str(thread_id).ok())
        } else {
            None
        };

        let (session, thread_id) = if let Some(target_thread_id) = approval_thread_uuid {
            let session = self
                .session_manager
                .get_or_create_session(&message.user_id)
                .await;
            let mut sess = session.lock().await;
            if let Some(thread) = sess.threads.get(&target_thread_id) {
                // Block ExecApproval (JSON from the approval UI) when there
                // is no pending approval — prevents hijacking a thread by UUID.
                // ApprovalResponse (bare keywords) is allowed through so the
                // should_route_as_approval guard can downgrade it to UserInput.
                if thread.pending_approval.is_none()
                    && matches!(submission, Submission::ExecApproval { .. })
                {
                    tracing::warn!(
                        %target_thread_id,
                        approval_channel = %message.channel,
                        "Blocked approval for thread with no pending approval"
                    );
                    drop(sess);
                    return Ok(HandleOutcome::Respond(OutgoingResponse::text(
                        "Error: no pending approval on this thread",
                    )));
                }
                // ApprovalResponse (bare "yes"/"no"/"always") without a
                // pending approval: fall through to normal handling so the
                // should_route_as_approval guard can downgrade to UserInput.
                // Skip the cross-channel auth check — it only matters for
                // actual approvals, not messages about to become UserInput.

                if thread.pending_approval.is_some() {
                    let authorized = crate::agent::session::is_approval_authorized(
                        thread.source_channel.as_deref(),
                        &message.channel,
                    );
                    if !authorized {
                        tracing::warn!(
                            %target_thread_id,
                            source_channel = ?thread.source_channel,
                            approval_channel = %message.channel,
                            "Blocked cross-channel approval attempt"
                        );
                        drop(sess);
                        return Ok(HandleOutcome::Respond(OutgoingResponse::text(
                            "Error: approval not authorized for this channel",
                        )));
                    }
                }
                sess.active_thread = Some(target_thread_id);
                sess.last_active_at = chrono::Utc::now();
                drop(sess);
                self.session_manager
                    .register_thread(
                        &message.user_id,
                        &message.channel,
                        target_thread_id,
                        Arc::clone(&session),
                    )
                    .await;
                (session, target_thread_id)
            } else {
                drop(sess);
                self.session_manager
                    .resolve_thread_with_parsed_uuid(
                        &message.user_id,
                        &message.channel,
                        message.conversation_scope(),
                        approval_thread_uuid,
                    )
                    .await
            }
        } else {
            self.session_manager
                .resolve_thread(
                    &message.user_id,
                    &message.channel,
                    message.conversation_scope(),
                )
                .await
        };
        tracing::debug!(
            message_id = %message.id,
            thread_id = %thread_id,
            "Resolved session and thread"
        );

        // Auth mode interception: if the thread is awaiting a token, route
        // the message directly to the credential store. Nothing touches
        // logs, turns, history, or compaction.
        let pending_auth = {
            let sess = session.lock().await;
            sess.threads
                .get(&thread_id)
                .and_then(|t| t.pending_auth.clone())
        };

        if let Some(pending) = pending_auth {
            if pending.is_expired() {
                // TTL exceeded — clear stale auth mode
                tracing::warn!(
                    extension = %pending.extension_name,
                    "Auth mode expired after TTL, clearing"
                );
                {
                    let mut sess = session.lock().await;
                    if let Some(thread) = sess.threads.get_mut(&thread_id) {
                        thread.pending_auth = None;
                    }
                }
                // If this was a user message (possibly a pasted token), return an
                // explicit error instead of forwarding it to the LLM/history.
                if matches!(submission, Submission::UserInput { .. }) {
                    return Ok(HandleOutcome::Respond(OutgoingResponse::text(format!(
                        "Authentication for **{}** expired. Please try again.",
                        pending.extension_name
                    ))));
                }
                // Control submissions (interrupt, undo, etc.) fall through to normal handling
            } else {
                match &submission {
                    Submission::UserInput { content } => {
                        return self
                            .process_auth_token(message, &pending, content, session, thread_id)
                            .await
                            .map(HandleOutcome::from_legacy);
                    }
                    _ => {
                        // Any control submission (interrupt, undo, etc.) cancels auth mode
                        let mut sess = session.lock().await;
                        if let Some(thread) = sess.threads.get_mut(&thread_id) {
                            thread.pending_auth = None;
                        }
                        // Fall through to normal handling
                    }
                }
            }
        }

        tracing::trace!(
            "Received message from {} on {} ({} chars)",
            message.user_id,
            message.channel,
            message.content.len()
        );

        // Audit log for command submissions (Phase 3.4: Command audit logging)
        match &submission {
            Submission::UserInput { .. } => {} // Don't log user text
            sub => {
                let cmd_name = match sub {
                    Submission::Undo => "undo",
                    Submission::Redo => "redo",
                    Submission::Interrupt => "interrupt",
                    Submission::Compact => "compact",
                    Submission::Clear => "clear",
                    Submission::NewThread => "new",
                    Submission::Heartbeat => "heartbeat",
                    Submission::Summarize => "summarize",
                    Submission::Suggest => "suggest",
                    Submission::Quit => "quit",
                    Submission::SwitchThread { .. } => "switch_thread",
                    Submission::Resume { .. } => "resume",
                    Submission::ExecApproval { .. } => "exec_approval",
                    Submission::ApprovalResponse { .. } => "approval_response",
                    Submission::SystemCommand { command, .. } => command.as_str(),
                    Submission::UserInput { .. } => unreachable!(),
                };
                tracing::info!(
                    target: "audit",
                    command = cmd_name,
                    channel = message.channel.as_str(),
                    user = message.user_id.as_str(),
                    thread = %thread_id,
                );
            }
        }

        // Daily session reset check (Phase 3.3)
        if let Some(reset_hour) = self.config.daily_reset_hour {
            let should_reset = {
                let sess = session.lock().await;
                if let Some(thread) = sess.threads.get(&thread_id) {
                    let last_active = thread.updated_at;
                    let now = chrono::Utc::now();
                    // Check if last activity was before today's reset hour
                    let today_reset = now
                        .date_naive()
                        .and_hms_opt(reset_hour as u32, 0, 0)
                        .map(|dt| dt.and_utc());
                    if let Some(reset_time) = today_reset {
                        last_active < reset_time && now >= reset_time
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if should_reset && matches!(submission, Submission::UserInput { .. }) {
                tracing::info!(
                    "Daily session reset triggered (reset_hour={})",
                    reset_hour
                );
                if let Some(ws) = self.workspace() {
                    let _ = self
                        .save_thread_to_workspace_before_new(session.clone(), thread_id, ws.as_ref())
                        .await;
                }
                // Create new thread for this user/channel
                let _ = self.process_new_thread(message).await;
                // Re-resolve the thread after reset
                let (new_session, new_thread_id) = self
                    .session_manager
                    .resolve_thread(
                        &message.user_id,
                        &message.channel,
                        message.thread_id.as_deref(),
                    )
                    .await;
                // Continue processing with the new thread
                return self
                    .process_user_input(
                        message,
                        new_session,
                        new_thread_id,
                        &message.content,
                    )
                    .await
                    .map(|r| match r {
                        SubmissionResult::Response { content } => Some(content),
                        SubmissionResult::Ok { message } => message,
                        SubmissionResult::Error { message } => Some(format!("Error: {}", message)),
                        _ => Some(String::new()),
                    });
            }
        }

        // Process based on submission type
        let result = match submission {
            Submission::UserInput { content } => {
                let mut result = self
                    .process_user_input(
                        message,
                        tenant.clone(),
                        session.clone(),
                        thread_id,
                        &content,
                    )
                    .await;

                // Drain any messages queued during processing.
                // Messages are merged (newline-separated) so the LLM receives
                // full context from rapid consecutive inputs instead of
                // processing each as a separate turn with partial context (#259).
                //
                // Only `Response` continues the drain — the user got a normal
                // reply and there may be more queued messages to process.
                //
                // Everything else stops the loop:
                // - `NeedApproval`: thread is blocked on user approval
                // - `Interrupted`: turn was cancelled
                // - `Ok`: control-command acknowledgment (including the "queued"
                //    ack returned when a message arrives during Processing)
                // - `Error`: soft error — draining more messages after an error
                //    would produce confusing interleaved output
                // - `Err(_)`: hard error
                while let Ok(SubmissionResult::Response { content: outgoing }) = &result {
                    let merged = {
                        let mut sess = session.lock().await;
                        sess.threads
                            .get_mut(&thread_id)
                            .and_then(|t| t.drain_pending_messages())
                    };
                    let Some(next_content) = merged else {
                        break;
                    };

                    tracing::debug!(
                        thread_id = %thread_id,
                        merged_len = next_content.len(),
                        "Drain loop: processing merged queued messages"
                    );

                    // Send the completed turn's response before starting the next.
                    //
                    // Known limitations:
                    // - One-shot channels (HttpChannel) consume the response
                    //   sender on the first respond() call keyed by msg.id.
                    //   Subsequent calls (including the outer handler's final
                    //   respond) are silently dropped. For one-shot channels
                    //   only this intermediate response is delivered.
                    // - All drain-loop responses are routed via the original
                    //   `message`, so channels that key routing on message
                    //   identity will attribute every response to the first
                    //   message. This is acceptable for the current
                    //   single-user-per-thread model.
                    let response =
                        build_outgoing_response_for_thread(&session, thread_id, outgoing.clone())
                            .await;
                    if let Err(e) = self.respond_then_done(message, response).await {
                        tracing::warn!(
                            thread_id = %thread_id,
                            "Failed to send intermediate drain-loop response: {e}"
                        );
                    }

                    // Process merged queued messages as a single turn.
                    // Use a message clone with cleared attachments so
                    // augment_with_attachments doesn't re-apply the original
                    // message's attachments to unrelated queued text.
                    let mut queued_msg = message.clone();
                    queued_msg.attachments.clear();
                    result = self
                        .process_user_input(
                            &queued_msg,
                            tenant.clone(),
                            session.clone(),
                            thread_id,
                            &next_content,
                        )
                        .await;

                    // If processing failed, re-queue the drained content so it
                    // isn't lost. It will be picked up on the next successful turn.
                    if !matches!(&result, Ok(SubmissionResult::Response { .. })) {
                        let mut sess = session.lock().await;
                        if let Some(thread) = sess.threads.get_mut(&thread_id) {
                            thread.requeue_drained(next_content);
                            tracing::debug!(
                                thread_id = %thread_id,
                                "Re-queued drained content after non-Response result"
                            );
                        }
                    }
                }

                result
            }
            Submission::SystemCommand { command, args } => {
                tracing::debug!(
                    "[agent_loop] SystemCommand: command={}, channel={}",
                    command,
                    message.channel
                );
                // /reasoning is special-cased here (not in handle_system_command)
                // because it needs the session + thread_id to read turn reasoning
                // data, which handle_system_command's signature doesn't provide.
                if command == "reasoning" {
                    let result = self
                        .handle_reasoning_command(&args, &session, thread_id)
                        .await;
                    return match result {
                        SubmissionResult::Response { content } => {
                            Ok(HandleOutcome::Respond(OutgoingResponse::text(content)))
                        }
                        SubmissionResult::Ok { message } => Ok(HandleOutcome::from_legacy(message)),
                        SubmissionResult::Error { message } => Ok(HandleOutcome::Respond(
                            OutgoingResponse::text(format!("Error: {}", message)),
                        )),
                        _ => {
                            if is_single_message_repl(message) {
                                Ok(HandleOutcome::Shutdown)
                            } else {
                                Ok(HandleOutcome::NoResponse)
                            }
                        }
                    };
                }
                // Authorization checks (including restart channel check) are enforced in handle_system_command
                self.handle_system_command(&command, &args, &message.channel, &tenant)
                    .await
            }
            Submission::Undo => self.process_undo(session, thread_id).await,
            Submission::Redo => self.process_redo(session, thread_id).await,
            Submission::Interrupt => self.process_interrupt(session, thread_id).await,
            Submission::Compact => self.process_compact(session, thread_id).await,
            Submission::Clear => self.process_clear(session, thread_id).await,
            Submission::NewThread => {
                if let Some(ws) = self.workspace() {
                    if let Err(e) = self
                        .save_thread_to_workspace_before_new(session.clone(), thread_id, ws.as_ref())
                        .await
                    {
                        tracing::warn!("Failed to save thread to workspace on /new: {}", e);
                    }
                }
                self.process_new_thread(message).await
            }
            Submission::Heartbeat => self.process_heartbeat().await,
            Submission::Summarize => self.process_summarize(session, thread_id).await,
            Submission::Suggest => self.process_suggest(session, thread_id).await,
            Submission::Quit => return Ok(None),
            Submission::SwitchThread { thread_id: target } => {
                self.process_switch_thread(message, target).await
            }
            Submission::Resume { checkpoint_id } => {
                self.process_resume(session.clone(), thread_id, checkpoint_id)
                    .await
            }
            Submission::ListThreads => self.process_list_threads(session.clone(), message).await,
            Submission::ExecApproval {
                request_id,
                approved,
                always,
            } => {
                self.process_approval(
                    message,
                    session.clone(),
                    thread_id,
                    Some(request_id),
                    approved,
                    always,
                )
                .await
            }
            Submission::ExternalCallback { .. } => Ok(SubmissionResult::Error {
                message: "External callbacks require ENGINE_V2".to_string(),
            }),
            Submission::GateAuthResolution { .. } => Ok(SubmissionResult::Error {
                message: "Auth gate resolution requires ENGINE_V2".to_string(),
            }),
            Submission::ApprovalResponse { approved, always } => {
                let thread_state = {
                    let sess = session.lock().await;
                    sess.threads
                        .get(&thread_id)
                        .map(|t| t.state)
                        .unwrap_or(ThreadState::Idle)
                };
                // NOTE: TOCTOU possible — state could change between check
                // and process_approval; process_approval handles stale cases.
                if should_route_as_approval(thread_state, &message.content) {
                    self.process_approval(
                        message,
                        session.clone(),
                        thread_id,
                        None,
                        approved,
                        always,
                    )
                    .await
                } else {
                    // Run BeforeInbound hooks for the downgraded content —
                    // the hook check above only fires for UserInput submissions,
                    // and this was parsed as ApprovalResponse.
                    let content = message.content.clone();
                    let hook_event = crate::hooks::HookEvent::Inbound {
                        user_id: message.user_id.clone(),
                        channel: message.channel.clone(),
                        content: content.clone(),
                        thread_id: message.thread_id.as_ref().map(|t| t.as_str().to_string()),
                    };
                    let content = match self.hooks().run(&hook_event).await {
                        Err(crate::hooks::HookError::Rejected { reason }) => {
                            // Match the main UserInput path's rejection behavior.
                            return Ok(HandleOutcome::Respond(OutgoingResponse::text(format!(
                                "[Message rejected: {reason}]"
                            ))));
                        }
                        Err(err) => {
                            // Match the main UserInput path's error behavior.
                            return Ok(HandleOutcome::Respond(OutgoingResponse::text(format!(
                                "[Message blocked by hook policy: {err}]"
                            ))));
                        }
                        Ok(crate::hooks::HookOutcome::Continue {
                            modified: Some(new_content),
                        }) => new_content,
                        _ => content, // Continue — no modification
                    };

                    // Process as user input with the drain loop so queued
                    // messages during processing are merged, matching the
                    // Submission::UserInput arm's behavior.
                    let mut result = self
                        .process_user_input(
                            message,
                            tenant.clone(),
                            session.clone(),
                            thread_id,
                            &content,
                        )
                        .await;

                    while let Ok(SubmissionResult::Response { content: outgoing }) = &result {
                        let merged = {
                            let mut sess = session.lock().await;
                            sess.threads
                                .get_mut(&thread_id)
                                .and_then(|thread| thread.drain_pending_messages())
                        };
                        let Some(next_content) = merged else {
                            break;
                        };

                        let response = build_outgoing_response_for_thread(
                            &session,
                            thread_id,
                            outgoing.clone(),
                        )
                        .await;
                        if let Err(e) = self.respond_then_done(message, response).await {
                            tracing::warn!(
                                %thread_id,
                                "Failed to send intermediate drain-loop response: {e}"
                            );
                        }

                        let mut queued_msg = message.clone();
                        queued_msg.attachments.clear();
                        result = self
                            .process_user_input(
                                &queued_msg,
                                tenant.clone(),
                                session.clone(),
                                thread_id,
                                &next_content,
                            )
                            .await;

                        if !matches!(&result, Ok(SubmissionResult::Response { .. })) {
                            let mut sess = session.lock().await;
                            if let Some(thread) = sess.threads.get_mut(&thread_id) {
                                thread.requeue_drained(next_content);
                            }
                        }
                    }

                    result
                }
            }
            Submission::PairingClaim { channel, code } => {
                // Pairing approval is independent of engine_v2 — it only
                // touches the pairing store and the extension manager.
                // Reuse the bridge handler so v1 and v2 surfaces behave
                // identically (#3317).
                match crate::bridge::handle_pairing_claim(self, message, &channel, &code).await {
                    Ok(crate::bridge::BridgeOutcome::Respond(text)) => {
                        Ok(SubmissionResult::Response { content: text })
                    }
                    Ok(crate::bridge::BridgeOutcome::NoResponse)
                    | Ok(crate::bridge::BridgeOutcome::Pending) => {
                        Ok(SubmissionResult::Ok { message: None })
                    }
                    Err(e) => Ok(SubmissionResult::Error {
                        message: format!("Pairing approval failed: {e}"),
                    }),
                }
            }
            Submission::Plan { sub } => {
                use crate::agent::submission::PlanSubcommand;
                let rewritten = match sub {
                    PlanSubcommand::Create { description } => {
                        format!("[PLAN MODE] Create a plan for: {description}")
                    }
                    PlanSubcommand::Approve { plan_ref } => {
                        let r = plan_ref.as_deref().unwrap_or("the most recent plan");
                        format!(
                            "[PLAN MODE] Approve and execute plan {r}. \
                             Create a mission from the plan content using mission_create, \
                             then fire it with mission_fire."
                        )
                    }
                    PlanSubcommand::Status { plan_ref } => {
                        let r = plan_ref.as_deref().unwrap_or("the most recent plan");
                        format!(
                            "[PLAN MODE] Show status of plan {r}. \
                             Check the associated mission's thread_history, \
                             current_focus, and approach_history."
                        )
                    }
                    PlanSubcommand::Revise { plan_ref, feedback } => {
                        let r = plan_ref.as_deref().unwrap_or("the most recent plan");
                        format!("[PLAN MODE] Revise plan {r} based on: {feedback}")
                    }
                    PlanSubcommand::List => {
                        "[PLAN MODE] List all plans. Search memory for plan documents \
                         and show their status."
                            .to_string()
                    }
                };
                self.process_user_input(message, tenant, session.clone(), thread_id, &rewritten)
                    .await
            }
        };

        // Convert SubmissionResult to a HandleOutcome.
        match result? {
            SubmissionResult::Response { content } => {
                // Suppress silent replies (e.g. from group chat "nothing to say" responses).
                // Silent replies exit single-message REPL invocations.
                if ironclaw_llm::is_silent_reply(&content) {
                    tracing::debug!("Suppressing silent reply token");
                    Ok(HandleOutcome::Shutdown)
                } else if content.is_empty() {
                    Ok(HandleOutcome::NoResponse)
                } else {
                    Ok(HandleOutcome::Respond(
                        build_outgoing_response_for_thread(&session, thread_id, content).await,
                    ))
                }
            }
            SubmissionResult::Ok {
                message: output_message,
            } => {
                let should_exit =
                    if output_message.as_deref() == Some("") && is_single_message_repl(message) {
                        let sess = session_for_empty_exit.lock().await;
                        sess.threads
                            .get(&thread_id)
                            .map(|thread| thread.state != ThreadState::AwaitingApproval)
                            .unwrap_or(true)
                    } else {
                        false
                    };

                if should_exit {
                    Ok(HandleOutcome::Shutdown)
                } else {
                    Ok(HandleOutcome::from_legacy(output_message))
                }
            }
            SubmissionResult::Error { message } => Ok(HandleOutcome::Respond(
                OutgoingResponse::text(format!("Error: {}", message)),
            )),
            SubmissionResult::Interrupted => Ok(HandleOutcome::Respond(OutgoingResponse::text(
                "Interrupted.",
            ))),
            SubmissionResult::AuthPending => {
                // Auth-required status already sent by handle_auth_intercept.
                // Thread is in auth mode — suppress text response and Done.
                Ok(HandleOutcome::Pending)
            }
            SubmissionResult::NeedApproval { .. } => {
                // ApprovalNeeded status was already sent by thread_ops.rs before
                // returning this result. The thread is now in AwaitingApproval —
                // do NOT emit a terminal Done because the turn is paused, not
                // complete. Sending Done here would also trip the web UI's
                // missing-response safety net (see #2079).
                Ok(HandleOutcome::Pending)
            }
        }
    }

    /// Hydrate a historical thread from DB into memory if not already present.
    ///
    /// Called before `resolve_thread` so that the session manager finds the
    /// thread on lookup instead of creating a new one.
    ///
    /// Creates an in-memory thread with the exact UUID the frontend sent,
    /// even when the conversation has zero messages (e.g. a brand-new
    /// assistant thread). Without this, `resolve_thread` would mint a
    /// fresh UUID and all messages would land in the wrong conversation.
    async fn maybe_hydrate_thread(&self, message: &IncomingMessage, external_thread_id: &str) {
        // Only hydrate UUID-shaped thread IDs (web gateway uses UUIDs)
        let thread_uuid = match Uuid::parse_str(external_thread_id) {
            Ok(id) => id,
            Err(_) => return,
        };

        // Check if already in memory
        let session = self
            .session_manager
            .get_or_create_session(&message.user_id)
            .await;
        {
            let sess = session.lock().await;
            if sess.threads.contains_key(&thread_uuid) {
                return;
            }
        }

        // Load history from DB (may be empty for a newly created thread).
        let mut chat_messages: Vec<ChatMessage> = Vec::new();
        let msg_count;

        if let Some(store) = self.store() {
            let db_messages = store
                .list_conversation_messages(thread_uuid)
                .await
                .unwrap_or_default();
            msg_count = db_messages.len();
            chat_messages = db_messages
                .iter()
                .filter_map(|m| match m.role.as_str() {
                    "user" => Some(ChatMessage::user(&m.content)),
                    "assistant" => Some(ChatMessage::assistant(&m.content)),
                    _ => None,
                })
                .collect();
        } else {
            msg_count = 0;
        }

        // Create thread with the historical ID and restore messages
        let session_id = {
            let sess = session.lock().await;
            sess.id
        };

        let mut thread = crate::agent::session::Thread::with_id(thread_uuid, session_id);
        if !chat_messages.is_empty() {
            thread.restore_from_messages(chat_messages);
        }

        // Restore response chain from conversation metadata
        if let Some(store) = self.store()
            && let Ok(Some(metadata)) = store.get_conversation_metadata(thread_uuid).await
            && let Some(rid) = metadata
                .get("last_response_id")
                .and_then(|v| v.as_str())
                .map(String::from)
        {
            thread.last_response_id = Some(rid.clone());
            self.llm()
                .seed_response_chain(&thread_uuid.to_string(), rid);
            tracing::debug!("Restored response chain for thread {}", thread_uuid);
        }

        // Insert into session and register with session manager
        {
            let mut sess = session.lock().await;
            sess.threads.insert(thread_uuid, thread);
            sess.active_thread = Some(thread_uuid);
            sess.last_active_at = chrono::Utc::now();
        }

        self.session_manager
            .register_thread(
                &message.user_id,
                &message.channel,
                thread_uuid,
                Arc::clone(&session),
            )
            .await;

        tracing::debug!(
            "Hydrated thread {} from DB ({} messages)",
            thread_uuid,
            msg_count
        );
    }

    async fn process_user_input(
        &self,
        message: &IncomingMessage,
        session: Arc<Mutex<Session>>,
        thread_id: Uuid,
        content: &str,
    ) -> Result<SubmissionResult, Error> {
        // First check thread state without holding lock during I/O
        let thread_state = {
            let sess = session.lock().await;
            let thread = sess
                .threads
                .get(&thread_id)
                .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;
            thread.state
        };

        // Check thread state
        match thread_state {
            ThreadState::Processing => {
                return Ok(SubmissionResult::error(
                    "Turn in progress. Use /interrupt to cancel.",
                ));
            }
            ThreadState::AwaitingApproval => {
                return Ok(SubmissionResult::error(
                    "Waiting for approval. Use /interrupt to cancel.",
                ));
            }
            ThreadState::Completed => {
                return Ok(SubmissionResult::error(
                    "Thread completed. Use /thread new.",
                ));
            }
            ThreadState::Idle | ThreadState::Interrupted => {
                // Can proceed
            }
        }

        // Safety validation for user input
        let validation = self.safety().validate_input(content);
        if !validation.is_valid {
            let details = validation
                .errors
                .iter()
                .map(|e| format!("{}: {}", e.field, e.message))
                .collect::<Vec<_>>()
                .join("; ");
            return Ok(SubmissionResult::error(format!(
                "Input rejected by safety validation: {}",
                details
            )));
        }

        let violations = self.safety().check_policy(content);
        if violations
            .iter()
            .any(|rule| rule.action == crate::safety::PolicyAction::Block)
        {
            return Ok(SubmissionResult::error("Input rejected by safety policy."));
        }

        // Handle explicit commands (starting with /) directly
        // Everything else goes through the normal agentic loop with tools
        let temp_message = IncomingMessage {
            content: content.to_string(),
            ..message.clone()
        };

        if let Some(intent) = self.router.route_command(&temp_message) {
            // Explicit command like /status, /job, /list - handle directly
            return self.handle_job_or_command(intent, message).await;
        }

        // Natural language goes through the agentic loop
        // Job tools (create_job, list_jobs, etc.) are in the tool registry

        // Auto-compact if needed BEFORE adding new turn
        {
            let mut sess = session.lock().await;
            let thread = sess
                .threads
                .get_mut(&thread_id)
                .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;

            let messages = thread.messages();
            if let Some(strategy) = self.context_monitor.suggest_compaction(&messages) {
                let pct = self.context_monitor.usage_percent(&messages);
                tracing::info!("Context at {:.1}% capacity, auto-compacting", pct);

                // Pre-compaction memory flush: one silent turn to remind model to write durable memory
                let compaction_count = thread
                    .metadata
                    .get("compaction_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    + 1;
                let last_flush = thread
                    .metadata
                    .get("memory_flush_compaction_count")
                    .and_then(|v| v.as_u64());
                let run_flush = self
                    .config
                    .memory_flush
                    .as_ref()
                    .map(|m| m.enabled)
                    .unwrap_or(false)
                    && self.workspace().is_some()
                    && last_flush != Some(compaction_count);

                if run_flush {
                    ensure_metadata_object(thread);
                    thread
                        .metadata
                        .as_object_mut()
                        .unwrap()
                        .insert(
                            "memory_flush_compaction_count".to_string(),
                            serde_json::json!(compaction_count),
                        );
                    let flush_cfg = self.config.memory_flush.clone().unwrap();
                    drop(sess);
                    if let Err(e) = self
                        .run_memory_flush_turn(&flush_cfg, message.user_id.as_str())
                        .await
                    {
                        tracing::warn!("Pre-compaction memory flush failed: {}", e);
                    }
                    sess = session.lock().await;
                }

                let thread = sess
                    .threads
                    .get_mut(&thread_id)
                    .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;
                ensure_metadata_object(thread);
                thread
                    .metadata
                    .as_object_mut()
                    .unwrap()
                    .insert("compaction_count".to_string(), serde_json::json!(compaction_count));

                // Notify the user that compaction is happening
                let _ = self
                    .channels
                    .send_status(
                        &message.channel,
                        StatusUpdate::Status(format!(
                            "Context at {:.0}% capacity, compacting...",
                            pct
                        )),
                        &message.metadata,
                    )
                    .await;

                let compactor = ContextCompactor::new(self.llm().clone());
                if let Err(e) = compactor
                    .compact(thread, strategy, self.workspace().map(|w| w.as_ref()))
                    .await
                {
                    tracing::warn!("Auto-compaction failed: {}", e);
                }
            }
        }

        // Create checkpoint before turn
        let undo_mgr = self.session_manager.get_undo_manager(thread_id).await;
        {
            let sess = session.lock().await;
            let thread = sess
                .threads
                .get(&thread_id)
                .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;

            let mut mgr = undo_mgr.lock().await;
            mgr.checkpoint(
                thread.turn_number(),
                thread.messages(),
                format!("Before turn {}", thread.turn_number()),
            );
        }

        // Start the turn and get messages
        let turn_messages = {
            let mut sess = session.lock().await;
            let thread = sess
                .threads
                .get_mut(&thread_id)
                .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;
            thread.start_turn(content);
            thread.messages()
        };

        // Send thinking status
        let _ = self
            .channels
            .send_status(
                &message.channel,
                StatusUpdate::Thinking("Processing...".into()),
                &message.metadata,
            )
            .await;

        // Run the agentic tool execution loop
        let result = self
            .run_agentic_loop(message, session.clone(), thread_id, turn_messages, false)
            .await;

        // Re-acquire lock and check if interrupted
        let mut sess = session.lock().await;
        let thread = sess
            .threads
            .get_mut(&thread_id)
            .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;

        if thread.state == ThreadState::Interrupted {
            let _ = self
                .channels
                .send_status(
                    &message.channel,
                    StatusUpdate::Status("Interrupted".into()),
                    &message.metadata,
                )
                .await;
            return Ok(SubmissionResult::Interrupted);
        }

        // Complete, fail, or request approval
        match result {
            Ok(AgenticLoopResult::Response(response)) => {
                thread.complete_turn(&response);
                self.persist_response_chain(thread);
                let _ = self
                    .channels
                    .send_status(
                        &message.channel,
                        StatusUpdate::Status("Done".into()),
                        &message.metadata,
                    )
                    .await;

                // Fire-and-forget: persist turn to DB
                self.persist_turn(thread_id, &message.user_id, content, Some(&response));

                Ok(SubmissionResult::response(response))
            }
            Ok(AgenticLoopResult::NeedApproval { pending }) => {
                // Store pending approval in thread and update state
                let request_id = pending.request_id;
                let tool_name = pending.tool_name.clone();
                let description = pending.description.clone();
                let parameters = pending.parameters.clone();
                thread.await_approval(pending);
                let _ = self
                    .channels
                    .send_status(
                        &message.channel,
                        StatusUpdate::Status("Awaiting approval".into()),
                        &message.metadata,
                    )
                    .await;
                Ok(SubmissionResult::NeedApproval {
                    request_id,
                    tool_name,
                    description,
                    parameters,
                })
            }
            Err(e) => {
                thread.fail_turn(e.to_string());

                // Persist the user message even on failure
                self.persist_turn(thread_id, &message.user_id, content, None);

                Ok(SubmissionResult::error(e.to_string()))
            }
        }
    }

    /// Fire-and-forget: persist a turn (user message + optional assistant response) to the DB.
    fn persist_turn(
        &self,
        thread_id: Uuid,
        user_id: &str,
        user_input: &str,
        response: Option<&str>,
    ) {
        let store = match self.store() {
            Some(s) => Arc::clone(s),
            None => return,
        };

        let user_id = user_id.to_string();
        let user_input = user_input.to_string();
        let response = response.map(String::from);

        tokio::spawn(async move {
            if let Err(e) = store
                .ensure_conversation(thread_id, "gateway", &user_id, None)
                .await
            {
                tracing::warn!("Failed to ensure conversation {}: {}", thread_id, e);
                return;
            }

            if let Err(e) = store
                .add_conversation_message(thread_id, "user", &user_input)
                .await
            {
                tracing::warn!("Failed to persist user message: {}", e);
                return;
            }

            if let Some(ref resp) = response
                && let Err(e) = store
                    .add_conversation_message(thread_id, "assistant", resp)
                    .await
            {
                tracing::warn!("Failed to persist assistant message: {}", e);
            }
        });
    }

    /// Sync the provider's response chain ID to the thread and DB metadata.
    ///
    /// Call after a successful agentic loop to persist the latest
    /// `previous_response_id` so chaining survives restarts.
    fn persist_response_chain(&self, thread: &mut crate::agent::session::Thread) {
        let tid = thread.id.to_string();
        let response_id = match self.llm().get_response_chain_id(&tid) {
            Some(rid) => rid,
            None => return,
        };

        // Update in-memory thread
        thread.last_response_id = Some(response_id.clone());

        // Fire-and-forget DB write
        let store = match self.store() {
            Some(s) => Arc::clone(s),
            None => return,
        };
        let thread_id = thread.id;
        tokio::spawn(async move {
            let val = serde_json::json!(response_id);
            if let Err(e) = store
                .update_conversation_metadata_field(thread_id, "last_response_id", &val)
                .await
            {
                tracing::warn!(
                    "Failed to persist response chain for thread {}: {}",
                    thread_id,
                    e
                );
            }
        });
    }

    /// Run the agentic loop: call LLM, execute tools, repeat until text response.
    ///
    /// Returns `AgenticLoopResult::Response` on completion, or
    /// `AgenticLoopResult::NeedApproval` if a tool requires user approval.
    ///
    /// When `resume_after_tool` is true the loop already knows a tool was
    /// executed earlier in this turn (e.g. an approved tool), so it won't
    /// force the LLM to use tools if it responds with text.
    async fn run_agentic_loop(
        &self,
        message: &IncomingMessage,
        session: Arc<Mutex<Session>>,
        thread_id: Uuid,
        initial_messages: Vec<ChatMessage>,
        resume_after_tool: bool,
    ) -> Result<AgenticLoopResult, Error> {
        // Load workspace system prompt (identity files + optional MEMORY.md in main session)
        let system_prompt = if let Some(ws) = self.workspace() {
            let include_memory = is_main_session(&message.channel);
            let logseq_context = self
                .config
                .logseq
                .as_ref()
                .map(|c| crate::workspace::load_logseq_context(c, &self.config.name))
                .unwrap_or_default();
            let logseq_opt = if logseq_context.is_empty() {
                None
            } else {
                Some(logseq_context.as_str())
            };

            // Load active learnings for main sessions
            let learnings_context = if include_memory {
                if let Some(ref repo) = self.deps.learning_repo {
                    repo.format_for_prompt(&message.user_id, &self.config.agent_id, 15)
                        .await
                        .unwrap_or_default()
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            let learnings_opt = if learnings_context.is_empty() {
                None
            } else {
                Some(learnings_context.as_str())
            };

            match ws
                .system_prompt_with_learnings(include_memory, logseq_opt, learnings_opt)
                .await
            {
                Ok(prompt) if !prompt.is_empty() => Some(prompt),
                Ok(_) => None,
                Err(e) => {
                    tracing::debug!("Could not load workspace system prompt: {}", e);
                    None
                }
            }
        } else {
            None
        };

        // Build skills prompt (progressive disclosure — only name + description in system prompt)
        let skills_prompt = {
            let load_opts = self.config.skills.to_load_options(None);
            let snapshot = crate::skills::build_skill_snapshot(&load_opts);
            if !snapshot.skills.is_empty() {
                tracing::debug!(
                    "Loaded {} skills: {}",
                    snapshot.skills.len(),
                    snapshot
                        .skills
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if snapshot.prompt.is_empty() {
                None
            } else {
                Some(snapshot.prompt)
            }
        };

        // Combine workspace system prompt + skills prompt
        let system_prompt = match (system_prompt, skills_prompt) {
            (Some(ws), Some(sk)) => Some(format!("{}\n\n{}", ws, sk)),
            (Some(ws), None) => Some(ws),
            (None, Some(sk)) => Some(sk),
            (None, None) => None,
        };

        let mut reasoning = Reasoning::new(self.llm().clone(), self.safety().clone());
        if let Some(prompt) = system_prompt {
            reasoning = reasoning.with_system_prompt(prompt);
        }

        // Build context with messages that we'll mutate during the loop
        let mut context_messages = initial_messages;

        // Create a JobContext for tool execution (chat doesn't have a real job)
        let job_ctx = JobContext::with_user(&message.user_id, "chat", "Interactive chat session");

        const MAX_TOOL_ITERATIONS: usize = 10;
        let mut iteration = 0;
        let mut tools_executed = resume_after_tool;

        loop {
            iteration += 1;
            if iteration > MAX_TOOL_ITERATIONS {
                return Err(crate::error::LlmError::InvalidResponse {
                    provider: "agent".to_string(),
                    reason: format!("Exceeded maximum tool iterations ({})", MAX_TOOL_ITERATIONS),
                }
                .into());
            }

            // Check if interrupted
            {
                let sess = session.lock().await;
                if let Some(thread) = sess.threads.get(&thread_id)
                    && thread.state == ThreadState::Interrupted
                {
                    return Err(crate::error::JobError::ContextError {
                        id: thread_id,
                        reason: "Interrupted".to_string(),
                    }
                    .into());
                }
            }

            // Refresh tool definitions each iteration so newly built tools become visible
            let tool_defs = self.tools().tool_definitions().await;

            // Call LLM with current context
            let context = ReasoningContext::new()
                .with_messages(context_messages.clone())
                .with_tools(tool_defs)
                .with_metadata({
                    let mut m = std::collections::HashMap::new();
                    m.insert("thread_id".to_string(), thread_id.to_string());
                    m
                });

            let output = reasoning.respond_with_tools(&context).await?;

            // Track token usage for budget enforcement
            tracing::debug!(
                "LLM call used {} input + {} output tokens",
                output.usage.input_tokens,
                output.usage.output_tokens
            );

            match output.result {
                RespondResult::Text(text) => {
                    // If no tools have been executed yet, prompt the LLM to use tools
                    // This handles the case where the model explains what it will do
                    // instead of actually calling tools
                    if !tools_executed && iteration < 3 {
                        tracing::debug!(
                            "No tools executed yet (iteration {}), prompting for tool use",
                            iteration
                        );
                        context_messages.push(ChatMessage::assistant(&text));
                        context_messages.push(ChatMessage::user(
                            "Please proceed and use the available tools to complete this task.",
                        ));
                        continue;
                    }

                    // Tools have been executed or we've tried multiple times, return response
                    return Ok(AgenticLoopResult::Response(text));
                }
                RespondResult::ToolCalls {
                    tool_calls,
                    content,
                } => {
                    tools_executed = true;

                    // Add the assistant message with tool_calls to context.
                    // OpenAI protocol requires this before tool-result messages.
                    context_messages.push(ChatMessage::assistant_with_tool_calls(
                        content,
                        tool_calls.clone(),
                    ));

                    // Execute tools and add results to context
                    let _ = self
                        .channels
                        .send_status(
                            &message.channel,
                            StatusUpdate::Thinking(format!(
                                "Executing {} tool(s)...",
                                tool_calls.len()
                            )),
                            &message.metadata,
                        )
                        .await;

                    // Record tool calls in the thread
                    {
                        let mut sess = session.lock().await;
                        if let Some(thread) = sess.threads.get_mut(&thread_id)
                            && let Some(turn) = thread.last_turn_mut()
                        {
                            for tc in &tool_calls {
                                turn.record_tool_call(&tc.name, tc.arguments.clone());
                            }
                        }
                    }

                    // Execute each tool (with approval checking)
                    for tc in tool_calls {
                        // Check if tool requires approval
                        if let Some(tool) = self.tools().get(&tc.name).await
                            && tool.requires_approval()
                        {
                            // Check if auto-approved for this session
                            let mut is_auto_approved = {
                                let sess = session.lock().await;
                                sess.is_tool_auto_approved(&tc.name)
                            };

                            // For shell commands, override auto-approval for
                            // destructive patterns that should always require
                            // explicit per-invocation approval.
                            if is_auto_approved
                                && tc.name == "shell"
                                && let Some(cmd) = tc
                                    .arguments
                                    .get("command")
                                    .and_then(|c| c.as_str().map(String::from))
                                    .or_else(|| {
                                        tc.arguments
                                            .as_str()
                                            .and_then(|s| {
                                                serde_json::from_str::<serde_json::Value>(s).ok()
                                            })
                                            .and_then(|v| {
                                                v.get("command")
                                                    .and_then(|c| c.as_str().map(String::from))
                                            })
                                    })
                                && crate::tools::builtin::shell::requires_explicit_approval(&cmd)
                            {
                                tracing::info!(
                                    "Shell command '{}' requires explicit approval despite auto-approve",
                                    cmd.chars().take(80).collect::<String>()
                                );
                                is_auto_approved = false;
                            }

                            if !is_auto_approved {
                                // Need approval - store pending request and return
                                let pending = PendingApproval {
                                    request_id: Uuid::new_v4(),
                                    tool_name: tc.name.clone(),
                                    parameters: tc.arguments.clone(),
                                    description: tool.description().to_string(),
                                    tool_call_id: tc.id.clone(),
                                    context_messages: context_messages.clone(),
                                };

                                return Ok(AgenticLoopResult::NeedApproval { pending });
                            }
                        }

                        let _ = self
                            .channels
                            .send_status(
                                &message.channel,
                                StatusUpdate::ToolStarted {
                                    name: tc.name.clone(),
                                },
                                &message.metadata,
                            )
                            .await;

                        let tool_result = self
                            .execute_chat_tool(&tc.name, &tc.arguments, &job_ctx)
                            .await;

                        let _ = self
                            .channels
                            .send_status(
                                &message.channel,
                                StatusUpdate::ToolCompleted {
                                    name: tc.name.clone(),
                                    success: tool_result.is_ok(),
                                },
                                &message.metadata,
                            )
                            .await;

                        if let Ok(ref output) = tool_result
                            && !output.is_empty()
                        {
                            let _ = self
                                .channels
                                .send_status(
                                    &message.channel,
                                    StatusUpdate::ToolResult {
                                        name: tc.name.clone(),
                                        preview: output.clone(),
                                    },
                                    &message.metadata,
                                )
                                .await;
                        }

                        // Record result in thread
                        {
                            let mut sess = session.lock().await;
                            if let Some(thread) = sess.threads.get_mut(&thread_id)
                                && let Some(turn) = thread.last_turn_mut()
                            {
                                match &tool_result {
                                    Ok(output) => {
                                        turn.record_tool_result(serde_json::json!(output));
                                    }
                                    Err(e) => {
                                        turn.record_tool_error(e.to_string());
                                    }
                                }
                            }
                        }

                        // If tool_auth returned awaiting_token, enter auth mode
                        // and short-circuit: return the instructions directly so
                        // the LLM doesn't get a chance to hallucinate tool calls.
                        if let Some((ext_name, instructions)) =
                            detect_auth_awaiting(&tc.name, &tool_result)
                        {
                            let auth_data = parse_auth_result(&tool_result);
                            {
                                let mut sess = session.lock().await;
                                if let Some(thread) = sess.threads.get_mut(&thread_id) {
                                    thread.enter_auth_mode(ext_name.clone());
                                }
                            }
                            let _ = self
                                .channels
                                .send_status(
                                    &message.channel,
                                    StatusUpdate::AuthRequired {
                                        extension_name: ext_name,
                                        instructions: Some(instructions.clone()),
                                        auth_url: auth_data.auth_url,
                                        setup_url: auth_data.setup_url,
                                    },
                                    &message.metadata,
                                )
                                .await;
                            return Ok(AgenticLoopResult::Response(instructions));
                        }

                        // Add tool result to context for next LLM call
                        let result_content = match tool_result {
                            Ok(output) => {
                                // Sanitize output before showing to LLM
                                let sanitized =
                                    self.safety().sanitize_tool_output(&tc.name, &output);
                                self.safety().wrap_for_llm(
                                    &tc.name,
                                    &sanitized.content,
                                    sanitized.was_modified,
                                )
                            }
                            Err(e) => format!("Error: {}", e),
                        };

                        context_messages.push(ChatMessage::tool_result(
                            &tc.id,
                            &tc.name,
                            result_content,
                        ));
                    }
                }
            }
        }
    }

    /// Execute a tool for chat (without full job context).
    async fn execute_chat_tool(
        &self,
        tool_name: &str,
        params: &serde_json::Value,
        job_ctx: &JobContext,
    ) -> Result<String, Error> {
        let tool =
            self.tools()
                .get(tool_name)
                .await
                .ok_or_else(|| crate::error::ToolError::NotFound {
                    name: tool_name.to_string(),
                })?;

        // Validate tool parameters
        let validation = self.safety().validator().validate_tool_params(params);
        if !validation.is_valid {
            let details = validation
                .errors
                .iter()
                .map(|e| format!("{}: {}", e.field, e.message))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(crate::error::ToolError::InvalidParameters {
                name: tool_name.to_string(),
                reason: format!("Invalid tool parameters: {}", details),
            }
            .into());
        }

        tracing::debug!(
            tool = %tool_name,
            params = %params,
            "Tool call started"
        );

        // Execute with per-tool timeout
        let timeout = tool.execution_timeout();
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(timeout, async {
            tool.execute(params.clone(), job_ctx).await
        })
        .await;
        let elapsed = start.elapsed();

        match &result {
            Ok(Ok(output)) => {
                let result_str = serde_json::to_string(&output.result)
                    .unwrap_or_else(|_| "<serialize error>".to_string());
                tracing::debug!(
                    tool = %tool_name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    result = %result_str,
                    "Tool call succeeded"
                );
            }
            Ok(Err(e)) => {
                tracing::debug!(
                    tool = %tool_name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    error = %e,
                    "Tool call failed"
                );
            }
            Err(_) => {
                tracing::debug!(
                    tool = %tool_name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    timeout_secs = timeout.as_secs(),
                    "Tool call timed out"
                );
            }
        }

        let result = result
            .map_err(|_| crate::error::ToolError::Timeout {
                name: tool_name.to_string(),
                timeout,
            })?
            .map_err(|e| crate::error::ToolError::ExecutionFailed {
                name: tool_name.to_string(),
                reason: e.to_string(),
            })?;

        // Convert result to string
        serde_json::to_string_pretty(&result.result).map_err(|e| {
            crate::error::ToolError::ExecutionFailed {
                name: tool_name.to_string(),
                reason: format!("Failed to serialize result: {}", e),
            }
            .into()
        })
    }

    /// Handle job-related intents without turn tracking.
    async fn handle_job_or_command(
        &self,
        intent: MessageIntent,
        message: &IncomingMessage,
    ) -> Result<SubmissionResult, Error> {
        // Send thinking status for non-trivial operations
        if let MessageIntent::CreateJob { .. } = &intent {
            let _ = self
                .channels
                .send_status(
                    &message.channel,
                    StatusUpdate::Thinking("Processing...".into()),
                    &message.metadata,
                )
                .await;
        }

        let response = match intent {
            MessageIntent::CreateJob {
                title,
                description,
                category,
            } => {
                self.handle_create_job(&message.user_id, title, description, category)
                    .await?
            }
            MessageIntent::CheckJobStatus { job_id } => {
                self.handle_check_status(&message.user_id, job_id).await?
            }
            MessageIntent::CancelJob { job_id } => {
                self.handle_cancel_job(&message.user_id, &job_id).await?
            }
            MessageIntent::ListJobs { filter } => {
                self.handle_list_jobs(&message.user_id, filter).await?
            }
            MessageIntent::HelpJob { job_id } => {
                self.handle_help_job(&message.user_id, &job_id).await?
            }
            MessageIntent::Command { command, args } => {
                match self.handle_command(&command, &args).await? {
                    Some(s) => s,
                    None => return Ok(SubmissionResult::Ok { message: None }), // Shutdown signal
                }
            }
            _ => "Unknown intent".to_string(),
        };
        Ok(SubmissionResult::response(response))
    }

    async fn process_undo(
        &self,
        session: Arc<Mutex<Session>>,
        thread_id: Uuid,
    ) -> Result<SubmissionResult, Error> {
        let undo_mgr = self.session_manager.get_undo_manager(thread_id).await;
        let mut mgr = undo_mgr.lock().await;

        if !mgr.can_undo() {
            return Ok(SubmissionResult::ok_with_message("Nothing to undo."));
        }

        let mut sess = session.lock().await;
        let thread = sess
            .threads
            .get_mut(&thread_id)
            .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;

        // Save current state to redo, get previous checkpoint
        let current_messages = thread.messages();
        let current_turn = thread.turn_number();

        if let Some(checkpoint) = mgr.undo(current_turn, current_messages) {
            // Extract values before consuming the reference
            let turn_number = checkpoint.turn_number;
            let messages = checkpoint.messages.clone();
            let undo_count = mgr.undo_count();
            // Restore thread from checkpoint
            thread.restore_from_messages(messages);
            Ok(SubmissionResult::ok_with_message(format!(
                "Undone to turn {}. {} undo(s) remaining.",
                turn_number, undo_count
            )))
        } else {
            Ok(SubmissionResult::error("Undo failed."))
        }
    }

    async fn process_redo(
        &self,
        session: Arc<Mutex<Session>>,
        thread_id: Uuid,
    ) -> Result<SubmissionResult, Error> {
        let undo_mgr = self.session_manager.get_undo_manager(thread_id).await;
        let mut mgr = undo_mgr.lock().await;

        if !mgr.can_redo() {
            return Ok(SubmissionResult::ok_with_message("Nothing to redo."));
        }

        // Capture current state before redo so redo() can save it to undo stack
        let (current_turn, current_messages) = {
            let sess = session.lock().await;
            let thread = sess
                .threads
                .get(&thread_id)
                .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;
            (thread.turn_number(), thread.messages().to_vec())
        };

        if let Some(checkpoint) = mgr.redo(current_turn, current_messages) {
            let mut sess = session.lock().await;
            let thread = sess
                .threads
                .get_mut(&thread_id)
                .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;
            thread.restore_from_messages(checkpoint.messages);
            Ok(SubmissionResult::ok_with_message(format!(
                "Redone to turn {}.",
                checkpoint.turn_number
            )))
        } else {
            Ok(SubmissionResult::error("Redo failed."))
        }
    }

    async fn process_interrupt(
        &self,
        session: Arc<Mutex<Session>>,
        thread_id: Uuid,
    ) -> Result<SubmissionResult, Error> {
        let mut sess = session.lock().await;
        let thread = sess
            .threads
            .get_mut(&thread_id)
            .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;

        match thread.state {
            ThreadState::Processing | ThreadState::AwaitingApproval => {
                thread.interrupt();
                Ok(SubmissionResult::ok_with_message("Interrupted."))
            }
            _ => Ok(SubmissionResult::ok_with_message("Nothing to interrupt.")),
        }
    }

    async fn process_compact(
        &self,
        session: Arc<Mutex<Session>>,
        thread_id: Uuid,
    ) -> Result<SubmissionResult, Error> {
        let mut sess = session.lock().await;
        let thread = sess
            .threads
            .get_mut(&thread_id)
            .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;

        let messages = thread.messages();
        let usage = self.context_monitor.usage_percent(&messages);
        let strategy = self
            .context_monitor
            .suggest_compaction(&messages)
            .unwrap_or(
                crate::agent::context_monitor::CompactionStrategy::Summarize { keep_recent: 5 },
            );

        let compactor = ContextCompactor::new(self.llm().clone());
        match compactor
            .compact(thread, strategy, self.workspace().map(|w| w.as_ref()))
            .await
        {
            Ok(result) => {
                let mut msg = format!(
                    "Compacted: {} turns removed, {} → {} tokens (was {:.1}% full)",
                    result.turns_removed, result.tokens_before, result.tokens_after, usage
                );
                if result.summary_written {
                    msg.push_str(", summary saved to workspace");
                }
                Ok(SubmissionResult::ok_with_message(msg))
            }
            Err(e) => Ok(SubmissionResult::error(format!("Compaction failed: {}", e))),
        }
    }

    async fn process_clear(
        &self,
        session: Arc<Mutex<Session>>,
        thread_id: Uuid,
    ) -> Result<SubmissionResult, Error> {
        let mut sess = session.lock().await;
        let thread = sess
            .threads
            .get_mut(&thread_id)
            .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;
        thread.turns.clear();
        thread.state = ThreadState::Idle;

        // Clear undo history too
        let undo_mgr = self.session_manager.get_undo_manager(thread_id).await;
        undo_mgr.lock().await.clear();

        Ok(SubmissionResult::ok_with_message("Thread cleared."))
    }

    /// Process an approval or rejection of a pending tool execution.
    async fn process_approval(
        &self,
        message: &IncomingMessage,
        session: Arc<Mutex<Session>>,
        thread_id: Uuid,
        request_id: Option<Uuid>,
        approved: bool,
        always: bool,
    ) -> Result<SubmissionResult, Error> {
        // Get thread state and pending approval
        let (_thread_state, pending) = {
            let mut sess = session.lock().await;
            let thread = sess
                .threads
                .get_mut(&thread_id)
                .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;

            if thread.state != ThreadState::AwaitingApproval {
                return Ok(SubmissionResult::error("No pending approval request."));
            }

            let pending = thread.take_pending_approval();
            (thread.state, pending)
        };

        let pending = match pending {
            Some(p) => p,
            None => return Ok(SubmissionResult::error("No pending approval request.")),
        };

        // Verify request ID if provided
        if let Some(req_id) = request_id
            && req_id != pending.request_id
        {
            // Put it back and return error
            let mut sess = session.lock().await;
            if let Some(thread) = sess.threads.get_mut(&thread_id) {
                thread.await_approval(pending);
            }
            return Ok(SubmissionResult::error(
                "Request ID mismatch. Use the correct request ID.",
            ));
        }

        if approved {
            // If always, add to auto-approved set
            if always {
                let mut sess = session.lock().await;
                sess.auto_approve_tool(&pending.tool_name);
                tracing::info!(
                    "Auto-approved tool '{}' for session {}",
                    pending.tool_name,
                    sess.id
                );
            }

            // Reset thread state to processing
            {
                let mut sess = session.lock().await;
                if let Some(thread) = sess.threads.get_mut(&thread_id) {
                    thread.state = ThreadState::Processing;
                }
            }

            // Execute the approved tool and continue the loop
            let job_ctx =
                JobContext::with_user(&message.user_id, "chat", "Interactive chat session");

            let _ = self
                .channels
                .send_status(
                    &message.channel,
                    StatusUpdate::ToolStarted {
                        name: pending.tool_name.clone(),
                    },
                    &message.metadata,
                )
                .await;

            let tool_result = self
                .execute_chat_tool(&pending.tool_name, &pending.parameters, &job_ctx)
                .await;

            let _ = self
                .channels
                .send_status(
                    &message.channel,
                    StatusUpdate::ToolCompleted {
                        name: pending.tool_name.clone(),
                        success: tool_result.is_ok(),
                    },
                    &message.metadata,
                )
                .await;

            if let Ok(ref output) = tool_result
                && !output.is_empty()
            {
                let _ = self
                    .channels
                    .send_status(
                        &message.channel,
                        StatusUpdate::ToolResult {
                            name: pending.tool_name.clone(),
                            preview: output.clone(),
                        },
                        &message.metadata,
                    )
                    .await;
            }

            // Build context including the tool result
            let mut context_messages = pending.context_messages;

            // Record result in thread
            {
                let mut sess = session.lock().await;
                if let Some(thread) = sess.threads.get_mut(&thread_id)
                    && let Some(turn) = thread.last_turn_mut()
                {
                    match &tool_result {
                        Ok(output) => {
                            turn.record_tool_result(serde_json::json!(output));
                        }
                        Err(e) => {
                            turn.record_tool_error(e.to_string());
                        }
                    }
                }
            }

            // If tool_auth returned awaiting_token, enter auth mode and
            // return instructions directly (skip agentic loop continuation).
            if let Some((ext_name, instructions)) =
                detect_auth_awaiting(&pending.tool_name, &tool_result)
            {
                let auth_data = parse_auth_result(&tool_result);
                {
                    let mut sess = session.lock().await;
                    if let Some(thread) = sess.threads.get_mut(&thread_id) {
                        thread.enter_auth_mode(ext_name.clone());
                        thread.complete_turn(&instructions);
                    }
                }
                let _ = self
                    .channels
                    .send_status(
                        &message.channel,
                        StatusUpdate::AuthRequired {
                            extension_name: ext_name,
                            instructions: Some(instructions.clone()),
                            auth_url: auth_data.auth_url,
                            setup_url: auth_data.setup_url,
                        },
                        &message.metadata,
                    )
                    .await;
                return Ok(SubmissionResult::response(instructions));
            }

            // Add tool result to context
            let result_content = match tool_result {
                Ok(output) => {
                    let sanitized = self
                        .safety()
                        .sanitize_tool_output(&pending.tool_name, &output);
                    self.safety().wrap_for_llm(
                        &pending.tool_name,
                        &sanitized.content,
                        sanitized.was_modified,
                    )
                }
                Err(e) => format!("Error: {}", e),
            };

            context_messages.push(ChatMessage::tool_result(
                &pending.tool_call_id,
                &pending.tool_name,
                result_content,
            ));

            // Continue the agentic loop (a tool was already executed this turn)
            let result = self
                .run_agentic_loop(message, session.clone(), thread_id, context_messages, true)
                .await;

            // Handle the result
            let mut sess = session.lock().await;
            let thread = sess
                .threads
                .get_mut(&thread_id)
                .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;

            match result {
                Ok(AgenticLoopResult::Response(response)) => {
                    thread.complete_turn(&response);
                    self.persist_response_chain(thread);
                    let _ = self
                        .channels
                        .send_status(
                            &message.channel,
                            StatusUpdate::Status("Done".into()),
                            &message.metadata,
                        )
                        .await;
                    Ok(SubmissionResult::response(response))
                }
                Ok(AgenticLoopResult::NeedApproval {
                    pending: new_pending,
                }) => {
                    let request_id = new_pending.request_id;
                    let tool_name = new_pending.tool_name.clone();
                    let description = new_pending.description.clone();
                    let parameters = new_pending.parameters.clone();
                    thread.await_approval(new_pending);
                    let _ = self
                        .channels
                        .send_status(
                            &message.channel,
                            StatusUpdate::Status("Awaiting approval".into()),
                            &message.metadata,
                        )
                        .await;
                    Ok(SubmissionResult::NeedApproval {
                        request_id,
                        tool_name,
                        description,
                        parameters,
                    })
                }
                Err(e) => {
                    thread.fail_turn(e.to_string());
                    Ok(SubmissionResult::error(e.to_string()))
                }
            }
        } else {
            // Rejected - clear approval and return to idle
            {
                let mut sess = session.lock().await;
                if let Some(thread) = sess.threads.get_mut(&thread_id) {
                    thread.clear_pending_approval();
                }
            }

            let _ = self
                .channels
                .send_status(
                    &message.channel,
                    StatusUpdate::Status("Rejected".into()),
                    &message.metadata,
                )
                .await;

            Ok(SubmissionResult::response(format!(
                "Tool '{}' was rejected. The agent will not execute this tool.\n\n\
                 You can continue the conversation or try a different approach.",
                pending.tool_name
            )))
        }
    }

    /// Handle an auth token submitted while the thread is in auth mode.
    ///
    /// The token goes directly to the extension manager's credential store,
    /// completely bypassing logging, turn creation, history, and compaction.
    async fn process_auth_token(
        &self,
        message: &IncomingMessage,
        pending: &crate::agent::session::PendingAuth,
        token: &str,
        session: Arc<Mutex<Session>>,
        thread_id: Uuid,
    ) -> Result<Option<String>, Error> {
        let token = token.trim();

        // Clear auth mode regardless of outcome
        {
            let mut sess = session.lock().await;
            if let Some(thread) = sess.threads.get_mut(&thread_id) {
                thread.pending_auth = None;
            }
        }

        let ext_mgr = match self.deps.extension_manager.as_ref() {
            Some(mgr) => mgr,
            None => return Ok(Some("Extension manager not available.".to_string())),
        };

        match ext_mgr.auth(&pending.extension_name, Some(token)).await {
            Ok(result) if result.status == "authenticated" => {
                tracing::info!(
                    "Extension '{}' authenticated via auth mode",
                    pending.extension_name
                );

                // Auto-activate so tools are available immediately after auth
                match ext_mgr.activate(&pending.extension_name).await {
                    Ok(activate_result) => {
                        let tool_count = activate_result.tools_loaded.len();
                        let tool_list = if activate_result.tools_loaded.is_empty() {
                            String::new()
                        } else {
                            format!("\n\nTools: {}", activate_result.tools_loaded.join(", "))
                        };
                        let msg = format!(
                            "{} authenticated and activated ({} tools loaded).{}",
                            pending.extension_name, tool_count, tool_list
                        );
                        let _ = self
                            .channels
                            .send_status(
                                &message.channel,
                                StatusUpdate::AuthCompleted {
                                    extension_name: pending.extension_name.clone(),
                                    success: true,
                                    message: msg.clone(),
                                },
                                &message.metadata,
                            )
                            .await;
                        Ok(Some(msg))
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Extension '{}' authenticated but activation failed: {}",
                            pending.extension_name,
                            e
                        );
                        let msg = format!(
                            "{} authenticated successfully, but activation failed: {}. \
                             Try activating manually.",
                            pending.extension_name, e
                        );
                        let _ = self
                            .channels
                            .send_status(
                                &message.channel,
                                StatusUpdate::AuthCompleted {
                                    extension_name: pending.extension_name.clone(),
                                    success: true,
                                    message: msg.clone(),
                                },
                                &message.metadata,
                            )
                            .await;
                        Ok(Some(msg))
                    }
                }
            }
            Ok(result) => {
                // Invalid token, re-enter auth mode
                {
                    let mut sess = session.lock().await;
                    if let Some(thread) = sess.threads.get_mut(&thread_id) {
                        thread.enter_auth_mode(pending.extension_name.clone());
                    }
                }
                let msg = result
                    .instructions
                    .clone()
                    .unwrap_or_else(|| "Invalid token. Please try again.".to_string());
                // Re-emit AuthRequired so web UI re-shows the card
                let _ = self
                    .channels
                    .send_status(
                        &message.channel,
                        StatusUpdate::AuthRequired {
                            extension_name: pending.extension_name.clone(),
                            instructions: Some(msg.clone()),
                            auth_url: result.auth_url,
                            setup_url: result.setup_url,
                        },
                        &message.metadata,
                    )
                    .await;
                Ok(Some(msg))
            }
            Err(e) => {
                let msg = format!(
                    "Authentication failed for {}: {}",
                    pending.extension_name, e
                );
                let _ = self
                    .channels
                    .send_status(
                        &message.channel,
                        StatusUpdate::AuthCompleted {
                            extension_name: pending.extension_name.clone(),
                            success: false,
                            message: msg.clone(),
                        },
                        &message.metadata,
                    )
                    .await;
                Ok(Some(msg))
            }
        }
    }

    /// Run one silent LLM turn to remind the model to write durable memory (pre-compaction).
    /// Maximum iterations for memory flush tool calls (prevents runaway).
    const MEMORY_FLUSH_MAX_ITERATIONS: usize = 3;

    async fn run_memory_flush_turn(
        &self,
        flush_cfg: &MemoryFlushConfig,
        user_id: &str,
    ) -> Result<(), Error> {
        // Memory tools available during flush (no shell, no file, no HTTP).
        let tool_defs = self
            .tools()
            .tool_definitions_for(&[
                "memory_write",
                "memory_read",
                "memory_search",
                "memory_append",
            ])
            .await;

        let reasoning = Reasoning::new(self.llm().clone(), self.safety().clone())
            .with_system_prompt(flush_cfg.system_prompt.clone());
        let mut messages = vec![ChatMessage::user(&flush_cfg.prompt)];
        let job_ctx = JobContext::with_user(user_id, "memory_flush", "Pre-compaction memory flush");

        for iteration in 0..Self::MEMORY_FLUSH_MAX_ITERATIONS {
            let context = ReasoningContext::new()
                .with_messages(messages.clone())
                .with_tools(tool_defs.clone());

            let output = reasoning.respond_with_tools(&context).await?;

            match &output.result {
                RespondResult::Text(text) => {
                    if text.trim() == "NO_REPLY" {
                        tracing::debug!("Memory flush: model had nothing to store");
                    } else if !text.is_empty() {
                        tracing::debug!(
                            "Memory flush response (iter {}): {}",
                            iteration,
                            truncate_for_preview(text, 200)
                        );
                    }
                    return Ok(());
                }
                RespondResult::ToolCalls { tool_calls, content } => {
                    if let Some(text) = content {
                        tracing::debug!(
                            "Memory flush text (iter {}): {}",
                            iteration,
                            truncate_for_preview(text, 200)
                        );
                    }

                    // Execute each tool call and collect results
                    for tc in tool_calls {
                        let result = self
                            .execute_chat_tool(&tc.name, &tc.arguments, &job_ctx)
                            .await;
                        let result_content = match result {
                            Ok(output) => output,
                            Err(e) => format!("Error: {}", e),
                        };
                        tracing::debug!(
                            "Memory flush tool {}(iter {}): {}",
                            tc.name,
                            iteration,
                            truncate_for_preview(&result_content, 100)
                        );
                        messages.push(ChatMessage::tool_result(&tc.id, &tc.name, result_content));
                    }
                }
            }
        }

        tracing::debug!("Memory flush reached max iterations ({})", Self::MEMORY_FLUSH_MAX_ITERATIONS);
        Ok(())
    }

    /// Run BOOT.md instructions on startup (if present in workspace).
    ///
    /// Reads BOOT.md content and executes it as a single agent turn with
    /// full tool access. Output is suppressed (NO_REPLY expected).
    async fn run_boot_if_present(&self, user_id: &str) -> Result<(), Error> {
        let workspace = match self.workspace() {
            Some(ws) => ws,
            None => return Ok(()),
        };

        let boot_content = match workspace.read("BOOT.md").await {
            Ok(doc) if !doc.content.trim().is_empty() => doc.content,
            _ => return Ok(()),
        };

        tracing::info!("Running BOOT.md startup instructions");

        let system_prompt = workspace
            .system_prompt(true, None)
            .await
            .unwrap_or_else(|_| "You are a helpful assistant.".to_string());

        let reasoning = Reasoning::new(self.llm().clone(), self.safety().clone())
            .with_system_prompt(system_prompt);

        let boot_prompt = format!(
            "Execute the following startup instructions. When done, reply with NO_REPLY.\n\n{}",
            boot_content
        );
        let messages = vec![ChatMessage::user(&boot_prompt)];

        // Full tool access for boot instructions (same trust as AGENTS.md)
        let tools = self.tools().tool_definitions().await;

        let context = ReasoningContext::new()
            .with_messages(messages)
            .with_tools(tools);

        match reasoning.respond_with_tools(&context).await {
            Ok(output) => {
                let text = match &output.result {
                    RespondResult::Text(s) => s.clone(),
                    RespondResult::ToolCalls { content, .. } => {
                        content.clone().unwrap_or_default()
                    }
                };
                if text.trim() != "NO_REPLY" && !text.is_empty() {
                    tracing::info!(
                        "BOOT.md output: {}",
                        truncate_for_preview(&text, 200)
                    );
                }
                tracing::info!("BOOT.md startup instructions completed");
            }
            Err(e) => {
                tracing::warn!("BOOT.md execution failed: {}", e);
            }
        }

        // Audit log
        tracing::info!(
            target: "audit",
            command = "boot",
            user = user_id,
            "Ran BOOT.md startup instructions"
        );

        Ok(())
    }

    /// Save the current thread's last N messages to workspace when user runs /new.
    const SESSION_SAVE_MESSAGE_COUNT: usize = 15;

    async fn save_thread_to_workspace_before_new(
        &self,
        session: Arc<Mutex<Session>>,
        thread_id: Uuid,
        workspace: &Workspace,
    ) -> Result<(), Error> {
        let messages = {
            let sess = session.lock().await;
            let thread = match sess.threads.get(&thread_id) {
                Some(t) => t,
                None => return Ok(()),
            };
            let msgs = thread.messages();
            if msgs.is_empty() {
                return Ok(());
            }
            let start = msgs.len().saturating_sub(Self::SESSION_SAVE_MESSAGE_COUNT);
            msgs[start..].to_vec()
        };

        let now = chrono::Utc::now();
        let date_str = now.format("%Y-%m-%d").to_string();
        let time_str = now.format("%H%M%S").to_string();
        let path = format!("daily/{}-session-{}.md", date_str, time_str);

        let mut lines = vec![
            format!("# Session: {} UTC", now.format("%Y-%m-%d %H:%M:%S")),
            String::new(),
            format!("- **Thread ID**: {}", thread_id),
            format!("- **Source**: /new"),
            String::new(),
            "## Conversation".to_string(),
            String::new(),
        ];
        for msg in &messages {
            let role = match msg.role {
                crate::llm::Role::User => "user",
                crate::llm::Role::Assistant => "assistant",
                crate::llm::Role::System => "system",
                crate::llm::Role::Tool => "tool",
            };
            lines.push(format!("**{}**: {}", role, msg.content.trim()));
            lines.push(String::new());
        }
        let content = lines.join("\n");

        // Use content-hash dedup to prevent duplicates during cross-machine sync
        match workspace.write_dedup(&path, &content).await {
            Ok(true) => tracing::info!("Saved thread to workspace: {}", path),
            Ok(false) => tracing::debug!("Session file deduplicated (already exists): {}", path),
            Err(e) => {
                // Fall back to regular write if dedup fails (e.g., no postgres)
                tracing::debug!("Dedup write failed, falling back to regular write: {}", e);
                workspace.write(&path, &content).await.map_err(Error::from)?;
                tracing::info!("Saved thread to workspace (fallback): {}", path);
            }
        }
        Ok(())
    }

    async fn process_new_thread(
        &self,
        message: &IncomingMessage,
    ) -> Result<SubmissionResult, Error> {
        let session = self
            .session_manager
            .get_or_create_session(&message.user_id)
            .await;
        let mut sess = session.lock().await;
        let thread = sess.create_thread();
        let thread_id = thread.id;
        Ok(SubmissionResult::ok_with_message(format!(
            "New thread: {}",
            thread_id
        )))
    }

    async fn process_switch_thread(
        &self,
        message: &IncomingMessage,
        target_thread_id: Uuid,
    ) -> Result<SubmissionResult, Error> {
        let session = self
            .session_manager
            .get_or_create_session(&message.user_id)
            .await;
        let mut sess = session.lock().await;

        if sess.switch_thread(target_thread_id) {
            Ok(SubmissionResult::ok_with_message(format!(
                "Switched to thread {}",
                target_thread_id
            )))
        } else {
            Ok(SubmissionResult::error("Thread not found."))
        }
    }

    async fn process_resume(
        &self,
        session: Arc<Mutex<Session>>,
        thread_id: Uuid,
        checkpoint_id: Uuid,
    ) -> Result<SubmissionResult, Error> {
        let undo_mgr = self.session_manager.get_undo_manager(thread_id).await;
        let mut mgr = undo_mgr.lock().await;

        if let Some(checkpoint) = mgr.restore(checkpoint_id) {
            let mut sess = session.lock().await;
            let thread = sess
                .threads
                .get_mut(&thread_id)
                .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;
            thread.restore_from_messages(checkpoint.messages);
            Ok(SubmissionResult::ok_with_message(format!(
                "Resumed from checkpoint: {}",
                checkpoint.description
            )))
        } else {
            Ok(SubmissionResult::error("Checkpoint not found."))
        }
    }

    async fn handle_create_job(
        &self,
        user_id: &str,
        title: String,
        description: String,
        category: Option<String>,
    ) -> Result<String, Error> {
        // Create job context
        let job_id = self
            .context_manager
            .create_job_for_user(user_id, &title, &description)
            .await?;

        // Update category if provided
        if let Some(cat) = category {
            self.context_manager
                .update_context(job_id, |ctx| {
                    ctx.category = Some(cat);
                })
                .await?;
        }

        // Persist new job to database (fire-and-forget)
        if let Some(store) = self.store()
            && let Ok(ctx) = self.context_manager.get_context(job_id).await
        {
            let store = store.clone();
            tokio::spawn(async move {
                if let Err(e) = store.save_job(&ctx).await {
                    tracing::warn!("Failed to persist new job {}: {}", job_id, e);
                }
            });
        }

        // Schedule for execution
        self.scheduler.schedule(job_id).await?;

        Ok(format!(
            "Created job: {}\nID: {}\n\nThe job has been scheduled and is now running.",
            title, job_id
        ))
    }

    async fn handle_check_status(
        &self,
        user_id: &str,
        job_id: Option<String>,
    ) -> Result<String, Error> {
        match job_id {
            Some(id) => {
                let uuid = Uuid::parse_str(&id)
                    .map_err(|_| crate::error::JobError::NotFound { id: Uuid::nil() })?;

                let ctx = self.context_manager.get_context(uuid).await?;
                if ctx.user_id != user_id {
                    return Err(crate::error::JobError::NotFound { id: uuid }.into());
                }

                Ok(format!(
                    "Job: {}\nStatus: {:?}\nCreated: {}\nStarted: {}\nActual cost: {}",
                    ctx.title,
                    ctx.state,
                    ctx.created_at.format("%Y-%m-%d %H:%M:%S"),
                    ctx.started_at
                        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_else(|| "Not started".to_string()),
                    ctx.actual_cost
                ))
            }
            None => {
                // Show summary of all jobs
                let summary = self.context_manager.summary_for(user_id).await;
                Ok(format!(
                    "Jobs summary:\n  Total: {}\n  In Progress: {}\n  Completed: {}\n  Failed: {}\n  Stuck: {}",
                    summary.total,
                    summary.in_progress,
                    summary.completed,
                    summary.failed,
                    summary.stuck
                ))
            }
        }
    }

    async fn handle_cancel_job(&self, user_id: &str, job_id: &str) -> Result<String, Error> {
        let uuid = Uuid::parse_str(job_id)
            .map_err(|_| crate::error::JobError::NotFound { id: Uuid::nil() })?;

        let ctx = self.context_manager.get_context(uuid).await?;
        if ctx.user_id != user_id {
            return Err(crate::error::JobError::NotFound { id: uuid }.into());
        }

        self.scheduler.stop(uuid).await?;

        Ok(format!("Job {} has been cancelled.", job_id))
    }

    async fn handle_list_jobs(
        &self,
        user_id: &str,
        _filter: Option<String>,
    ) -> Result<String, Error> {
        let jobs = self.context_manager.all_jobs_for(user_id).await;

        if jobs.is_empty() {
            return Ok("No jobs found.".to_string());
        }

        let mut output = String::from("Jobs:\n");
        for job_id in jobs {
            if let Ok(ctx) = self.context_manager.get_context(job_id).await
                && ctx.user_id == user_id
            {
                output.push_str(&format!("  {} - {} ({:?})\n", job_id, ctx.title, ctx.state));
            }
        }

        Ok(output)
    }

    async fn handle_help_job(&self, user_id: &str, job_id: &str) -> Result<String, Error> {
        let uuid = Uuid::parse_str(job_id)
            .map_err(|_| crate::error::JobError::NotFound { id: Uuid::nil() })?;

        let ctx = self.context_manager.get_context(uuid).await?;
        if ctx.user_id != user_id {
            return Err(crate::error::JobError::NotFound { id: uuid }.into());
        }

        if ctx.state == crate::context::JobState::Stuck {
            // Attempt recovery
            self.context_manager
                .update_context(uuid, |ctx| ctx.attempt_recovery())
                .await?
                .map_err(|s| crate::error::JobError::ContextError {
                    id: uuid,
                    reason: s,
                })?;

            // Reschedule
            self.scheduler.schedule(uuid).await?;

            Ok(format!(
                "Job {} was stuck. Attempting recovery (attempt #{}).",
                job_id,
                ctx.repair_attempts + 1
            ))
        } else {
            Ok(format!(
                "Job {} is not stuck (current state: {:?}). No help needed.",
                job_id, ctx.state
            ))
        }
    }

    /// Trigger a manual heartbeat check.
    async fn process_heartbeat(&self) -> Result<SubmissionResult, Error> {
        let Some(workspace) = self.workspace() else {
            return Ok(SubmissionResult::error(
                "Heartbeat requires a workspace (database must be connected).",
            ));
        };

        let runner = crate::agent::HeartbeatRunner::new(
            crate::agent::HeartbeatConfig::default(),
            workspace.clone(),
            self.llm().clone(),
        );

        match runner.check_heartbeat().await {
            crate::agent::HeartbeatResult::Ok => Ok(SubmissionResult::ok_with_message(
                "Heartbeat: all clear, nothing needs attention.",
            )),
            crate::agent::HeartbeatResult::NeedsAttention(msg) => Ok(SubmissionResult::response(
                format!("Heartbeat findings:\n\n{}", msg),
            )),
            crate::agent::HeartbeatResult::Skipped => Ok(SubmissionResult::ok_with_message(
                "Heartbeat skipped: no HEARTBEAT.md checklist found in workspace.",
            )),
            crate::agent::HeartbeatResult::Failed(err) => Ok(SubmissionResult::error(format!(
                "Heartbeat failed: {}",
                err
            ))),
        }
    }

    /// Summarize the current thread's conversation.
    async fn process_summarize(
        &self,
        session: Arc<Mutex<Session>>,
        thread_id: Uuid,
    ) -> Result<SubmissionResult, Error> {
        let messages = {
            let sess = session.lock().await;
            let thread = sess
                .threads
                .get(&thread_id)
                .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;
            thread.messages()
        };

        if messages.is_empty() {
            return Ok(SubmissionResult::ok_with_message(
                "Nothing to summarize (empty thread).",
            ));
        }

        // Build a summary prompt with the conversation
        let mut context = Vec::new();
        context.push(ChatMessage::system(
            "Summarize the conversation so far in 3-5 concise bullet points. \
             Focus on decisions made, actions taken, and key outcomes. \
             Be brief and factual.",
        ));
        // Include the conversation messages (truncate to last 20 to avoid context overflow)
        let start = if messages.len() > 20 {
            messages.len() - 20
        } else {
            0
        };
        context.extend_from_slice(&messages[start..]);
        context.push(ChatMessage::user("Summarize this conversation."));

        let request = crate::llm::CompletionRequest::new(context)
            .with_max_tokens(512)
            .with_temperature(0.3);

        match self.llm().complete(request).await {
            Ok(response) => Ok(SubmissionResult::response(format!(
                "Thread Summary:\n\n{}",
                response.content.trim()
            ))),
            Err(e) => Ok(SubmissionResult::error(format!("Summarize failed: {}", e))),
        }
    }

    /// Suggest next steps based on the current thread.
    async fn process_suggest(
        &self,
        session: Arc<Mutex<Session>>,
        thread_id: Uuid,
    ) -> Result<SubmissionResult, Error> {
        let messages = {
            let sess = session.lock().await;
            let thread = sess
                .threads
                .get(&thread_id)
                .ok_or_else(|| Error::from(crate::error::JobError::NotFound { id: thread_id }))?;
            thread.messages()
        };

        if messages.is_empty() {
            return Ok(SubmissionResult::ok_with_message(
                "Nothing to suggest from (empty thread).",
            ));
        }

        let mut context = Vec::new();
        context.push(ChatMessage::system(
            "Based on the conversation so far, suggest 2-4 concrete next steps the user could take. \
             Be actionable and specific. Format as a numbered list.",
        ));
        let start = if messages.len() > 20 {
            messages.len() - 20
        } else {
            0
        };
        context.extend_from_slice(&messages[start..]);
        context.push(ChatMessage::user("What should I do next?"));

        let request = crate::llm::CompletionRequest::new(context)
            .with_max_tokens(512)
            .with_temperature(0.5);

        match self.llm().complete(request).await {
            Ok(response) => Ok(SubmissionResult::response(format!(
                "Suggested Next Steps:\n\n{}",
                response.content.trim()
            ))),
            Err(e) => Ok(SubmissionResult::error(format!("Suggest failed: {}", e))),
        }
    }

    /// Handle system commands that bypass thread-state checks entirely.
    async fn handle_system_command(
        &self,
        command: &str,
        args: &[String],
    ) -> Result<SubmissionResult, Error> {
        match command {
            "help" => Ok(SubmissionResult::response(concat!(
                "System:\n",
                "  /help             Show this help\n",
                "  /model [name]     Show or switch the active model\n",
                "  /version          Show version info\n",
                "  /tools            List available tools\n",
                "  /debug            Toggle debug mode\n",
                "  /ping             Connectivity check\n",
                "\n",
                "Jobs:\n",
                "  /job <desc>       Create a new job\n",
                "  /status [id]      Check job status\n",
                "  /cancel <id>      Cancel a job\n",
                "  /list             List all jobs\n",
                "\n",
                "Session:\n",
                "  /undo             Undo last turn\n",
                "  /redo             Redo undone turn\n",
                "  /compact          Compress context window\n",
                "  /clear            Clear current thread\n",
                "  /interrupt        Stop current operation\n",
                "  /new              New conversation thread\n",
                "  /thread <id>      Switch to thread\n",
                "  /resume <id>      Resume from checkpoint\n",
                "\n",
                "Agent:\n",
                "  /heartbeat        Run heartbeat check\n",
                "  /summarize        Summarize current thread\n",
                "  /suggest          Suggest next steps\n",
                "\n",
                "  /quit             Exit",
            ))),

            "ping" => Ok(SubmissionResult::response("pong!")),

            "version" => Ok(SubmissionResult::response(format!(
                "{} v{}",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            ))),

            "tools" => {
                let tools = self.tools().list().await;
                Ok(SubmissionResult::response(format!(
                    "Available tools: {}",
                    tools.join(", ")
                )))
            }

            "debug" => {
                // Debug toggle is handled client-side in the REPL.
                // For non-REPL channels, just acknowledge.
                Ok(SubmissionResult::ok_with_message(
                    "Debug toggle is handled by your client.",
                ))
            }

            "model" => {
                if args.is_empty() {
                    // Show current model
                    let name = self.llm().active_model_name();
                    Ok(SubmissionResult::response(format!(
                        "Active model: {}",
                        name
                    )))
                } else {
                    let requested = &args[0];

                    // Validate the model exists
                    match self.llm().list_models().await {
                        Ok(models) if !models.is_empty() => {
                            if !models.iter().any(|m| m == requested) {
                                return Ok(SubmissionResult::error(format!(
                                    "Unknown model: {}. Available models:\n  {}",
                                    requested,
                                    models.join("\n  ")
                                )));
                            }
                        }
                        Ok(_) => {
                            // Empty model list, can't validate but try anyway
                        }
                        Err(e) => {
                            tracing::warn!("Could not fetch model list for validation: {}", e);
                            // Proceed anyway, the provider will error on the next call if invalid
                        }
                    }

                    match self.llm().set_model(requested) {
                        Ok(()) => Ok(SubmissionResult::response(format!(
                            "Switched model to: {}",
                            requested
                        ))),
                        Err(e) => Ok(SubmissionResult::error(format!(
                            "Failed to switch model: {}",
                            e
                        ))),
                    }
                }
            }

            _ => Ok(SubmissionResult::error(format!(
                "Unknown command: {}. Try /help",
                command
            ))),
        }
    }

    /// Handle legacy command routing from the Router (job commands that go through
    /// process_user_input -> router -> handle_job_or_command -> here).
    async fn handle_command(
        &self,
        command: &str,
        args: &[String],
    ) -> Result<Option<String>, Error> {
        // System commands are now handled directly via Submission::SystemCommand,
        // but the router may still send us unknown /commands.
        match self.handle_system_command(command, args).await? {
            SubmissionResult::Response { content } => Ok(Some(content)),
            SubmissionResult::Ok { message } => Ok(message),
            SubmissionResult::Error { message } => Ok(Some(format!("Error: {}", message))),
            _ => Ok(None),
        }
    }
}

/// Parsed auth result fields for emitting StatusUpdate::AuthRequired.
struct ParsedAuthData {
    auth_url: Option<String>,
    setup_url: Option<String>,
}

/// Extract auth_url and setup_url from a tool_auth result JSON string.
fn parse_auth_result(result: &Result<String, Error>) -> ParsedAuthData {
    let parsed = result
        .as_ref()
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    ParsedAuthData {
        auth_url: parsed
            .as_ref()
            .and_then(|v| v.get("auth_url"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        setup_url: parsed
            .as_ref()
            .and_then(|v| v.get("setup_url"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

/// Check if a tool_auth result indicates the extension is awaiting a token.
///
/// Returns `Some((extension_name, instructions))` if the tool result contains
/// `awaiting_token: true`, meaning the thread should enter auth mode.
fn detect_auth_awaiting(
    tool_name: &str,
    result: &Result<String, Error>,
) -> Option<(String, String)> {
    if tool_name != "tool_auth" && tool_name != "tool_activate" {
        return None;
    }
    let output = result.as_ref().ok()?;
    let parsed: serde_json::Value = serde_json::from_str(output).ok()?;
    if parsed.get("awaiting_token") != Some(&serde_json::Value::Bool(true)) {
        return None;
    }
    let name = parsed.get("name")?.as_str()?.to_string();
    let instructions = parsed
        .get("instructions")
        .and_then(|v| v.as_str())
        .unwrap_or("Please provide your API token/key.")
        .to_string();
    Some((name, instructions))
}

#[cfg(test)]
mod tests {
    use super::{
        chat_tool_execution_metadata, is_single_message_repl, resolve_routine_notification_user,
        should_fallback_routine_notification, truncate_for_preview,
    };
    use crate::agent::agent_loop::{Agent, AgentDeps, HandleOutcome};
    use crate::agent::cost_guard::{CostGuard, CostGuardConfig};
    use crate::agent::submission::{AuthGateResolution, Submission};
    use crate::channels::{AttachmentKind, IncomingAttachment, IncomingMessage};
    use crate::config::{AgentConfig, SafetyConfig, SkillsConfig};
    use crate::error::ChannelError;
    use crate::hooks::HookRegistry;
    use crate::tools::ToolRegistry;
    use ironclaw_llm::{
        CompletionRequest, CompletionResponse, FinishReason, LlmProvider, ToolCompletionRequest,
        ToolCompletionResponse,
    };
    use ironclaw_safety::SafetyLayer;
    use rust_decimal::Decimal;
    use std::sync::Arc;
    use std::time::Duration;
    use uuid::Uuid;

    struct StaticLlmProvider;

    #[async_trait::async_trait]
    impl LlmProvider for StaticLlmProvider {
        fn model_name(&self) -> &str {
            "static-mock"
        }

        fn cost_per_token(&self) -> (Decimal, Decimal) {
            (Decimal::ZERO, Decimal::ZERO)
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, crate::error::LlmError> {
            Ok(CompletionResponse {
                content: "ok".to_string(),
                input_tokens: 0,
                output_tokens: 0,
                finish_reason: FinishReason::Stop,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            })
        }

        async fn complete_with_tools(
            &self,
            _request: ToolCompletionRequest,
        ) -> Result<ToolCompletionResponse, crate::error::LlmError> {
            Ok(ToolCompletionResponse {
                content: Some("ok".to_string()),
                tool_calls: Vec::new(),
                input_tokens: 0,
                output_tokens: 0,
                finish_reason: FinishReason::Stop,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                reasoning: None,
            })
        }
    }

    fn make_legacy_handle_message_test_agent() -> Agent {
        let deps = AgentDeps {
            owner_id: "default".to_string(),
            store: None,
            settings_store: None,
            llm: Arc::new(StaticLlmProvider),
            cheap_llm: None,
            safety: Arc::new(SafetyLayer::new(&SafetyConfig {
                max_output_length: 100_000,
                injection_check_enabled: true,
            })),
            tools: Arc::new(ToolRegistry::new()),
            workspace: None,
            extension_manager: None,
            skill_registry: None,
            skill_catalog: None,
            skills_config: SkillsConfig::default(),
            hooks: Arc::new(HookRegistry::new()),
            auth_manager: None,
            cost_guard: Arc::new(CostGuard::new(CostGuardConfig::default())),
            sse_tx: None,
            http_interceptor: None,
            transcription: None,
            document_extraction: None,
            sandbox_readiness: crate::agent::routine_engine::SandboxReadiness::DisabledByConfig,
            builder: None,
            llm_backend: "nearai".to_string(),
            tenant_rates: Arc::new(crate::tenant::TenantRateRegistry::new(4, 3)),
        };

        Agent::new(
            AgentConfig {
                name: "agent-loop-test-agent".to_string(),
                max_parallel_jobs: 1,
                job_timeout: Duration::from_secs(60),
                stuck_threshold: Duration::from_secs(60),
                repair_check_interval: Duration::from_secs(30),
                max_repair_attempts: 1,
                use_planning: false,
                session_idle_timeout: Duration::from_secs(300),
                allow_local_tools: false,
                max_cost_per_day_cents: None,
                max_actions_per_hour: None,
                max_cost_per_user_per_day_cents: None,
                max_tool_iterations: 50,
                auto_approve_tools: false,
                default_timezone: "UTC".to_string(),
                max_jobs_per_user: None,
                max_tokens_per_job: 0,
                multi_tenant: false,
                max_llm_concurrent_per_user: None,
                max_jobs_concurrent_per_user: None,
                engine_v2: false,
            },
            deps,
            Arc::new(crate::channels::ChannelManager::new()),
            None,
            None,
            None,
            Some(Arc::new(crate::context::ContextManager::new(1))),
            None,
        )
    }

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn store_extracted_documents_writes_to_message_user_workspace() {
        let (db, _dir) = crate::agent::test_support::make_libsql_test_db().await;
        let owner_workspace = Arc::new(crate::workspace::Workspace::new_with_db(
            "owner-scope",
            Arc::clone(&db),
        ));
        let mut agent = make_legacy_handle_message_test_agent();
        agent.deps.store = Some(Arc::clone(&db));
        agent.deps.workspace = Some(owner_workspace);

        let message = IncomingMessage::new("gateway", "alice", "uploaded a document")
            .with_attachments(vec![IncomingAttachment {
                id: "doc-1".to_string(),
                kind: AttachmentKind::Document,
                mime_type: "text/plain".to_string(),
                filename: Some("conversation-notes.txt".to_string()),
                size_bytes: Some(42),
                source_url: None,
                storage_key: None,
                local_path: None,
                extracted_text: Some("alice-only extracted conversation text".to_string()),
                data: Vec::new(),
                duration_secs: None,
            }]);

        let before_date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        agent.store_extracted_documents(&message).await;
        let after_date = chrono::Utc::now().format("%Y-%m-%d").to_string();

        let mut candidate_paths = vec![format!("documents/{before_date}/conversation-notes.txt")];
        if after_date != before_date {
            candidate_paths.push(format!("documents/{after_date}/conversation-notes.txt"));
        }

        let alice_ws = crate::workspace::Workspace::new_with_db("alice", Arc::clone(&db));
        let mut stored = None;
        for path in &candidate_paths {
            if let Ok(doc) = alice_ws.read(path).await {
                stored = Some((path.clone(), doc));
                break;
            }
        }
        let (path, alice_doc) =
            stored.expect("extracted document should be stored under the message user");
        assert!(
            alice_doc
                .content
                .contains("alice-only extracted conversation text")
        );

        let owner_ws = crate::workspace::Workspace::new_with_db("owner-scope", Arc::clone(&db));
        assert!(
            owner_ws.read(&path).await.is_err(),
            "extracted document must not be stored under the startup owner scope"
        );
    }

    #[test]
    fn test_truncate_short_input() {
        assert_eq!(truncate_for_preview("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_empty_input() {
        assert_eq!(truncate_for_preview("", 10), "");
    }

    #[test]
    fn test_truncate_exact_length() {
        assert_eq!(truncate_for_preview("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_over_limit() {
        let result = truncate_for_preview("hello world, this is long", 10);
        assert!(result.ends_with("..."));
        // "hello worl" = 10 chars + "..."
        assert_eq!(result, "hello worl...");
    }

    #[test]
    fn test_truncate_collapses_newlines() {
        let result = truncate_for_preview("line1\nline2\nline3", 100);
        assert!(!result.contains('\n'));
        assert_eq!(result, "line1 line2 line3");
    }

    #[test]
    fn test_truncate_collapses_whitespace() {
        let result = truncate_for_preview("hello   world", 100);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_truncate_multibyte_utf8() {
        // Each emoji is 4 bytes. Truncating at char boundary must not panic.
        let input = "😀😁😂🤣😃😄😅😆😉😊";
        let result = truncate_for_preview(input, 5);
        assert!(result.ends_with("..."));
        // First 5 chars = 5 emoji
        assert_eq!(result, "😀😁😂🤣😃...");
    }

    #[test]
    fn test_truncate_cjk_characters() {
        // CJK chars are 3 bytes each in UTF-8.
        let input = "你好世界测试数据很长的字符串";
        let result = truncate_for_preview(input, 4);
        assert_eq!(result, "你好世界...");
    }

    #[test]
    fn test_truncate_mixed_multibyte_and_ascii() {
        let input = "hello 世界 foo";
        let result = truncate_for_preview(input, 8);
        // 'h','e','l','l','o',' ','世','界' = 8 chars
        assert_eq!(result, "hello 世界...");
    }

    #[test]
    fn resolve_routine_notification_user_prefers_explicit_target() {
        let metadata = serde_json::json!({
            "notify_user": "12345",
            "owner_id": "owner-scope",
        });

        let resolved = resolve_routine_notification_user(&metadata);
        assert_eq!(resolved.as_deref(), Some("12345")); // safety: test-only assertion
    }

    #[test]
    fn resolve_routine_notification_user_falls_back_to_owner_scope() {
        let metadata = serde_json::json!({
            "notify_user": null,
            "owner_id": "owner-scope",
        });

        let resolved = resolve_routine_notification_user(&metadata);
        assert_eq!(resolved.as_deref(), Some("owner-scope")); // safety: test-only assertion
    }

    #[test]
    fn resolve_routine_notification_user_rejects_missing_values() {
        let metadata = serde_json::json!({
            "notify_user": "   ",
        });

        assert_eq!(resolve_routine_notification_user(&metadata), None); // safety: test-only assertion
    }

    #[test]
    fn chat_tool_execution_metadata_prefers_message_routing_target() {
        let message = IncomingMessage::new("telegram", "owner-scope", "hello")
            .with_sender_id("telegram-user")
            .with_thread("thread-7")
            .with_metadata(serde_json::json!({
                "chat_id": 424242,
                "chat_type": "private",
            }));

        let metadata = chat_tool_execution_metadata(&message);
        assert_eq!(
            metadata.get("notify_channel").and_then(|v| v.as_str()),
            Some("telegram")
        ); // safety: test-only assertion
        assert_eq!(
            metadata.get("notify_user").and_then(|v| v.as_str()),
            Some("424242")
        ); // safety: test-only assertion
        assert_eq!(
            metadata.get("notify_thread_id").and_then(|v| v.as_str()),
            Some("thread-7")
        ); // safety: test-only assertion
    }

    #[test]
    fn chat_tool_execution_metadata_falls_back_to_user_scope_without_route() {
        let message = IncomingMessage::new("gateway", "owner-scope", "hello").with_sender_id("");

        let metadata = chat_tool_execution_metadata(&message);
        assert_eq!(
            metadata.get("notify_channel").and_then(|v| v.as_str()),
            Some("gateway")
        ); // safety: test-only assertion
        assert_eq!(
            metadata.get("notify_user").and_then(|v| v.as_str()),
            Some("owner-scope")
        ); // safety: test-only assertion
        assert_eq!(
            metadata.get("notify_thread_id"),
            Some(&serde_json::Value::Null)
        ); // safety: test-only assertion
    }

    #[test]
    fn targeted_routine_notifications_do_not_fallback_without_owner_route() {
        let error = ChannelError::MissingRoutingTarget {
            name: "telegram".to_string(),
            reason: "No stored owner routing target for channel 'telegram'.".to_string(),
        };

        assert!(!should_fallback_routine_notification(&error)); // safety: test-only assertion
    }

    #[test]
    fn targeted_routine_notifications_may_fallback_for_other_errors() {
        let error = ChannelError::SendFailed {
            name: "telegram".to_string(),
            reason: "timeout talking to channel".to_string(),
        };

        assert!(should_fallback_routine_notification(&error)); // safety: test-only assertion
    }

    /// Regression: bare "yes"/"no" when thread is Idle must NOT route as
    /// approval. Exercises the `should_route_as_approval` guard that the
    /// legacy match arm uses to decide between `process_approval` and
    /// `process_user_input`.
    #[test]
    fn should_route_as_approval_rejects_bare_keywords_when_idle() {
        use super::should_route_as_approval;
        use crate::agent::session::ThreadState;

        // Bare keywords with non-approval thread states → downgrade to UserInput
        for state in [
            ThreadState::Idle,
            ThreadState::Processing,
            ThreadState::Completed,
            ThreadState::Interrupted,
        ] {
            assert!(
                !should_route_as_approval(state, "yes"),
                "bare 'yes' should not route as approval in {state:?}"
            );
            assert!(
                !should_route_as_approval(state, "no"),
                "bare 'no' should not route as approval in {state:?}"
            );
            assert!(
                !should_route_as_approval(state, "always"),
                "bare 'always' should not route as approval in {state:?}"
            );
            assert!(
                !should_route_as_approval(state, "ok"),
                "bare 'ok' should not route as approval in {state:?}"
            );
        }
    }

    /// When thread IS AwaitingApproval, bare keywords must route as approval.
    #[test]
    fn should_route_as_approval_accepts_keywords_when_awaiting() {
        use super::should_route_as_approval;
        use crate::agent::session::ThreadState;

        assert!(should_route_as_approval(
            ThreadState::AwaitingApproval,
            "yes"
        ));
        assert!(should_route_as_approval(
            ThreadState::AwaitingApproval,
            "no"
        ));
        assert!(should_route_as_approval(
            ThreadState::AwaitingApproval,
            "always"
        ));
    }

    /// Slash commands (/approve, /deny) always route as approval regardless
    /// of thread state — they are explicit user intent.
    #[test]
    fn should_route_as_approval_always_routes_slash_commands() {
        use super::should_route_as_approval;
        use crate::agent::session::ThreadState;

        assert!(should_route_as_approval(ThreadState::Idle, "/approve"));
        assert!(should_route_as_approval(ThreadState::Idle, "/deny"));
        assert!(should_route_as_approval(ThreadState::Idle, "/yes"));
        assert!(should_route_as_approval(ThreadState::Processing, "/always"));
    }

    /// The thread-resolution guard must only early-reject `ExecApproval`
    /// when no approval is pending. `ApprovalResponse` (bare keywords)
    /// must be allowed through so `should_route_as_approval` can downgrade
    /// them to `UserInput`. Regression test for the E2E timeout where
    /// bare "yes"/"no" via API never produced a response in history.
    #[test]
    fn approval_guard_rejects_exec_but_allows_bare_keywords_when_no_pending() {
        use crate::agent::submission::Submission;
        use uuid::Uuid;

        // Simulate: thread has no pending approval, submission is ApprovalResponse
        let pending_approval: Option<()> = None;
        let submission_approval_response = Submission::ApprovalResponse {
            approved: true,
            always: false,
        };

        // ApprovalResponse should NOT be blocked (falls through to downgrade)
        let blocked = pending_approval.is_none()
            && matches!(
                submission_approval_response,
                Submission::ExecApproval { .. }
            );
        assert!(
            !blocked,
            "ApprovalResponse with no pending approval must not be early-rejected"
        );

        // ExecApproval SHOULD be blocked
        let submission_exec = Submission::ExecApproval {
            request_id: Uuid::new_v4(),
            approved: true,
            always: false,
        };
        let blocked = pending_approval.is_none()
            && matches!(submission_exec, Submission::ExecApproval { .. });
        assert!(
            blocked,
            "ExecApproval with no pending approval must be early-rejected"
        );

        // When pending approval IS present, neither should be early-rejected
        let pending_approval: Option<()> = Some(());
        let submission_exec2 = Submission::ExecApproval {
            request_id: Uuid::new_v4(),
            approved: true,
            always: false,
        };
        let blocked = pending_approval.is_none()
            && matches!(submission_exec2, Submission::ExecApproval { .. });
        assert!(
            !blocked,
            "ExecApproval with pending approval must not be early-rejected"
        );
    }

    #[test]
    fn single_message_repl_detection_requires_repl_channel_and_metadata_flag() {
        let repl = IncomingMessage::new("repl", "owner-scope", "hello")
            .with_metadata(serde_json::json!({ "single_message_mode": true }));
        let gateway = IncomingMessage::new("gateway", "owner-scope", "hello")
            .with_metadata(serde_json::json!({ "single_message_mode": true }));
        let plain_repl = IncomingMessage::new("repl", "owner-scope", "hello");

        assert!(is_single_message_repl(&repl)); // safety: test-only assertion
        assert!(!is_single_message_repl(&gateway)); // safety: test-only assertion
        assert!(!is_single_message_repl(&plain_repl)); // safety: test-only assertion
    }

    #[tokio::test]
    async fn build_outgoing_response_for_thread_includes_generated_image_inline_attachments() {
        use super::build_outgoing_response_for_thread;
        use crate::agent::session::Session;
        use std::sync::Arc;

        let session: Arc<tokio::sync::Mutex<Session>> =
            Arc::new(tokio::sync::Mutex::new(Session::new("user-123")));

        let thread_id = {
            let mut sess = session.lock().await;
            let thread = sess.create_thread(None);
            let thread_id = thread.id;
            let turn = thread.start_turn("draw a cat");
            turn.record_tool_call("image_generate", serde_json::json!({ "prompt": "cat" }));
            turn.record_tool_result(serde_json::json!({
                "type": "image_generated",
                "data": "data:image/png;base64,cG5nLWJ5dGVz",
                "media_type": "image/png",
            }));
            thread_id
        };

        let response = build_outgoing_response_for_thread(&session, thread_id, "done").await;

        assert_eq!(response.content, "done");
        assert!(response.attachments.is_empty());
        assert_eq!(response.inline_attachments.len(), 1);
        assert_eq!(
            response.inline_attachments[0].filename,
            "generated-image-1.png"
        );
        assert_eq!(response.inline_attachments[0].mime_type, "image/png");
        assert_eq!(response.inline_attachments[0].data, b"png-bytes");
    }

    #[tokio::test]
    async fn v2_only_structured_submissions_do_not_switch_threads_when_engine_v2_disabled() {
        let agent = make_legacy_handle_message_test_agent();
        let session = agent.session_manager.get_or_create_session("alice").await;
        let active_thread_id = Uuid::new_v4();
        let target_thread_id = Uuid::new_v4();

        {
            let mut sess = session.lock().await;
            sess.create_thread_with_id(active_thread_id, Some("gateway"));
            sess.create_thread_with_id(target_thread_id, Some("gateway"));
            sess.active_thread = Some(active_thread_id);
        }

        agent
            .session_manager
            .register_thread("alice", "gateway", active_thread_id, Arc::clone(&session))
            .await;
        agent
            .session_manager
            .register_thread("alice", "gateway", target_thread_id, Arc::clone(&session))
            .await;

        let gate_resolution = serde_json::to_string(&Submission::GateAuthResolution {
            request_id: Uuid::new_v4(),
            resolution: AuthGateResolution::Cancelled,
        })
        .expect("serialize gate resolution");
        let gate_message = IncomingMessage::new("gateway", "alice", &gate_resolution)
            .with_thread(target_thread_id);

        let outcome = agent
            .handle_message(&gate_message)
            .await
            .expect("handle message");
        assert!(matches!(
            outcome,
            HandleOutcome::Respond(ref msg) if msg.content == "Error: Auth gate resolution requires ENGINE_V2"
        ));
        {
            let sess = session.lock().await;
            assert_eq!(sess.active_thread, Some(active_thread_id));
        }

        let callback = serde_json::to_string(&Submission::ExternalCallback {
            request_id: Uuid::new_v4(),
        })
        .expect("serialize external callback");
        let callback_message =
            IncomingMessage::new("gateway", "alice", &callback).with_thread(target_thread_id);

        let outcome = agent
            .handle_message(&callback_message)
            .await
            .expect("handle callback");
        assert!(matches!(
            outcome,
            HandleOutcome::Respond(ref msg) if msg.content == "Error: External callbacks require ENGINE_V2"
        ));
        {
            let sess = session.lock().await;
            assert_eq!(sess.active_thread, Some(active_thread_id));
        }
    }
}
