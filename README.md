# Mermaid Quicklook

Preview Mermaid diagrams in Zed. Open a `.mmd` file, hit `Ctrl+.`, and the diagram opens
as an image tab next to your source. Save the file and the preview updates in place.

**Your source file is never modified.**

## How it works

Zed extensions cannot draw their own UI — the extension API exposes language servers,
grammars, and themes, but no panels or views. So instead of building a preview pane,
this renders the diagram to a PNG in a temp directory and asks the running Zed instance
to open it as an image tab (via the `wu`/`zed` CLI). Zed watches that file, so re-rendering
on save repaints the open tab: a live preview, using Zed's own image viewer, which
already supports zoom.

Rendering is done by the Mermaid CLI (`mmdc`).

## Requirements

- Zed (or its Wu fork — the `wu` CLI replaces `zed`)
- Rust toolchain (to build)
- `mmdc` — `npm install -g @mermaid-js/mermaid-cli`

## Install

```bash
./scripts/install.sh
```

Then load the extension (in Wu the palette entry is `wu: install dev extension`):

1. `Ctrl+Shift+P` → **`zed: install dev extension`** (or `wu: install dev extension`)
2. Select this directory

## Usage

Open a `.mmd` file and press `Ctrl+.`:

| Action | What it does |
| --- | --- |
| **Open Mermaid Preview** | Renders and opens the diagram as an image tab. Re-run it to focus a preview you closed. |
| **Export Mermaid Diagram as PNG** | Writes `<name>.png` next to the source file. |
| **Export Mermaid Diagram as SVG** | Writes `<name>.svg` next to the source file. |

Once a preview is open, **every save re-renders it** and the tab updates itself. Zoom
with the image viewer's controls.

If a diagram has a syntax error, you get an error toast and the preview keeps showing
the last render that worked, rather than going blank.

## Credits

Inspired by [dawsh2/zed-mermaid-preview](https://github.com/dawsh2/zed-mermaid-preview),
which renders diagrams by rewriting your Markdown in place. This takes the opposite
approach: preview only, source untouched.

Syntax highlighting comes from [monaqa/tree-sitter-mermaid](https://github.com/monaqa/tree-sitter-mermaid)
(`languages/mermaid/highlights.scm` is that grammar's own query file, MIT, © 2022 Mogami Shinichi).

## Notes

- Preview files live in `/tmp/zed-mermaid-quicklook/` and are removed when you close the
  source file. Exports go next to your source and are yours to keep.
- The preview tab's lifecycle is invisible to a language server (an image tab is not a
  text document), so closing the preview isn't something this can detect. It just keeps
  re-rendering on save while the source file is open; the cost of guessing wrong is one
  wasted render.
- Mermaid's parser rejects a comment marker with nothing after it (`%%` alone on a line)
  and blames line 1 regardless of where it actually is. Those lines are stripped before
  rendering, so a `%%` spacer in a comment header works here even though it fails in
  plain `mmdc`.

## License

MIT
