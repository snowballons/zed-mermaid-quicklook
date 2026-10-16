//! Mermaid Quicklook language server.
//!
//! Renders a `.mmd` file to a PNG in a temp directory and asks Zed to open it as an
//! image tab. Zed watches that file, so re-rendering on save updates the open tab in
//! place: a live preview. Source files are never modified.

mod render;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Sender;
use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, Command as LspCommand, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, ExecuteCommandOptions,
    ExecuteCommandParams, MessageType, ServerCapabilities, ShowMessageParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Url, WorkDoneProgressOptions,
};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    env,
    fs::{self, DirBuilder},
    hash::{DefaultHasher, Hash, Hasher},
    io::ErrorKind,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

const CMD_PREVIEW: &str = "mermaid.preview";
const CMD_EXPORT_PNG: &str = "mermaid.exportPng";
const CMD_EXPORT_SVG: &str = "mermaid.exportSvg";

/// Documents we've opened a preview tab for. We cannot observe the preview tab's
/// lifecycle (an image tab is not a text document, so no didOpen/didClose reaches us),
/// so we simply keep re-rendering on save while the *source* document is open. If the
/// user closed the preview, the cost is one wasted render.
#[derive(Default)]
struct State {
    documents: HashMap<String, String>,
    previewing: HashSet<String>,
    /// Per-URI render coordination, shared with the detached render threads.
    renders: Renders,
}

/// Tracks the in-flight render for one preview URI so rapid autosaves coalesce into at
/// most one running render plus one queued follow-up, and so didClose can defer temp
/// cleanup until the render thread is done touching the directory.
#[derive(Default)]
struct RenderSlot {
    /// A render thread is currently running for this URI.
    rendering: bool,
    /// The newest source that arrived while a render was running; rendered next.
    pending: Option<String>,
    /// The source document closed mid-render; the thread cleans up when it finishes.
    closing: bool,
}

type Renders = Arc<Mutex<HashMap<String, RenderSlot>>>;

fn main() -> Result<()> {
    eprintln!("mermaid-quicklook-lsp starting");

    let (connection, io_threads) = Connection::stdio();

    let capabilities = serde_json::to_value(ServerCapabilities {
        // Diagrams are small; full sync keeps the server trivially correct.
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                ..Default::default()
            },
        )),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![
                CMD_PREVIEW.to_string(),
                CMD_EXPORT_PNG.to_string(),
                CMD_EXPORT_SVG.to_string(),
            ],
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        ..Default::default()
    })?;

    connection.initialize(capabilities)?;
    eprintln!("initialized");

    let mut state = State::default();

    for message in &connection.receiver {
        match message {
            Message::Request(request) => {
                if connection.handle_shutdown(&request)? {
                    break;
                }
                handle_request(&connection, &mut state, request);
            }
            Message::Notification(notification) => {
                handle_notification(&connection, &mut state, notification);
            }
            Message::Response(_) => {}
        }
    }

    // The connection owns the sender feeding the writer thread. Joining before dropping
    // it deadlocks: the writer waits on a channel that can never close, so the process
    // hangs on exit instead of terminating.
    drop(connection);
    io_threads.join()?;

    // Sweep this session's per-document preview directories. We only touch our own
    // <hash> subdirs, never the shared base dir, which other instances may still use.
    for uri in &state.previewing {
        if let Ok(dir) = preview_dir(uri) {
            let _ = fs::remove_dir_all(dir);
        }
    }

    eprintln!("mermaid-quicklook-lsp exiting");
    Ok(())
}

fn handle_request(connection: &Connection, state: &mut State, request: Request) {
    let id = request.id.clone();

    let result = match request.method.as_str() {
        "textDocument/codeAction" => {
            cast::<CodeActionParams>(request).map(|params| json!(code_actions(&params)))
        }
        "workspace/executeCommand" => cast::<ExecuteCommandParams>(request).map(|params| {
            if let Err(error) = execute_command(connection, state, &params) {
                show_error(&connection.sender, &error.to_string());
            }
            json!(null)
        }),
        _ => Ok(json!(null)),
    };

    let response = match result {
        Ok(value) => Response::new_ok(id, value),
        Err(error) => {
            eprintln!("bad request: {error}");
            Response::new_ok(id, json!(null))
        }
    };

    let _ = connection.sender.send(Message::Response(response));
}

