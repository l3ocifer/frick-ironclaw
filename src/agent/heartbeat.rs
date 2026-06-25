//! Proactive heartbeat system for periodic execution.
//!
//! The heartbeat runner executes periodically (default: every 30 minutes) and:
//! 1. Reads the HEARTBEAT.md checklist
//! 2. Runs an agent turn to process the checklist
//! 3. Reports any findings to the configured channel
//!
//! If nothing needs attention, the agent replies "HEARTBEAT_OK" and no
//! message is sent to the user.
//!
//! # Usage
//!
//! Create a HEARTBEAT.md in the workspace with a checklist of things to monitor:
//!
//! ```markdown
//! # Heartbeat Checklist
//!
//! - [ ] Check for unread emails
//! - [ ] Review calendar for upcoming events
//! - [ ] Check project build status
//! ```
//!
//! The agent will process this checklist on each heartbeat and only notify
//! if action is needed.

use std::sync::Arc;
use std::time::Duration;

use chrono::TimeZone as _;
use chrono_tz::Tz;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::channels::OutgoingResponse;
use crate::context::JobContext;
use crate::extensions::ExtensionManager;
use crate::tenant::SystemScope;
use crate::tools::{
    ToolError, ToolRegistry, autonomous_allowed_tool_names, autonomous_unavailable_message,
    prepare_tool_params,
};
use crate::workspace::Workspace;
use crate::workspace::hygiene::HygieneConfig;
use ironclaw_llm::{
    ChatMessage, CompletionRequest, LlmProvider, Reasoning, ToolCall, ToolCompletionRequest,
};
use ironclaw_safety::SafetyLayer;

/// Configuration for the heartbeat runner.
#[derive(Debug, Clone)]
pub struct HeartbeatConfig {
    /// Interval between heartbeat checks (used when fire_at is not set).
    pub interval: Duration,
    /// Whether heartbeat is enabled.
    pub enabled: bool,
    /// Maximum consecutive failures before disabling.
    pub max_failures: u32,
    /// User ID to notify on heartbeat findings.
    pub notify_user_id: Option<String>,
    /// Channel to notify on heartbeat findings.
    pub notify_channel: Option<String>,
    /// Fixed time-of-day to fire (24h). When set, interval is ignored.
    pub fire_at: Option<chrono::NaiveTime>,
    /// Hour (0-23) when quiet hours start.
    pub quiet_hours_start: Option<u32>,
    /// Hour (0-23) when quiet hours end.
    pub quiet_hours_end: Option<u32>,
    /// Timezone for fire_at and quiet hours evaluation (IANA name).
    pub timezone: Option<String>,
    /// When true, cycle through all users with routines instead of
    /// running heartbeat for a single user. Requires a database store.
    pub multi_tenant: bool,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30 * 60), // 30 minutes
            enabled: true,
            max_failures: 3,
            notify_user_id: None,
            notify_channel: None,
            fire_at: None,
            quiet_hours_start: None,
            quiet_hours_end: None,
            timezone: None,
            multi_tenant: false,
        }
    }
}

impl HeartbeatConfig {
    /// Create a config with a specific interval.
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Disable heartbeat.
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// Check whether the current time falls within configured quiet hours.
    pub fn is_quiet_hours(&self) -> bool {
        use chrono::Timelike;
        let (Some(start), Some(end)) = (self.quiet_hours_start, self.quiet_hours_end) else {
            return false;
        };
        let tz = self
            .timezone
            .as_deref()
            .and_then(crate::timezone::parse_timezone)
            .unwrap_or(chrono_tz::UTC);
        let now_hour = crate::timezone::now_in_tz(tz).hour();
        if start <= end {
            now_hour >= start && now_hour < end
        } else {
            // Wraps midnight, e.g. 22..06
            now_hour >= start || now_hour < end
        }
    }

    /// Set the notification target.
    pub fn with_notify(mut self, user_id: impl Into<String>, channel: impl Into<String>) -> Self {
        self.notify_user_id = Some(user_id.into());
        self.notify_channel = Some(channel.into());
        self
    }

    /// Set a fixed time-of-day to fire (overrides interval).
    pub fn with_fire_at(mut self, time: chrono::NaiveTime, tz: Option<String>) -> Self {
        self.fire_at = Some(time);
        self.timezone = tz;
        self
    }

    /// Resolve timezone string to chrono_tz::Tz (defaults to UTC).
    fn resolved_tz(&self) -> Tz {
        self.timezone
            .as_deref()
            .and_then(crate::timezone::parse_timezone)
            .unwrap_or(chrono_tz::UTC)
    }
}

/// Result of a heartbeat check.
#[derive(Debug)]
pub enum HeartbeatResult {
    /// Nothing needs attention.
    Ok,
    /// Something needs attention, with the message to send.
    NeedsAttention(String),
    /// Heartbeat was skipped (no checklist or disabled).
    Skipped,
    /// Heartbeat failed.
    Failed(String),
}

