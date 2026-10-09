//! xtask: developer tooling entry point (`cargo xtask setup` / `cargo xtask status`).
//!
//! Downloads and unpacks the native toolchain that the `rtsp` feature needs
//! (FFmpeg 7.1 shared build + libclang for bindgen) into `target/vendor/`, so
//! a `cargo clean` wipes everything and re-running setup is cheap. `.cargo/
//! config.toml` points FFMPEG_DIR/LIBCLANG_PATH at these paths, making
//! `cargo build --features rtsp` work with zero manual env vars.
//!
//! Why a setup task and not a build-script auto-fetch: cargo runs build
//! scripts in dependency order, and ffmpeg-sys-next (which reads FFMPEG_DIR)
//! builds before any crate that could populate it — we'd have to fork the sys
//! crate to close that loop. Download + sha256 pin + manifest.json per
//! artifact is the 80% solution, and the manifest records what each cached
//! directory contains and where it came from.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod download;
use download::{
    DEFAULT_DOWNLOAD_CHUNK_MIB, DEFAULT_DOWNLOAD_WORKERS, DownloadConfig, download_archive,
};

/// Version pins for the rtsp toolchain.
///
/// FFmpeg is constrained by rust-ffmpeg 9.x: it probes the FFmpeg ABI and
/// rejects 8.x (BtbN's rolling "latest" tag ships FFmpeg master = avcodec-63;
/// never pin that URL). 7.1 is the supported tip. BtbN publishes Linux and
/// Windows builds only — macOS users get a brew instruction instead.
mod pins {
    pub const BTBN_TAG: &str = "autobuild-2026-07-31-14-10";
    pub const BTBN_BASE: &str = "ffmpeg-n7.1.5-12-g1fdbca85aa";
    /// sha256 of the downloaded archive, per BtbN platform key. win64 was
    /// captured from a verified setup run; others get computed on first use.
    pub const FFMPEG_SHA: &[(&str, &str)] = &[(
        "win64",
        "3e61e96b44bce30f0fad9fc31955be7fe4d6690d6a5a2b65c62494e262f8369e",
    )];
    /// libclang via the PyPI wheel (smallest blessed source for the DLL
    /// bindgen needs). Candidates tried in order: pythonhosted (works from
    /// CI runners) then a CN mirror (useful from this dev machine).
    /// Linux/macOS rely on system LLVM (`apt install
    /// libclang-dev` / brew llvm).
    pub const LIBCLANG_WIN_WHEELS: &[&str] = &[
        "https://files.pythonhosted.org/packages/0b/2d/3f480b1e1d31eb3d6de5e3ef641954e5c67430d5ac93b7fa7e07589576c7/libclang-18.1.1-py2.py3-none-win_amd64.whl",
        "https://mirrors.ustc.edu.cn/pypi/web/packages/0b/2d/3f480b1e1d31eb3d6de5e3ef641954e5c67430d5ac93b7fa7e07589576c7/libclang-18.1.1-py2.py3-none-win_amd64.whl",
    ];
    // sha256 of the .whl archive itself (PyPI's published hash for this file),
    // NOT the extracted libclang.dll.
    pub const LIBCLANG_WIN_SHA: &str =
        "4dd2d3b82fab35e2bf9ca717d7b63ac990a3519c7e312f19fa8e86dcc712f7fb";
    /// Upstream FFmpeg release for `setup --from-source` (the only route to a
    /// FFmpeg of our own choosing on macOS, and to byte-reproducible
    /// artifacts in CI). sha256 computed from the downloaded tarball on
    /// 2026-09-03; cross-check against ffmpeg.org's release MD5/GPG if paranoid.
    pub const SRC_URL: &str = "https://ffmpeg.org/releases/ffmpeg-7.1.5.tar.xz";
    pub const SRC_SHA: &str = "de668509caf9e35e3cd162473441fdb29538c6d96ed080292b3cf9e6fc5d558f";
}

fn platform() -> &'static str {
    if cfg!(windows) {
        "win64"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_arch = "aarch64") {
        "linuxarm64"
    } else {
        "linux64"
    }
}

