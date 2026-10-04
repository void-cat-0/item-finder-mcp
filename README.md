# where-i-put

[![CI](https://github.com/void-cat-0/where-i-put/actions/workflows/ci.yml/badge.svg)](https://github.com/void-cat-0/where-i-put/actions/workflows/ci.yml)

Vision-based item memory: watch cameras (or a Frigate NVR's events), remember
where objects were last seen, answer "where are my keys?".

**Cross-platform by design**: Windows, Linux and macOS are all first-class
targets — not a port to be attempted later. Platform-specific behaviour
(services, signals, atomic file replacement) must be stated explicitly rather
than silently defaulting to one OS; `#[cfg(...)]` is reserved for genuinely
platform-specific code (signal kinds, locking primitives). The only
deliberately single-platform pieces today are the RTSP toolchain fallbacks
(no BtbN prebuilt for macOS, which uses a system/brew FFmpeg instead).

Crates, one-way dependencies (`item-ingest`/`item-query`/`item-web` -> `item-core`;
`item-web` also reuses `item-query`'s prompt+VLM client):

- **crates/core (`item-core`)** — domain model (`Detection`, `Observation`,
  `Region`), box geometry (IoU, greedy NMS), SQLite storage. The unit of truth
  is the *Observation*: "label X was in zone Z of camera C during \[first_seen,
  last_seen]", merged while sightings stay within a 5-min dedup window.
- **crates/item-ingest** — the write side. `FrameSource` (Mock; USB/built-in
  webcams via nokhwa behind `--features camera`; RTSP/IP cameras via
  ffmpeg-next behind `--features rtsp`) -> `Detector` (Null without features;
  real YOLOv8-onnx behind `--features yolo` via ort; open-vocabulary VLM
  grounding behind `--features vlm` — an OpenAI-compatible multimodal sidecar
  answers with labeled boxes, so keys/remotes beyond COCO-80 work) -> NMS ->
  zone mapping -> store. Also an axum webhook server that ingests Frigate
  events directly, skipping local detection entirely, plus an MJPEG web
  preview bridge for RTSP cameras (`--preview`, feature `rtsp`). Runs as a
  resident daemon (`--daemon --config`, see below) that drives every configured
  camera at once, publishes a health snapshot, and keeps its own database and
  snapshot directory bounded.
- **crates/item-query** — the read side. CLI (`log`, `ask`, `candidates`) over
  observations, conservative 2-D cover hypotheses, and size/container
  evidence from an optional priors file,
  with an OpenAI-compatible VLM client (llama.cpp/Ollama/cloud sidecar) used
  only when `ITEM_VLM_BASE_URL`/`ITEM_VLM_MODEL` are set. The Rust core never
  embeds a VLM.
- **crates/item-web** — the display side. Read-only web UI (binds 127.0.0.1:8478
  by default): one binary serving a zero-build vanilla-JS card grid (snapshot,
  zone/camera chips, hit count, relative time; live-search filter; ask bar that
  answers with the direct last sighting plus hypothesis evidence cards, worded
  via the VLM sidecar when configured; click-to-zoom
  snapshot modal; 10 s auto-refresh) plus a small JSON API (`/api/observations`,
  `/api/observation/{id}/snapshot`, `/api/geometry/{id}/snapshot`, `/api/ask`). Opens the same SQLite WAL file
  the daemon writes, read-only — no write path, no contention, no shared process.

## MCP queries and camera frames (stdio)

**crates/item-mcp** is a standalone MCP stdio server using the official `rmcp`
SDK, locked to **3.5.1**. It exposes seven tools; `get_camera_frame` asks the daemon
to save a temporary immutable image asset, while the other six tools are read-only:

| Tool | Purpose |
| --- | --- |
| `server_info` | Check the connection; returns `mode: queries_and_camera_frames`, not database health. |
| `list_recorded_items` | List actual recorded labels with observation counts, last seen time, cameras and zones. |
| `locate_item` | Find an item's last direct sighting using an explicit label or an English keyword heuristic. |
| `list_observations` | List sightings with optional label, camera, zone and last-seen time filters. |
| `get_observation` | Read basic fields of one observation by ID. |
| `get_item_images` | Read verified unannotated raw JPEGs, including unexpired camera frame assets, through the authenticated local daemon. |
| `get_camera_frame` | Get a fresh cached camera frame and freeze it as an expiring raw asset, with capture time, age and hash. |

The ingest daemon remains a separate process: it writes the observations;
MCP opens that SQLite database **read-only** for each query. It never creates
or migrates the database and does not start/stop cameras or the daemon. A missing
or unreadable database produces a `database_unavailable` tool error, not an
empty result. If ingest later creates the database, subsequent queries can
read it without restarting MCP. Old observation-only schemas remain readable.

The SDK declares Rust 1.88, but the locked `uuid` dependency requires 1.89;
`item-mcp` declares **Rust >= 1.89** and is tested with stable Rust 1.98.1.
No camera/model/FFmpeg setup is needed for the MCP process.

```sh
cargo +stable build -p item-mcp
cargo +stable test -p item-mcp
# Linux/macOS: the MCP host runs this command with JSON-RPC on stdin.
./target/debug/item-mcp --stdio --db /absolute/path/to/data/items.db
# Windows:
./target/debug/item-mcp.exe --stdio --db C:/absolute/path/to/data/items.db
```

`--stdio` is optional. Database selection is `--db` > `ITEM_DB` > `data/items.db`;
relative paths resolve against the process's startup cwd, so use an absolute
path in a host configuration. Stdout contains only MCP messages, logs go to
stderr, and closing stdin after initialization stops the server. Legacy
initialize versions `2024-11-05`, `2025-03-26`, and `2025-11-25` are tested.

For an unknown name (especially a Chinese question), call `list_recorded_items`
first, then pass a recorded label to `locate_item`. `query` is a keyword
heuristic, not translation or semantic search. For example:

```json
{"camera":"desk","limit":200}
```

is an input to `list_recorded_items`; after discovering `keys`, use:

```json
{"label":"keys","camera":"desk","zone":"tray","limit":20}
```

as input to `locate_item`. Explicit `label` takes priority over `query`.
Observation lists accept literal label substrings (no wildcard/SQL syntax),
exact camera/zone values, and inclusive RFC3339 `since`/`until` bounds on
**last_seen**. Filters apply before the limit; results are newest first with
an ID tie-break. Limits must be 1–500 (default 20 for observations, 200 for
labels); inventory includes `truncated` when more labels match. Its count is
observation rows, **not** frame hits. Empty matches are successful empty lists;
invalid inputs and unknown IDs return `invalid_input` and `not_found` errors.

Observation queries return historical direct sightings, **not current location/presence**.
Labels may contain detector false positives. `get_item_images` reads raw evidence
through the daemon. `get_camera_frame` returns the most recent frame within an
explicit freshness limit; its capture time describes when the daemon received
the frame, not the exact instant the MCP request was made. Inferred locations,
evidence details, resources, prompts, annotation writes and daemon management
are not registered yet. No local snapshot paths are exposed.

To enable raw image reads, use a separate loopback listener in the ingest config:

```toml
[daemon]
internal_listen = "127.0.0.1:8480"
internal_max_asset_bytes = 8388608
```

Set a shared `ITEM_DAEMON_TOKEN` in the environment of both daemon and MCP.
The daemon refuses to enable the internal listener without a token. Set
`ITEM_DAEMON_ENDPOINT=http://127.0.0.1:8480` for MCP, or pass
`--daemon-endpoint http://127.0.0.1:8480`. This HTTP API is separate from
MCP's stdio transport and the Frigate webhook/preview listener.

```json
{"image_kind":"raw","observation_ids":[12],"max_images":4,"max_bytes_each":4194304}
```

The image tool returns structured metadata plus MCP image content; each
`content_index` identifies the corresponding image block. Raw assets are
unannotated captured RGB frames encoded as JPEG, persisted separately from
existing detector snapshots. Historical snapshots and Frigate references do
not gain raw provenance retroactively; missing raw returns `raw_unavailable`.
The daemon owns raw publication, recovery and cleanup. MCP verifies the stable
source identity of its `--db` against each daemon asset, so configure both to
use the same dataset; different datasets return `dataset_mismatch`. A stopped daemon yields
`daemon_unavailable` for images while SQLite history queries remain usable.
For a current camera image, call `get_camera_frame` with the configured camera ID:

```json
{"camera_id":"laptop","max_age_ms":2000,"wait_timeout_ms":3000,"max_bytes":4194304}
```

The daemon caches the latest decoded RGB frame independently of detector results.
A request freezes an eligible frame into an immutable JPEG asset; it does not open
another camera connection, trigger detection or create an observation. If the
cached frame is too old, the request waits up to `wait_timeout_ms` for a fresh
frame. Offline cameras and expired frames produce explicit errors. The returned
`asset_id` can be reread using `get_item_images` before `expires_at`; reading it
does not refresh its lifetime. This limited daemon-side asset write is why
`get_camera_frame` is not advertised as a read-only tool. Annotation submission
is still unimplemented.

See [the daemon asset API contract](docs/daemon-asset-api.md) for cache/asset
lifetimes, limits, error semantics, identity/hash verification and configuration.

In **ZCode Settings → MCP**, use the executable's absolute path and pass an
absolute `--db`. Merge this example into `mcp.servers` without replacing other
servers:

```json
{
  "mcp": {
    "servers": {
      "item-finders-mcp": {
        "type": "stdio",
        "command": "C:/path/to/item-finders-mcp/target/debug/item-mcp.exe",
        "args": ["--stdio", "--db", "C:/path/to/item-finders-mcp/data/items.db"]
      }
    }
  }
}
```

If Windows keeps a running MCP executable locked during a rebuild, build with
`--target-dir target/mcp-query` and point the host at
`target/mcp-query/debug/item-mcp.exe` instead. Reconnect/restart the host session
to reload the executable and tool list. See [the MCP plan](docs/mcp-server-plan.md)
for subsequent stages.

## Quick start

```sh
# pipeline smoke test, no hardware needed
cargo run -p item-ingest -- --demo

# RTSP camera (one-time: `cargo xtask setup` to fetch FFmpeg + libclang)
cargo run --features rtsp -p item-ingest -- --rtsp "rtsp://user:pass@192.168.1.50:554/Streaming/Channels/101" --camera-id living

# Built-in/USB webcam: same closed loop WITHOUT FFmpeg/native setup
cargo run --features "camera,yolo" -p item-ingest -- --webcam 0 --camera-id laptop --detect-fps 2

# live web preview (MJPEG bridge; browsers can't speak RTSP) — open http://<host>:8477/preview
cargo run --features rtsp -p item-ingest -- --preview "rtsp://user:pass@192.168.1.50:554/Streaming/Channels/101"

# object detection on one image (needs a local models/yolov8n.onnx, see below)
cargo run --features yolo -p item-ingest -- --detect path/to/photo.jpg

# open-vocabulary detection on one image via a VLM sidecar (see
# docs/vlm-sidecar.md for standing up llama.cpp + Qwen2.5-VL in minutes)
ITEM_VLM_BASE_URL=http://127.0.0.1:8080/v1 ITEM_VLM_MODEL=qwen2.5-vl-3b-instruct \
cargo run --features vlm -p item-ingest -- \
    --detect path/to/photo.jpg --detector vlm --targets "remote,keys" --out out.png

# THE CLOSED LOOP with the VLM detector: camera -> throttled grounding ->
# zone-mapped observations + snapshots, labels beyond COCO (keys, remote, ...)
ITEM_VLM_BASE_URL=http://127.0.0.1:8080/v1 ITEM_VLM_MODEL=qwen2.5-vl-3b-instruct \
cargo run --features "camera,vlm" -p item-ingest -- --webcam 0 --camera-id desk

# THE CLOSED LOOP: camera -> throttled YOLO -> zone-mapped observations + snapshots
cat > config.toml <<'EOF'
[[camera]]
id = "living"
url = "rtsp://user:pass@192.168.1.64:554/Streaming/Channels/102"
[camera.regions]                      # rects in camera pixels
desk = [0.0, 360.0, 1280.0, 720.0]
EOF
cargo run --features "rtsp,yolo" -p item-ingest -- \
    --rtsp "rtsp://user:pass@192.168.1.64:554/Streaming/Channels/102" \
    --camera-id living --config config.toml --detect-fps 1
cargo run -p item-query -- log            # what was seen where
cargo run -p item-query -- ask "where is the cup"

# THE WEB UI (read-only; open http://127.0.0.1:8478 while the loop's db exists)
cargo run -p item-web -- --db data/closed-loop.db
# with a VLM sidecar running, `ask` answers in sentences instead of raw log
# rows (works for item-web's ask bar too):
ITEM_VLM_BASE_URL=http://127.0.0.1:8080/v1 ITEM_VLM_MODEL=qwen2.5vl cargo run -p item-web

# webhook receiver (point Frigate event forwarding at POST /frigate/webhook)
cargo run -p item-ingest -- --listen 127.0.0.1:8477
```

## Resident daemon (`--daemon --config <file>`)

One process for a deployment that runs for weeks: the Frigate webhook server, the
optional preview, and one thread per `[[camera]]` row that names a source (`url`, or
`webcam = <index>` for a local camera), all writing through one `Store`. It runs until
it is asked to stop (Ctrl-C, or `SIGTERM` on Unix) and is the only mode that takes the
single-instance lock at `<db dir>/ingest.lock`.

```sh
cat > config.toml <<'EOF'
[daemon]
db = "data/items.db"
snapshots_dir = "data/snapshots"
health_file = "data/health.json"
events_file = "data/events.jsonl"   # appeared/disappeared timeline; "" = off
checkpoint_secs = 300     # WAL checkpoint (TRUNCATE)
retention_days = 90       # 0 = keep observations forever
sweep_secs = 3600         # how often expired observations are pruned
reconcile_secs = 604800   # how often orphan snapshot files are swept

[webhook]
enabled = true
listen = "127.0.0.1:8477"

[[camera]]
id = "living"
url = "rtsp://user:pass@192.168.1.64:554/Streaming/Channels/102"
detector = "yolo"
detect_fps = 1.0
[camera.regions]                      # rects in camera pixels
desk = [0.0, 360.0, 1280.0, 720.0]
EOF

cargo run --features "rtsp,yolo" -p item-ingest -- --daemon --config config.toml
```

Relative paths in the config resolve against **the config file's directory**, not the
process cwd: a service manager picks the cwd, and snapshots have to land where
`item-web` looks for them. A row with neither `url` nor `webcam` is fed by Frigate's
webhooks; `enabled = false` keeps a camera out of the daemon without deleting its
regions.

### Is it alive? (`data/health.json`)

Rewritten every 15 s, atomically (`.tmp` + rename), so readers must tolerate the file
being briefly missing or stale:

```json
{"schema": 1, "pid": 12345, "started_at": "…", "updated_at": "2026-09-09T02:37:15Z",
 "seq": 148, "detector_default": "yolo",
 "cameras": [{"id": "living", "state": "running", "frames": 210344, "detections": 21033,
              "recorded": 87, "reconnects": 3, "last_frame_age_s": 1, "last_error": null}]}
```

- **`updated_at` older than 90 s means the process is dead or hung** — the *only*
  liveness signal. `pid` is advisory: Windows would need `OpenProcess`, Unix reuses pids.
- `state` is `starting` / `running` / `reconnecting` / `failed` (gave up, needs a human)
  / `stopped`.
- `last_frame_age_s` staying above ~3x the camera's `detect_fps` means the decode side
  is stuck. Every 5 minutes each camera also logs a `heartbeat` line, so silence never
  looks like success.

### What appeared and disappeared (`data/events.jsonl`)

Each camera also keeps a timeline: an observation becomes an `appeared` line when it
is first seen, and a `disappeared` line once it has gone unseen for `max(30s, 3/detect_fps)`.
Append-only JSONL, one event per line, `jq`/`grep`-ready — and rotated at 64MB keeping
two older generations, so a week of running stays bounded:

```json
{"ts":"2026-09-25T04:14:48Z","camera":"desk","zone":"frame","label":"laptop",
 "obs_id":3,"event":"appeared","hits":1}
{"ts":"2026-09-25T04:15:19Z","camera":"desk","zone":"frame","label":"laptop",
 "obs_id":3,"event":"disappeared","hits":1,"seen_for_s":30.5}
```

`hits` and `seen_for_s` describe the **last sighting** (not the moment the sweep ran),
which is what makes them comparable with the row's own `hit_count` and its
`last_seen - first_seen`. A `daemon_started` marker separates each run, so a
disappearance is never read across a restart boundary. On shutdown every still-open key
is closed first — otherwise the last real disappearance of a run would be lost.

**JSONL is an operational/debug mirror, not authoritative state.** G1 now persists
lifecycle facts in SQLite's `observation_events`, with source and noticed timestamps,
session/reason metadata, and optional immutable event-boundary geometry. Query rules
read those rows, not JSONL. Frigate webhooks persist discrete `appeared` facts (and
geometry when supplied), but cannot observe a continuous-frame disappearance.

### Keeping it bounded (`--maintenance`)

Observations older than `retention_days` (by `last_seen`) are pruned, each taking its
snapshot JPEG with it; every `reconcile_secs` a pass deletes snapshot files whose row is
gone. VACUUM is never automatic — run it on demand, with the daemon stopped:

```sh
item-ingest --maintenance --config config.toml          # refuses while a daemon holds the lock
item-ingest --maintenance --force --config config.toml  # only for a lock left by a killed daemon
```

The 90-day default is deliberate: the roadmap's containment inference needs exactly the
disappearance history this deletes. Read the comment on
`config::defaults::RETENTION_DAYS` before lowering it.

**One camera, one source**: a camera is either fed by Frigate or driven locally, never
both. Frigate sends only discrete events (no frames, so no disappearance can be
observed), and both paths on one camera produce two observations of the same object
fighting over the same zone.

### Running it as a service (restart, and surviving a kill)

Ready-to-edit templates for the three platforms live in [`deploy/`](deploy/README.md):
a systemd unit, a launchd plist, and a Windows Task Scheduler script. Each one gives
the daemon a 30 s stop budget, which is what its 10 s camera drain plus final health
write and WAL checkpoint need.

They rely on two behaviours worth knowing about:

- **A killed daemon does not wedge the next start.** The single-instance lock file at
  `<db dir>/ingest.lock` is re-stamped every 15 s while the daemon runs. A start that
  finds a lock silent for 45 s **takes it over** — logging
  `previous run died without shutdown` — instead of refusing forever. A `health.json`
  written in the last 90 s vetoes that takeover, so a live-but-wedged daemon is never
  robbed of its lock; the conservative side of that trade is that a restart right after
  a hard kill waits out those windows (~90 s) before it can take over.
- **The stop signals are the ones a service manager sends.** `SIGTERM`/`SIGINT`/`SIGHUP`
  on Unix; on Windows the console events (`Ctrl-C`, `Ctrl-Break`, console close, logoff,
  shutdown). A clean stop drains camera threads, writes a final health snapshot, closes
  every open event key and checkpoints the WAL before exiting 0.

`--maintenance` keeps the strict form: it never takes over a lock, because a human is
running it and the alternative to refusing is deleting data the daemon may be writing.

## RTSP backend (`--features rtsp`)

Pulls IP cameras through `ffmpeg-next` (libavformat + swscale, see
`item_ingest::source::rtsp`). This is the only backend that links native C
libraries, so building it needs FFmpeg 7.1 + libclang on top of the usual
MSVC toolchain. One command:

```sh
cargo xtask setup        # fetches into target/vendor/, verified by sha256
cargo build --features rtsp
```

No manual env vars: `.cargo/config.toml` points `FFMPEG_DIR`,
`LIBCLANG_PATH`, and the runtime `PATH` at `target/vendor/`, and `xtask
setup` fills exactly those paths (pinned URLs + hashes live in
`crates/xtask/src/main.rs`, module `pins`; each cached dir carries a
`manifest.json` recording url/sha/contents). Because the cache lives under
`target/`, `cargo clean` wipes it — re-run setup to restore.
`cargo xtask status` reports cached/missing/stale.

Linux works the same way; macOS has no BtbN build — use `brew install
ffmpeg` + system LLVM (setup prints hints, and externally-set
`FFMPEG_DIR`/`LIBCLANG_PATH` win over config.toml).

### Building FFmpeg from source: `cargo xtask setup --from-source`

Alternative to the prebuilt zip: downloads the pinned upstream tarball
(FFmpeg 7.1.5, sha256-checked), then `./configure` with a minimal whitelist
(`--disable-everything` + only rtsp/tcp/udp, h264/hevc/mjpeg decode, swscale)
and `make -j` into `target/vendor/ffmpeg/` — same include/lib/bin layout and
FFMPEG_DIR contract as the zip, so everything downstream is identical.
Manifest records flavor `source`; `setup` (no flag) restores the zip.
Why bother: the only first-class route for macOS (BtbN doesn't ship it),
byte-reproducible CI artifacts, and trimming features to our use.

The local build needs a POSIX toolchain — `sh` + `perl` + `make` + `nasm`,
and `cl.exe` on PATH on Windows (run from an MSVC Developer prompt, or use
MSYS2 which provides the rest). Preflight reports exactly what's missing;
minutes-scale, not seconds. The zip path remains the default precisely to
avoid this — use `--from-source` in CI or on Linux/macOS.

Verified in CI: the `rtsp-source` job (ubuntu) runs the full loop —
configure, make (~6 min), cargo test against the produced tree. Windows
and macOS source builds are deliberately not in the matrix yet (Windows
needs a proven cl.exe-in-MSYS2-sh handshake; macOS wants brew-llvm
pathing and has no zip fallback anyway). Layout note: a source install
puts `.so` files under `lib/` with no `bin/` (the zip ships `bin/*.dll`
for Windows); on Linux runtime resolution goes through the baked rpath,
so item-ingest's build.rs DLL staging is a silent no-op there.

Gotchas discovered during CI bring-up, kept in `xtask` so you don't
re-discover them: relative `--prefix` makes `make install` exit 0 into a
path inside the (later-deleted) source tree — `build_from_source`
absolutizes it; `configure` must be run as `sh ./configure` because our
tar unpack may not preserve the exec bit; and `--disable-everything`
disables the LIBRARIES too, so all six are re-enabled explicitly or the
bindgen headers go missing.

Version warning: rust-ffmpeg 9.x supports FFmpeg ≤ 7.x. BtbN's rolling
`latest` tag is FFmpeg 8 (`avcodec-63.dll`) and the sys crate's probe
rejects it — stay on the pinned 7.1 asset.

Dev-loop tip: `vendor/mediamtx.exe` + the vendored ffmpeg binary make a
local RTSP test stream —
`ffmpeg -re -stream_loop -1 -i test.mp4 -c copy -f rtsp -rtsp_transport tcp
rtsp://127.0.0.1:8554/cam1`. Verified end to end: decode -> RGB8 -> store.

### Web preview (`--preview <rtsp-url>`)

Browsers can't consume RTSP, so the daemon bridges it: a background thread
owns an `RtspSource` (with reconnect loop), JPEG-encodes at ~8 fps, and
serves the frames to any browser as MJPEG (`multipart/x-mixed-replace`) —
no JS, no external media server. Routes: `/preview` (page), `/preview.mjpg`
(live), `/preview.jpg` (single frame). Same listener as the webhook; url
credentials stay out of logs. LAN-only for now: anyone who can reach the
port can watch (gate with a proxy/Tailscale before exposing beyond the LAN).

## VLM grounding detector (`--features vlm`)

`--detector vlm` swaps the local YOLO for an open-vocabulary grounding pass
over an OpenAI-compatible multimodal sidecar (llama.cpp server, Ollama,
vLLM, cloud): the frame goes out as a base64 JPEG data URL with an
instruction to return a JSON array of `{"label","bbox_2d"}` (0-1000
normalized, Qwen-VL convention; replies with any coordinate >1000 are
auto-read as pixels), and everything downstream — NMS, zone mapping, dedup,
snapshot burn-in with highlight — is unchanged. Labels are whatever you ask
for via `--targets` (household defaults: remote, keys, scissors, charger, …;
empty string = "list everything visible"), reusing
`ITEM_VLM_BASE_URL`/`ITEM_VLM_MODEL` from the ask bar. Caveats: VLMs do not
provide calibrated detection confidence; an optional `score` is only carried
through as an input signal (otherwise it defaults to 1.0), not an accuracy
claim. One detection = one HTTP round trip against a local LLM — the loop therefore
defaults to `--detect-fps 0.2` and skips frames (never dies) while the
sidecar is down.

Hardware reality: grounding is an LLM round trip per frame — orders of
magnitude heavier than the YOLO path (~0.3 s/frame on CPU). Measured on a
Core Ultra 9 285H with a 3B model at Q4: 7-9 s/frame on the Arc 140T iGPU
(Vulkan build), 14-26 s/frame on 16 CPU threads. **Run the sidecar on
hardware with real inference headroom (a strong GPU; the Vulkan build also
drives Intel/AMD iGPUs), or point `ITEM_VLM_BASE_URL`/`ITEM_VLM_MODEL` at
an external sidecar** — any OpenAI-compatible endpoint on the LAN (vLLM,
llama.cpp server on another box) or a cloud API works with zero local setup,
and the ingest machine itself stays model-free. Runbook with download/start
commands: [docs/vlm-sidecar.md](docs/vlm-sidecar.md).

## Roadmap: location inference from occlusion & containment

The flagship target: answer "where are my keys?" even when they are no longer
visible. Evidence pattern on a single fixed camera: an object disappears from
detections while a covering/container object appears at its last-seen spot.
A single viewpoint and overlapping 2-D boxes cannot establish "inside the box".
The implemented rules offer only `possibly_occluded_by` / `possibly_under`
visibility hypotheses; containment additionally needs size, usable interior,
and affordance evidence. Scores rank candidates, not calibrated probabilities.
Building blocks, in order:

1. **Resident ingest** (prerequisite, not optional): the disappearance moment
   and the covering event are temporal facts only continuous observation
   produces — sporadic manual runs never see them. Design draft (process model,
   config, health file, retention, event observation):
   [docs/resident-ingest.md](docs/resident-ingest.md). **P0–P5 all landed**
   (process split, daemon skeleton, health/heartbeat/backoff, retention +
   `--maintenance`, the appeared/disappeared timeline, the three service
   templates), each with a real-hardware verification pass; the known gaps are
   listed in §9 of that document (chiefly: Linux/macOS service installs are
   template-level only).
2. **G1 — data model v2: landed.** SQLite schema v2 adds
   `observation_geometry`, `observation_events`, and `depth_evidence`.
   Geometry is bounded to first/last samples plus immutable event-boundary
   copies, not raw per-frame detections. Lifecycle facts are `appeared` /
   `disappeared`; `covered` / `contained` remain derived claims, not detector
   events. [docs/geometry-containment.md](docs/geometry-containment.md) separates
   this implementation from the remaining design contract.
3. **G2 — conservative 2-D candidates/CLI: landed.** `item-query candidates`
   matches missed-gap disappearances to persistent overlapping covers in the
   same camera, zone, and ingest session, with timing and geometry checks.
   It emits `possibly_occluded_by` / `possibly_under`, explanations, evidence
   ids, and heuristic scores. CLI `ask` keeps the direct last sighting first;
   VLM wording is optional and is not a guarantee of accuracy.
4. **G3 — optional relative-depth provider: landed.** `item-ingest` exposes a
   provider interface and an OpenAI-compatible ordinal VLM backend behind
   `--features relative-depth`. Depth is optional and provenance-bearing;
   comparable same-frame/provider-response ordering can support or conflict
   with a cover candidate. Missing or incomparable evidence stays `unknown`.
   This is not metric depth or validated containment accuracy.
5. **G4 — size and container affordance: landed (read side).**
   `item-query --priors <file>` adds object size intervals and measured,
   per-camera/zone container records ([example](crates/item-query/priors.example.toml)).
   A cover candidate becomes `possibly_contained_in` only when the priors file
   records that container as open with a usable interior the target fits by
   interval logic, the cover reaches over most of the target's last box, and
   identity is unambiguous; built-in class priors alone never promote. It is
   still a qualified hypothesis, never an observed location.
6. **G5 — evidence cards: landed.** `item-web --priors <file>` answers the
   ask bar with the direct last sighting first, then each hypothesis as a
   separate card: relation, heuristic score, timeline, every evidence item
   marked supporting / conflicting / unknown, both boxes drawn over their own
   snapshots, depth provenance, and rule/priors versions. Try it without a
   camera: `cargo run -p item-web --example evidence_demo`, then
   `cargo run -p item-web -- --db target/evidence-demo/items.db --priors crates/item-query/priors.example.toml`.
7. **Still open.** Persisted inferences with evidence links, inference expiry,
   visual opening-state/interior estimation, and the design's acceptance
   scenarios on real footage.
   Any future moved-with/container tracking or open-lid VLM checks must remain
   qualified hypotheses with auditable evidence, never an automatic transfer
   of an item's observed position to a container.

```sh
cargo run -p item-query -- --db data/items.db candidates keys --camera desk --window-secs 120 --json
cargo run -p item-query -- --db data/items.db --priors priors.toml ask "where are my keys?"
```

The command reads lifecycle evidence already written by ingest; it does not
create a containment record or persist a new inference.

Identity stays cheap: position-continuity + label association for large
static containers, manual naming in the WebUI over automatic re-ID (no
appearance-embedding trackers). Known hard edges: keys are not in COCO-80
(use `--detector vlm --targets`), small-object VLM grounding is the weak
spot, and transient occluders (people) vs persistent ones must be told apart
— validate the chain with detectable stand-in objects first.

## Deliberate choices / non-goals (for now)

- No tracker crate: `norfair` has no maintained Rust port, and fixed-camera
  zone aggregation needs only IoU association; see `core::geo`.
- Regions are manual pixel rects per camera; no auto spatial calibration.
- sqlite-vec embeddings not wired yet: keyword lookup carries v1, and vectors
  should be a rebuildable cache.
- ort pinned to 2.0-rc (1.16.x all yanked); the `yolo` module is verified
  against that API: detection on bus.jpg matches Ultralytics reference
  output, and `--detect` runs on live camera frames. Models are not vendored
  (`/models` is git-ignored); drop any stock Ultralytics YOLOv8/11 ONNX
  export (opset ≤ 17) at `models/yolov8n.onnx`. Beware hobby exports with
  custom preprocessing nodes (e.g. HzPreprocess) — ort can't run them.
- Snapshots: the closed loop writes one representative JPEG per NEW
  observation (`{snapshot-dir}/{id}.jpg`, attached via `set_sample_snapshot`),
  burned in with that frame's surviving detection boxes (label-colored,
  "label conf" chip via the embedded 8x8 font) and gray dashed zone rects;
  the new row's OWN box is highlighted (thicker stroke + white ring) --
  per-row rendering, so same-frame births get distinct files. SQLite stores
  bounded first/last geometry plus immutable event-boundary geometry in
  `observation_geometry`; it does not store raw per-frame boxes. The Frigate
  webhook path keeps storing `frigate://` refs when no image is available.
- Detection runs at `--detect-fps` (default 1; 720p CPU inference measured
  ~280ms/frame at quality 60) while decode runs at stream rate; a person
  lingering in one zone yields ONE merged observation with a hit count, not
  per-frame rows. COCO-only is a YOLO limitation: everyday objects like keys
  are exactly what the `--detector vlm` grounding backend (above) is for.
