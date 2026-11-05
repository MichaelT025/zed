use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{
    AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, FutureExt as _, channel::oneshot,
    io::BufReader, lock::Mutex as AsyncMutex,
};
use gpui::{BackgroundExecutor, Task};
use parking_lot::Mutex;
use serde_json::{Value, json};
use smol::net::TcpStream;
use util::ResultExt as _;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_MESSAGE_LENGTH: usize = 16 * 1024 * 1024;

type PendingRequests = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceFolder {
    pub uri: String,
    pub name: String,
}

impl WorkspaceFolder {
    pub fn from_uri(uri: &str) -> Self {
        let name = uri
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string();
        Self {
            uri: uri.to_string(),
            name,
        }
    }

    fn to_json(&self) -> Value {
        json!({ "uri": self.uri, "name": self.name })
    }
}

/// A minimal LSP client for the language server's TCP LSP port. The server learns about
/// workspaces and open documents through this channel, which completions depend on in addition
/// to the editor-context refresh.
pub struct LspConnection {
    writer: Arc<AsyncMutex<TcpStream>>,
    pending: PendingRequests,
    next_id: AtomicU64,
    workspace_folders: Arc<Mutex<Vec<WorkspaceFolder>>>,
    documents: AsyncMutex<HashMap<String, OpenDocument>>,
    executor: BackgroundExecutor,
    _reader: Task<()>,
}

struct OpenDocument {
    version: i32,
    text: String,
}

impl LspConnection {
    pub async fn connect(
        port: u16,
        workspace: WorkspaceFolder,
        executor: BackgroundExecutor,
    ) -> Result<Self> {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .context("failed to connect to Devin's LSP port")?;
        let writer = Arc::new(AsyncMutex::new(stream.clone()));
        let pending = PendingRequests::default();
        let workspace_folders = Arc::new(Mutex::new(vec![workspace.clone()]));
        let reader = executor.spawn(read_messages(
            stream,
            writer.clone(),
            pending.clone(),
            workspace_folders.clone(),
        ));
        let connection = Self {
            writer,
            pending,
            next_id: AtomicU64::new(1),
            workspace_folders,
            documents: AsyncMutex::new(HashMap::default()),
            executor,
            _reader: reader,
        };

        let root_path = url::Url::parse(&workspace.uri)
            .ok()
            .and_then(|uri| uri.to_file_path().ok())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        connection
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "clientInfo": { "name": "zed", "version": "0.1.0" },
                    "rootUri": workspace.uri,
                    "rootPath": root_path,
                    "capabilities": {
                        "workspace": { "workspaceFolders": true },
                        "textDocument": { "synchronization": { "dynamicRegistration": false } }
                    },
                    "workspaceFolders": [workspace.to_json()],
                    "trace": "off"
                }),
            )
            .await
            .context("LSP initialize failed")?;
        connection.notify("initialized", json!({})).await?;
        Ok(connection)
    }

    pub async fn add_workspace_folder(&self, workspace: WorkspaceFolder) -> Result<()> {
        {
            let mut folders = self.workspace_folders.lock();
            if folders.contains(&workspace) {
                return Ok(());
            }
            folders.push(workspace.clone());
        }
        self.notify(
            "workspace/didChangeWorkspaceFolders",
            json!({ "event": { "added": [workspace.to_json()], "removed": [] } }),
        )
        .await
    }

    /// Sends the document's full text, opening it the first time it's seen.
    pub async fn sync_document(&self, uri: &str, language_id: &str, text: &str) -> Result<()> {
        let mut documents = self.documents.lock().await;
        match documents.get_mut(uri) {
            Some(document) if document.text == text => Ok(()),
            Some(document) => {
                document.version += 1;
                document.text = text.to_string();
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": document.version },
                        "contentChanges": [{ "text": text }]
                    }),
                )
                .await
            }
            None => {
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id,
                            "version": 1,
                            "text": text
                        }
                    }),
                )
                .await?;
                documents.insert(
                    uri.to_string(),
                    OpenDocument {
                        version: 1,
                        text: text.to_string(),
                    },
                );
                Ok(())
            }
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().insert(id, sender);
        let result = async {
            write_message(
                &self.writer,
                &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
            )
            .await?;
            let mut timeout = self.executor.timer(REQUEST_TIMEOUT).fuse();
            let mut receiver = receiver.fuse();
            futures::select_biased! {
                response = receiver => response.context("LSP connection closed")?,
                _ = timeout => bail!("LSP {method} timed out"),
            }
        }
        .await;
        self.pending.lock().remove(&id);
        result
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        write_message(
            &self.writer,
            &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
        )
        .await
        .with_context(|| format!("failed to send LSP {method}"))
    }
}

