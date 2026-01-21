use anyhow::{Context, Result};
use async_stream::try_stream;
use rmcp::model::{CallToolRequestParams, CallToolResult, Content, Role, Tool};
use sacp::schema::{
    ContentBlock, ContentChunk, InitializeRequest, NewSessionRequest, PromptRequest,
    ProtocolVersion, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionId, SessionNotification, SessionUpdate, SetSessionModeRequest, StopReason, TextContent,
    ToolCallContent, ToolCallStatus,
};
use sacp::{ClientToAgent, JrConnectionCx};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex as TokioMutex};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::acp::{map_permission_response, PermissionDecision, PermissionMapping};
use crate::config::GooseMode;
use crate::conversation::message::{Message, MessageContent};
use crate::model::ModelConfig;
use crate::permission::permission_confirmation::PrincipalType;
use crate::permission::{Permission, PermissionConfirmation};
use crate::providers::base::{MessageStream, PermissionRouting, Provider, ProviderUsage, Usage};
use crate::providers::errors::ProviderError;
use crate::session::{Session, SessionManager, SessionType};

#[derive(Clone, Debug)]
pub struct AcpProviderConfig {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub work_dir: PathBuf,
    pub mcp_servers: Vec<sacp::schema::McpServer>,
    pub session_mode_id: Option<String>,
    pub permission_mapping: PermissionMapping,
}

enum ClientRequest {
    NewSession {
        response_tx: oneshot::Sender<Result<SessionId>>,
    },
    Prompt {
        session_id: SessionId,
        content: Vec<ContentBlock>,
        response_tx: mpsc::Sender<AcpUpdate>,
    },
    Shutdown,
}

#[derive(Debug)]
enum AcpUpdate {
    Text(String),
    Thought(String),
    ToolCallStart {
        id: String,
        title: String,
        raw_input: Option<serde_json::Value>,
    },
    ToolCallComplete {
        id: String,
        status: ToolCallStatus,
        content: Vec<ToolCallContent>,
    },
    PermissionRequest {
        request: Box<RequestPermissionRequest>,
        response_tx: oneshot::Sender<RequestPermissionResponse>,
    },
    Complete(StopReason),
    Error(String),
}

pub struct AcpProvider {
    name: String,
    model: ModelConfig,
    goose_mode: GooseMode,
    tx: mpsc::Sender<ClientRequest>,
    permission_mapping: PermissionMapping,
    rejected_tool_calls: Arc<TokioMutex<HashSet<String>>>,
    pending_confirmations:
        Arc<TokioMutex<HashMap<String, oneshot::Sender<PermissionConfirmation>>>>,
    sessions: Arc<TokioMutex<HashMap<String, Session>>>,
}

impl std::fmt::Debug for AcpProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpProvider")
            .field("name", &self.name)
            .field("model", &self.model)
            .finish()
    }
}

impl AcpProvider {
    pub async fn connect(
        name: String,
        model: ModelConfig,
        goose_mode: GooseMode,
        config: AcpProviderConfig,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel(32);
        let (init_tx, init_rx) = oneshot::channel();
        let permission_mapping = config.permission_mapping.clone();
        let rejected_tool_calls = Arc::new(TokioMutex::new(HashSet::new()));

        tokio::spawn(run_client_loop(config, rx, init_tx));

        init_rx
            .await
            .context("ACP client initialization cancelled")??;

        Ok(Self::new_with_runtime(
            name,
            model,
            goose_mode,
            tx,
            permission_mapping,
            rejected_tool_calls,
        ))
    }

    pub async fn connect_with_transport<R, W>(
        name: String,
        model: ModelConfig,
        goose_mode: GooseMode,
        config: AcpProviderConfig,
        read: R,
        write: W,
    ) -> Result<Self>
    where
        R: futures::AsyncRead + Unpin + Send + 'static,
        W: futures::AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel(32);
        let (init_tx, init_rx) = oneshot::channel();
        let permission_mapping = config.permission_mapping.clone();
        let rejected_tool_calls = Arc::new(TokioMutex::new(HashSet::new()));
        let transport = sacp::ByteStreams::new(write, read);
        let init_tx = Arc::new(Mutex::new(Some(init_tx)));
        tokio::spawn(async move {
            if let Err(e) =
                run_protocol_loop_with_transport(config, transport, &mut rx, init_tx.clone()).await
            {
                tracing::error!("ACP protocol error: {e}");
            }
        });

