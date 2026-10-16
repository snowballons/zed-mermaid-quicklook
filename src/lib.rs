use zed_extension_api::{self as zed, LanguageServerId, Result};

const LSP_BINARY: &str = "mermaid-quicklook-lsp";
const LSP_PATH_OVERRIDE: &str = "MERMAID_QUICKLOOK_LSP";

struct MermaidQuicklookExtension;

impl zed::Extension for MermaidQuicklookExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        // The full shell environment carries secrets (AWS keys, tokens, ...) that the
        // LSP would in turn hand to mmdc/Chromium and the `zed`/`wu` CLI. Forward only
        // the variables those subprocesses actually need.
        let shell_env = worktree.shell_env();

        let command = shell_env
            .iter()
            .find(|(key, _)| key == LSP_PATH_OVERRIDE)
            .map(|(_, value)| value.clone())
            .or_else(|| worktree.which(LSP_BINARY))
            // GUI launches (Zed and the Wu fork alike) often have a minimal PATH that
            // misses `~/.cargo/bin`, where `./scripts/install.sh` puts the server via
            // `cargo install`. Fall back to the default cargo install location so the
            // extension works without the user hand-editing PATH.
            .or_else(|| default_cargo_lsp_path(&shell_env))
            .ok_or_else(|| {
                format!(
                    "`{LSP_BINARY}` not found on PATH. Build and install it with ./scripts/install.sh, \
                     or set {LSP_PATH_OVERRIDE} to its location."
                )
            })?;

        // Allowlist: PATH/HOME to locate and run the tools, plus the display/session
        // handles the Zed/Wu GUI and Chromium need to reach the user's desktop.
        const ALLOWED: &[&str] = &[
            "PATH",
            "HOME",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
            "LANG",
            "LC_ALL",
            "TERM",
            "MMDC_PATH",
            // mmdc runs via Puppeteer. Forward its browser resolution knobs so a
            // system Chrome works without downloading chrome-headless-shell, e.g.
            // `export PUPPETEER_EXECUTABLE_PATH=/usr/bin/google-chrome-stable`.
            "PUPPETEER_EXECUTABLE_PATH",
            "PUPPETEER_CACHE_DIR",
            "CHROME_PATH",
        ];

        let mut env: Vec<(String, String)> = shell_env
            .iter()
            .filter(|(key, _)| ALLOWED.contains(&key.as_str()))
            .cloned()
            .collect();

        // Render depends on the Mermaid CLI. Resolving it here means the LSP gets an
        // absolute path even when Zed's own PATH differs from the user's shell PATH.
        if let Some(mmdc) = worktree.which("mmdc") {
            env.retain(|(key, _)| key != "MMDC_PATH");
            env.push(("MMDC_PATH".to_string(), mmdc));
        }

        Ok(zed::Command {
            command,
            args: vec![],
            env,
        })
    }
}

zed::register_extension!(MermaidQuicklookExtension);

/// Default `cargo install` location for the language server (`~/.cargo/bin/...`).
/// Returned blindly (the WASM sandbox cannot stat the filesystem): the host's spawn
/// reports a clear ENOENT if it is truly absent, and in the common GUI-launch case
/// the file *is* there but PATH just doesn't include it.
fn default_cargo_lsp_path(shell_env: &[(String, String)]) -> Option<String> {
    let home = shell_env
        .iter()
        .find(|(key, _)| key == "HOME")
        .map(|(_, value)| value)?;
    if home.is_empty() {
        return None;
    }
    Some(format!("{home}/.cargo/bin/{LSP_BINARY}"))
}