struct Artifact {
    /// Local directory name under target/vendor/.
    name: &'static str,
    /// Download candidates, tried in order (e.g. canonical then a CN mirror).
    /// The manifest pins the first as identity.
    urls: Vec<String>,
    sha256: Option<String>,
    kind: ArtifactKind,
    /// A file that proves a cached directory is a *complete* unpack of this
    /// artifact, relative to that directory. The directory alone is not
    /// evidence: `manifest.json` lives in it too, so a half-deleted (or
    /// AV-quarantined) payload would still read as `[cached]`.
    marker: &'static str,
}

impl Artifact {
    fn url(&self) -> &str {
        self.urls.first().map(String::as_str).unwrap_or("")
    }
}

enum ArtifactKind {
    /// Unpack whole archive, stripping the single wrapping top-level dir.
    ArchiveFlatten,
    /// Pull `*/clang/native/libclang.dll` out of a wheel to `native/`.
    LibclangDll,
    /// Nothing to fetch on this platform; setup prints `hint` instead.
    NotProvided,
}

fn artifacts() -> Vec<Artifact> {
    let os = platform();
    let ffmpeg = match os {
        "win64" | "linux64" | "linuxarm64" => {
            let ext = if os == "win64" { "zip" } else { "tar.xz" };
            Artifact {
                name: "ffmpeg",
                urls: vec![format!(
                    "https://github.com/BtbN/FFmpeg-Builds/releases/download/{}/{BTBN_BASE}-{os}-gpl-shared-7.1.{ext}",
                    pins::BTBN_TAG,
                    BTBN_BASE = pins::BTBN_BASE,
                    os = os
                )],
                sha256: pins::FFMPEG_SHA
                    .iter()
                    .find(|(k, _)| *k == os)
                    .map(|(_, v)| v.to_string()),
                kind: ArtifactKind::ArchiveFlatten,
                marker: "include/libavcodec/avcodec.h",
            }
        }
        _ => Artifact {
            name: "ffmpeg",
            urls: vec![],
            sha256: None,
            kind: ArtifactKind::NotProvided,
            marker: "",
        },
    };
    let libclang = if os == "win64" {
        Artifact {
            name: "libclang",
            urls: pins::LIBCLANG_WIN_WHEELS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            sha256: Some(pins::LIBCLANG_WIN_SHA.into()),
            kind: ArtifactKind::LibclangDll,
            marker: "native/libclang.dll",
        }
    } else {
        Artifact {
            name: "libclang",
            urls: vec![],
            sha256: None,
            kind: ArtifactKind::NotProvided,
            marker: "",
        }
    };
    vec![ffmpeg, libclang]
}

#[derive(Parser)]
#[command(name = "xtask")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Download + verify + unpack the native toolchain into target/vendor/.
    Setup {
        /// Refetch even when the cache matches the pins.
        #[arg(long)]
        force: bool,
        /// Build FFmpeg 7.1.5 from upstream source into target/vendor/
        /// ffmpeg/ instead of using the prebuilt zip. Needs a POSIX
        /// build environment (sh, make, nasm; MSVC inside a Developer
        /// shell on Windows). Minutes, not seconds.
        #[arg(long)]
        from_source: bool,
        /// Number of concurrent HTTP range workers.
        #[arg(long, default_value_t = DEFAULT_DOWNLOAD_WORKERS)]
        download_workers: usize,
        /// Range chunk size in MiB.
        #[arg(long, default_value_t = DEFAULT_DOWNLOAD_CHUNK_MIB)]
        download_chunk_mib: u64,
    },
    /// Report which artifacts are cached vs. missing/stale.
    Status,
}

