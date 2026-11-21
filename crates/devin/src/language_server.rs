use std::{
    collections::HashMap,
    io::Read as _,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{
    AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, FutureExt as _, channel::oneshot,
};
use gpui::{BackgroundExecutor, Task};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, Method, Request, StatusCode};
use parking_lot::Mutex;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use util::ResultExt as _;
use uuid::Uuid;

use crate::lsp_connection::{LspConnection, WorkspaceFolder};
use crate::protocol::{
    CONNECT_COMPRESSED_FLAG, CONNECT_END_STREAM_FLAG, Document, EXTENSION_SERVER_SERVICE,
    EditorOptions, GetUserStatusResponse, HandleStreamingTabV2Request,
    HandleStreamingTabV2Response, IdeAction, LANGUAGE_SERVER_SERVICE, Metadata, MetadataRequest,
    ProtobufValue, ProvideCompletionFeedbackRequest, ProviderSource,
    RefreshContextForIdeActionRequest, StreamTabSuggestionsRequest, StreamTabSuggestionsResponse,
    SupercompleteTriggerCondition, decode_protobuf_fields, encode_connect_envelope,
    encode_native_values_response, encode_set_native_value_response, parse_language_server_started,
};

pub const API_SERVER_URL: &str = "https://server.codeium.com";
const INFERENCE_API_SERVER_URL: &str = "https://inference.codeium.com";

/// Model routing in the language server's rollout configuration matches on this exact IDE name,
/// so requests identify as the Windsurf extension. Other names fall back to an older model whose
/// requests fail upstream.
const COMPATIBLE_IDE_NAME: &str = "windsurf";

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const RPC_TIMEOUT: Duration = Duration::from_secs(15);
/// The server holds a completion request open instead of answering when it has nothing to
/// suggest, so a request that runs this long is treated as having no suggestion.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(15);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HEARTBEAT_FAILURES: u32 = 3;
/// The proven startup sequence waited this long for the server's configuration refresh before
/// the first completion. A shorter minimum hasn't been established.
const CONFIGURATION_WARMUP: Duration = Duration::from_secs(5);
const MAX_CALLBACK_BODY: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct DevinInstallation {
    pub binary_path: PathBuf,
    pub app_root: PathBuf,
    pub extension_path: PathBuf,
    pub distribution_version: String,
    pub protocol_version: String,
}

impl DevinInstallation {
    pub fn locate() -> Result<Self> {
        let binary_path = default_binary_path()?;
        if !binary_path.is_file() {
            bail!(
                "Devin's language server was not found at {}. Install Devin Desktop to use Devin edit predictions.",
                binary_path.display()
            );
        }
        let extension_path = binary_path
            .parent()
            .and_then(|bin_dir| bin_dir.parent())
            .context("unexpected language server location")?
            .to_path_buf();
        let app_root = extension_path
            .parent()
            .and_then(|extensions_dir| extensions_dir.parent())
            .context("unexpected language server location")?
            .to_path_buf();
        let product_json = std::fs::read_to_string(app_root.join("product.json"))
            .context("failed to read Devin's product.json")?;
        let product: serde_json::Value =
            serde_json::from_str(&product_json).context("failed to parse Devin's product.json")?;
        let version_field = |name: &str| {
            product
                .get(name)
                .and_then(|value| value.as_str())
                .map(ToString::to_string)
                .with_context(|| format!("Devin's product.json has no {name}"))
        };
        Ok(Self {
            distribution_version: version_field("windsurfVersion")?,
            protocol_version: version_field("codeiumVersion")?,
            binary_path,
            app_root,
            extension_path,
        })
    }
}

#[cfg(target_os = "windows")]
fn default_binary_path() -> Result<PathBuf> {
    let local_app_data = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(local_app_data).join(
        "Programs/Devin/resources/app/extensions/windsurf/bin/language_server_windows_x64.exe",
    ))
}

#[cfg(not(target_os = "windows"))]
fn default_binary_path() -> Result<PathBuf> {
    bail!("Devin edit predictions are currently only supported on Windows")
}

pub struct DevinLanguageServer {
    rpc: Arc<Rpc>,
    metadata: Mutex<Metadata>,
    next_request_id: AtomicU64,
    healthy: Arc<AtomicBool>,
    context_state: Mutex<ContextState>,
    process: Mutex<util::process::Child>,
    lsp: Option<LspConnection>,
    executor: BackgroundExecutor,
    _callback_server: CallbackServer,
    _tasks: Vec<Task<()>>,
}

pub struct CompletionFeedback {
    pub source: ProviderSource,
    pub completion_id: String,
    pub prompt_id: String,
    pub document: Document,
    /// The document text with the completion applied.
    pub completion_text: String,
    pub is_accepted: bool,
    pub is_intentional_reject: bool,
    pub feedback_delay: Duration,
}

#[derive(Default)]
struct ContextState {
    startup_refreshed: bool,
    active_document_uri: Option<String>,
}

impl DevinLanguageServer {
    pub async fn start(
        installation: DevinInstallation,
        api_key: Arc<str>,
        http_client: Arc<dyn HttpClient>,
        executor: BackgroundExecutor,
        initial_workspace: WorkspaceFolder,
    ) -> Result<Self> {
        let csrf_token = Uuid::new_v4().to_string();
        let data_dir = paths::data_dir().join("devin");
        let state_dir = data_dir.join("state");
        let database_dir = data_dir.join("database");
        let manager_dir = data_dir.join("manager");
        for dir in [&state_dir, &database_dir, &manager_dir] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }

        let mut metadata = Metadata {
            ide_name: COMPATIBLE_IDE_NAME.into(),
            ide_version: installation.distribution_version.clone(),
            extension_name: COMPATIBLE_IDE_NAME.into(),
            extension_version: installation.protocol_version.clone(),
            extension_path: installation.extension_path.to_string_lossy().into_owned(),
            api_key: api_key.to_string(),
            session_id: Uuid::new_v4().to_string(),
            request_id: "1".into(),
            locale: "en".into(),
            os: std::env::consts::OS.into(),
            disable_telemetry: true,
            plan_name: "Unset".into(),
            team_id: String::new(),
        };

        let (callback_server, ports) = CallbackServer::start(csrf_token.clone())?;

        let mut command = util::command::new_std_command(&installation.binary_path);
        command
            .args([
                "--run_child",
                "--enable_lsp",
                "--random_port",
                "--extension_server_port",
                &callback_server.port.to_string(),
                "--ide_name",
                "zed",
                "--api_server_url",
                API_SERVER_URL,
                "--inference_api_server_url",
                INFERENCE_API_SERVER_URL,
                "--stdin_initial_metadata",
                "--detect_proxy=false",
                "--windsurf_version",
                &installation.distribution_version,
                "--enable_index_service=true",
                "--enable_local_search=true",
                "--record_cortex_telemetry=false",
            ])
            .arg("--codeium_dir")
            .arg(&state_dir)
            .arg("--database_dir")
            .arg(&database_dir)
            .arg("--manager_dir")
            .arg(&manager_dir)
            .env_remove("WINDSURF_API_KEY")
            .env("WINDSURF_CSRF_TOKEN", &csrf_token)
            .env("CODEIUM_EDITOR_APP_ROOT", &installation.app_root)
            .current_dir(&state_dir);

        let mut process =
            util::process::Child::spawn(command, Stdio::piped(), Stdio::piped(), Stdio::piped())
                .context("failed to start Devin's language server")?;

        let secrets: Arc<[String]> = Arc::from([api_key.to_string(), csrf_token.clone()]);
        let mut tasks = Vec::new();
        if let Some(stdout) = process.stdout.take() {
            tasks.push(executor.spawn(log_output(stdout, secrets.clone())));
        }
        if let Some(stderr) = process.stderr.take() {
            tasks.push(executor.spawn(log_output(stderr, secrets)));
        }

        let mut stdin = process
            .stdin
            .take()
            .context("language server has no stdin")?;
        stdin.write_all(&metadata.to_protobuf()).await?;
        stdin.close().await?;
        drop(stdin);

        let mut ports = ports.fuse();
        let mut startup_timeout = executor.timer(STARTUP_TIMEOUT).fuse();
        let (port, lsp_port) = loop {
            let mut poll_interval = executor.timer(Duration::from_millis(100)).fuse();
            futures::select_biased! {
                ports = ports => break ports.context("callback server stopped during startup")?,
                _ = startup_timeout => bail!("Devin's language server did not start within {STARTUP_TIMEOUT:?}"),
                _ = poll_interval => {
                    if let Some(status) = process.try_status()? {
                        bail!("Devin's language server exited during startup ({status})");
                    }
                }
            }
        };

        let rpc = Arc::new(Rpc {
            http_client,
            base_url: format!("http://127.0.0.1:{port}"),
            csrf_token,
            devin_version: installation.distribution_version.clone(),
        });

        let status: GetUserStatusResponse = rpc
            .call(
                "GetUserStatus",
                &MetadataRequest {
                    metadata: metadata.clone(),
                },
                RPC_TIMEOUT,
            )
            .await
            .context("failed to get Devin account status")?;
        if let Some(user_status) = status.user_status {
            let plan_info = user_status
                .get("planStatus")
                .and_then(|plan_status| plan_status.get("planInfo"));
            if let Some(plan_name) = plan_info
                .and_then(|plan_info| plan_info.get("planName"))
                .and_then(|plan_name| plan_name.as_str())
            {
                metadata.plan_name = plan_name.to_string();
            }
            if let Some(team_id) = user_status
                .get("teamId")
                .and_then(|team_id| team_id.as_str())
            {
                metadata.team_id = team_id.to_string();
            }
        }

        rpc.call::<_, serde_json::Value>(
            "Heartbeat",
            &MetadataRequest {
                metadata: metadata.clone(),
            },
            HEARTBEAT_TIMEOUT,
        )
        .await
        .context("Devin's language server did not respond to its first heartbeat")?;

        let healthy = Arc::new(AtomicBool::new(true));
        tasks.push(executor.spawn({
            let rpc = rpc.clone();
            let healthy = healthy.clone();
            let request = MetadataRequest {
                metadata: metadata.clone(),
            };
            let executor = executor.clone();
            async move {
                let mut failures = 0;
                loop {
                    executor.timer(HEARTBEAT_INTERVAL).await;
                    match rpc
                        .call::<_, serde_json::Value>("Heartbeat", &request, HEARTBEAT_TIMEOUT)
                        .await
                    {
                        Ok(_) => failures = 0,
                        Err(error) => {
                            failures += 1;
                            log::warn!("Devin heartbeat failed ({failures}): {error:#}");
                            if failures >= MAX_HEARTBEAT_FAILURES {
                                healthy.store(false, Ordering::SeqCst);
                                return;
                            }
                        }
                    }
                }
            }
        }));