fn cast<P: serde::de::DeserializeOwned>(request: Request) -> Result<P> {
    serde_json::from_value(request.params).map_err(|error| anyhow!("{error}"))
}

fn handle_notification(connection: &Connection, state: &mut State, notification: Notification) {
    match notification.method.as_str() {
        "textDocument/didOpen" => {
            if let Ok(params) =
                serde_json::from_value::<DidOpenTextDocumentParams>(notification.params)
            {
                let uri = params.text_document.uri.to_string();
                let text = params.text_document.text;
                eprintln!(
                    "didOpen {uri} ({} bytes, {} lines)",
                    text.len(),
                    text.lines().count()
                );
                state.documents.insert(uri, text);
            }
        }
        "textDocument/didChange" => {
            // Full sync: the last change carries the whole document.
            if let Ok(params) = serde_json::from_value::<serde_json::Value>(notification.params) {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or_default();
                if let Some(text) = params["contentChanges"]
                    .as_array()
                    .and_then(|changes| changes.last())
                    .and_then(|change| change["text"].as_str())
                {
                    state.documents.insert(uri.to_string(), text.to_string());
                }
            }
        }
        "textDocument/didSave" => {
            if let Ok(params) =
                serde_json::from_value::<DidSaveTextDocumentParams>(notification.params)
            {
                let uri = params.text_document.uri.to_string();
                if state.previewing.contains(&uri) {
                    if let Some(source) = state.documents.get(&uri).cloned() {
                        // Overwrite the same file the preview tab is showing; Zed repaints it.
                        refresh_preview(&connection.sender, &state.renders, uri, source);
                    }
                }
            }
        }
        "textDocument/didClose" => {
            if let Ok(params) =
                serde_json::from_value::<DidCloseTextDocumentParams>(notification.params)
            {
                let uri = params.text_document.uri.to_string();
                state.documents.remove(&uri);
                state.previewing.remove(&uri);
                close_preview(&state.renders, &uri);
            }
        }
        _ => {}
    }
}

fn code_actions(params: &CodeActionParams) -> Vec<CodeActionOrCommand> {
    let uri = params.text_document.uri.to_string();
    if !is_mermaid_document(&uri) {
        return Vec::new();
    }

    let argument = json!({ "uri": uri });

    [
        ("Open Mermaid Preview", CMD_PREVIEW),
        ("Export Mermaid Diagram as PNG", CMD_EXPORT_PNG),
        ("Export Mermaid Diagram as SVG", CMD_EXPORT_SVG),
    ]
    .into_iter()
    .map(|(title, command)| {
        CodeActionOrCommand::CodeAction(CodeAction {
            title: title.to_string(),
            kind: Some(CodeActionKind::EMPTY),
            command: Some(LspCommand {
                title: title.to_string(),
                command: command.to_string(),
                arguments: Some(vec![argument.clone()]),
            }),
            ..Default::default()
        })
    })
    .collect()
}

fn execute_command(
    connection: &Connection,
    state: &mut State,
    params: &ExecuteCommandParams,
) -> Result<()> {
    let uri = params
        .arguments
        .first()
        .and_then(|argument| argument.get("uri"))
        .and_then(|uri| uri.as_str())
        .ok_or_else(|| anyhow!("command is missing its uri argument"))?
        .to_string();

    let source = state
        .documents
        .get(&uri)
        .cloned()
        .ok_or_else(|| anyhow!("document is not open: {uri}"))?;

    eprintln!(
        "command {} on {uri} ({} bytes, {} lines)",
        params.command,
        source.len(),
        source.lines().count()
    );

    match params.command.as_str() {
        CMD_PREVIEW => {
            let target = preview_path(&uri)?;
            render::render(&source, &target)?;
            open_in_editor(&target)?;
            state.previewing.insert(uri);
            Ok(())
        }
        CMD_EXPORT_PNG => export(&connection.sender, &uri, &source, "png"),
        CMD_EXPORT_SVG => export(&connection.sender, &uri, &source, "svg"),
        other => Err(anyhow!("unknown command: {other}")),
    }
}