async fn write_message(writer: &AsyncMutex<TcpStream>, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    let mut writer = writer.lock().await;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_messages(
    stream: TcpStream,
    writer: Arc<AsyncMutex<TcpStream>>,
    pending: PendingRequests,
    workspace_folders: Arc<Mutex<Vec<WorkspaceFolder>>>,
) {
    let mut reader = BufReader::new(stream);
    loop {
        let message = match read_message(&mut reader).await {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) => {
                log::warn!("Devin LSP connection failed: {error:#}");
                break;
            }
        };

        let id = message.get("id").cloned();
        if let Some(method) = message.get("method").and_then(|method| method.as_str()) {
            // Requests from the server need a response; notifications are ignored.
            let Some(id) = id else {
                continue;
            };
            let params = message.get("params").cloned().unwrap_or_default();
            let response = match method {
                "workspace/configuration" => {
                    let item_count = params
                        .get("items")
                        .and_then(|items| items.as_array())
                        .map_or(0, Vec::len);
                    json!({ "jsonrpc": "2.0", "id": id, "result": vec![json!({}); item_count] })
                }
                "workspace/workspaceFolders" => {
                    let folders = workspace_folders
                        .lock()
                        .iter()
                        .map(WorkspaceFolder::to_json)
                        .collect::<Vec<_>>();
                    json!({ "jsonrpc": "2.0", "id": id, "result": folders })
                }
                "workspace/applyEdit" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "applied": false, "failureReason": "Zed does not apply edits from Devin" }
                }),
                "client/registerCapability"
                | "client/unregisterCapability"
                | "window/workDoneProgress/create" => {
                    json!({ "jsonrpc": "2.0", "id": id, "result": null })
                }
                _ => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "unsupported by Zed" }
                }),
            };
            write_message(&writer, &response).await.log_err();
        } else if let Some(id) = id.and_then(|id| id.as_u64()) {
            if let Some(sender) = pending.lock().remove(&id) {
                let result = match message.get("error") {
                    Some(error) => Err(anyhow!("LSP error: {error}")),
                    None => Ok(message.get("result").cloned().unwrap_or_default()),
                };
                sender.send(result).ok();
            }
        }
    }

    for (_, sender) in pending.lock().drain() {
        sender.send(Err(anyhow!("LSP connection closed"))).ok();
    }
}

async fn read_message(reader: &mut BufReader<TcpStream>) -> Result<Option<Value>> {
    let mut content_length = None;
    let mut header = String::new();
    loop {
        header.clear();
        if reader.read_line(&mut header).await? == 0 {
            return Ok(None);
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = Some(value.trim().parse::<usize>()?);
        }
    }
    let content_length = content_length.context("LSP message has no Content-Length")?;
    if content_length > MAX_MESSAGE_LENGTH {
        bail!("LSP message is too large ({content_length} bytes)");
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_folder_name() {
        assert_eq!(
            WorkspaceFolder::from_uri("file:///C:/Users/me/Projects/Dispatch/").name,
            "Dispatch"
        );
        assert_eq!(WorkspaceFolder::from_uri("file:///home/me/zed").name, "zed");
    }
}