        let lsp = if lsp_port == 0 {
            log::warn!("Devin's language server did not publish an LSP port");
            None
        } else {
            LspConnection::connect(lsp_port, initial_workspace, executor.clone())
                .await
                .log_err()
        };

        executor.timer(CONFIGURATION_WARMUP).await;

        log::info!(
            "Devin language server ready (Devin {}, protocol {})",
            installation.distribution_version,
            installation.protocol_version
        );

        Ok(Self {
            rpc,
            metadata: Mutex::new(metadata),
            next_request_id: AtomicU64::new(2),
            healthy,
            context_state: Mutex::default(),
            process: Mutex::new(process),
            lsp,
            executor,
            _callback_server: callback_server,
            _tasks: tasks,
        })
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::SeqCst) && matches!(self.process.lock().try_status(), Ok(None))
    }

    fn next_metadata(&self) -> Metadata {
        let mut metadata = self.metadata.lock().clone();
        metadata.request_id = self
            .next_request_id
            .fetch_add(1, Ordering::SeqCst)
            .to_string();
        metadata
    }

    pub async fn complete(
        &self,
        document: Document,
        other_documents: Vec<Document>,
        editor_options: EditorOptions,
        trigger: SupercompleteTriggerCondition,
    ) -> Result<HandleStreamingTabV2Response> {
        if let Some(lsp) = &self.lsp {
            lsp.add_workspace_folder(WorkspaceFolder::from_uri(&document.workspace_uri))
                .await
                .log_err();
            lsp.sync_document(
                &document.absolute_uri,
                &document.editor_language,
                &document.text,
            )
            .await
            .log_err();
        }
        self.refresh_context_if_needed(&document, &other_documents)
            .await?;
        let request = StreamTabSuggestionsRequest {
            request: HandleStreamingTabV2Request {
                metadata: self.next_metadata(),
                document,
                editor_options,
                supercomplete_trigger_condition: trigger,
                other_documents,
                disable_supercomplete: false,
                disable_tab_jump: false,
                supercomplete_aggression: "SUPERCOMPLETE_AGGRESSION_MEDIUM",
            },
        };
        let started_at = Instant::now();
        // The HTTP timeout only backs up the completion timeout, which ends the request first.
        let call = async {
            let mut stream = self
                .rpc
                .begin_server_stream("StreamTabSuggestions", &request, COMPLETION_TIMEOUT * 2)
                .await?;
            read_suggestion(&mut stream, trigger).await
        }
        .fuse();
        let timeout = self.executor.timer(COMPLETION_TIMEOUT).fuse();
        futures::pin_mut!(call, timeout);
        let result = futures::select_biased! {
            result = call => result,
            _ = timeout => {
                log::debug!(
                    "Devin completion request ({trigger:?}) had no suggestion within {COMPLETION_TIMEOUT:?}"
                );
                return Ok(HandleStreamingTabV2Response::default());
            }
        };
        let elapsed_ms = started_at.elapsed().as_millis();
        match &result {
            Err(error)
                if error
                    .downcast_ref::<RpcError>()
                    .is_some_and(|error| error.code == "canceled") =>
            {
                // The extension ignores requests the server cancels, as it does when a newer
                // request supersedes them.
                log::debug!(
                    "Devin completion request ({trigger:?}) was canceled by the server after {elapsed_ms}ms"
                );
                return Ok(HandleStreamingTabV2Response::default());
            }
            Ok(response) => {
                if response.diff.is_some() {
                    log::debug!(
                        "Devin completion request ({trigger:?}) returned an edit in {elapsed_ms}ms"
                    );
                } else if response.tab_jump.is_some() {
                    log::debug!(
                        "Devin completion request ({trigger:?}) returned a jump in {elapsed_ms}ms"
                    );
                } else {
                    log::debug!(
                        "Devin completion request ({trigger:?}) returned nothing in {elapsed_ms}ms (stop reason: {:?}, filter reason: {:?})",
                        response.stop_reason,
                        response.filter_reason
                    );
                }
            }
            Err(error) => {
                log::debug!(
                    "Devin completion request ({trigger:?}) failed after {elapsed_ms}ms: {error:#}"
                );
            }
        }
        result
    }

    /// Reports whether a shown completion was accepted. Recent feedback is part of the model's
    /// input for later completions.
    pub async fn provide_feedback(&self, feedback: CompletionFeedback) -> Result<()> {
        let request = ProvideCompletionFeedbackRequest {
            metadata: self.next_metadata(),
            is_accepted: feedback.is_accepted,
            feedback_delay_ms: u64::try_from(feedback.feedback_delay.as_millis())
                .unwrap_or(u64::MAX),
            completion_id: feedback.completion_id,
            prompt_id: feedback.prompt_id,
            source: feedback.source,
            document: feedback.document,
            is_intentional_reject: feedback.is_intentional_reject,
            completion_text: feedback.completion_text,
        };
        self.rpc
            .call::<_, serde_json::Value>("ProvideCompletionFeedback", &request, RPC_TIMEOUT)
            .await
            .context("failed to send Devin completion feedback")?;
        Ok(())
    }

    /// Completions fail upstream until the active document has been registered through this
    /// editor-context lifecycle, which is separate from LSP document synchronization.
    async fn refresh_context_if_needed(
        &self,
        document: &Document,
        other_documents: &[Document],
    ) -> Result<()> {
        let actions = {
            let state = self.context_state.lock();
            if state.active_document_uri.as_deref() == Some(document.absolute_uri.as_str()) {
                return Ok(());
            }
            if state.startup_refreshed {
                vec![IdeAction::ActiveEditorChanged]
            } else {
                vec![IdeAction::Startup, IdeAction::ActiveEditorChanged]
            }
        };

        let active_document = Document {
            cursor_position: None,
            selections: Vec::new(),
            language: None,
            ..document.clone()
        };
        for ide_action in actions {
            self.rpc
                .call::<_, serde_json::Value>(
                    "RefreshContextForIdeAction",
                    &RefreshContextForIdeActionRequest {
                        metadata: self.next_metadata(),
                        active_document: active_document.clone(),
                        other_documents: other_documents.to_vec(),
                        open_document_uris: std::iter::once(document)
                            .chain(other_documents)
                            .map(|document| document.absolute_uri.clone())
                            .collect(),
                        blocking: true,
                        ide_action,
                    },
                    RPC_TIMEOUT,
                )
                .await
                .context("failed to refresh Devin's editor context")?;
        }

        let mut state = self.context_state.lock();
        state.startup_refreshed = true;
        state.active_document_uri = Some(document.absolute_uri.clone());
        Ok(())
    }
}