        init_rx
            .await
            .context("ACP client initialization cancelled")??;

        Ok(Self::new_with_runtime(
            name,
            model,
            goose_mode,
            tx,
            permission_mapping,
            rejected_tool_calls,
        ))
    }

    fn new_with_runtime(
        name: String,
        model: ModelConfig,
        goose_mode: GooseMode,
        tx: mpsc::Sender<ClientRequest>,
        permission_mapping: PermissionMapping,
        rejected_tool_calls: Arc<TokioMutex<HashSet<String>>>,
    ) -> Self {
        Self {
            name,
            model,
            goose_mode,
            tx,
            permission_mapping,
            rejected_tool_calls,
            pending_confirmations: Arc::new(TokioMutex::new(HashMap::new())),
            sessions: Arc::new(TokioMutex::new(HashMap::new())),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn model(&self) -> ModelConfig {
        self.model.clone()
    }

    pub fn permission_routing(&self) -> PermissionRouting {
        PermissionRouting::ActionRequired
    }

    pub async fn new_session(&self) -> Result<SessionId> {
        let (response_tx, response_rx) = oneshot::channel();
        self.tx
            .send(ClientRequest::NewSession { response_tx })
            .await
            .context("ACP client is unavailable")?;
        response_rx.await.context("ACP session/new cancelled")?
    }

    pub async fn handle_permission_confirmation(
        &self,
        request_id: &str,
        confirmation: &PermissionConfirmation,
    ) -> bool {
        let mut pending = self.pending_confirmations.lock().await;
        if let Some(tx) = pending.remove(request_id) {
            let _ = tx.send(confirmation.clone());
            return true;
        }
        false
    }

    pub async fn complete_with_model(
        &self,
        session_id: &str,
        model_config: &ModelConfig,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<(Message, ProviderUsage), ProviderError> {
        let stream = self.stream(session_id, system, messages, tools).await?;

        use futures::StreamExt;
        tokio::pin!(stream);

        let mut content: Vec<MessageContent> = Vec::new();
        while let Some(result) = stream.next().await {
            if let Ok((Some(msg), _)) = result {
                content.extend(msg.content);
            }
        }

        if content.is_empty() {
            return Err(ProviderError::RequestFailed(
                "No response received from ACP agent".to_string(),
            ));
        }

        let mut message = Message::assistant();
        message.content = content;

        Ok((
            message,
            ProviderUsage::new(model_config.model_name.clone(), Usage::default()),
        ))
    }

    pub async fn stream(
        &self,
        session_id: &str,
        _system: &str,
        messages: &[Message],
        _tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        let prompt_blocks = messages_to_prompt(messages);
        let mut rx = self
            .prompt(SessionId::new(session_id.to_string()), prompt_blocks)
            .await
            .map_err(|e| ProviderError::RequestFailed(format!("Failed to send ACP prompt: {e}")))?;

        let pending_confirmations = self.pending_confirmations.clone();
        let rejected_tool_calls = self.rejected_tool_calls.clone();
        let permission_mapping = self.permission_mapping.clone();
        let goose_mode = self.goose_mode;

        Ok(Box::pin(try_stream! {
            while let Some(update) = rx.recv().await {
                match update {
                    AcpUpdate::Text(text) => {
                        let message = Message::assistant().with_text(text);
                        yield (Some(message), None);
                    }
                    AcpUpdate::Thought(text) => {
                        let message = Message::assistant()
                            .with_thinking(text, "")
                            .with_visibility(true, false);
                        yield (Some(message), None);
                    }
                    AcpUpdate::ToolCallStart { id, title, raw_input } => {
                        let arguments = raw_input
                            .and_then(|v| v.as_object().cloned())
                            .unwrap_or_default();

                        let tool_call = CallToolRequestParams {
                            meta: None,
                            task: None,
                            name: title.into(),
                            arguments: Some(arguments),
                        };
                        let message = Message::assistant().with_tool_request(id.clone(), Ok(tool_call));
                        yield (Some(message), None);
                    }
                    AcpUpdate::ToolCallComplete { id, status, content } => {
                        let result_text = tool_call_content_to_text(&content);
                        let is_error = tool_call_is_error(&rejected_tool_calls, &permission_mapping, &id, status).await;

                        let call_result = CallToolResult {
                            content: if result_text.is_empty() {
                                content_blocks_to_rmcp(&content)
                            } else {
                                vec![Content::text(result_text)]
                            },
                            structured_content: None,
                            is_error: Some(is_error),
                            meta: None,
                        };

                        let message = Message::assistant().with_tool_response(id, Ok(call_result));
                        yield (Some(message), None);
                    }
                    AcpUpdate::PermissionRequest { request, response_tx } => {
                        if let Some(decision) = permission_decision_from_mode(goose_mode) {
                            let response = permission_response(&permission_mapping, &rejected_tool_calls, &request, decision).await;
                            let _ = response_tx.send(response);
                            continue;
                        }

                        let request_id = request.tool_call.tool_call_id.0.to_string();
                        let (tx, rx) = oneshot::channel();

                        pending_confirmations
                            .lock()
                            .await
                            .insert(request_id.clone(), tx);

                        if let Some(action_required) = build_action_required_message(&request) {
                            yield (Some(action_required), None);
                        }

                        let confirmation = rx.await.unwrap_or(PermissionConfirmation {
                            principal_type: PrincipalType::Tool,
                            permission: Permission::Cancel,
                        });

                        pending_confirmations.lock().await.remove(&request_id);

                        let decision = permission_decision_from_confirmation(&confirmation);
                        let response = permission_response(&permission_mapping, &rejected_tool_calls, &request, decision).await;
                        let _ = response_tx.send(response);
                    }
                    AcpUpdate::Complete(_reason) => {
                        break;
                    }
                    AcpUpdate::Error(e) => {
                        Err(ProviderError::RequestFailed(e))?;
                    }
                }
            }
        }))
    }

    pub async fn create_session(
        &self,
        session_manager: &SessionManager,
        working_dir: PathBuf,
        name: String,
        session_type: SessionType,
    ) -> Result<Session> {
        let acp_session_id = self.new_session().await?;
        let session = session_manager
            .create_session_with_id(
                acp_session_id.0.to_string(),
                working_dir,
                name,
                session_type,
            )
            .await?;
        self.sessions
            .lock()
            .await
            .insert(session.id.clone(), session.clone());
        Ok(session)
    }

    pub async fn ensure_session<'a>(
        &self,
        session_id: Option<&'a str>,
    ) -> Result<&'a str, ProviderError> {
        let session_id = session_id.ok_or_else(|| {
            ProviderError::RequestFailed("ACP session_id is required".to_string())
        })?;

        let has_session = self.sessions.lock().await.contains_key(session_id);
        if has_session {
            Ok(session_id)
        } else {
            Err(ProviderError::RequestFailed(format!(
                "ACP session '{}' not found; resume is not supported",
                session_id
            )))
        }
    }

    async fn prompt(
        &self,
        session_id: SessionId,
        content: Vec<ContentBlock>,
    ) -> Result<mpsc::Receiver<AcpUpdate>> {
        let (response_tx, response_rx) = mpsc::channel(64);
        self.tx
            .send(ClientRequest::Prompt {
                session_id,
                content,
                response_tx,
            })
            .await
            .context("ACP client is unavailable")?;
        Ok(response_rx)
    }
}

