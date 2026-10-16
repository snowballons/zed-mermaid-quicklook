#!/bin/bash
#
# Builds the language server and puts it on PATH, where the extension looks for it.
# Works for Zed and its Wu fork (wu-bin): both use the same extension API, the only
# runtime difference is the editor CLI (`zed` vs `wu`) used to open the preview tab.
# The extension itself is loaded via `zed/wu: install dev extension` (the editor
# compiles the WASM and the tree-sitter grammar from this directory).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

echo "==> Checking the Mermaid CLI (mmdc)"
if ! command -v mmdc >/dev/null 2>&1; then
    echo "    mmdc not found on PATH."
    echo "    Install it with: npm install -g @mermaid-js/mermaid-cli"
    exit 1
fi
echo "    ok: $(command -v mmdc)"

echo "==> Ensuring mmdc's browser (chrome-headless-shell)"
# mmdc renders via Puppeteer, which needs its own browser download in
# ~/.cache/puppeteer -- a system Chrome is NOT picked up automatically. Without it
# every render fails with "Could not find chrome-headless-shell". Smoke-test a real
# render; only download the browser if the smoke test fails.
SMOKE_DIR="$(mktemp -d)"
trap 'rm -rf "$SMOKE_DIR"' EXIT
printf 'flowchart TB\n  A-->B\n' > "$SMOKE_DIR/smoke.mmd"
if mmdc -i "$SMOKE_DIR/smoke.mmd" -o "$SMOKE_DIR/smoke.png" -b white >/dev/null 2>&1; then
    echo "    ok: mmdc renders"
else
    echo "    mmdc has no browser; installing chrome-headless-shell (one-time download)..."
    if npx -y puppeteer browsers install chrome-headless-shell && \
        mmdc -i "$SMOKE_DIR/smoke.mmd" -o "$SMOKE_DIR/smoke.png" -b white >/dev/null 2>&1; then
        echo "    ok: browser installed, mmdc renders"
    else
        echo "    Warning: automatic browser install failed. Alternatives:" >&2
        echo "      - use your system Chrome: export PUPPETEER_EXECUTABLE_PATH=/usr/bin/google-chrome-stable" >&2
        echo "      - install manually: npx puppeteer browsers install chrome-headless-shell" >&2
    fi
fi

echo "==> Installing the language server to ~/.cargo/bin"
cargo install --path lsp --force

if ! command -v mermaid-quicklook-lsp >/dev/null 2>&1; then
    echo "    Warning: mermaid-quicklook-lsp is not on PATH." >&2
    echo "    Add ~/.cargo/bin to PATH, or set MERMAID_QUICKLOOK_LSP to its location." >&2
    exit 1
fi
echo "    ok: $(command -v mermaid-quicklook-lsp)"

echo "==> Building the editor extension (WASM)"
rustup target add wasm32-wasip2 >/dev/null 2>&1 || true
cargo build --lib --target wasm32-wasip2 --release

# The preview tab is opened via the editor CLI, so it must exist for previews to work.
# Wu's CLI is `wu`; Zed's is `zed`/`zeditor`.
if command -v wu >/dev/null 2>&1; then
    echo "    ok: $(command -v wu) (Wu fork detected)"
elif command -v zed >/dev/null 2>&1; then
    echo "    ok: $(command -v zed)"
else
    echo "    Warning: neither \`wu\` nor \`zed\` found on PATH. Preview tabs need one of them." >&2
fi

echo
echo "Done. To load the extension:"
echo "  1. Ctrl+Shift+P  ->  zed: install dev extension  (or wu: install dev extension in Wu)"
echo "  2. Select $REPO_ROOT"
echo
echo "Then open any .mmd file, press Ctrl+. and choose \"Open Mermaid Preview\"."