impl Drop for DevinLanguageServer {
    fn drop(&mut self) {
        self.process.get_mut().kill().log_err();
    }
}

/// An error status returned by the server, with the Connect error's code and message.
#[derive(Debug)]
struct RpcError {
    method: String,
    status: String,
    code: String,
    message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.status.is_empty() {
            return write!(
                formatter,
                "{} returned {}: {}",
                self.method, self.code, self.message
            );
        }
        write!(
            formatter,
            "{} returned {} {}: {}",
            self.method, self.status, self.code, self.message
        )
    }
}

impl std::error::Error for RpcError {}

struct Rpc {
    http_client: Arc<dyn HttpClient>,
    base_url: String,
    csrf_token: String,
    devin_version: String,
}

impl Rpc {
    fn http_request(
        &self,
        method: &str,
        content_type: &str,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<Request<AsyncBody>> {
        Ok(Request::builder()
            .method(Method::POST)
            .uri(format!(
                "{}{LANGUAGE_SERVER_SERVICE}{method}",
                self.base_url
            ))
            .header("Content-Type", content_type)
            .header("Connect-Protocol-Version", "1")
            .header("x-codeium-csrf-token", &self.csrf_token)
            .timeout(timeout)
            .body(AsyncBody::from(body))?)
    }

    /// Builds the error `call` and the stream reader report for a non-2xx response.
    fn rpc_error(&self, method: &str, status: StatusCode, body: &[u8]) -> RpcError {
        rpc_error(method, &self.devin_version, status, body)
    }

    async fn call<Req: Serialize, Res: DeserializeOwned>(
        &self,
        method: &str,
        request: &Req,
        timeout: Duration,
    ) -> Result<Res> {
        let http_request = self.http_request(
            method,
            "application/json",
            serde_json::to_vec(request)?,
            timeout,
        )?;
        let response = self
            .http_client
            .send(http_request)
            .await
            .with_context(|| format!("{method} request failed"))?;
        let status = response.status();
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes).await?;
        if !status.is_success() {
            return Err(self.rpc_error(method, status, &bytes).into());
        }
        if bytes.is_empty() {
            bytes = b"{}".to_vec();
        }
        serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {method} response"))
    }

    /// Starts a Connect server-streaming call. The request body is a single enveloped message;
    /// the response body is read frame by frame by [`ServerStream`].
    async fn begin_server_stream<Req: Serialize>(
        &self,
        method: &str,
        request: &Req,
        timeout: Duration,
    ) -> Result<ServerStream> {
        let body = encode_connect_envelope(&serde_json::to_vec(request)?)?;
        let http_request = self.http_request(method, "application/connect+json", body, timeout)?;
        let response = self
            .http_client
            .send(http_request)
            .await
            .with_context(|| format!("{method} request failed"))?;
        let status = response.status();
        let mut body = response.into_body();
        if !status.is_success() {
            let mut bytes = Vec::new();
            body.read_to_end(&mut bytes).await?;
            return Err(self.rpc_error(method, status, &bytes).into());
        }
        Ok(ServerStream::new(method, body))
    }
}

/// The largest frame accepted from the stream, so that a malformed length can't force a huge
/// allocation. Language server frames carry a document-sized diff, well under this.
const MAX_STREAM_FRAME_BYTES: usize = 64 * 1024 * 1024;