/// Compute how long to sleep until the next occurrence of `fire_at` in `tz`.
///
/// If the target time today is still in the future, sleep until then.
/// Otherwise sleep until the same time tomorrow.
fn duration_until_next_fire(fire_at: chrono::NaiveTime, tz: Tz) -> Duration {
    let now = chrono::Utc::now().with_timezone(&tz);
    let today = now.date_naive();

    // Try to build today's target datetime in the given timezone.
    // `.earliest()` picks the first occurrence if DST creates ambiguity.
    let candidate = tz.from_local_datetime(&today.and_time(fire_at)).earliest();

    let target = match candidate {
        Some(t) if t > now => t,
        _ => {
            // Already past (or ambiguous) — schedule for tomorrow
            let tomorrow = today + chrono::Duration::days(1);
            tz.from_local_datetime(&tomorrow.and_time(fire_at))
                .earliest()
                .unwrap_or_else(|| now + chrono::Duration::days(1))
        }
    };

    let secs = (target - now).num_seconds().max(1) as u64;
    Duration::from_secs(secs)
}

/// Heartbeat runner for proactive periodic execution.
pub struct HeartbeatRunner {
    config: HeartbeatConfig,
    hygiene_config: HygieneConfig,
    workspace: Arc<Workspace>,
    llm: Arc<dyn LlmProvider>,
    response_tx: Option<mpsc::Sender<OutgoingResponse>>,
    store: Option<SystemScope>,
    /// Tool registry for tool-executing heartbeat runs. When present (with
    /// `safety`), `check_heartbeat` dispatches the model's tool calls through a
    /// bounded agentic loop instead of emitting a single toolless completion
    /// the model can only narrate as unexecuted `<tool_call>` text.
    tools: Option<Arc<ToolRegistry>>,
    /// Safety layer for tool-output sanitization in tool-executing runs.
    safety: Option<Arc<SafetyLayer>>,
    /// Owner-scoped extension activation state for autonomous tool resolution.
    extension_manager: Option<Arc<ExtensionManager>>,
    consecutive_failures: u32,
}

impl HeartbeatRunner {
    /// Create a new heartbeat runner.
    pub fn new(
        config: HeartbeatConfig,
        hygiene_config: HygieneConfig,
        workspace: Arc<Workspace>,
        llm: Arc<dyn LlmProvider>,
    ) -> Self {
        Self {
            config,
            hygiene_config,
            workspace,
            llm,
            response_tx: None,
            store: None,
            tools: None,
            safety: None,
            extension_manager: None,
            consecutive_failures: 0,
        }
    }

    /// Set the response channel for notifications.
    pub fn with_response_channel(mut self, tx: mpsc::Sender<OutgoingResponse>) -> Self {
        self.response_tx = Some(tx);
        self
    }

    /// Set the system-scoped database store for persistent heartbeat conversations.
    pub fn with_store(mut self, store: SystemScope) -> Self {
        self.store = Some(store);
        self
    }

    /// Wire the tool surface so heartbeat runs execute the model's tool calls
    /// instead of emitting a single toolless completion. Mirrors the routine
    /// engine's lightweight-with-tools path: when `tools` + `safety` are set,
    /// `check_heartbeat` drives a bounded agentic loop.
    pub fn with_tools(
        mut self,
        tools: Arc<ToolRegistry>,
        safety: Arc<SafetyLayer>,
        extension_manager: Option<Arc<ExtensionManager>>,
    ) -> Self {
        self.tools = Some(tools);
        self.safety = Some(safety);
        self.extension_manager = extension_manager;
        self
    }

    /// Run the heartbeat loop.
    ///
    /// This runs forever, checking periodically based on the configured interval.
    pub async fn run(&mut self) {
        if !self.config.enabled {
            tracing::info!("Heartbeat is disabled, not starting loop");
            return;
        }

        // Two scheduling modes:
        //   fire_at → sleep until the next occurrence (recalculated each iteration)
        //   interval → tokio::time::interval (drift-free, accounts for loop body time)
        let mut tick_interval = if self.config.fire_at.is_none() {
            let mut iv = tokio::time::interval(self.config.interval);
            // Don't fire immediately on startup.
            iv.tick().await;
            Some(iv)
        } else {
            None
        };

        if let Some(fire_at) = self.config.fire_at {
            tracing::info!(
                "Starting heartbeat loop: fire daily at {:?} {:?}",
                fire_at,
                self.config.timezone
            );
        } else {
            tracing::info!(
                "Starting heartbeat loop with interval {:?}",
                self.config.interval
            );
        }

        loop {
            if let Some(fire_at) = self.config.fire_at {
                let sleep_dur = duration_until_next_fire(fire_at, self.config.resolved_tz());
                tracing::info!("Next heartbeat in {:.1}h", sleep_dur.as_secs_f64() / 3600.0);
                tokio::time::sleep(sleep_dur).await;
            } else if let Some(ref mut iv) = tick_interval {
                iv.tick().await;
            }

            // Skip during quiet hours
            if self.config.is_quiet_hours() {
                tracing::trace!("Heartbeat skipped: quiet hours");
                continue;
            }

            // Run memory hygiene in the background so it never delays the
            // heartbeat checklist. Failures are logged inside run_if_due.
            let hygiene_workspace = Arc::clone(&self.workspace);
            let hygiene_config = self.hygiene_config.clone();
            tokio::spawn(async move {
                let report =
                    crate::workspace::hygiene::run_if_due(&hygiene_workspace, &hygiene_config)
                        .await;
                if report.had_work() {
                    tracing::info!(
                        directories_cleaned = ?report.directories_cleaned,
                        versions_pruned = report.versions_pruned,
                        "heartbeat: memory hygiene deleted stale documents"
                    );
                }
            });

            match self.check_heartbeat().await {
                HeartbeatResult::Ok => {
                    tracing::trace!("Heartbeat OK");
                    self.consecutive_failures = 0;
                }
                HeartbeatResult::NeedsAttention(message) => {
                    tracing::info!("Heartbeat needs attention: {}", message);
                    self.consecutive_failures = 0;
                    self.send_notification(&message).await;
                }
                HeartbeatResult::Skipped => {
                    tracing::trace!("Heartbeat skipped");
                }
                HeartbeatResult::Failed(error) => {
                    tracing::error!("Heartbeat failed: {}", error);
                    self.consecutive_failures += 1;

                    if self.consecutive_failures >= self.config.max_failures {
                        tracing::error!(
                            "Heartbeat disabled after {} consecutive failures",
                            self.consecutive_failures
                        );
                        break;
                    }
                }
            }
        }
    }

