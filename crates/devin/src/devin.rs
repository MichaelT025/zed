mod devin_edit_prediction_delegate;
mod language_server;
mod lsp_connection;
mod protocol;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use futures::{AsyncReadExt as _, FutureExt as _, future::Shared};
use gpui::{App, AppContext as _, Context, Entity, EntityId, Global, SharedString, Task};
use http_client::{AsyncBody, HttpClient, Method, Request};
use util::ResultExt as _;

use devin_edit_prediction_delegate::CurrentPrediction;
pub use devin_edit_prediction_delegate::DevinEditPredictionDelegate;
use language_server::{API_SERVER_URL, DevinInstallation, DevinLanguageServer};
use lsp_connection::WorkspaceFolder;
use protocol::{ExchangeDevinCodeResponse, SEAT_MANAGEMENT_SERVICE};

const SIGN_IN_URL: &str = "https://app.devin.ai/auth/windsurf/show-auth-code";
const CREDENTIALS_USERNAME: &str = "devin";
const SESSION_TOKEN_PREFIX: &str = "devin-session-token$";
const RESTART_BACKOFF: Duration = Duration::from_secs(30);
const MAX_RECENT_BUFFERS: usize = 64;

type ServerTask = Shared<Task<Result<Arc<DevinLanguageServer>, SharedString>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevinStatus {
    LoadingCredentials,
    SignedOut,
    Starting,
    Ready,
    Error(SharedString),
}

pub struct Devin {
    http_client: Arc<dyn HttpClient>,
    api_key: Option<Arc<str>>,
    credentials_loaded: bool,
    server: Option<(ServerTask, Instant)>,
    sign_in_error: Option<SharedString>,
    sign_in_url: String,
    /// Buffers that requested predictions, most recent first, used to prioritize the open
    /// documents sent as context.
    recent_buffers: Vec<EntityId>,
    /// A prediction for a buffer other than the one that requested it, waiting for that
    /// buffer's editor to show it.
    prediction_for_other_buffer: Option<CurrentPrediction>,
    _load_credentials: Task<()>,
}

struct GlobalDevin(Entity<Devin>);

impl Global for GlobalDevin {}