fn rpc_error(method: &str, devin_version: &str, status: StatusCode, body: &[u8]) -> RpcError {
    let error = serde_json::from_slice::<serde_json::Value>(body).unwrap_or_default();
    let field = |name: &str| {
        error
            .get(name)
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string()
    };
    let mut message = field("message");
    if status == StatusCode::NOT_FOUND {
        // A missing route means Devin renamed or removed the RPC, not that the request was
        // wrong. Say so instead of leaving a bare 404 in the logs.
        let server_message = std::mem::take(&mut message);
        message = format!(
            "the {method} route is missing from Devin's language server (Devin {devin_version}), likely because Devin was updated and this provider needs updating"
        );
        if !server_message.is_empty() {
            message.push_str(&format!(": {server_message}"));
        }
    }
    RpcError {
        method: method.to_string(),
        status: status.to_string(),
        code: field("code"),
        message,
    }
}

/// A server-streaming Connect response, decoded one length-prefixed JSON frame at a time.
struct ServerStream {
    method: String,
    body: AsyncBody,
}

impl ServerStream {
    fn new(method: impl Into<String>, body: AsyncBody) -> Self {
        Self {
            method: method.into(),
            body,
        }
    }

    /// Returns the next message, or `None` once the server ends the stream. An error trailer ends
    /// the stream with the same error type a unary call would have produced.
    async fn next_frame<Res: DeserializeOwned>(&mut self) -> Result<Option<Res>> {
        let mut header = [0u8; 5];
        if self.body.read(&mut header[..1]).await? == 0 {
            return Ok(None);
        }
        self.body.read_exact(&mut header[1..]).await?;
        let flags = header[0];
        if flags & CONNECT_COMPRESSED_FLAG != 0 {
            bail!(
                "{} response is compressed, which is not supported",
                self.method
            );
        }
        let length = usize::try_from(u32::from_be_bytes([
            header[1], header[2], header[3], header[4],
        ]))?;
        if length > MAX_STREAM_FRAME_BYTES {
            bail!(
                "{} response frame of {length} bytes exceeds the {MAX_STREAM_FRAME_BYTES} byte limit",
                self.method
            );
        }
        let mut payload = vec![0u8; length];
        self.body.read_exact(&mut payload).await?;
        if flags & CONNECT_END_STREAM_FLAG != 0 {
            self.handle_end_stream(&payload)?;
            return Ok(None);
        }
        serde_json::from_slice(&payload)
            .map(Some)
            .with_context(|| format!("failed to parse {} response frame", self.method))
    }

    fn handle_end_stream(&self, payload: &[u8]) -> Result<()> {
        if payload.is_empty() {
            return Ok(());
        }
        let trailer: serde_json::Value = serde_json::from_slice(payload)
            .with_context(|| format!("failed to parse {} end-of-stream frame", self.method))?;
        let error = trailer.get("error");
        let text = |name: &str| {
            error
                .and_then(|error| error.get(name))
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string()
        };
        let code = text("code");
        if code.is_empty() {
            return Ok(());
        }
        Err(RpcError {
            method: self.method.clone(),
            status: String::new(),
            code,
            message: text("message"),
        }
        .into())
    }
}

/// Reads frames until the first usable suggestion or the end of the stream, returning the last
/// response payload seen so that stop and filter reasons still surface.
async fn read_suggestion(
    stream: &mut ServerStream,
    trigger: SupercompleteTriggerCondition,
) -> Result<HandleStreamingTabV2Response> {
    let mut last_response = HandleStreamingTabV2Response::default();
    while let Some(frame) = stream.next_frame::<StreamTabSuggestionsResponse>().await? {
        log::debug!(
            "Devin completion frame ({trigger:?}): {} isFinal={} clientRequestId={:?}",
            frame_summary(&frame),
            frame.is_final,
            frame.client_request_id
        );
        if let Some(candidate) = &frame.tab_jump_candidate
            && frame
                .response
                .as_ref()
                .is_none_or(|response| response.tab_jump.is_none())
        {
            // Devin's own client converts the candidate into a jump locally, which Zed doesn't
            // do yet, so the frame is ignored rather than misread as an edit.
            log::debug!(
                "Devin sent a tab jump candidate for {:?} (finalized: {}), which Zed does not apply yet",
                candidate.path,
                candidate.finalized
            );
        }
        let is_final = frame.is_final;
        if let Some(response) = frame.response {
            let is_suggestion = response.diff.is_some() || response.tab_jump.is_some();
            last_response = response;
            if is_suggestion {
                // Later frames are either refinements or prefetches for the next request; the
                // first usable one is what Devin's own client renders.
                return Ok(last_response);
            }
        }
        if is_final {
            break;
        }
    }
    Ok(last_response)
}

/// Names the fields a frame populated, for the per-frame debug log.
fn frame_summary(frame: &StreamTabSuggestionsResponse) -> String {
    let mut fields = Vec::new();
    if let Some(response) = &frame.response {
        if response.diff.is_some() {
            fields.push("diff");
        }
        if response.tab_jump.is_some() {
            fields.push("tabJump");
        }
        if response.noop.is_some() {
            fields.push("noop");
        }
        if response.stop_reason.is_some() {
            fields.push("stopReason");
        }
        if response.filter_reason.is_some() {
            fields.push("filterReason");
        }
        if fields.is_empty() {
            fields.push("response");
        }
    }
    if frame.trigger_state.is_some() {
        fields.push("triggerState");
    }
    if frame.tab_jump_candidate.is_some() {
        fields.push("tabJumpCandidate");
    }
    format!("[{}]", fields.join(", "))
}