/// Export writes next to the source file and takes a second or two. Run it off the main
/// loop so a long render does not stall every other LSP request; report the outcome via
/// a window/showMessage toast, just as an inline render would.
fn export(sender: &Sender<Message>, uri: &str, source: &str, format: &str) -> Result<()> {
    let target = source_path(uri)?.with_extension(format);
    let sender = sender.clone();
    let source = source.to_string();

    std::thread::spawn(move || match render::render(&source, &target) {
        Ok(()) => show_info(&sender, &format!("Exported {}", target.display())),
        Err(error) => show_error(&sender, &format!("Mermaid export failed: {error}")),
    });

    Ok(())
}

/// Re-render an open preview off the main loop. mmdc boots Chromium and takes a second
/// or two; blocking here would stall every other LSP request behind a save.
///
/// Single-flight per URI: if a render is already running, we just record the newest
/// source as `pending` (overwriting any earlier queued source) instead of spawning a
/// second Chromium. When the running render finishes it picks up that pending source and
/// renders once more. Net: at most one in-flight render plus one queued, per URI.
fn refresh_preview(sender: &Sender<Message>, renders: &Renders, uri: String, source: String) {
    {
        let mut map = renders.lock().unwrap();
        let slot = map.entry(uri.clone()).or_default();
        if slot.rendering {
            slot.pending = Some(source);
            return;
        }
        slot.rendering = true;
    }

    let sender = sender.clone();
    let renders = renders.clone();
    std::thread::spawn(move || {
        let target = match preview_path(&uri) {
            Ok(target) => target,
            Err(error) => {
                show_error(&sender, &error.to_string());
                renders.lock().unwrap().remove(&uri);
                return;
            }
        };

        let mut current = source;
        loop {
            if let Err(error) = render::render(&current, &target) {
                // A syntax error mid-edit is normal and expected. Report it, but leave
                // the last good render in the tab rather than blanking it.
                show_error(&sender, &format!("Mermaid: {error}"));
            }

            let mut map = renders.lock().unwrap();
            let slot = map.entry(uri.clone()).or_default();
            if slot.closing {
                // The document closed while we rendered; clean up its temp dir now that
                // no render is touching it.
                map.remove(&uri);
                drop(map);
                remove_preview_dir(&uri);
                return;
            }
            match slot.pending.take() {
                Some(next) => current = next,
                None => {
                    slot.rendering = false;
                    return;
                }
            }
        }
    });
}

/// Tear down a preview's temp directory when its source document closes. If a render is
/// in flight we defer to that thread (via `closing`) rather than yanking the directory
/// out from under it.
fn close_preview(renders: &Renders, uri: &str) {
    {
        let mut map = renders.lock().unwrap();
        if let Some(slot) = map.get_mut(uri) {
            if slot.rendering {
                slot.closing = true;
                slot.pending = None;
                return;
            }
        }
        map.remove(uri);
    }
    remove_preview_dir(uri);
}

/// Remove this document's `<hash>` preview directory and everything in it. Never removes
/// the shared base directory, which other instances may still be using.
fn remove_preview_dir(uri: &str) {
    if let Ok(dir) = preview_dir(uri) {
        let _ = fs::remove_dir_all(dir);
    }
}

/// Ask the running editor instance to open the rendered image as a tab.
///
/// Zed does not implement the LSP `window/showDocument` request for regular language
/// servers (only Copilot registers a handler, and it is a no-op stub), so we go through
/// the editor CLI instead (Wu is a Zed fork whose CLI is `wu`). Re-opening an
/// already-open path just focuses that tab.
fn open_in_editor(path: &Path) -> Result<()> {
    // `wu` first: on a Wu system `zed` is usually absent, and trying `wu` first avoids
    // a pointless spawn failure. `xdg-open` stays last: it opens the system image
    // viewer rather than an editor image tab, so it loses live re-render on save.
    for binary in ["wu", "zed", "zeditor", "xdg-open"] {
        let spawned = Command::new(binary)
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        if let Ok(child) = spawned {
            // Reap the launcher so it does not linger as a zombie. Detached, so the
            // caller is not blocked on the GUI process.
            std::thread::spawn(move || {
                let mut child = child;
                let _ = child.wait();
            });
            return Ok(());
        }
    }

    Err(anyhow!(
        "could not open {}: no `wu`/`zed` CLI on PATH",
        path.display()
    ))
}