impl Devin {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalDevin>() {
            return global.0.clone();
        }
        let devin = cx.new(Self::new);
        cx.set_global(GlobalDevin(devin.clone()));
        devin
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalDevin>()
            .map(|global| global.0.clone())
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let credentials_provider = zed_credentials_provider::global(cx);
        let load_credentials = cx.spawn(async move |this, cx| {
            let api_key = credentials_provider
                .read_credentials(API_SERVER_URL, cx)
                .await
                .context("failed to read Devin credentials")
                .log_err()
                .flatten()
                .and_then(|(_, token)| String::from_utf8(token).log_err())
                .map(Arc::from);
            this.update(cx, |this, cx| {
                this.api_key = api_key;
                this.credentials_loaded = true;
                cx.notify();
            })
            .ok();
        });
        Self {
            http_client: cx.http_client(),
            api_key: None,
            credentials_loaded: false,
            server: None,
            sign_in_error: None,
            // Generated once, because UI elements derive their identity from the URL and
            // wouldn't receive clicks if it changed on every render.
            sign_in_url: format!(
                "{SIGN_IN_URL}?state={}&prompt=select_account&from=redirect",
                uuid::Uuid::new_v4()
            ),
            recent_buffers: Vec::new(),
            prediction_for_other_buffer: None,
            _load_credentials: load_credentials,
        }
    }

    pub fn is_signed_in(&self) -> bool {
        self.api_key.is_some()
    }

    pub fn sign_in_error(&self) -> Option<SharedString> {
        self.sign_in_error.clone()
    }

    pub fn status(&self) -> DevinStatus {
        if !self.credentials_loaded {
            return DevinStatus::LoadingCredentials;
        }
        if self.api_key.is_none() {
            return DevinStatus::SignedOut;
        }
        match self.server.as_ref().map(|(server, _)| server.peek()) {
            None | Some(None) => DevinStatus::Starting,
            Some(Some(Ok(server))) if server.is_healthy() => DevinStatus::Ready,
            Some(Some(Ok(_))) => DevinStatus::Error("Devin's language server stopped".into()),
            Some(Some(Err(error))) => DevinStatus::Error(error.clone()),
        }
    }

    /// Returns the running language server, starting it if necessary. A server that failed to
    /// start is only retried after a backoff, and returns `None` until then, so that typing
    /// neither relaunches it nor reports the same failure on every keystroke.
    pub(crate) fn server(
        &mut self,
        workspace_uri: &str,
        cx: &mut Context<Self>,
    ) -> Option<ServerTask> {
        let api_key = self.api_key.clone()?;
        if let Some((server, started_at)) = &self.server {
            match server.peek() {
                None => return Some(server.clone()),
                Some(Ok(running)) if running.is_healthy() => return Some(server.clone()),
                Some(Err(_)) if started_at.elapsed() < RESTART_BACKOFF => return None,
                Some(_) => {}
            }
        }

        let http_client = self.http_client.clone();
        let executor = cx.background_executor().clone();
        let initial_workspace = WorkspaceFolder::from_uri(workspace_uri);
        let start = cx.background_spawn(async move {
            let installation = DevinInstallation::locate()?;
            DevinLanguageServer::start(
                installation,
                api_key,
                http_client,
                executor,
                initial_workspace,
            )
            .await
        });
        let server = cx
            .spawn(async move |this, cx| {
                let result = start.await.map(Arc::new).map_err(|error| {
                    log::error!("failed to start Devin: {error:#}");
                    SharedString::from(format!("{error:#}"))
                });
                this.update(cx, |_, cx| cx.notify()).ok();
                result
            })
            .shared();
        self.server = Some((server.clone(), Instant::now()));
        cx.notify();
        Some(server)
    }

    pub(crate) fn record_buffer_access(&mut self, buffer_id: EntityId) {
        self.recent_buffers.retain(|recent| *recent != buffer_id);
        self.recent_buffers.insert(0, buffer_id);
        self.recent_buffers.truncate(MAX_RECENT_BUFFERS);
    }

    pub(crate) fn recent_buffers(&self) -> &[EntityId] {
        &self.recent_buffers
    }

    pub(crate) fn set_prediction_for_other_buffer(&mut self, prediction: CurrentPrediction) {
        self.prediction_for_other_buffer = Some(prediction);
    }

    pub(crate) fn take_prediction_for_buffer(
        &mut self,
        buffer_id: EntityId,
    ) -> Option<CurrentPrediction> {
        if self
            .prediction_for_other_buffer
            .as_ref()
            .is_some_and(|prediction| prediction.buffer_id() == buffer_id)
        {
            self.prediction_for_other_buffer.take()
        } else {
            None
        }
    }

    pub fn sign_in_url(&self) -> String {
        self.sign_in_url.clone()
    }

    /// Exchanges the one-time code shown after signing in at [`Self::sign_in_url`] for a session
    /// token and stores it in the system keychain.
    pub fn sign_in_with_code(&mut self, code: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        let http_client = self.http_client.clone();
        let credentials_provider = zed_credentials_provider::global(cx);
        self.sign_in_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = async {
                let code = code.trim();
                if code.is_empty() {
                    bail!("Enter the code shown after signing in to Devin.");
                }
                let session_token = exchange_devin_code(http_client.as_ref(), code).await?;
                credentials_provider
                    .write_credentials(
                        API_SERVER_URL,
                        CREDENTIALS_USERNAME,
                        session_token.as_bytes(),
                        cx,
                    )
                    .await
                    .context("failed to store Devin credentials")?;
                anyhow::Ok(session_token)
            }
            .await;
            this.update(cx, |this, cx| {
                match &result {
                    Ok(session_token) => {
                        this.api_key = Some(session_token.as_str().into());
                        this.credentials_loaded = true;
                        this.server = None;
                    }
                    Err(error) => this.sign_in_error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })?;
            result.map(|_| ())
        })
    }

    pub fn sign_out(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.api_key = None;
        self.server = None;
        self.sign_in_error = None;
        cx.notify();
        let credentials_provider = zed_credentials_provider::global(cx);
        cx.spawn(async move |_, cx| {
            credentials_provider
                .delete_credentials(API_SERVER_URL, cx)
                .await
                .context("failed to delete Devin credentials")
        })
    }
}

async fn exchange_devin_code(http_client: &dyn HttpClient, code: &str) -> Result<String> {
    let body = serde_json::to_vec(&serde_json::json!({ "code": code }))?;
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!(
            "{API_SERVER_URL}{SEAT_MANAGEMENT_SERVICE}ExchangeDevinCode"
        ))
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .body(AsyncBody::from(body))?;
    let mut response = http_client
        .send(request)
        .await
        .context("failed to reach Devin's sign-in service")?;
    let status = response.status();
    if !status.is_success() {
        bail!("Devin rejected the sign-in code ({status}). Codes expire after about a minute.");
    }
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    let response: ExchangeDevinCodeResponse =
        serde_json::from_slice(&body).context("failed to parse Devin's sign-in response")?;
    if !response.session_token.starts_with(SESSION_TOKEN_PREFIX) {
        bail!("Devin's sign-in service did not return a session token");
    }
    Ok(response.session_token)
}