async fn log_output(output: impl futures::AsyncRead + Unpin, secrets: Arc<[String]>) {
    // Keep draining until EOF regardless of content: if this reader stopped, the server would
    // block once the pipe buffer filled up.
    let mut reader = futures::io::BufReader::new(output);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) => return,
            Ok(_) => {
                let mut text = String::from_utf8_lossy(&line).trim_end().to_string();
                for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
                    text = text.replace(secret.as_str(), "[redacted]");
                }
                log::debug!("devin language server: {text}");
            }
            Err(error) => {
                log::warn!("stopped reading Devin language server output: {error}");
                return;
            }
        }
    }
}

/// The language server calls back into the editor's extension host. This implements the subset
/// of that service needed for completions: port publication and ephemeral editor storage.
struct CallbackServer {
    port: u16,
    stopped: Arc<AtomicBool>,
}

#[derive(Default)]
struct NativeValues {
    binary: HashMap<(Vec<u8>, String), Vec<u8>>,
    json: HashMap<(String, String), serde_json::Value>,
}

struct CallbackState {
    csrf_token: String,
    ports: Mutex<Option<oneshot::Sender<(u16, u16)>>>,
    native_values: Mutex<NativeValues>,
}

impl CallbackServer {
    fn start(csrf_token: String) -> Result<(Self, oneshot::Receiver<(u16, u16)>)> {
        let server = tiny_http::Server::http("127.0.0.1:0")
            .map_err(|error| anyhow!("failed to start Devin callback server: {error}"))?;
        let port = server
            .server_addr()
            .to_ip()
            .context("callback server is not bound to an IP address")?
            .port();
        let (ports_sender, ports_receiver) = oneshot::channel();
        let state = CallbackState {
            csrf_token,
            ports: Mutex::new(Some(ports_sender)),
            native_values: Mutex::default(),
        };
        let stopped = Arc::new(AtomicBool::new(false));
        std::thread::Builder::new()
            .name("devin-callback-server".into())
            .spawn({
                let stopped = stopped.clone();
                move || {
                    while !stopped.load(Ordering::SeqCst) {
                        match server.recv_timeout(Duration::from_millis(250)) {
                            Ok(Some(request)) => handle_callback(&state, request),
                            Ok(None) => {}
                            Err(error) => {
                                log::error!("Devin callback server failed: {error}");
                                return;
                            }
                        }
                    }
                }
            })?;
        Ok((Self { port, stopped }, ports_receiver))
    }
}

impl Drop for CallbackServer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

fn handle_callback(state: &CallbackState, mut request: tiny_http::Request) {
    let header = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|header| header.field.equiv(name))
            .map(|header| header.value.as_str().to_string())
            .unwrap_or_default()
    };
    let csrf_token = header("x-codeium-csrf-token");
    let binary = header("Content-Type").split(';').next() == Some("application/proto");
    let method = request
        .url()
        .strip_prefix(EXTENSION_SERVER_SERVICE)
        .map(ToString::to_string);

    // The body is read before any rejection so that Windows delivers the error response instead
    // of resetting a socket that still has unread data.
    let mut body = Vec::new();
    let read_result = request
        .as_reader()
        .take(MAX_CALLBACK_BODY)
        .read_to_end(&mut body);

    let (status, response_body) = if *request.method() != tiny_http::Method::Post {
        (405, connect_error("unimplemented", "unsupported method"))
    } else if read_result.is_err() {
        (
            400,
            connect_error("invalid_argument", "failed to read body"),
        )
    } else if !constant_time_eq(csrf_token.as_bytes(), state.csrf_token.as_bytes()) {
        (
            403,
            connect_error("permission_denied", "invalid CSRF token"),
        )
    } else if let Some(method) = method {
        match handle_callback_method(state, &method, &body, binary) {
            Ok(Some(response_body)) => (200, response_body),
            Ok(None) => {
                log::debug!("Devin callback {method} is not supported");
                (501, connect_error("unimplemented", "unsupported by Zed"))
            }
            Err(error) => {
                log::warn!("Devin callback {method} failed: {error:#}");
                (400, connect_error("invalid_argument", "invalid callback"))
            }
        }
    } else {
        (404, connect_error("not_found", "unknown service"))
    };

    let content_type = if status == 200 && binary {
        "application/proto"
    } else {
        "application/json"
    };
    let mut response = tiny_http::Response::from_data(response_body).with_status_code(status);
    if let Ok(header) = tiny_http::Header::from_bytes("Content-Type", content_type) {
        response = response.with_header(header);
    }
    if let Err(error) = request.respond(response) {
        log::warn!("failed to respond to Devin callback: {error}");
    }
}