#[async_trait::async_trait]
impl Provider for AcpProvider {
    fn get_name(&self) -> &str {
        self.name()
    }

    fn get_model_config(&self) -> ModelConfig {
        self.model()
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn permission_routing(&self) -> PermissionRouting {
        AcpProvider::permission_routing(self)
    }

    async fn handle_permission_confirmation(
        &self,
        request_id: &str,
        confirmation: &PermissionConfirmation,
    ) -> bool {
        AcpProvider::handle_permission_confirmation(self, request_id, confirmation).await
    }

    async fn complete_with_model(
        &self,
        session_id: Option<&str>,
        model_config: &ModelConfig,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<(Message, ProviderUsage), ProviderError> {
        let session_id = self.ensure_session(session_id).await?;
        AcpProvider::complete_with_model(self, session_id, model_config, system, messages, tools)
            .await
    }

    async fn stream(
        &self,
        session_id: &str,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        let session_id = self.ensure_session(Some(session_id)).await?;
        AcpProvider::stream(self, session_id, system, messages, tools).await
    }

    async fn create_session(
        &self,
        session_manager: &SessionManager,
        working_dir: PathBuf,
        name: String,
        session_type: SessionType,
    ) -> Result<Session> {
        AcpProvider::create_session(self, session_manager, working_dir, name, session_type).await
    }
}

impl Drop for AcpProvider {
    fn drop(&mut self) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(ClientRequest::Shutdown).await;
        });
    }
}