/// `target/vendor` — deliberately under target/ so `cargo clean` wipes it.
///
/// Resolved from the **workspace root**, not the process cwd: `cargo xtask`
/// runs from any subdirectory, and `.cargo/config.toml` points FFMPEG_DIR at
/// the workspace root's `target/vendor` — installing anywhere else would leave
/// the build reading a directory setup never filled. An explicit
/// `CARGO_TARGET_DIR` wins, because that is where cargo actually puts target/.
fn vendor_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return Ok(PathBuf::from(dir).join("vendor"));
    }
    // crates/xtask -> crates -> workspace root
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.parent().and_then(Path::parent).unwrap_or(manifest);
    Ok(root.join("target").join("vendor"))
}

/// Written after a successful install; its presence + field match gates
/// re-download, and it doubles as the audit trail (url, hashes, contents note)
/// for whatever sits in the cached directory.
#[derive(Serialize, Deserialize)]
struct Manifest {
    url: String,
    /// Pinned hash, or the hash computed on first fetch when unpinned.
    sha256: Option<String>,
    source_note: String,
    platform: String,
    /// "zip" (default, prebuilt) or "source" (built by setup --from-source).
    /// Old manifests without the field load as "zip" via serde default.
    #[serde(default)]
    flavor: String,
}

impl Manifest {
    fn load(root: &Path, name: &str) -> Option<Self> {
        let raw = fs::read_to_string(root.join(name).join("manifest.json")).ok()?;
        serde_json::from_str(&raw).ok()
    }
}

enum Status {
    Cached,
    Missing,
    Stale,
    NotProvided,
}

fn install_status(root: &Path, art: &Artifact) -> Status {
    if matches!(art.kind, ArtifactKind::NotProvided) {
        return Status::NotProvided;
    }
    let Some(m) = Manifest::load(root, art.name) else {
        return if root.join(art.name).exists() {
            Status::Stale
        } else {
            Status::Missing
        };
    };
    // url+platform must match the pins; a sha recorded in the manifest may be
    // the computed one (unpinned platform), so only compare when pinned. A
    // "source" install has a different identity: pinned upstream tarball built
    // locally, recognized independently of the zip artifact url.
    let fresh = if m.flavor == "source" {
        m.platform == platform()
            && m.url == pins::SRC_URL
            && m.sha256.as_deref() == Some(pins::SRC_SHA)
            && root.join(art.name).join("include").exists()
    } else {
        m.platform == platform()
            && m.url == art.url()
            && art
                .sha256
                .as_ref()
                .is_none_or(|pin| m.sha256.as_deref() == Some(pin))
            && root.join(art.name).join(art.marker).exists()
    };
    if fresh { Status::Cached } else { Status::Stale }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn verify_sha256(bytes: &[u8], want: &str) -> Result<()> {
    let got = sha256_hex(bytes);
    if !got.eq_ignore_ascii_case(want) {
        bail!("sha256 mismatch: expected {want}, got {got}");
    }
    Ok(())
}

/// Reject an archive member that would land outside `dst`.
///
/// Pins make this near-unreachable (the bytes are sha256-verified *before*
/// unpacking), but an unpacker that can be talked into writing `../` by the
/// archive it is given is the wrong default, and `zip::ZipFile::name` and
/// `tar::Entry::path` are both documented as untrusted.
fn safe_join(dst: &Path, rel: &Path) -> Result<PathBuf> {
    use std::path::Component;
    let escapes = rel.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if rel.as_os_str().is_empty() || escapes {
        bail!(
            "refusing archive member {}: it would write outside {}",
            rel.display(),
            dst.display()
        );
    }
    Ok(dst.join(rel))
}

/// Unpack a .zip (Windows FFmpeg build, libclang wheel). When `strip_top`,
/// drop the first path component (BtbN archives wrap contents in one dir).
/// `only` keeps members whose path contains that substring; they are written
/// relative to `dst` after stripping everything before the match.
fn unpack_zip(bytes: &[u8], dst: &Path, strip_top: bool, only: Option<&str>) -> Result<()> {
    let mut zr = zip::ZipArchive::new(Cursor::new(bytes)).context("opening zip")?;
    for i in 0..zr.len() {
        let mut entry = zr.by_index(i)?;
        let raw = entry.name().to_string();
        let rel = if strip_top {
            match raw.split_once('/') {
                Some((_, r)) if !r.is_empty() => r.to_string(),
                _ => continue, // top-level dir entry itself, or nothing to strip
            }
        } else {
            raw.clone()
        };
        if rel.is_empty() || entry.is_dir() {
            continue;
        }
        let rel = match only {
            Some(f) => {
                let idx = match rel.find(f) {
                    Some(i) => i,
                    None => continue,
                };
                rel[idx..].to_string()
            }
            None => rel,
        };
        let target = safe_join(dst, Path::new(&rel))?;
        if let Some(p) = target.parent() {
            fs::create_dir_all(p)?;
        }
        let mut out = fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

/// Unpack a .tar.xz (Linux FFmpeg builds) with the top dir stripped. Uses
/// `Entry::unpack`, which restores the header's mode on unix -- FFmpeg's
/// extensionless ./configure otherwise hits exit 126 (Permission denied).
fn unpack_tar_xz(bytes: &[u8], dst: &Path) -> Result<()> {
    let mut tar_bytes = Vec::new();
    lzma_rs::xz_decompress(&mut Cursor::new(bytes), &mut tar_bytes)
        .map_err(|e| anyhow::anyhow!("xz decompress: {e:?}"))?;
    let mut ar = tar::Archive::new(Cursor::new(tar_bytes));
    for entry in ar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let stripped = path.components().skip(1).collect::<PathBuf>();
        let Some(rel) = stripped.to_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let target = safe_join(dst, Path::new(rel))?;
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(target)?;
            continue;
        }
        entry.unpack(&target)?;
    }
    Ok(())
}

fn note_for(name: &str) -> &'static str {
    match name {
        "ffmpeg" => {
            "FFmpeg 7.1 shared build (BtbN): include/, lib/ (import libs), \
                     bin/ (runtime DLLs). Consumed by ffmpeg-sys-next via FFMPEG_DIR \
                     (.cargo/config.toml). Pinned 7.1: rust-ffmpeg 9.x rejects FFmpeg 8."
        }
        "libclang" => {
            "libclang.dll extracted from the libclang PyPI wheel; bindgen \
                       (ffmpeg-sys-next build.rs) needs it via LIBCLANG_PATH."
        }
        _ => "vendor toolchain",
    }
}