fn handle_callback_method(
    state: &CallbackState,
    method: &str,
    body: &[u8],
    binary: bool,
) -> Result<Option<Vec<u8>>> {
    let empty_response = if binary { Vec::new() } else { b"{}".to_vec() };
    match method {
        "LanguageServerStarted" => {
            let ports = parse_language_server_started(body, binary)?;
            if let Some(sender) = state.ports.lock().take() {
                sender.send(ports).ok();
            }
            Ok(Some(empty_response))
        }
        "GetNativeValues" | "SetNativeValue" | "ClearNativeValue" => {
            let mut native_values = state.native_values.lock();
            if binary {
                handle_binary_native_values(&mut native_values.binary, method, body).map(Some)
            } else {
                handle_json_native_values(&mut native_values.json, method, body).map(Some)
            }
        }
        "LogEvent"
        | "HandleAsyncPostMessage"
        | "NotifyMcpStateChanged"
        | "UpdateCascadeTrajectorySummaries" => Ok(Some(empty_response)),
        _ => Ok(None),
    }
}

fn handle_binary_native_values(
    store: &mut HashMap<(Vec<u8>, String), Vec<u8>>,
    method: &str,
    body: &[u8],
) -> Result<Vec<u8>> {
    let fields = decode_protobuf_fields(body)?;
    let bytes_fields = |number: u64| {
        fields
            .iter()
            .filter_map(move |(field_number, value)| match value {
                ProtobufValue::Bytes(bytes) if *field_number == number => Some(bytes.clone()),
                _ => None,
            })
    };
    let target = bytes_fields(1).next().unwrap_or_default();
    let keys = bytes_fields(2)
        .map(String::from_utf8)
        .collect::<Result<Vec<_>, _>>()?;
    let value = bytes_fields(3).next();
    let expected = bytes_fields(4).next();

    match method {
        "GetNativeValues" => {
            let found = keys
                .into_iter()
                .filter_map(|key| {
                    let value = store.get(&(target.clone(), key.clone()))?.clone();
                    Some((key, value))
                })
                .collect::<Vec<_>>();
            Ok(encode_native_values_response(&found))
        }
        "SetNativeValue" => {
            let key = keys.into_iter().next().context("missing key")?;
            let current = store.get(&(target.clone(), key.clone())).cloned();
            let conflict = expected.is_some() && current != expected;
            if !conflict && let Some(value) = value {
                store.insert((target, key), value);
            }
            Ok(encode_set_native_value_response(
                conflict,
                current.as_deref(),
            ))
        }
        _ => {
            let key = keys.into_iter().next().context("missing key")?;
            store.remove(&(target, key));
            Ok(Vec::new())
        }
    }
}

fn handle_json_native_values(
    store: &mut HashMap<(String, String), serde_json::Value>,
    method: &str,
    body: &[u8],
) -> Result<Vec<u8>> {
    let request: serde_json::Value = serde_json::from_slice(body)?;
    let target = request
        .get("target")
        .cloned()
        .unwrap_or_default()
        .to_string();
    match method {
        "GetNativeValues" => {
            let values = request
                .get("keys")
                .and_then(|keys| keys.as_array())
                .into_iter()
                .flatten()
                .filter_map(|key| key.as_str())
                .filter_map(|key| {
                    let value = store.get(&(target.clone(), key.to_string()))?;
                    Some((key.to_string(), json!({ "value": value })))
                })
                .collect::<serde_json::Map<_, _>>();
            Ok(serde_json::to_vec(&json!({ "values": values }))?)
        }
        "SetNativeValue" => {
            let key = request
                .get("key")
                .and_then(|key| key.as_str())
                .context("missing key")?
                .to_string();
            let expected = request
                .get("expectedPreviousValue")
                .or_else(|| request.get("expected_previous_value"));
            let current = store.get(&(target.clone(), key.clone()));
            let conflict = expected.is_some() && current != expected;
            if !conflict && let Some(value) = request.get("value") {
                store.insert((target, key), value.clone());
            }
            Ok(serde_json::to_vec(
                &json!({ "concurrencyConflict": conflict }),
            )?)
        }
        _ => {
            if let Some(key) = request.get("key").and_then(|key| key.as_str()) {
                store.remove(&(target, key.to_string()));
            }
            Ok(b"{}".to_vec())
        }
    }
}