async fn run_client_loop(
    config: AcpProviderConfig,
    mut rx: mpsc::Receiver<ClientRequest>,
    init_tx: oneshot::Sender<Result<()>>,
) {
    let init_tx = Arc::new(Mutex::new(Some(init_tx)));

    let child = match spawn_acp_process(&config).await {
        Ok(c) => c,
        Err(e) => {
            let message = e.to_string();
            send_init_result(&init_tx, Err(anyhow::anyhow!(message.clone())));
            tracing::error!("failed to spawn ACP process: {message}");
            return;
        }
    };

    if let Err(e) = run_protocol_loop_with_child(config, child, &mut rx, init_tx.clone()).await {
        let message = e.to_string();
        send_init_result(&init_tx, Err(anyhow::anyhow!(message.clone())));
        tracing::error!("ACP protocol error: {message}");
    }
}

async fn spawn_acp_process(config: &AcpProviderConfig) -> Result<Child> {
    let mut cmd = Command::new(&config.command);
    cmd.args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    for (key, value) in &config.env {
        cmd.env(key, value);
    }

    cmd.spawn().context("failed to spawn ACP process")
}

async fn run_protocol_loop_with_child(
    config: AcpProviderConfig,
    mut child: Child,
    rx: &mut mpsc::Receiver<ClientRequest>,
    init_tx: Arc<Mutex<Option<oneshot::Sender<Result<()>>>>>,
) -> Result<()> {
    let stdin = child.stdin.take().context("no stdin")?;
    let stdout = child.stdout.take().context("no stdout")?;
    let transport = sacp::ByteStreams::new(stdin.compat_write(), stdout.compat());
    run_protocol_loop_with_transport(config, transport, rx, init_tx).await
}

