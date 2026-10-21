# callrec

TUI call recorder: mic (L) + default-sink monitor (R) into one stereo FLAC, always
following the current default PipeWire devices. Full specification: `instruction.md`
(Russian). It is the source of truth; read the relevant section before changing behavior.

## Environment

- Dev shell: `flake.nix` (loaded by direnv via `.envrc`, or `nix develop`).
  Provides Rust stable, clippy, rustfmt, rust-analyzer, pkg-config, PipeWire headers,
  bindgen/libclang, `flac`, `ffmpeg`, `pw-*`, `jq`.
- Do not install tools with `cargo install` or rustup; add them to `flake.nix`.
- New files must be `git add`-ed, otherwise the flake (and the Nix package) does not see them.

## Commands

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo run
```

Done means: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and
`cargo test` all pass. The Stop hook (`.claude/hooks/stop-gate.sh`) enforces this when
Rust files have uncommitted changes; the PostToolUse hook runs rustfmt + clippy after
each `.rs` edit.

## Layout (spec section 7)

- `main.rs` — thread startup, signals (Ctrl+C, SIGTERM), terminal restore incl. panic hook.
- `pw.rs` — PipeWire: MainLoop, two capture streams, registry, `default` metadata.
- `mixer.rs` — per-channel queues, 100 ms clock, padding/trimming, RMS → dBFS.
- `writer.rs` — `flac` child process, `.flac.part` files, JSON metadata, recovery.
- `app.rs` — state machine `Idle → Recording ⇄ Paused → Saving → Idle`, commands.
- `ui.rs` — ratatui rendering, keys, mouse.

Implementation order and per-stage scope: spec section 7.1. One commit per stage.

## Rules

Reliability first: losing a recording of an important call is the main failure mode.

- No `unwrap`/`expect`/`panic!`/unchecked indexing in runtime code (enforced by lints in
  `Cargo.toml`; allowed in tests via `clippy.toml`). Handle every error path: log it to
  the UI event log and keep recording where the spec says so (section 6).
- `unsafe` is denied. If a PipeWire API truly needs it, isolate it in `pw.rs` with a
  `// SAFETY:` comment and a scoped `#[allow(unsafe_code)]`.
- PipeWire objects are not `Send`: all of them live in the PipeWire thread. Other threads
  talk to it only via `pipewire::channel`.
- Realtime `process` callback: no allocation, no locks, no blocking, no logging. Copy into
  a preallocated buffer or `try_send` and drop on overflow.
- Never set `target.object` or `node.dont-reconnect` on capture streams (spec 5.2).
- `mixer` and `writer` must not depend on `pipewire`. Keep them pure enough to unit-test
  without audio hardware. Required tests are listed in spec section 7.
- Thresholds and timings (1600, 3200, 4800, 320 samples, 100 ms, 2 s, −60 dB / 60 s, 1 GB) are named
  constants, not literals.
- Errors: typed errors (`thiserror`-style enums or small structs) at module boundaries
  where callers branch on the cause; `anyhow` with `.context(...)` at the top level and
  for one-way reporting.
- Child processes: always check exit status; never leave a `flac` process unreaped.
- Files: write `.part`, then rename. Never delete user recordings without an explicit
  user choice (spec 6.6).
- Keep the terminal recoverable on every exit path, including panics.
- UI text is Russian, as in the spec. Code, identifiers and comments are English.
