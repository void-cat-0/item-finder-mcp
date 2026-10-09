# AGENTS.md

Notes for AI coding agents (and humans) working in this repo.

## Version control: jj (Jujutsu)

This repo is managed with **jujutsu (`jj`)**, colocated with git
(`jj git init --colocate` was used; `.git/` still exists for remotes and
interop). Default to jj for all version-control operations:

- **Do not** run `git commit`, `git add`, `git checkout`, or `git reset`
  here. jj is the source of truth for the working copy; mixing raw git
  writes into a colocated repo causes conflicts and surprising divergent
  heads.
- `git push` / `git fetch` remain fine when the user explicitly asks to
  talk to a remote (colocated mode syncs jj automatically afterwards).
  Otherwise prefer `jj git push` / `jj git fetch`.

Common commands an agent will need:

| Task | Command |
| --- | --- |
| See current state + changes | `jj status` |
| History | `jj log -r 'main::@'` (or plain `jj log`) |
| Describe ("commit") the working copy | `jj describe -m "..."` |
| New change on top ("commit + start next") | `jj new` |
| Squash a change into its parent | `jj squash -r <change>` |
| Abandon a change | `jj abandon -r <change>` |
| Diff a change | `jj diff --change <change>` / `obslog` for history |

Commands that open an interactive editor hang agent bash sessions on this
Windows box (no usable `$EDITOR`): `jj describe` without `--stdin`,
`jj split` even with `--message`. Always describe via
`jj describe --stdin <<'MSG' ... MSG`; treat any jj subcommand that might
spawn an editor as forbidden in non-interactive runs.

jj concepts differ from git: every snapshot of the working copy **is** a
commit (change); there is no index/staging area. `jj describe` replaces
"commit with message". A change left without a description is normal
mid-work state, not an error to "fix" by committing prematurely.

Before ending a work session, make sure `@` has a meaningful description
(via `jj describe`) so the user's `jj log` reads cleanly.

## Toolchain / build

- Rust workspace, edition 2024 (rustc ≥ 1.85). `cargo check` / `cargo test`
  work with **no native deps** by default.
- The `rtsp` feature of `item-ingest` needs FFmpeg 7.1 + libclang. Setup is
  automated: run `cargo xtask setup` (downloads verified archives into
  `target/vendor/`), then `cargo build --features rtsp` — `.cargo/config.toml`
  injects `FFMPEG_DIR`/`LIBCLANG_PATH`/runtime `PATH`, so agents must **not**
  export these manually. `cargo xtask status` reports cache state.
- Pinning policy: all artifact URLs + sha256s live in `crates/xtask/src/main.rs`
  module `pins`. FFmpeg must stay on **7.1** (rust-ffmpeg 9.x rejects FFmpeg 8;
  BtbN's rolling `latest` tag ships 8 — never pin it). Each `target/vendor/*`
  dir carries a `manifest.json` recording url/sha/platform (plus flavor
  zip|source); a mismatch or missing manifest means stale cache →
  `cargo xtask setup [--force]`.
- `cargo xtask setup --from-source` builds the pinned upstream FFmpeg tarball
  (configure whitelist: rtsp/tcp/udp + h264/hevc/mjpeg only) into the same
  `target/vendor/ffmpeg` layout. Requires sh/perl/make/nasm (+ cl.exe via MSVC
  Developer shell on Windows) — on this Windows dev machine `make` is absent,
  so the preflight fails with guidance; treat that as the designed behavior.
- `cargo clean` wipes the toolchain cache (it lives under `target/`); that is
  by design, re-run setup after cleaning.
- `vendor/` (dev-only test gear: mediamtx, test.mp4) and `data/` (SQLite) are
  git-ignored; don't commit them, don't delete them to "clean up".
- **`cargo test` does not refresh `target/debug/<bin>.exe`** — it builds test
  harnesses instead. Run `cargo build -p <crate> --features ...` before executing
  the binary, or you will spend a while debugging one that predates your edit.

## Verifying with real hardware

Unit tests cover the plumbing; the camera-facing paths need a device or a
stream. Machine-specific recipes (which camera exists, exact commands, measured
numbers) live in `AGENT-MEMORIES/`; the generic shape is:

- **Local camera**: `cargo build -p item-ingest --features camera,yolo`, then
  `item-ingest --webcam <index> --detector yolo --frames N ...`. The index is
  not portable across OSes (DirectShow / V4L2 / AVFoundation enumerate
  differently), so treat it as a per-machine value.
- **RTSP without a camera**: the repo ships `vendor/mediamtx.exe` and
  `vendor/test.mp4`, and `target/vendor/ffmpeg/bin/ffmpeg.exe` is a full build
  with the RTSP muxer. Run mediamtx with `WorkingDirectory = vendor/` (it reads
  `mediamtx.yml` from the cwd), publish the clip with
  `ffmpeg -re -stream_loop -1 -i test.mp4 -c copy -f rtsp rtsp://127.0.0.1:8554/test`,
  then point `--rtsp` at that url.
- **The daemon** drives a local camera through `[[camera]] webcam = <index>` in
  config.toml (`url` wins when both are set); it has no `--webcam` flag.

Run the real paths before claiming a camera-facing change works. The 2026-09-14
pass found three defects no unit test had caught: a camera that died at startup
still reported `starting` in health.json, `--model` was cwd-relative (so a
daemon started by a service manager lost its model), and the daemon could not
use a webcam at all.

## Project conventions

- Three crates with one-way deps: `item-ingest` / `item-query` → `item-core`.
  Don't introduce a dependency back into `item-core` or between the two
  binaries.
- `Observation` (deduplicated, zone-scoped sightings) is the unit of
  persistence — never store raw per-frame detections in SQLite.
- New optional native backends go behind cargo features, default off, so a
  fresh clone builds and tests green with zero system dependencies.

## Local-only agent notes: `AGENT-MEMORIES/`

Machine-specific agent memory lives in `AGENT-MEMORIES/` (git-ignored):
LAN camera credentials (`camera-access.md`), CN-network workarounds,
tooling quirks, and a volatile session-state snapshot. Read it before
working on this repo, and write new machine-specific facts there rather
than into any single tool's private memory. The split:

- Durable + project-scoped usage/operational guidance → this file, `README.md`, or `docs/` (tracked).
- Design documents and API/architecture plans → `design/` (git-ignored, local only). Do not add or force-track this directory; a fresh clone will not contain these files.
- Machine/environment-specific or secret → `AGENT-MEMORIES/` (ignored).

Never commit that folder, never "clean it up", and never copy its contents
(credentials) into tracked files. In piped shell commands, use
`set -o pipefail` — `head`/`tail` in a pipeline swallow upstream exit codes.
