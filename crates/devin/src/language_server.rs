use std::{
    collections::HashMap,
    io::Read as _,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{
    AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, FutureExt as _, channel::oneshot,
};
use gpui::{BackgroundExecutor, Task};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, Method, Request};
use parking_lot::Mutex;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use util::ResultExt as _;
use uuid::Uuid;

use crate::lsp_connection::{LspConnection, WorkspaceFolder};
use crate::protocol::{
    Document, EXTENSION_SERVER_SERVICE, EditorOptions, GetUserStatusResponse,
    HandleStreamingTabV2Request, HandleStreamingTabV2Response, IdeAction, LANGUAGE_SERVER_SERVICE,
    Metadata, MetadataRequest, ProtobufValue, RefreshContextForIdeActionRequest,
    SupercompleteTriggerCondition, decode_protobuf_fields, encode_native_values_response,
    encode_set_native_value_response, parse_language_server_started,
};

pub const API_SERVER_URL: &str = "https://server.codeium.com";
const INFERENCE_API_SERVER_URL: &str = "https://inference.codeium.com";

/// Model routing in the language server's rollout configuration matches on this exact IDE name,
/// so requests identify as the Windsurf extension. Other names fall back to an older model whose
/// requests fail upstream.
const COMPATIBLE_IDE_NAME: &str = "windsurf";

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const RPC_TIMEOUT: Duration = Duration::from_secs(15);
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
    _callback_server: CallbackServer,
    _tasks: Vec<Task<()>>,
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
        });

        rpc.call::<_, serde_json::Value>(
            "InitializeCascadePanelState",
            &json!({ "metadata": metadata, "is_workspace_trusted": true }),
            RPC_TIMEOUT,
        )
        .await
        .context("InitializeCascadePanelState")
        .log_err();

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
            rpc.call::<_, serde_json::Value>(
                "UpdatePanelStateWithUserStatus",
                &json!({ "metadata": metadata, "user_status": user_status }),
                RPC_TIMEOUT,
            )
            .await
            .context("UpdatePanelStateWithUserStatus")
            .log_err();
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
        let request = HandleStreamingTabV2Request {
            metadata: self.next_metadata(),
            document,
            editor_options,
            supercomplete_trigger_condition: trigger,
            other_documents,
            disable_supercomplete: false,
            disable_tab_jump: false,
            supercomplete_aggression: "SUPERCOMPLETE_AGGRESSION_MEDIUM",
        };
        self.rpc
            .call("HandleStreamingTabV2", &request, RPC_TIMEOUT)
            .await
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
                        recently_opened_uris: std::iter::once(document)
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

struct Rpc {
    http_client: Arc<dyn HttpClient>,
    base_url: String,
    csrf_token: String,
}

impl Rpc {
    async fn call<Req: Serialize, Res: DeserializeOwned>(
        &self,
        method: &str,
        request: &Req,
        timeout: Duration,
    ) -> Result<Res> {
        let body = serde_json::to_vec(request)?;
        let http_request = Request::builder()
            .method(Method::POST)
            .uri(format!(
                "{}{LANGUAGE_SERVER_SERVICE}{method}",
                self.base_url
            ))
            .header("Content-Type", "application/json")
            .header("Connect-Protocol-Version", "1")
            .header("x-codeium-csrf-token", &self.csrf_token)
            .timeout(timeout)
            .body(AsyncBody::from(body))?;
        let mut response = self
            .http_client
            .send(http_request)
            .await
            .with_context(|| format!("{method} request failed"))?;
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;
        if !response.status().is_success() {
            let detail = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .map(|error| {
                    let code = error
                        .get("code")
                        .and_then(|code| code.as_str())
                        .unwrap_or("");
                    let message = error
                        .get("message")
                        .and_then(|message| message.as_str())
                        .unwrap_or("");
                    format!("{code}: {message}")
                })
                .unwrap_or_default();
            bail!("{method} returned {} {detail}", response.status());
        }
        if body.is_empty() {
            body = b"{}".to_vec();
        }
        serde_json::from_slice(&body).with_context(|| format!("failed to parse {method} response"))
    }
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