fn is_mermaid_document(uri: &str) -> bool {
    uri.ends_with(".mmd") || uri.ends_with(".mermaid")
}

fn source_path(uri: &str) -> Result<PathBuf> {
    Url::parse(uri)
        .context("parsing document uri")?
        .to_file_path()
        .map_err(|_| anyhow!("document is not a local file: {uri}"))
}

/// Per-user base directory for all previews. Everything below it inherits its 0700,
/// user-only access, which is what neutralizes the /tmp symlink-planting and diagram
/// disclosure risks: an attacker cannot traverse into, read, or plant paths inside a
/// directory only the victim can enter.
///
/// Prefers `$XDG_RUNTIME_DIR` (on Linux that is /run/user/UID, already a private 0700
/// tmpfs), falling back to a euid-qualified name under the system temp dir. Either way
/// the directory is created 0700, and if it already exists it must be a real directory
/// owned by us with exactly 0700 permissions or we refuse to use it.
fn base_dir() -> Result<PathBuf> {
    // SAFETY: geteuid is always successful and has no preconditions.
    let euid = unsafe { libc::geteuid() };

    let base = match env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) if Path::new(&runtime).is_dir() => {
            Path::new(&runtime).join("zed-mermaid-quicklook")
        }
        _ => env::temp_dir().join(format!("zed-mermaid-quicklook-{euid}")),
    };

    ensure_secure_dir(&base, euid)?;
    Ok(base)
}

/// Create `dir` as a private 0700 directory, or verify an existing one is safe to reuse.
/// Uses `symlink_metadata` so a planted symlink is seen as a symlink (not its target)
/// and rejected.
fn ensure_secure_dir(dir: &Path, euid: u32) -> Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            if !meta.file_type().is_dir() {
                return Err(anyhow!(
                    "refusing to use {}: not a directory (possible symlink attack)",
                    dir.display()
                ));
            }
            if meta.uid() != euid {
                return Err(anyhow!(
                    "refusing to use {}: owned by uid {}, not {euid}",
                    dir.display(),
                    meta.uid()
                ));
            }
            if meta.mode() & 0o777 != 0o700 {
                return Err(anyhow!(
                    "refusing to use {}: permissions are {:04o}, expected 0700",
                    dir.display(),
                    meta.mode() & 0o777
                ));
            }
            Ok(())
        }
        // Non-recursive create issues a single mkdir(2): if the directory is planted
        // between the stat above and here, this fails with EEXIST rather than adopting
        // it. The parent (XDG_RUNTIME_DIR or the system temp dir) always exists already.
        Err(error) if error.kind() == ErrorKind::NotFound => DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display())),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", dir.display())),
    }
}

/// Stable per-document directory (`<base>/<hash>`), holding just this document's preview.
fn preview_dir(uri: &str) -> Result<PathBuf> {
    let mut hasher = DefaultHasher::new();
    uri.hash(&mut hasher);
    Ok(base_dir()?.join(format!("{:016x}", hasher.finish())))
}

/// Stable per-document path, so re-rendering overwrites the file the preview tab holds
/// open. The basename is the source's, so the tab reads `diagram.png` rather than a hash.
fn preview_path(uri: &str) -> Result<PathBuf> {
    let source = source_path(uri)?;
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("diagram");

    Ok(preview_dir(uri)?.join(format!("{stem}.png")))
}

fn show_error(sender: &Sender<Message>, message: &str) {
    notify(sender, MessageType::ERROR, message);
}

fn show_info(sender: &Sender<Message>, message: &str) {
    notify(sender, MessageType::INFO, message);
}

fn notify(sender: &Sender<Message>, typ: MessageType, message: &str) {
    let params = ShowMessageParams {
        typ,
        message: message.to_string(),
    };

    let _ = sender.send(Message::Notification(Notification {
        method: "window/showMessage".to_string(),
        params: json!(params),
    }));
}
