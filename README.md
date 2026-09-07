# meridian-core

The framework-free core of [Meridian](https://github.com/Yuerchu/meridian), a
multi-provider AI desktop client with coding-agent capabilities. Everything in
here runs without Tauri, without a window and without a WebView: the desktop
app is a thin shell over this workspace, and the same crates drive the headless
runners (a OneBot/QQ bot, the Claude Code hook gates) and the ACP host.

## Crates

| Crate | What it is |
|-------|------------|
| `core` (`meridian-core`) | The agent turn loop and the ports its runners plug into; providers (OpenAI-compatible, Anthropic, Codex transport); the tool set, MCP client and sandboxed command execution; SQLite persistence with embedded migrations; secrets and keyring storage; the OneBot and hook runners; ACP hosting of another coding agent; offline speech-to-text and TTS. |
| `sandbox-types` | Absolute-path and permission types shared by the sandbox backends. Ported from Codex. |
| `sandbox-windows` | The Windows restricted-token sandbox. Ported from Codex. |

`meridian-core` has no dependency on any UI framework, and that is enforced by
the crate boundary rather than by review: the desktop shell is the only place
that knows about Tauri.

## Building

```bash
cargo test --workspace
```

Requirements:

- A stable Rust toolchain (edition 2024).
- On Linux, `libasound2-dev` for microphone capture (`cpal`).
- `sherpa-onnx-sys` downloads a prebuilt library archive for the host platform
  on first build. On Android it cannot, and `SHERPA_ONNX_LIB_DIR` has to point
  at prebuilt libraries fetched separately; keep that variable scoped to the
  build that needs it, since it is honoured for every target.

Style gates are `cargo fmt --all --check` and
`cargo clippy --workspace --all-targets -- -D warnings`. A pre-commit hook that
runs both is in `.githooks/`; enable it with
`git config core.hooksPath .githooks`.

## How the desktop app uses it

The Meridian repository vendors this one as a git submodule at
`src-tauri/crates` and depends on the crates by path. Its workspace `exclude`s
the submodule directory, so this workspace is built on its own terms — which is
why no crate here uses `[workspace.package]` inheritance (see the comment in
`Cargo.toml`).

## License

Apache-2.0. See `LICENSE`, and `NOTICE` for the code ported from OpenAI Codex
and for the design patterns borrowed from Grok Build.