async fn run_protocol_loop_with_transport<R, W>(
    config: AcpProviderConfig,
    transport: sacp::ByteStreams<W, R>,
    rx: &mut mpsc::Receiver<ClientRequest>,
    init_tx: Arc<Mutex<Option<oneshot::Sender<Result<()>>>>>,
) -> Result<()>
where
    R: futures::AsyncRead + Unpin + Send + 'static,
    W: futures::AsyncWrite + Unpin + Send + 'static,
{
    let prompt_response_tx: Arc<Mutex<Option<mpsc::Sender<AcpUpdate>>>> =
        Arc::new(Mutex::new(None));

    ClientToAgent::builder()
        .on_receive_notification(
            {
                let prompt_response_tx = prompt_response_tx.clone();
                async move |notification: SessionNotification, _cx| {
                    if let Some(tx) = prompt_response_tx.lock().unwrap().as_ref() {
                        match notification.update {
                            SessionUpdate::AgentMessageChunk(ContentChunk {
                                content: ContentBlock::Text(TextContent { text, .. }),
                                ..
                            }) => {
                                let _ = tx.try_send(AcpUpdate::Text(text));
                            }
                            SessionUpdate::AgentThoughtChunk(ContentChunk {
                                content: ContentBlock::Text(TextContent { text, .. }),
                                ..
                            }) => {
                                let _ = tx.try_send(AcpUpdate::Thought(text));
                            }
                            SessionUpdate::ToolCall(tool_call) => {
                                let _ = tx.try_send(AcpUpdate::ToolCallStart {
                                    id: tool_call.tool_call_id.0.to_string(),
                                    title: tool_call.title,
                                    raw_input: tool_call.raw_input,
                                });
                            }
                            SessionUpdate::ToolCallUpdate(update) => {
                                if let Some(status) = update.fields.status {
                                    let _ = tx.try_send(AcpUpdate::ToolCallComplete {
                                        id: update.tool_call_id.0.to_string(),
                                        status,
                                        content: update.fields.content.unwrap_or_default(),
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(())
                }
            },
            sacp::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let prompt_response_tx = prompt_response_tx.clone();
                async move |request: RequestPermissionRequest, request_cx, _connection_cx| {
                    let (response_tx, response_rx) = oneshot::channel();

                    let handler = prompt_response_tx.lock().unwrap().as_ref().cloned();
                    let tx = handler.ok_or_else(sacp::Error::internal_error)?;

                    if tx.is_closed() {
                        return Err(sacp::Error::internal_error());
                    }

                    tx.try_send(AcpUpdate::PermissionRequest {
                        request: Box::new(request),
                        response_tx,
                    })
                    .map_err(|_| sacp::Error::internal_error())?;

                    let response = response_rx.await.unwrap_or_else(|_| {
                        RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
                    });
                    request_cx.respond(response)
                }
            },
            sacp::on_receive_request!(),
        )
        .connect_to(transport)?
        .run_until({
            let prompt_response_tx = prompt_response_tx.clone();
            move |cx: JrConnectionCx<ClientToAgent>| {
                handle_requests(config, cx, rx, prompt_response_tx, init_tx.clone())
            }
        })
        .await?;

    Ok(())
}

async fn handle_requests(
    config: AcpProviderConfig,
    cx: JrConnectionCx<ClientToAgent>,
    rx: &mut mpsc::Receiver<ClientRequest>,
    prompt_response_tx: Arc<Mutex<Option<mpsc::Sender<AcpUpdate>>>>,
    init_tx: Arc<Mutex<Option<oneshot::Sender<Result<()>>>>>,
) -> Result<(), sacp::Error> {
    cx.send_request(InitializeRequest::new(ProtocolVersion::LATEST))
        .block_task()
        .await
        .map_err(|err| {
            let message = format!("ACP initialize failed: {err}");
            send_init_result(&init_tx, Err(anyhow::anyhow!(message.clone())));
            sacp::Error::internal_error().data(message)
        })?;

    send_init_result(&init_tx, Ok(()));

    while let Some(request) = rx.recv().await {
        match request {
            ClientRequest::NewSession { response_tx } => {
                let session = cx
                    .send_request(
                        NewSessionRequest::new(config.work_dir.clone())
                            .mcp_servers(config.mcp_servers.clone()),
                    )
                    .block_task()
                    .await;

                let result = match session {
                    Ok(session) => {
                        let session_id = session.session_id.clone();
                        let mut result = Ok(session_id);

                        if let Some(mode_id) = config.session_mode_id.clone() {
                            let modes = match session.modes {
                                Some(modes) => Some(modes),
                                None => {
                                    result = Err(anyhow::anyhow!(
                                        "ACP agent did not advertise SessionModeState"
                                    ));
                                    None
                                }
                            };

                            if let (Some(modes), Ok(_)) = (modes, result.as_ref()) {
                                if modes.current_mode_id.0.as_ref() != mode_id.as_str() {
                                    let available: Vec<String> = modes
                                        .available_modes
                                        .iter()
                                        .map(|mode| mode.id.0.to_string())
                                        .collect();

                                    if !available.iter().any(|id| id == &mode_id) {
                                        result = Err(anyhow::anyhow!(
                                            "Requested mode '{}' not offered by agent. Available modes: {}",
                                            mode_id,
                                            available.join(", ")
                                        ));
                                    } else if let Err(err) = cx
                                        .send_request(SetSessionModeRequest::new(
                                            session.session_id.clone(),
                                            mode_id,
                                        ))
                                        .block_task()
                                        .await
                                    {
                                        result = Err(anyhow::anyhow!(
                                            "ACP agent rejected session/set_mode: {err}"
                                        ));
                                    }
                                }
                            }
                        }

                        result
                    }
                    Err(err) => Err(anyhow::anyhow!("ACP session/new failed: {err}")),
                };

                let _ = response_tx.send(result);
            }
            ClientRequest::Prompt {
                session_id,
                content,
                response_tx,
            } => {
                *prompt_response_tx.lock().unwrap() = Some(response_tx.clone());

                let response = cx
                    .send_request(PromptRequest::new(session_id, content))
                    .block_task()
                    .await;

                match response {
                    Ok(r) => {
                        let _ = response_tx.try_send(AcpUpdate::Complete(r.stop_reason));
                    }
                    Err(e) => {
                        let _ = response_tx.try_send(AcpUpdate::Error(e.to_string()));
                    }
                }

                *prompt_response_tx.lock().unwrap() = None;
            }
            ClientRequest::Shutdown => break,
        }
    }

    Ok(())
}

fn send_init_result(init_tx: &Arc<Mutex<Option<oneshot::Sender<Result<()>>>>>, result: Result<()>) {
    if let Some(tx) = init_tx.lock().unwrap().take() {
        let _ = tx.send(result);
    }
}

async fn permission_response(
    mapping: &PermissionMapping,
    rejected_tool_calls: &Arc<TokioMutex<HashSet<String>>>,
    request: &RequestPermissionRequest,
    decision: PermissionDecision,
) -> RequestPermissionResponse {
    if decision.should_record_rejection() {
        rejected_tool_calls
            .lock()
            .await
            .insert(request.tool_call.tool_call_id.0.to_string());
    }

    map_permission_response(mapping, request, decision)
}

async fn tool_call_is_error(
    rejected_tool_calls: &Arc<TokioMutex<HashSet<String>>>,
    mapping: &PermissionMapping,
    tool_call_id: &str,
    status: ToolCallStatus,
) -> bool {
    let was_rejected = rejected_tool_calls.lock().await.remove(tool_call_id);

    match status {
        ToolCallStatus::Failed => true,
        ToolCallStatus::Completed => {
            was_rejected && mapping.rejected_tool_status == ToolCallStatus::Completed
        }
        _ => false,
    }
}

fn text_content(text: impl Into<String>) -> ContentBlock {
    ContentBlock::Text(TextContent::new(text))
}

fn messages_to_prompt(messages: &[Message]) -> Vec<ContentBlock> {
    let mut content_blocks = Vec::new();

    let last_user = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User && m.is_agent_visible());

    if let Some(message) = last_user {
        for content in &message.content {
            if let MessageContent::Text(text) = content {
                content_blocks.push(text_content(text.text.clone()));
            }
        }
    }

    content_blocks
}

fn build_action_required_message(request: &RequestPermissionRequest) -> Option<Message> {
    let tool_title = request
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| "Tool".to_string());

    let arguments = request
        .tool_call
        .fields
        .raw_input
        .as_ref()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    let prompt = request
        .tool_call
        .fields
        .content
        .as_ref()
        .and_then(|content| {
            content.iter().find_map(|c| match c {
                ToolCallContent::Content(val) => match &val.content {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                },
                _ => None,
            })
        });

    Some(
        Message::assistant()
            .with_action_required(
                request.tool_call.tool_call_id.0.to_string(),
                tool_title,
                arguments,
                prompt,
            )
            .user_only(),
    )
}

fn permission_decision_from_confirmation(
    confirmation: &PermissionConfirmation,
) -> PermissionDecision {
    match confirmation.permission {
        Permission::AlwaysAllow => PermissionDecision::AllowAlways,
        Permission::AllowOnce => PermissionDecision::AllowOnce,
        Permission::DenyOnce => PermissionDecision::RejectOnce,
        Permission::AlwaysDeny => PermissionDecision::RejectAlways,
        Permission::Cancel => PermissionDecision::Cancel,
    }
}

fn permission_decision_from_mode(goose_mode: GooseMode) -> Option<PermissionDecision> {
    match goose_mode {
        GooseMode::Auto => Some(PermissionDecision::AllowOnce),
        GooseMode::Chat => Some(PermissionDecision::RejectOnce),
        GooseMode::Approve | GooseMode::SmartApprove => None,
    }
}

fn tool_call_content_to_text(content: &[ToolCallContent]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            ToolCallContent::Content(val) => match &val.content {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn content_blocks_to_rmcp(content: &[ToolCallContent]) -> Vec<Content> {
    content
        .iter()
        .filter_map(|c| match c {
            ToolCallContent::Content(val) => match &val.content {
                ContentBlock::Text(text) => Some(Content::text(text.text.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}