fn connect_error(code: &str, message: &str) -> Vec<u8> {
    json!({ "code": code, "message": message })
        .to_string()
        .into_bytes()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        pin::Pin,
        task::{Context as TaskContext, Poll},
    };

    use futures::AsyncRead;
    use pretty_assertions::assert_eq;

    use super::*;

    /// Yields at most `chunk_size` bytes per read, so that frames can be split across reads the
    /// way a real HTTP body can deliver them.
    struct ChunkedReader {
        data: Vec<u8>,
        position: usize,
        chunk_size: usize,
    }

    impl AsyncRead for ChunkedReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            buffer: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let remaining = self.data.len() - self.position;
            let len = remaining.min(buffer.len()).min(self.chunk_size);
            buffer[..len].copy_from_slice(&self.data[self.position..self.position + len]);
            self.position += len;
            Poll::Ready(Ok(len))
        }
    }

    fn envelope(flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![flags];
        frame.extend_from_slice(
            &u32::try_from(payload.len())
                .expect("test frame fits in u32")
                .to_be_bytes(),
        );
        frame.extend_from_slice(payload);
        frame
    }

    fn stream_with_body(body: Vec<u8>, chunk_size: usize) -> ServerStream {
        ServerStream::new(
            "StreamTabSuggestions",
            AsyncBody::from_reader(ChunkedReader {
                data: body,
                position: 0,
                chunk_size,
            }),
        )
    }

    fn message_frame(response: serde_json::Value) -> Vec<u8> {
        envelope(0, response.to_string().as_bytes())
    }

    fn end_stream_frame(trailer: serde_json::Value) -> Vec<u8> {
        envelope(CONNECT_END_STREAM_FLAG, trailer.to_string().as_bytes())
    }

    #[test]
    fn test_connect_envelope_round_trip() {
        let encoded = encode_connect_envelope(br#"{"request":{}}"#).unwrap();
        assert_eq!(&encoded[..5], &[0, 0, 0, 0, 14]);

        let mut stream = stream_with_body(encoded, 3);
        let frame: serde_json::Value = smol::block_on(stream.next_frame())
            .unwrap()
            .expect("expected one frame");
        assert_eq!(frame, serde_json::json!({ "request": {} }));
        assert!(
            smol::block_on(stream.next_frame::<serde_json::Value>())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_stream_decodes_frames_split_across_reads() {
        let mut body = Vec::new();
        body.extend(message_frame(serde_json::json!({
            "triggerState": { "filePathUri": "file:///project/a.rs", "contents": "one\n" }
        })));
        body.extend(message_frame(serde_json::json!({
            "response": {
                "completionId": "completion",
                "diff": { "path": "file:///project/a.rs" }
            },
            "isFinal": true,
            "clientRequestId": "42"
        })));
        body.extend(end_stream_frame(serde_json::json!({ "metadata": {} })));

        // A chunk size that divides no frame boundary, so every header and payload spans reads.
        let mut stream = stream_with_body(body, 3);

        let first: StreamTabSuggestionsResponse =
            smol::block_on(stream.next_frame()).unwrap().unwrap();
        assert!(first.response.is_none());
        assert_eq!(
            first.trigger_state.unwrap().file_path_uri,
            "file:///project/a.rs"
        );

        let second: StreamTabSuggestionsResponse =
            smol::block_on(stream.next_frame()).unwrap().unwrap();
        assert_eq!(second.client_request_id, Some(42));
        assert!(second.is_final);
        let response = second.response.expect("expected a response payload");
        assert_eq!(response.completion_id, "completion");
        assert!(response.diff.is_some());

        assert!(
            smol::block_on(stream.next_frame::<StreamTabSuggestionsResponse>())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_stream_end_of_stream_error_is_rpc_error() {
        let body = end_stream_frame(serde_json::json!({
            "error": { "code": "canceled", "message": "request superseded" }
        }));
        let mut stream = stream_with_body(body, 7);

        let error = smol::block_on(stream.next_frame::<StreamTabSuggestionsResponse>())
            .expect_err("expected the end-of-stream error");
        let rpc_error = error
            .downcast_ref::<RpcError>()
            .expect("expected an RPC error");
        assert_eq!(rpc_error.code, "canceled");
        assert_eq!(rpc_error.message, "request superseded");
    }

    #[test]
    fn test_stream_reads_first_usable_suggestion() {
        let mut body = Vec::new();
        body.extend(message_frame(serde_json::json!({
            "triggerState": { "contents": "one\n" }
        })));
        body.extend(message_frame(serde_json::json!({
            "response": { "completionId": "empty", "stopReason": "STOP_REASON_PARTIAL" }
        })));
        body.extend(message_frame(serde_json::json!({
            "response": {
                "completionId": "first",
                "diff": { "path": "file:///project/a.rs" }
            }
        })));
        // Reading past the first usable frame would fail on this truncated payload.
        body.extend(envelope(0, b"{"));

        let mut stream = stream_with_body(body, 5);
        let response = smol::block_on(read_suggestion(
            &mut stream,
            SupercompleteTriggerCondition::Typing,
        ))
        .unwrap();
        assert_eq!(response.completion_id, "first");
        assert!(response.diff.is_some());
    }

    #[test]
    fn test_stream_returns_last_response_when_no_suggestion_arrives() {
        let mut body = Vec::new();
        body.extend(message_frame(serde_json::json!({
            "triggerState": { "contents": "one\n" }
        })));
        body.extend(message_frame(serde_json::json!({
            "response": { "completionId": "empty", "stopReason": "STOP_REASON_NO_EDIT" },
            "isFinal": true
        })));
        body.extend(end_stream_frame(serde_json::json!({})));

        let mut stream = stream_with_body(body, 4);
        let response = smol::block_on(read_suggestion(
            &mut stream,
            SupercompleteTriggerCondition::Typing,
        ))
        .unwrap();
        assert_eq!(response.completion_id, "empty");
        assert!(response.diff.is_none());
        assert_eq!(
            response.stop_reason,
            Some(serde_json::json!("STOP_REASON_NO_EDIT"))
        );
    }

    #[test]
    fn test_missing_route_error_names_route_and_version() {
        let error = rpc_error(
            "StreamTabSuggestions",
            "3.12.59",
            StatusCode::NOT_FOUND,
            br#"{"code":"unimplemented","message":"not found"}"#,
        );
        assert_eq!(error.code, "unimplemented");
        let message = error.to_string();
        assert!(message.contains("StreamTabSuggestions"));
        assert!(message.contains("Devin 3.12.59"));
        assert!(message.contains("needs updating"));
        assert!(message.contains("not found"));
    }
}