/// Minimal toolchain required to build FFmpeg from source: its ./configure is
/// a POSIX shell script (needs sh + perl), the build needs make, x86 asm
/// needs nasm, and on Windows configure drives MSVC (cl.exe). Probe without
/// assuming --version flags work uniformly.
fn probe(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Does the command exist at all? `cl.exe` exits non-zero when invoked
/// argument-less (it prints its banner and a usage error), so its status is
/// not a usable signal -- being able to start it is.
fn probe_starts(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn require_build_tools() -> Result<()> {
    let mut missing: Vec<(&str, &str)> = Vec::new();
    if !probe("sh", &["-c", "true"]) {
        missing.push(("sh", "Windows: Git Bash (already typical) or MSYS2"));
    }
    if !probe("perl", &["--version"]) {
        missing.push((
            "perl",
            "configure is a perl-driven script; Git Bash ships perl; MSYS2: pacman -S perl",
        ));
    }
    if !probe("make", &["--version"]) {
        missing.push(("make", "Windows: MSYS2 `pacman -S make` (Git Bash does NOT ship make); Linux: build-essential; macOS: CLT"));
    }
    if !probe("nasm", &["-v"]) {
        missing.push(("nasm", "x86 asm backend; MSYS2: pacman -S nasm; Linux: apt install nasm; macOS: brew install nasm"));
    }
    if cfg!(windows) && !probe_starts("cl", &["/?"]) {
        missing.push((
            "cl",
            "Windows configure runs with --toolchain=msvc: start this from an \
             x64 Native Tools command prompt (or the MSVC developer shell), or \
             run the configure/make steps inside MSYS2 with cl.exe on PATH",
        ));
    }
    if !missing.is_empty() {
        let mut msg =
            String::from("FFmpeg from-source needs a POSIX build environment; missing:\n");
        for (tool, hint) in &missing {
            msg.push_str(&format!("  - {tool}: {hint}\n"));
        }
        msg.push_str(
            "On Windows also run from an MSVC Developer shell (cl.exe on PATH),\n\
             or skip all of this: the default `cargo xtask setup` (no flags)\n\
             fetches the prebuilt zip instead.",
        );
        bail!(msg);
    }
    Ok(())
}

/// Components we actually use (RTSP pull -> H.264/HEVC decode -> swscale
/// RGB), mirroring prpr-avc-ffmpeg's `--disable-everything` + whitelist shape,
/// minus their Hz custom op and plus the live-streaming stack they don't need.
/// IMPORTANT: `--disable-everything` disables the LIBRARIES too -- enabling a
/// decoder does not re-enable libavcodec. Every library ffmpeg-sys-next may
/// probe (per cargo features of consumers) is therefore enabled explicitly:
/// all six, matching the BtbN zip's header set, so both zip- and
/// source-flavored FFMPEG_DIR trees satisfy the build.rs check.c probe
/// (run 5: source tree died at "Compile failed" precisely because the
/// minimal whitelist left some library headers uninstalled).
/// Shared build keeps the FFMPEG_DIR contract identical to the zip path
/// (include/ + lib/ import libs + bin/ DLLs); a static flavor would need
/// ffmpeg-next's static feature too -- deferred.
const SRC_CONFIGURE: &[&str] = &[
    "--disable-everything",
    "--disable-programs",
    "--disable-doc",
    "--disable-debug",
    "--disable-autodetect",
    "--enable-gpl",
    "--enable-shared",
    // all six libraries, explicitly
    "--enable-avcodec",
    "--enable-avformat",
    "--enable-avdevice",
    "--enable-avfilter",
    "--enable-swscale",
    "--enable-swresample",
    "--enable-network",
    // decoders: video we ingest + mjpeg (prebuilt-zip compat)
    "--enable-decoder=h264,hevc,mjpeg",
    "--enable-demuxer=rtsp,rtp,mpegts,sdp,h264,hevc",
    "--enable-parser=h264,hevc,mpeg4video,mpegaudio,ac3",
    "--enable-protocol=rtsp,tcp,udp,rtp,file",
    "--enable-filter=scale",
    "--enable-indev=lavfi",
    "--enable-outdev=null",
];

/// Build FFmpeg from the pinned upstream tarball into target/vendor/ffmpeg/
/// (the canonical FFMPEG_DIR; this intentionally replaces a zip install, and
/// `cargo xtask setup --force` is what puts the prebuilt zip back -- plain
/// `setup` sees a valid cache and skips). Several minutes: configure's
/// self-tests dominate, then make.
fn build_from_source(root: &Path, config: DownloadConfig) -> Result<()> {
    require_build_tools()?;

    println!("downloading FFmpeg source (pinned {})", pins::SRC_URL);
    let source_urls = vec![pins::SRC_URL.to_owned()];
    let archive = download_archive(
        root,
        "ffmpeg-source",
        &source_urls,
        Some(pins::SRC_SHA),
        config,
    )?;
    let bytes = fs::read(&archive).with_context(|| format!("reading {}", archive.display()))?;

    let src = root.join("ffmpeg-src");
    if src.exists() {
        fs::remove_dir_all(&src)
            .with_context(|| format!("clearing the previous source tree at {}", src.display()))?;
    }
    fs::create_dir_all(&src)?;
    unpack_tar_xz(&bytes, &src)?;

    let prefix = root.join("ffmpeg");
    if prefix.exists() {
        // full replace keeps the artifact reproducible from the pinned tarball
        fs::remove_dir_all(&prefix).with_context(|| {
            format!(
                "clearing {} before the source install (is a loaded DLL using it?)",
                prefix.display()
            )
        })?;
    }
    fs::create_dir_all(&prefix)?;

    // --prefix MUST be absolute: configure/make run with cwd = the source
    // tree, so a relative prefix installs into ffmpeg-7.1.5/target/... and
    // exits 0 -- into the tree we delete afterwards (run 8: prefix contained
    // only our manifest.json). current_dir-join, not canonicalize (no \\?\
    // verbatim prefixes for sh to choke on). Windows wants forward slashes.
    let prefix_abs = if prefix.is_absolute() {
        prefix.clone()
    } else {
        std::env::current_dir()
            .context("resolving cwd")?
            .join(&prefix)
    };
    let mut cfg_args: Vec<String> = SRC_CONFIGURE.iter().map(|s| s.to_string()).collect();
    cfg_args.push(format!(
        "--prefix={}",
        prefix_abs.display().to_string().replace('\\', "/")
    ));
    if cfg!(windows) {
        cfg_args.push("--toolchain=msvc".into());
    }

    let jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .to_string();
    // configure is an extensionless shell script without a guaranteed exec
    // bit (our tar unpack may or may not preserve it depending on platform);
    // `sh ./configure` runs it regardless. Args via "$@".
    let mut cargs: Vec<&str> = vec!["-c", "sh ./configure \"$@\"", "configure"];
    cargs.extend(cfg_args.iter().map(|s| s.as_str()));
    run("sh", &cargs, Some(&src), "configure")?;
    run("make", &["-j", &jobs], Some(&src), "make")?;
    run("make", &["install"], Some(&src), "make install")?;

    let manifest = Manifest {
        url: pins::SRC_URL.into(),
        sha256: Some(pins::SRC_SHA.into()),
        source_note: format!(
            "FFmpeg 7.1.5 built FROM SOURCE via `cargo xtask setup --from-source`; \
             shared libs + DLLs, same layout as the zip install; configure flags: {:?}",
            SRC_CONFIGURE
        ),
        platform: platform().into(),
        flavor: "source".into(),
    };
    fs::write(
        prefix.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?.as_bytes(),
    )?;
    fs::remove_dir_all(&src).ok(); // ~300MB of sources; the artifact is the point
    println!("from-source complete -> {}", prefix.display());
    Ok(())
}

fn run(program: &str, args: &[&str], cwd: Option<&Path>, label: &str) -> Result<()> {
    println!("  {label}: {program} {}...", args.join(" "));
    let mut cmd = std::process::Command::new(program);
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    cmd.args(args);
    let status = cmd
        .status()
        .with_context(|| format!("spawn {label}: {program}"))?;
    if !status.success() {
        bail!(
            "{label} failed (exit {:?}); see output above",
            status.code()
        );
    }
    Ok(())
}

fn install(root: &Path, art: &Artifact, config: DownloadConfig) -> Result<()> {
    println!("downloading {} ...", art.name);
    let archive = download_archive(root, art.name, &art.urls, art.sha256.as_deref(), config)?;
    let bytes = fs::read(&archive).with_context(|| format!("reading {}", archive.display()))?;
    let recorded_sha = match &art.sha256 {
        Some(pin) => {
            verify_sha256(&bytes, pin)?;
            pin.clone()
        }
        None => {
            let got = sha256_hex(&bytes);
            println!("  note: unpinned artifact; recorded sha256 {got} in manifest.json");
            got
        }
    };
    let dir = root.join(art.name);
    if dir.exists() {
        // Must succeed: unpacking over a partly-removed tree would leave a
        // mixture the manifest then certifies as a clean install of these
        // bytes. On Windows a loaded DLL (the staged FFmpeg runtime) makes
        // this fail, and failing loudly is the only honest answer.
        fs::remove_dir_all(&dir).with_context(|| {
            format!(
                "clearing {} before reinstall (is a build or a staged DLL using it?)",
                dir.display()
            )
        })?;
    }
    fs::create_dir_all(&dir)?;
    match art.kind {
        ArtifactKind::ArchiveFlatten if art.url().ends_with(".zip") => {
            unpack_zip(&bytes, &dir, true, None)?
        }
        ArtifactKind::ArchiveFlatten => unpack_tar_xz(&bytes, &dir)?,
        ArtifactKind::LibclangDll => unpack_zip(&bytes, &dir, false, Some("native/libclang.dll"))?,
        ArtifactKind::NotProvided => unreachable!("install is never called for NotProvided"),
    }
    let manifest = Manifest {
        url: art.url().into(),
        sha256: Some(recorded_sha),
        source_note: note_for(art.name).into(),
        platform: platform().into(),
        flavor: "zip".into(),
    };
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?.as_bytes(),
    )?;
    println!("  -> {}", dir.display());
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let root = vendor_dir()?;

    match args.cmd {
        Cmd::Status => {
            let mut all_ready = true;
            let mut from_system = false;
            for art in artifacts() {
                match install_status(&root, &art) {
                    // The flavor matters: a `source` tree and the prebuilt zip
                    // are both "cached", and only one of them is what plain
                    // `setup` would install.
                    Status::Cached => {
                        let flavor = Manifest::load(&root, art.name)
                            .map(|m| {
                                if m.flavor == "source" {
                                    " (built from source)"
                                } else {
                                    ""
                                }
                            })
                            .unwrap_or("");
                        println!("[cached]  {}{flavor}", art.name);
                    }
                    Status::Missing => {
                        all_ready = false;
                        println!("[missing] {}", art.name)
                    }
                    Status::Stale => {
                        all_ready = false;
                        println!("[stale]   {} (pins or platform changed)", art.name)
                    }
                    Status::NotProvided => {
                        from_system = true;
                        println!(
                            "[n/a]     {} ({} — see setup message)",
                            art.name,
                            platform()
                        )
                    }
                }
            }
            println!(
                "\n{}",
                match (all_ready, from_system) {
                    (true, false) => "ready: cargo build --features rtsp".to_string(),
                    // macOS: nothing was fetched on purpose, so "ready" only
                    // holds if the system FFmpeg is actually installed.
                    (true, true) => "ready: cargo build --features rtsp (using the system \
                                     FFmpeg/libclang where marked n/a)"
                        .to_string(),
                    (false, _) => "run: cargo xtask setup".to_string(),
                }
            );
            Ok(())
        }
        Cmd::Setup {
            force,
            from_source,
            download_workers,
            download_chunk_mib,
        } => {
            let download_config = DownloadConfig::new(download_workers, download_chunk_mib, force)?;
            if from_source {
                build_from_source(&root, download_config)?;
                // libclang (or system llvm) is still needed for bindgen.
                for art in artifacts() {
                    if art.name != "libclang" {
                        continue;
                    }
                    match install_status(&root, &art) {
                        Status::Cached if !force => {
                            println!("[cached] libclang (pass --force to refetch)")
                        }
                        Status::NotProvided => println!(
                            "libclang: using system LLVM on {} (export LIBCLANG_PATH if not on default search path)",
                            platform()
                        ),
                        _ => install(&root, &art, download_config)?,
                    }
                }
                println!(
                    "\nfrom-source setup complete: FFMPEG_DIR is a locally built \
                     shared tree (same layout as the zip). This counts as a valid \
                     cache, so `cargo xtask setup --force` is what restores the \
                     prebuilt zip."
                );
                return Ok(());
            }
            for art in artifacts() {
                match install_status(&root, &art) {
                    Status::Cached if !force => {
                        println!("[cached] {} (pass --force to refetch)", art.name);
                    }
                    Status::NotProvided => {
                        let hint = match art.name {
                            "ffmpeg" => "brew install ffmpeg (or set FFMPEG_DIR yourself)",
                            "libclang" => {
                                "apt install libclang-dev / brew install llvm; \
                                           then export LIBCLANG_PATH accordingly"
                            }
                            _ => "",
                        };
                        println!("skipping {} on {}: {hint}", art.name, platform());
                    }
                    _ => install(&root, &art, download_config)?,
                }
            }
            println!("\nsetup complete. cargo build --features rtsp should now work.");
            Ok(())
        }
    }
}