    /// Run a single heartbeat check.
    pub async fn check_heartbeat(&self) -> HeartbeatResult {
        // Get the heartbeat checklist
        let checklist = match self.workspace.heartbeat_checklist().await {
            Ok(Some(content)) if !is_effectively_empty(&content) => content,
            Ok(_) => return HeartbeatResult::Skipped,
            Err(e) => return HeartbeatResult::Failed(format!("Failed to read checklist: {}", e)),
        };

        // Build the heartbeat prompt
        let prompt = format!(
            "Read the HEARTBEAT.md checklist below and follow it strictly. \
             Do not infer or repeat old tasks. Check each item and report findings.\n\
             \n\
             If nothing needs attention, reply EXACTLY with: HEARTBEAT_OK\n\
             \n\
             If something needs attention, provide a concise summary of what needs action.\n\
             \n\
             ## HEARTBEAT.md\n\
             \n\
             {}",
            checklist
        );

        // Get the system prompt for context
        let system_prompt = match self.workspace.system_prompt().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to get system prompt for heartbeat: {}", e);
                String::new()
            }
        };

        // Run the agent turn
        let messages = if system_prompt.is_empty() {
            vec![ChatMessage::user(&prompt)]
        } else {
            vec![
                ChatMessage::system(&system_prompt),
                ChatMessage::user(&prompt),
            ]
        };

        // Use the model's context_length to set max_tokens. The API returns
        // the total context window; we cap output at half of that (the rest is
        // the prompt) with a floor of 4096.
        let max_tokens = match self.llm.model_metadata().await {
            Ok(meta) => {
                let from_api = meta.context_length.map(|ctx| ctx / 2).unwrap_or(4096);
                from_api.max(4096)
            }
            Err(e) => {
                tracing::warn!(
                    "Could not fetch model metadata, using default max_tokens: {}",
                    e
                );
                4096
            }
        };

        // Acquire the heartbeat response. When a tool registry + safety layer
        // are wired in (production agents), run a bounded tool-executing loop so
        // the model's tool calls actually run — instead of a single toolless
        // completion the model can only narrate as unexecuted `<tool_call>`
        // text. Otherwise fall back to the legacy single-shot text completion.
        let content = match (self.tools.as_ref(), self.safety.as_ref()) {
            (Some(tools), Some(safety)) => {
                match self
                    .run_with_tools(tools, safety, &system_prompt, &prompt, max_tokens)
                    .await
                {
                    Ok(c) => c,
                    Err(e) => return HeartbeatResult::Failed(format!("LLM call failed: {}", e)),
                }
            }
            _ => {
                let request = CompletionRequest::new(messages)
                    .with_max_tokens(max_tokens)
                    .with_temperature(0.3);

                let reasoning =
                    Reasoning::new(self.llm.clone()).with_model_name(self.llm.active_model_name());
                match reasoning.complete(request).await {
                    Ok((c, _usage)) => c,
                    Err(e) => return HeartbeatResult::Failed(format!("LLM call failed: {}", e)),
                }
            }
        };

        let content = content.trim();

        // Guard against empty content. Reasoning models (e.g. GLM-4.7) may
        // burn all output tokens on chain-of-thought and return content: null.
        if content.is_empty() {
            return HeartbeatResult::Failed("LLM returned empty content.".to_string());
        }

        // Check if nothing needs attention. Trimmed-equality / prefix match —
        // NOT a loose `contains`, which would suppress a genuine alert that
        // merely mentions the sentinel somewhere mid-text.
        if content == "HEARTBEAT_OK" || content.starts_with("HEARTBEAT_OK") {
            return HeartbeatResult::Ok;
        }

        HeartbeatResult::NeedsAttention(content.to_string())
    }

    /// Run the heartbeat checklist through a bounded, tool-executing agentic
    /// loop. Mirrors `routine_engine::execute_lightweight_with_tools`: offer the
    /// owner's autonomous tool surface, dispatch returned tool calls through the
    /// safety pipeline, feed results back, and force a text-only final turn at
    /// the iteration cap. Returns the model's final user-facing text.
    async fn run_with_tools(
        &self,
        tools: &Arc<ToolRegistry>,
        safety: &Arc<SafetyLayer>,
        system_prompt: &str,
        user_prompt: &str,
        max_tokens: u32,
    ) -> Result<String, String> {
        // Bounded like the routine lightweight loop (default
        // `lightweight_max_iterations` = 3). The final round forces a
        // text-only answer so the heartbeat always resolves to a summary or
        // the HEARTBEAT_OK sentinel rather than dangling on a tool call.
        const MAX_TOOL_ROUNDS: u32 = 3;
        const MAX_TOOL_OUTPUT_CHARS: usize = 8192;

        let user_id = self
            .config
            .notify_user_id
            .as_deref()
            .unwrap_or_else(|| self.workspace.user_id())
            .to_string();

        let mut messages = if system_prompt.is_empty() {
            vec![ChatMessage::user(user_prompt)]
        } else {
            vec![
                ChatMessage::system(system_prompt),
                ChatMessage::user(user_prompt),
            ]
        };

        let allowed_tools =
            autonomous_allowed_tool_names(tools, self.extension_manager.as_ref(), &user_id).await;

        // Minimal job context for tool execution (mirrors the lightweight
        // routine path). `source: heartbeat` lets the message tool and audit
        // trail attribute these dispatches.
        let job_ctx = JobContext {
            job_id: Uuid::new_v4(),
            user_id: user_id.clone(),
            title: "Heartbeat".to_string(),
            description: "Heartbeat checklist".to_string(),
            metadata: serde_json::json!({ "owner_id": user_id, "source": "heartbeat" }),
            ..Default::default()
        };

        let mut iteration = 0u32;
        loop {
            iteration += 1;

            // Final round: force a text-only response (no tools offered).
            if iteration >= MAX_TOOL_ROUNDS {
                // Claude rejects assistant prefill; NEAR AI rejects a non-user-
                // ending conversation. Ensure the last message is user-role.
                crate::util::ensure_ends_with_user_message(&mut messages);
                let request = CompletionRequest::new(messages)
                    .with_max_tokens(max_tokens)
                    .with_temperature(0.3);
                let response = self.llm.complete(request).await.map_err(|e| e.to_string())?;
                return Ok(response.content);
            }

            let tool_defs = tools
                .tool_definitions()
                .await
                .into_iter()
                .filter(|tool| allowed_tools.contains(&tool.name))
                .collect();

            let request = ToolCompletionRequest::new(messages.clone(), tool_defs)
                .with_max_tokens(max_tokens)
                .with_temperature(0.3);

            let response = self
                .llm
                .complete_with_tools(request)
                .await
                .map_err(|e| e.to_string())?;

            // No tool calls → the model produced its final text answer.
            if response.tool_calls.is_empty() {
                return Ok(response.content.unwrap_or_default());
            }

            // Record the assistant turn (carry reasoning so DeepSeek/Gemini
            // thinking-mode validate the chain), then execute the tools.
            messages.push(
                ChatMessage::assistant_with_tool_calls(
                    response.content.clone(),
                    response.tool_calls.clone(),
                )
                .with_reasoning(response.reasoning.clone()),
            );

            for tc in response.tool_calls {
                let result_content = self
                    .execute_heartbeat_tool(tools, safety, &job_ctx, &allowed_tools, &tc)
                    .await;
                let result_content = if result_content.len() > MAX_TOOL_OUTPUT_CHARS {
                    let truncated = &result_content
                        [..result_content.floor_char_boundary(MAX_TOOL_OUTPUT_CHARS)];
                    format!("{truncated}\n... [output truncated to {MAX_TOOL_OUTPUT_CHARS} chars]")
                } else {
                    result_content
                };
                messages.push(ChatMessage::tool_result(&tc.id, &tc.name, &result_content));
            }
        }
    }

    /// Execute a single heartbeat tool call through the safety pipeline
    /// (autonomous-scope gate → param prep → validation → timeout → execute →
    /// sanitize → wrap). Mirrors `routine_engine::execute_routine_tool`; always
    /// returns an LLM-ready string (tool output or a wrapped error message).
    async fn execute_heartbeat_tool(
        &self,
        tools: &Arc<ToolRegistry>,
        safety: &Arc<SafetyLayer>,
        job_ctx: &JobContext,
        allowed_tools: &std::collections::HashSet<String>,
        tc: &ToolCall,
    ) -> String {
        let outcome: Result<String, String> = async {
            if !allowed_tools.contains(&tc.name) {
                return Err(autonomous_unavailable_message(&tc.name, &job_ctx.user_id));
            }

            let tool = tools
                .get(&tc.name)
                .await
                .ok_or_else(|| format!("Tool '{}' not found", tc.name))?;
            let normalized_params = prepare_tool_params(tool.as_ref(), &tc.arguments);

            let validation = safety.validator().validate_tool_params(&normalized_params);
            if !validation.is_valid {
                let details = validation
                    .errors
                    .iter()
                    .map(|e| format!("{}: {}", e.field, e.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(format!("Invalid tool parameters: {}", details));
            }

            let timeout = tool.execution_timeout();
            let executed = tokio::time::timeout(timeout, async {
                tool.execute(normalized_params.clone(), job_ctx).await
            })
            .await
            .map_err(|_| ToolError::Timeout(timeout).to_string())?
            .map_err(|e| e.to_string())?;

            Ok(serde_json::to_string(&executed.result)
                .unwrap_or_else(|_| "<serialize error>".to_string()))
        }
        .await;

        // Sanitize + wrap both success and error so the loop always feeds the
        // model an `<tool_output>`-wrapped, leak-scanned string.
        let raw = match outcome {
            Ok(output) => output,
            Err(e) => format!("Tool '{}' failed: {}", tc.name, e),
        };
        let sanitized = safety.sanitize_tool_output(&tc.name, &raw);
        safety.wrap_for_llm(&tc.name, &sanitized.content)
    }

    /// Send a notification about heartbeat findings.
    async fn send_notification(&self, message: &str) {
        let Some(ref tx) = self.response_tx else {
            tracing::debug!("No response channel configured for heartbeat notifications");
            return;
        };

        let user_id = self
            .config
            .notify_user_id
            .as_deref()
            .unwrap_or_else(|| self.workspace.user_id());

        // Persist to heartbeat conversation and get thread_id
        let thread_id = if let Some(ref store) = self.store {
            match store.get_or_create_heartbeat_conversation(user_id).await {
                Ok(conv_id) => {
                    if let Err(e) = store
                        .add_conversation_message(conv_id, "assistant", message)
                        .await
                    {
                        tracing::error!("Failed to persist heartbeat message: {}", e);
                    }
                    Some(conv_id.to_string())
                }
                Err(e) => {
                    tracing::error!("Failed to get heartbeat conversation: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let response = OutgoingResponse {
            content: format!("🔔 *Heartbeat Alert*\n\n{}", message),
            // `thread_id` originates from the engine's internal `ConversationId`
            // (rendered as a UUID string) — trust it past the newtype boundary
            // because it was not supplied by a channel adapter.
            thread_id: thread_id.map(ironclaw_common::ExternalThreadId::from_trusted),
            attachments: Vec::new(),
            inline_attachments: Vec::new(),
            metadata: serde_json::json!({
                "source": "heartbeat",
                "owner_id": self.workspace.user_id(),
            }),
        };

        if let Err(e) = tx.send(response).await {
            tracing::error!("Failed to send heartbeat notification: {}", e);
        }
    }
}

/// Check if heartbeat content is effectively empty.
///
/// Returns true if the content contains only:
/// - Whitespace
/// - Markdown headers (lines starting with #)
/// - HTML comments (`<!-- ... -->`)
/// - Empty list items (`- [ ]`, `- [x]`, `-`, `*`)
///
/// This skips the LLM call when the user hasn't added real tasks yet,
/// saving API costs.
fn is_effectively_empty(content: &str) -> bool {
    let without_comments = strip_html_comments(content);

    without_comments.lines().all(|line| {
        let trimmed = line.trim();
        trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed == "- [ ]"
            || trimmed == "- [x]"
            || trimmed == "-"
            || trimmed == "*"
    })
}

/// Remove HTML comments from content.
fn strip_html_comments(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("<!--") {
        result.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => return result, // unclosed comment, treat rest as comment
        }
    }
    result.push_str(rest);
    result
}

/// Spawn the heartbeat runner as a background task.
///
/// Returns a handle that can be used to stop the runner.
#[allow(clippy::too_many_arguments)]
pub fn spawn_heartbeat(
    config: HeartbeatConfig,
    hygiene_config: HygieneConfig,
    workspace: Arc<Workspace>,
    llm: Arc<dyn LlmProvider>,
    response_tx: Option<mpsc::Sender<OutgoingResponse>>,
    store: Option<SystemScope>,
    tools: Option<Arc<ToolRegistry>>,
    safety: Option<Arc<SafetyLayer>>,
    extension_manager: Option<Arc<ExtensionManager>>,
) -> tokio::task::JoinHandle<()> {
    let mut runner = HeartbeatRunner::new(config, hygiene_config, workspace, llm);
    if let Some(tx) = response_tx {
        runner = runner.with_response_channel(tx);
    }
    if let Some(s) = store {
        runner = runner.with_store(s);
    }
    if let (Some(tools), Some(safety)) = (tools, safety) {
        runner = runner.with_tools(tools, safety, extension_manager);
    }

    tokio::spawn(async move {
        runner.run().await;
    })
}

/// Spawn a multi-user heartbeat runner that cycles through all users who
/// have routines (enabled or not). Each tick, it queries the DB for distinct
/// user_ids, creates a per-user workspace, and runs a heartbeat check for
/// each user concurrently. Per-user failure counts are tracked independently.
#[allow(clippy::too_many_arguments)]
pub fn spawn_multi_user_heartbeat(
    config: HeartbeatConfig,
    hygiene_config: HygieneConfig,
    llm: Arc<dyn LlmProvider>,
    response_tx: Option<mpsc::Sender<OutgoingResponse>>,
    store: SystemScope,
    tools: Option<Arc<ToolRegistry>>,
    safety: Option<Arc<SafetyLayer>>,
    extension_manager: Option<Arc<ExtensionManager>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !config.enabled {
            tracing::info!("Multi-user heartbeat is disabled");
            return;
        }

        let mut tick_interval = if config.fire_at.is_none() {
            let mut iv = tokio::time::interval(config.interval);
            iv.tick().await; // skip immediate tick
            Some(iv)
        } else {
            None
        };

        // Track consecutive failures per user so we can disable heartbeat
        // for persistently-failing users (same semantics as single-user mode).
        let mut user_failures: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();

        tracing::info!("Starting multi-user heartbeat loop");

        loop {
            if let Some(fire_at) = config.fire_at {
                let sleep_dur = duration_until_next_fire(fire_at, config.resolved_tz());
                tokio::time::sleep(sleep_dur).await;
            } else if let Some(ref mut iv) = tick_interval {
                iv.tick().await;
            }

            if config.is_quiet_hours() {
                continue;
            }

            // Get distinct user_ids from routines
            let user_ids = match store.list_all_routines().await {
                Ok(routines) => {
                    let mut ids: Vec<String> = routines
                        .iter()
                        .map(|r| r.user_id.clone())
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect();
                    ids.sort();
                    ids
                }
                Err(e) => {
                    tracing::error!("Multi-user heartbeat: failed to list routines: {}", e);
                    continue;
                }
            };

            // Run user heartbeats (and hygiene) concurrently so one slow LLM
            // call doesn't block others. Cap concurrency to avoid flooding the
            // LLM provider. Hygiene runs inside the same JoinSet so it is
            // tracked and bounded by the same concurrency cap.
            const MAX_CONCURRENT_HEARTBEATS: usize = 8;
            let mut join_set = tokio::task::JoinSet::new();

            for user_id in &user_ids {
                // Skip users that have exceeded max_failures
                let failures = user_failures.get(user_id).copied().unwrap_or(0);
                if failures >= config.max_failures {
                    continue;
                }

                let workspace = Arc::new(store.workspace_for_user(user_id.as_str()));

                // Drain completed tasks to stay within the concurrency cap.
                while join_set.len() >= MAX_CONCURRENT_HEARTBEATS {
                    if let Some(join_result) = join_set.join_next().await {
                        collect_heartbeat_result(join_result, &mut user_failures, &config);
                    }
                }

                let uid = user_id.clone();
                // In multi-tenant mode, clear notify_user_id so that
                // HeartbeatRunner::send_notification falls back to
                // workspace.user_id() — each user's heartbeat should persist
                // and notify that user, not the shared config target.
                let mut cfg = config.clone();
                cfg.notify_user_id = None;
                let hyg = hygiene_config.clone();
                let llm_clone = llm.clone();
                let tx = response_tx.clone();
                let system_store = store.clone();
                let tools_clone = tools.clone();
                let safety_clone = safety.clone();
                let ext_clone = extension_manager.clone();

                join_set.spawn(async move {
                    // Run memory hygiene per user (same as single-user heartbeat)
                    // inside the tracked task so concurrency is bounded.
                    let report = crate::workspace::hygiene::run_if_due(&workspace, &hyg).await;
                    if report.had_work() {
                        tracing::info!(
                            user_id = uid,
                            directories_cleaned = ?report.directories_cleaned,
                            versions_pruned = report.versions_pruned,
                            "multi-user heartbeat: memory hygiene deleted stale documents"
                        );
                    }

                    let mut runner = HeartbeatRunner::new(cfg, hyg, workspace, llm_clone);
                    if let Some(tx) = tx {
                        runner = runner.with_response_channel(tx);
                    }
                    runner = runner.with_store(system_store);
                    if let (Some(tools), Some(safety)) = (tools_clone, safety_clone) {
                        runner = runner.with_tools(tools, safety, ext_clone);
                    }

                    let result = runner.check_heartbeat().await;
                    if let HeartbeatResult::NeedsAttention(msg) = &result {
                        runner.send_notification(msg).await;
                    }
                    (uid, result)
                });
            }

            // Collect remaining results and update failure counts
            while let Some(join_result) = join_set.join_next().await {
                collect_heartbeat_result(join_result, &mut user_failures, &config);
            }
        }
    })
}

/// Process a single JoinSet result from the multi-user heartbeat loop.
fn collect_heartbeat_result(
    join_result: Result<(String, HeartbeatResult), tokio::task::JoinError>,
    user_failures: &mut std::collections::HashMap<String, u32>,
    config: &HeartbeatConfig,
) {
    let (uid, result) = match join_result {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!("Multi-user heartbeat task panicked: {}", e);
            return;
        }
    };
    match result {
        HeartbeatResult::Ok => {
            tracing::trace!(user_id = uid, "Multi-user heartbeat OK");
            user_failures.remove(&uid);
        }
        HeartbeatResult::NeedsAttention(_) => {
            tracing::info!(user_id = uid, "Multi-user heartbeat needs attention");
            user_failures.remove(&uid);
        }
        HeartbeatResult::Skipped => {}
        HeartbeatResult::Failed(err) => {
            let count = user_failures.entry(uid.clone()).or_insert(0);
            *count += 1;
            tracing::error!(
                user_id = uid,
                consecutive_failures = *count,
                "Multi-user heartbeat failed: {}",
                err
            );
            if *count >= config.max_failures {
                tracing::error!(
                    user_id = uid,
                    "Multi-user heartbeat disabled for user after {} consecutive failures",
                    count
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heartbeat_config_defaults() {
        let config = HeartbeatConfig::default();
        assert!(config.enabled);
        assert_eq!(config.interval, Duration::from_secs(30 * 60));
        assert_eq!(config.max_failures, 3);
    }

    #[test]
    fn test_heartbeat_config_builders() {
        let config = HeartbeatConfig::default()
            .with_interval(Duration::from_secs(60))
            .with_notify("user1", "telegram");

        assert_eq!(config.interval, Duration::from_secs(60));
        assert_eq!(config.notify_user_id, Some("user1".to_string()));
        assert_eq!(config.notify_channel, Some("telegram".to_string()));

        let disabled = HeartbeatConfig::default().disabled();
        assert!(!disabled.enabled);
    }

    // ==================== strip_html_comments ====================

    #[test]
    fn test_strip_html_comments_no_comments() {
        assert_eq!(strip_html_comments("hello world"), "hello world");
    }

    #[test]
    fn test_strip_html_comments_single() {
        assert_eq!(
            strip_html_comments("before<!-- gone -->after"),
            "beforeafter"
        );
    }

    #[test]
    fn test_strip_html_comments_multiple() {
        let input = "a<!-- 1 -->b<!-- 2 -->c";
        assert_eq!(strip_html_comments(input), "abc");
    }

    #[test]
    fn test_strip_html_comments_multiline() {
        let input = "# Title\n<!-- multi\nline\ncomment -->\nreal content";
        assert_eq!(strip_html_comments(input), "# Title\n\nreal content");
    }

    #[test]
    fn test_strip_html_comments_unclosed() {
        let input = "before<!-- never closed";
        assert_eq!(strip_html_comments(input), "before");
    }

    // ==================== is_effectively_empty ====================

    #[test]
    fn test_effectively_empty_empty_string() {
        assert!(is_effectively_empty(""));
    }

    #[test]
    fn test_effectively_empty_whitespace() {
        assert!(is_effectively_empty("   \n\n  \n  "));
    }

    #[test]
    fn test_effectively_empty_headers_only() {
        assert!(is_effectively_empty("# Title\n## Subtitle\n### Section"));
    }

    #[test]
    fn test_effectively_empty_html_comments_only() {
        assert!(is_effectively_empty("<!-- this is a comment -->"));
    }

    #[test]
    fn test_effectively_empty_empty_checkboxes() {
        assert!(is_effectively_empty("# Checklist\n- [ ]\n- [x]"));
    }

    #[test]
    fn test_effectively_empty_bare_list_markers() {
        assert!(is_effectively_empty("-\n*\n-"));
    }

    #[test]
    fn test_effectively_empty_seeded_template() {
        let template = "\
# Heartbeat Checklist

<!-- Keep this file empty to skip heartbeat API calls.
     Add tasks below when you want the agent to check something periodically.

     Example:
     - [ ] Check for unread emails needing a reply
     - [ ] Review today's calendar for upcoming meetings
     - [ ] Check CI build status for main branch
-->";
        assert!(is_effectively_empty(template));
    }

    #[test]
    fn test_effectively_empty_real_checklist() {
        let content = "\
# Heartbeat Checklist

- [ ] Check for unread emails needing a reply
- [ ] Review today's calendar for upcoming meetings";
        assert!(!is_effectively_empty(content));
    }

    #[test]
    fn test_effectively_empty_mixed_real_and_headers() {
        let content = "# Title\n\nDo something important";
        assert!(!is_effectively_empty(content));
    }

    #[test]
    fn test_effectively_empty_comment_plus_real_content() {
        let content = "<!-- comment -->\nActual task here";
        assert!(!is_effectively_empty(content));
    }

    // ==================== quiet hours ====================

    #[test]
    fn test_quiet_hours_inside() {
        use chrono::{Timelike, Utc};

        let now_utc = Utc::now();
        let hour = now_utc.hour();
        let start = hour;
        let end = (hour + 1) % 24;

        let config = HeartbeatConfig {
            quiet_hours_start: Some(start),
            quiet_hours_end: Some(end),
            timezone: Some("UTC".to_string()),
            ..HeartbeatConfig::default()
        };
        // Current UTC hour is inside [start, end) by construction
        assert!(config.is_quiet_hours());
    }

    #[test]
    fn test_quiet_hours_outside() {
        use chrono::{Timelike, Utc};

        let now_utc = Utc::now();
        let hour = now_utc.hour();
        let start = (hour + 1) % 24;
        let end = (hour + 2) % 24;

        let config = HeartbeatConfig {
            quiet_hours_start: Some(start),
            quiet_hours_end: Some(end),
            timezone: Some("UTC".to_string()),
            ..HeartbeatConfig::default()
        };
        // Current UTC hour is outside [start, end) by construction
        assert!(!config.is_quiet_hours());
    }

    #[test]
    fn test_quiet_hours_wraparound_excludes_now() {
        use chrono::{Timelike, Utc};

        let now_utc = Utc::now();
        let hour = now_utc.hour();
        // Window covers all hours except the current one
        let start = (hour + 1) % 24;
        let end = hour;

        let config = HeartbeatConfig {
            quiet_hours_start: Some(start),
            quiet_hours_end: Some(end),
            timezone: Some("UTC".to_string()),
            ..HeartbeatConfig::default()
        };
        assert!(!config.is_quiet_hours());
    }

    #[test]
    fn test_quiet_hours_none_configured() {
        let config = HeartbeatConfig::default();
        assert!(!config.is_quiet_hours());
    }

    #[test]
    fn test_quiet_hours_same_start_end() {
        let config = HeartbeatConfig {
            quiet_hours_start: Some(10),
            quiet_hours_end: Some(10),
            timezone: Some("UTC".to_string()),
            ..HeartbeatConfig::default()
        };
        // start == end means zero-width window, should be false
        assert!(!config.is_quiet_hours());
    }

    #[test]
    fn test_spawn_heartbeat_accepts_store_param() {
        // Regression: spawn_heartbeat must accept an optional Database store
        // for persisting heartbeat notifications to a dedicated conversation.
        // Compile-time check: the 7th parameter is `Option<Arc<dyn Database>>`.
        #[allow(clippy::type_complexity)]
        let _fn_ptr: fn(
            HeartbeatConfig,
            HygieneConfig,
            Arc<crate::workspace::Workspace>,
            Arc<dyn ironclaw_llm::LlmProvider>,
            Option<tokio::sync::mpsc::Sender<crate::channels::OutgoingResponse>>,
            Option<SystemScope>,
            Option<Arc<crate::tools::ToolRegistry>>,
            Option<Arc<ironclaw_safety::SafetyLayer>>,
            Option<Arc<crate::extensions::ExtensionManager>>,
        ) -> tokio::task::JoinHandle<()> = spawn_heartbeat;
        let _ = _fn_ptr;
    }

    // ==================== fire_at scheduling ====================

    #[test]
    fn test_default_config_has_no_fire_at() {
        let config = HeartbeatConfig::default();
        assert!(config.fire_at.is_none());
        // Interval-based scheduling should be the default
        assert_eq!(config.interval, Duration::from_secs(30 * 60));
    }

    #[test]
    fn test_with_fire_at_builder() {
        let time = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let config =
            HeartbeatConfig::default().with_fire_at(time, Some("Pacific/Auckland".to_string()));
        assert_eq!(config.fire_at, Some(time));
        assert_eq!(config.timezone, Some("Pacific/Auckland".to_string()));
    }

    #[test]
    fn test_duration_until_next_fire_is_bounded() {
        // Result must always be between 1 second and ~24 hours
        let time = chrono::NaiveTime::from_hms_opt(14, 0, 0).unwrap();
        let dur = duration_until_next_fire(time, chrono_tz::UTC);
        assert!(dur.as_secs() >= 1, "duration must be at least 1 second");
        assert!(
            dur.as_secs() <= 86_401,
            "duration must be at most ~24 hours, got {}s",
            dur.as_secs()
        );
    }

    #[test]
    fn test_duration_until_next_fire_dst_timezone_no_panic() {
        // Use a timezone with DST (US Eastern) — should never panic
        let tz: Tz = "America/New_York".parse().unwrap();
        // Test a range of times including midnight boundaries
        for hour in [0, 2, 3, 12, 23] {
            let time = chrono::NaiveTime::from_hms_opt(hour, 30, 0).unwrap();
            let dur = duration_until_next_fire(time, tz);
            assert!(dur.as_secs() >= 1);
            assert!(dur.as_secs() <= 86_401);
        }
    }

    #[test]
    fn test_resolved_tz_defaults_to_utc() {
        let config = HeartbeatConfig::default();
        assert_eq!(config.resolved_tz(), chrono_tz::UTC);
    }

    #[test]
    fn test_resolved_tz_parses_iana() {
        let time = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let config =
            HeartbeatConfig::default().with_fire_at(time, Some("Europe/London".to_string()));
        assert_eq!(config.resolved_tz(), chrono_tz::Europe::London);
    }
}
