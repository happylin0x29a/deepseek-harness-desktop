//! DeepSeek Harness desktop shell.
//!
//! The shell opens a small loading window immediately, spawns `dsh web` as a
//! child process, and streams startup states to the window while it waits for
//! the readiness line the web runtime prints once its Loader tree settles
//! (`dsh web: http://127.0.0.1:PORT`, possibly with a LAN suffix). Once the
//! URL arrives the window grows to its full size and navigates directly onto
//! the surface — no choice step, and no IPC from the surface. If the host
//! dies without publishing a URL, the failure reason stays on the loading
//! window. On Windows the host child runs windowless (`CREATE_NO_WINDOW`), so
//! no console window appears beside the app.
//!
//! The host resolution order is: `DSH_BIN` (development override), a `dsh`
//! already on `PATH`, then a first-run bootstrap that downloads a pinned
//! Node.js runtime and installs `@deepseek-ai/dsh` from the configured
//! mirrors (domestic China mirrors by default) into the app-data runtime
//! directory, and runs the host from there.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{Emitter, LogicalSize, Manager, Size, Url, WebviewWindow};

/// Ask `CreateProcess` to attach no console window to a child.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The pinned Node.js version the bootstrap downloads when no system `node`
/// is on `PATH`. Must satisfy the `dsh` engine range (`^22.19 || >=24`).
const NODE_VERSION: &str = "22.19.0";
/// SHA-256 of `node-v22.19.0-win-x64.zip` (from the mirror's SHASUMS256.txt).
const NODE_WIN_X64_ZIP_SHA256: &str = "ea3fad0e67a991d8477d8c01344b56e69c676ccb733f065b22436994b1253f86";
/// SHA-256 of `node-v22.19.0-darwin-arm64.tar.gz` (from the mirror's SHASUMS256.txt).
const NODE_MAC_ARM64_TGZ_SHA256: &str = "c59006db713c770d6ec63ae16cb3edc11f49ee093b5c415d667bb4f436c6526d";
/// SHA-256 of `node-v22.19.0-darwin-x64.tar.gz` (from the mirror's SHASUMS256.txt).
const NODE_MAC_X64_TGZ_SHA256: &str = "3cfed4795cd97277559763c5f56e711852d2cc2420bda1cea30c8aa9ac77ce0c";
/// The exact `@deepseek-ai/dsh` version the bootstrap installs.
const DSH_VERSION: &str = "0.1.0-rc.6";
/// The installed package's launcher relative to the npm prefix.
const DSH_BIN_REL: &str = "node_modules/@deepseek-ai/dsh/lib/bin.js";
/// Default Node.js distribution mirror (npmmirror, reachable in mainland China).
const DEFAULT_NODE_MIRROR: &str = "https://npmmirror.com/mirrors/node";
/// Default npm registry mirror.
const DEFAULT_NPM_REGISTRY: &str = "https://registry.npmmirror.com";

/// How long the shell waits for the host's readiness line once it has
/// spawned. The first-run bootstrap (Node download + dsh install) is not
/// counted against this timeout.
fn ready_timeout() -> Duration {
    let millis = std::env::var("DSH_READY_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(300_000);
    Duration::from_millis(millis)
}

/// How long the first-run bootstrap may take before the shell fails it
/// visibly. Cold npm installs of the dsh tree can take minutes.
fn bootstrap_timeout() -> Duration {
    let millis = std::env::var("DSH_BOOTSTRAP_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(1_200_000);
    Duration::from_millis(millis)
}

/// The shell's shared state: the spawned host process, the current startup
/// phase, the recorded failure, the host's recent stderr (kept so the
/// loading page can read everything without racing the event stream), and
/// the in-flight runtime download progress.
struct DesktopState {
    child: Mutex<Option<Child>>,
    phase: Mutex<String>,
    error: Mutex<Option<String>>,
    stderr_tail: Mutex<String>,
    ready: Mutex<bool>,
    spawned: Mutex<bool>,
    progress: Mutex<Option<DownloadProgress>>,
    installing: Mutex<bool>,
    npm_progress: Mutex<Option<NpmProgress>>,
}

/// Bytes downloaded so far out of the expected total, while the bootstrap
/// fetches the Node runtime.
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DownloadProgress {
    done: u64,
    total: u64,
}

/// Live npm install progress: the current activity phase, bytes written into
/// the npm cache, the smoothed download speed, and the package count already
/// extracted into the prefix.
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct NpmProgress {
    phase: String,
    bytes: u64,
    speed: u64,
    packages: u64,
}

/// Killing the host on drop keeps no orphan server behind the window.
impl Drop for DesktopState {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

/// Append one line to the shell log (temp dir), with a timestamp. The log is
/// the support surface when the window itself cannot show progress.
fn log_line(line: &str) {
    let path = std::env::temp_dir().join("dsh-desktop.log");
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{millis} {line}");
    }
}

/// Redact credentials from a proxy URL (`http://user:pass@host` → `***@host`).
fn redact_proxy(value: &str) -> String {
    match value.rsplit_once('@') {
        Some((_, host)) => format!("***@{host}"),
        None => value.to_string(),
    }
}

/// Log the proxy environment variables the downloaders (ureq, npm) honor, so
/// a stalled bootstrap is diagnosable from the log alone.
fn log_proxy_environment() {
    for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy", "NO_PROXY", "no_proxy"] {
        if let Ok(value) = std::env::var(name) {
            log_line(&format!("proxy env: {name}={}", redact_proxy(&value)));
        }
    }
}

/// Where the shell log lives — shown in failure messages.
fn log_path() -> String {
    std::env::temp_dir().join("dsh-desktop.log").to_string_lossy().into_owned()
}

/// The readiness-line contract: `dsh web:` or `dsh desktop:`, then the
/// canonical URL as the first whitespace-delimited token. The optional
/// ` (LAN: …)` suffix is dropped.
fn parse_url_line(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("dsh web: ")
        .or_else(|| line.strip_prefix("dsh desktop: "))?;
    rest.split_whitespace().next().map(str::to_string)
}

/// A URL this shell will open its window on: the loopback surface only.
/// Everything else is refused before it reaches the webview.
fn is_local_web_url(url: &str) -> bool {
    url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:")
}

/// Split `DSH_BIN` into program plus leading arguments (`node <path>/bin.js`).
fn parse_dsh_bin(raw: &str) -> (String, Vec<String>) {
    let mut parts = raw.split_whitespace();
    let program = parts.next().unwrap_or("dsh").to_string();
    (program, parts.map(str::to_string).collect())
}

/// Whether a program with this name resolves on `PATH` (spawns successfully).
fn command_on_path(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// Resolve `name` on `PATH`, probing the `PATHEXT` extensions (`dsh` →
/// `dsh.exe`, `dsh.cmd`, …). Rust's `Command` does not probe extensions, so
/// npm's `.cmd` shims are invisible to it. On Windows the bare name is
/// skipped: npm also drops an extensionless POSIX shim (`dsh`) next to
/// `dsh.cmd`, and neither it nor a bare `dsh` can be spawned by `Command`.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let candidates: Vec<String> = if cfg!(windows) {
        extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| format!("{name}{extension}"))
            .collect()
    } else {
        vec![name.to_string()]
    };
    for dir in std::env::split_paths(&std::env::var("PATH").unwrap_or_default()) {
        for candidate in &candidates {
            let path = dir.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// A `dsh` usable from `PATH`: a directly spawnable program, or an npm-style
/// `.cmd` shim whose underlying `node <bin.js>` invocation the shell can run
/// directly (spawning the shim through `cmd.exe` would leave an orphaned node
/// on exit).
fn resolve_path_dsh() -> Option<(String, Vec<String>)> {
    if command_on_path("dsh") {
        return Some(("dsh".to_string(), Vec::new()));
    }
    if !cfg!(windows) {
        return None;
    }
    // npm's `dsh.cmd` shim lives in `<prefix>/node_modules/.bin` and runs
    // `node "<dp0>\..\@deepseek-ai\dsh\lib\bin.js"`; follow that pattern.
    let shim = find_on_path("dsh")?;
    log_line(&format!("path dsh shim: {}", shim.display()));
    let extension = shim.extension().and_then(|ext| ext.to_str()).map(str::to_lowercase);
    if !matches!(extension.as_deref(), Some("cmd" | "bat")) {
        log_line(&format!("path dsh shim extension rejected: {extension:?}"));
        return None;
    }
    let bin_js = shim
        .parent()?
        .join("..")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js");
    let node_available = command_on_path("node");
    log_line(&format!(
        "path dsh bin.js exists: {}, node on path: {}",
        bin_js.is_file(),
        node_available,
    ));
    if !bin_js.is_file() || !node_available {
        return None;
    }
    Some(("node".to_string(), vec![bin_js.display().to_string()]))
}

/// The app-owned runtime directory: Node and dsh land here on first run.
fn runtime_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir())
    } else if cfg!(target_os = "macos") {
        std::env::var("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/dsh-desktop"))
            .unwrap_or_else(|_| std::env::temp_dir())
    } else {
        std::env::temp_dir()
    };
    base.join("dsh-desktop").join("runtime")
}

/// The configured Node.js distribution mirror (defaults to npmmirror).
fn node_mirror() -> String {
    std::env::var("DSH_NODE_MIRROR").unwrap_or_else(|_| DEFAULT_NODE_MIRROR.to_string())
}

/// The configured npm registry mirror (defaults to npmmirror).
fn npm_registry() -> String {
    std::env::var("DSH_NPM_REGISTRY").unwrap_or_else(|_| DEFAULT_NPM_REGISTRY.to_string())
}

/// The archive file name for this platform, or an error naming the platform.
fn node_archive_file_name() -> Result<String, String> {
    let base = format!("node-v{NODE_VERSION}");
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok(format!("{base}-win-x64.zip")),
        ("macos", "aarch64") => Ok(format!("{base}-darwin-arm64.tar.gz")),
        ("macos", "x86_64") => Ok(format!("{base}-darwin-x64.tar.gz")),
        (os, arch) => Err(format!(
            "当前平台（{os}/{arch}）不支持自动下载运行时。请安装 Node.js 和 @deepseek-ai/dsh，或设置 DSH_BIN 指向已有的 dsh 启动器。"
        )),
    }
}

/// The top-level directory inside the Node archive.
fn node_archive_top_dir() -> String {
    let base = format!("node-v{NODE_VERSION}");
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => format!("{base}-win-x64"),
        ("macos", "aarch64") => format!("{base}-darwin-arm64"),
        ("macos", "x86_64") => format!("{base}-darwin-x64"),
        _ => base,
    }
}

/// The pinned archive SHA-256, or `None` when `DSH_NODE_VERSION` overrides the
/// pinned version (its checksum is not known).
fn node_archive_sha256() -> Option<&'static str> {
    if std::env::var("DSH_NODE_VERSION").is_ok() {
        return None;
    }
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some(NODE_WIN_X64_ZIP_SHA256),
        ("macos", "aarch64") => Some(NODE_MAC_ARM64_TGZ_SHA256),
        ("macos", "x86_64") => Some(NODE_MAC_X64_TGZ_SHA256),
        _ => None,
    }
}

/// The Node executable inside an extracted runtime, and the bundled npm CLI.
fn node_runtime_paths(node_dir: &Path) -> (PathBuf, PathBuf) {
    if cfg!(windows) {
        (
            node_dir.join("node.exe"),
            node_dir.join("node_modules/npm/bin/npm-cli.js"),
        )
    } else {
        (
            node_dir.join("bin/node"),
            node_dir.join("lib/node_modules/npm/bin/npm-cli.js"),
        )
    }
}

/// The archive-relative path of an entry the bootstrap keeps (the Node binary
/// and the npm CLI bundle), or `None` to skip the entry. Path separators are
/// normalized to `/` because zip/tar entry names may carry either.
fn node_extract_relative(path: &str) -> Option<String> {
    let top = node_archive_top_dir();
    let rest = path.strip_prefix(&top)?.trim_start_matches('/');
    let keep = rest == "node.exe" || rest == "bin/node"
        || rest.starts_with("node_modules/npm/")
        || rest.starts_with("lib/node_modules/npm/");
    keep.then(|| rest.to_string())
}

/// SHA-256 hex digest of a byte slice.
fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(data);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Download a URL into memory, reporting byte progress through `on_progress`
/// (called after each read chunk), and verify its SHA-256 when pinned.
fn download(
    url: &str,
    expected_sha: Option<&str>,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<Vec<u8>, String> {
    log_line(&format!("downloading {url}"));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(300))
        .build();
    let response = agent.get(url).call().map_err(|error| format!("下载失败 {url}: {error}"))?;
    let total = response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let mut body = Vec::new();
    let mut reader = response.into_reader().take(512 * 1024 * 1024);
    let mut chunk = [0u8; 256 * 1024];
    loop {
        let read = reader.read(&mut chunk).map_err(|error| format!("下载中断 {url}: {error}"))?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
        on_progress(body.len() as u64, total);
    }
    if let Some(expected) = expected_sha {
        let actual = sha256_hex(&body);
        if actual != expected {
            return Err(format!("下载校验失败 {url}: 期望 {expected}，实际 {actual}"));
        }
    }
    log_line(&format!("downloaded {url} ({} bytes)", body.len()));
    Ok(body)
}

/// Extract a downloaded Node archive into `node_dir`, atomically: unpack into
/// a sibling staging directory (entries keep their archive paths, which start
/// with the top-level directory), then rename the top-level directory into
/// place. Windows ships a zip, macOS a tar.gz.
fn extract_node(archive: &[u8], node_dir: &Path) -> Result<(), String> {
    let staging = node_dir.with_extension("staging");
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|error| format!("无法清理临时目录 {}: {error}", staging.display()))?;
    }
    fs::create_dir_all(&staging).map_err(|error| format!("无法创建临时目录 {}: {error}", staging.display()))?;
    if cfg!(windows) {
        extract_zip(archive, &staging)?;
    } else {
        extract_tar_gz(archive, &staging)?;
    }
    let extracted = staging.join(node_archive_top_dir());
    if !extracted.is_dir() {
        return Err(format!("解压产物缺少顶层目录 {}", node_archive_top_dir()));
    }
    if node_dir.exists() {
        fs::remove_dir_all(node_dir).map_err(|error| format!("无法清理 {}: {error}", node_dir.display()))?;
    }
    fs::rename(&extracted, node_dir).map_err(|error| format!("无法落位 {}: {error}", node_dir.display()))?;
    fs::remove_dir_all(&staging).map_err(|error| format!("无法清理临时目录 {}: {error}", staging.display()))
}

/// Unzip a Node archive into `staging`, keeping only the binary and the npm
/// bundle (paths keep the archive's top-level directory).
fn extract_zip(archive: &[u8], staging: &Path) -> Result<(), String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(archive)).map_err(|error| format!("读取 zip 失败: {error}"))?;
    for index in 0..zip.len() {
        let mut file = zip.by_index(index).map_err(|error| format!("读取 zip 条目失败: {error}"))?;
        if file.is_dir() {
            continue;
        }
        let Some(entry) = file.enclosed_name() else { continue };
        let name = entry.to_string_lossy().replace('\\', "/");
        if node_extract_relative(&name).is_none() {
            continue;
        }
        let dest = staging.join(&entry);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("无法创建 {}: {error}", parent.display()))?;
        }
        let mut out = fs::File::create(&dest).map_err(|error| format!("无法写入 {}: {error}", dest.display()))?;
        std::io::copy(&mut file, &mut out).map_err(|error| format!("解压 {} 失败: {error}", dest.display()))?;
    }
    Ok(())
}

/// Untar (gzip) a Node archive into `staging`, keeping only the binary and
/// the npm bundle (paths keep the archive's top-level directory).
fn extract_tar_gz(archive: &[u8], staging: &Path) -> Result<(), String> {
    let decoder = flate2::read::GzDecoder::new(Cursor::new(archive));
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries().map_err(|error| format!("读取 tar 失败: {error}"))? {
        let mut entry = entry.map_err(|error| format!("读取 tar 条目失败: {error}"))?;
        let name = entry.path().map_err(|error| format!("读取 tar 路径失败: {error}"))?
            .into_owned()
            .to_string_lossy()
            .replace('\\', "/");
        if node_extract_relative(&name).is_none() {
            continue;
        }
        entry.unpack_in(staging).map_err(|error| format!("解压 {name} 失败: {error}"))?;
    }
    Ok(())
}

/// Parse a `node --version` line (`v24.3.0`) into its major/minor pair.
fn parse_node_version(version: &str) -> Option<(u64, u64)> {
    let version = version.trim().strip_prefix('v').unwrap_or(version.trim());
    let mut parts = version.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    Some((major, minor))
}

/// Whether a node version satisfies the `dsh` engine range
/// (`^22.19.0 || >=24.0.0`): the 22 line at 22.19+, or any 24+.
fn node_engine_compliant(version: Option<(u64, u64)>) -> bool {
    match version {
        Some((22, minor)) => minor >= 19,
        Some((major, _)) => major >= 24,
        None => false,
    }
}

/// Whether the `node` on `PATH` satisfies the `dsh` engine range.
fn system_node_compliant() -> bool {
    if !command_on_path("node") {
        return false;
    }
    let Ok(output) = Command::new("node")
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    output.status.success() && node_engine_compliant(parse_node_version(&String::from_utf8_lossy(&output.stdout)))
}

/// The system npm usable for the bootstrap: `"npm"` when a directly
/// spawnable npm exists, or the `npm-cli.js` path resolved from an npm
/// `.cmd`/`.bat` shim (Rust's `Command` cannot spawn shims; running
/// `node <npm-cli.js>` is the shim's own invocation). `None` when npm is not
/// usable.
fn resolve_system_npm() -> Option<String> {
    if command_on_path("npm") {
        return Some("npm".to_string());
    }
    if !cfg!(windows) {
        return None;
    }
    let shim = find_on_path("npm")?;
    let extension = shim.extension().and_then(|ext| ext.to_str()).map(str::to_lowercase);
    if !matches!(extension.as_deref(), Some("cmd" | "bat")) {
        return None;
    }
    let npm_cli = shim.parent()?.join("node_modules").join("npm").join("bin").join("npm-cli.js");
    if !npm_cli.is_file() {
        return None;
    }
    Some(npm_cli.display().to_string())
}

/// Ensure a usable Node runtime: the system `node`+`npm` when node is on
/// `PATH` and satisfies the `dsh` engine range, otherwise the pinned mirror
/// download inside the runtime dir. Returns the node program and the npm CLI
/// to run (the system `npm` name or the downloaded npm-cli.js path).
fn ensure_node(app: &tauri::AppHandle, runtime: &Path) -> Result<(String, String), String> {
    if command_on_path("node") && system_node_compliant() {
        if let Some(npm_cli) = resolve_system_npm() {
            log_line("using system node/npm (dsh engine compliant)");
            return Ok(("node".to_string(), npm_cli));
        }
    }
    if command_on_path("node") {
        log_line("system node present but not dsh-engine compliant or npm unusable; downloading the pinned runtime");
    }
    let node_dir = runtime.join("node");
    let (node_exe, npm_cli) = node_runtime_paths(&node_dir);
    if node_exe.is_file() && npm_cli.is_file() {
        return Ok((node_exe.display().to_string(), npm_cli.display().to_string()));
    }
    let version = std::env::var("DSH_NODE_VERSION").unwrap_or_else(|_| NODE_VERSION.to_string());
    let file_name = node_archive_file_name().map_err(|message| {
        if std::env::var("DSH_NODE_VERSION").is_ok() {
            format!("{message}（DSH_NODE_VERSION={version} 未被支持）")
        } else {
            message
        }
    })?;
    let url = format!("{}/v{version}/{file_name}", node_mirror());
    set_phase(app, "正在下载 Node.js 运行时（国内镜像）…");
    let progress_app = app.clone();
    let result = download(&url, node_archive_sha256(), move |done, total| {
        if let Some(state) = progress_app.try_state::<DesktopState>() {
            let mut progress = state.progress.lock().expect("desktop: progress mutex poisoned");
            *progress = Some(DownloadProgress { done, total });
        }
    });
    if let Some(state) = app.try_state::<DesktopState>() {
        *state.progress.lock().expect("desktop: progress mutex poisoned") = None;
    }
    let archive = result?;
    set_phase(app, "正在解压 Node.js 运行时…");
    extract_node(&archive, &node_dir)?;
    Ok((node_exe.display().to_string(), npm_cli.display().to_string()))
}

/// Recursive byte size and file count of a directory tree (0/0 when absent).
fn dir_stats(dir: &Path) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                let (sub_bytes, sub_files) = dir_stats(&path);
                bytes += sub_bytes;
                files += sub_files;
            } else if let Ok(metadata) = entry.metadata() {
                bytes += metadata.len();
                files += 1;
            }
        }
    }
    (bytes, files)
}

/// Number of packages in a node_modules dir: immediate subdirectories, plus
/// the subdirectories of every `@`-scoped directory (each scope nests its
/// packages).
fn count_directories(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    let mut count = 0u64;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else { continue };
        if !file_type.is_dir() {
            continue;
        }
        count += 1;
        if entry.file_name().to_string_lossy().starts_with('@') {
            count += count_directories(&entry.path());
        }
    }
    count
}

/// The `"version": "x.y.z"` field of a package.json manifest, read without a
/// JSON parser (npm's manifest shape is fixed and small).
fn package_version(manifest: &Path) -> Option<String> {
    let text = fs::read_to_string(manifest).ok()?;
    let key = "\"version\"";
    let key_end = text.find(key)? + key.len();
    let colon = text[key_end..].find(':')? + key_end;
    let quote = text[colon..].find('"')? + colon;
    let value_start = quote + 1;
    let value_end = text[value_start..].find('"')? + value_start;
    Some(text[value_start..value_end].to_string())
}

/// Ensure `@deepseek-ai/dsh` is installed in the runtime prefix, via npm from
/// the configured registry mirror. Returns the launcher's `lib/bin.js` path.
fn ensure_dsh(
    app: &tauri::AppHandle,
    runtime: &Path,
    node: &str,
    npm_cli: &str,
) -> Result<String, String> {
    let prefix = runtime.join("dsh");
    let bin_js = prefix.join(DSH_BIN_REL);
    // Reuse a complete install: the launcher present with the pinned version
    // installed. Checking the manifest version (not a shell-written marker)
    // makes an install that finished while the shell was closed reusable.
    let manifest = prefix.join("node_modules/@deepseek-ai/dsh/package.json");
    if bin_js.is_file() && package_version(&manifest).as_deref() == Some(DSH_VERSION) {
        return Ok(bin_js.display().to_string());
    }
    if prefix.exists() {
        fs::remove_dir_all(&prefix).map_err(|error| format!("无法清理 {}: {error}", prefix.display()))?;
    }
    fs::create_dir_all(&prefix).map_err(|error| format!("无法创建 {}: {error}", prefix.display()))?;
    // A runtime-owned npm cache: re-runs reuse it, and the live progress
    // monitor watches it for download speed.
    let cache_dir = runtime.join("npm-cache");
    set_phase(app, "正在安装 dsh 运行时（国内镜像）…");
    log_line(&format!("npm install @deepseek-ai/dsh@{DSH_VERSION} (registry {})", npm_registry()));
    let mut command = if npm_cli == "npm" {
        let mut command = Command::new("npm");
        command.arg("install");
        command
    } else {
        let mut command = Command::new(node);
        command.arg(npm_cli).arg("install");
        command
    };
    command
        .arg("--prefix").arg(&prefix)
        .arg("--cache").arg(&cache_dir)
        .arg("--registry").arg(npm_registry())
        .arg("--no-audit").arg("--no-fund")
        .arg("--no-update-notifier")
        // Lifecycle scripts (koffi, node-pty, …) run through cmd.exe and would
        // flash console windows beside the app; both packages ship prebuilt
        // binaries, so skipping the scripts loses nothing.
        .arg("--ignore-scripts")
        .arg("--loglevel").arg("error")
        .arg(format!("@deepseek-ai/dsh@{DSH_VERSION}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Lifecycle scripts (koffi, node-pty, …) invoke `node` by bare name; make
    // it resolvable even when node is not on the app's PATH.
    let mut search_paths = std::env::split_paths(&std::env::var("PATH").unwrap_or_default()).collect::<Vec<_>>();
    if let Some(node_dir) = Path::new(node).parent().filter(|dir| !dir.as_os_str().is_empty()) {
        if !search_paths.iter().any(|dir| dir == node_dir) {
            search_paths.insert(0, node_dir.to_path_buf());
        }
    }
    let joined = std::env::join_paths(search_paths).unwrap_or_default();
    command.env("PATH", joined);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|error| format!("无法启动 npm: {error}"))?;
    let stdout = child.stdout.take().expect("desktop: npm stdout is not piped");
    let stderr = child.stderr.take().expect("desktop: npm stderr is not piped");
    // Register the installer as the active child so closing the window during
    // the bootstrap kills npm instead of orphaning it.
    if let Some(state) = app.try_state::<DesktopState>() {
        *state.child.lock().expect("desktop: child mutex poisoned") = Some(child);
    }
    // Live install progress: sample the npm cache once a second and publish
    // the activity phase, bytes, download speed, and package count so the
    // loading page can show what npm is doing. npm's phases look very
    // different from the cache: packument fetches add many tiny files, the
    // tarball download adds bulk bytes, and the reify phase grows
    // node_modules without touching the cache — so the phase is inferred
    // from which signal is moving.
    if let Some(state) = app.try_state::<DesktopState>() {
        *state.installing.lock().expect("desktop: installing mutex poisoned") = true;
    }
    let monitor_app = app.clone();
    let monitor_prefix = prefix.clone();
    std::thread::spawn(move || {
        let (mut last_bytes, mut last_files) = dir_stats(&cache_dir);
        let mut last_packages = 0u64;
        let mut last_time = std::time::Instant::now();
        let mut smoothed_speed = 0u64;
        let mut wait_seconds = 0u64;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let Some(state) = monitor_app.try_state::<DesktopState>() else { return };
            let installing = *state.installing.lock().expect("desktop: installing mutex poisoned");
            if !installing {
                return;
            }
            let now = std::time::Instant::now();
            let (bytes, files) = dir_stats(&cache_dir);
            let dt = now.duration_since(last_time).as_secs_f64().max(0.001);
            let instant = ((bytes as f64 - last_bytes as f64) / dt).max(0.0) as u64;
            smoothed_speed = (smoothed_speed + instant) / 2;
            let packages = count_directories(&monitor_prefix.join("node_modules"));
            let moving = packages > last_packages
                || bytes.saturating_sub(last_bytes) >= 64 * 1024
                || files > last_files;
            wait_seconds = if moving { 0 } else { wait_seconds + 1 };
            let phase = if packages > last_packages {
                "正在解压安装…"
            } else if bytes.saturating_sub(last_bytes) >= 64 * 1024 {
                "正在下载依赖包…"
            } else if files > last_files {
                "正在获取包元数据…"
            } else if wait_seconds >= 15 {
                // Proxies that cannot reach the mirror stall every request;
                // the guidance beats a silent spinner.
                "等待网络响应…（长时间无进展请检查系统代理，或将 npmmirror.com 与 registry.npmmirror.com 加入 NO_PROXY）"
            } else {
                "等待网络响应…"
            };
            let mut progress = state.npm_progress.lock().expect("desktop: npm progress mutex poisoned");
            *progress = Some(NpmProgress {
                phase: phase.to_string(),
                bytes,
                speed: smoothed_speed,
                packages,
            });
            last_bytes = bytes;
            last_files = files;
            last_packages = packages;
            last_time = now;
        }
    });
    let out_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let err_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let out_reader = out_buf.clone();
    std::thread::spawn(move || {
        let mut buf = out_reader.lock().expect("desktop: npm out mutex poisoned");
        let _ = BufReader::new(stdout).read_to_end(&mut buf);
    });
    let err_reader = err_buf.clone();
    std::thread::spawn(move || {
        let mut buf = err_reader.lock().expect("desktop: npm err mutex poisoned");
        let _ = BufReader::new(stderr).read_to_end(&mut buf);
    });
    let status = match app
        .try_state::<DesktopState>()
        .and_then(|state| state.child.lock().ok().and_then(|mut guard| guard.take()))
    {
        Some(mut child) => child.wait().map_err(|error| format!("等待 npm 失败: {error}"))?,
        None => return Err("应用已退出，安装已中止。".to_string()),
    };
    if let Some(state) = app.try_state::<DesktopState>() {
        *state.installing.lock().expect("desktop: installing mutex poisoned") = false;
        *state.npm_progress.lock().expect("desktop: npm progress mutex poisoned") = None;
    }
    let out = out_buf.lock().expect("desktop: npm out mutex poisoned").clone();
    let err = err_buf.lock().expect("desktop: npm err mutex poisoned").clone();
    let mut tail = String::new();
    for (text, label) in [(&out, "npm out"), (&err, "npm err")] {
        let text = String::from_utf8_lossy(text);
        for line in text.lines() {
            log_line(&format!("{label}: {line}"));
        }
        if !text.trim().is_empty() {
            tail.push_str(&format!("{label}: {}\n", text.trim()));
        }
    }
    if !status.success() {
        return Err(format!(
            "dsh 安装失败（exit {:?}）。{}详情见日志 {}。",
            status.code(),
            if tail.is_empty() { String::new() } else { format!("npm 输出:\n{tail}") },
            log_path(),
        ));
    }
    if !bin_js.is_file() {
        return Err(format!("dsh 安装完成但找不到 {}。详情见日志 {}。", DSH_BIN_REL, log_path()));
    }
    Ok(bin_js.display().to_string())
}

/// Resolve the host invocation as program plus leading arguments; the `web`
/// mode argument is appended by the spawner. `DSH_BIN` wins (development),
/// then a `dsh` on `PATH`, then the first-run bootstrap.
fn resolve_host_invocation(app: &tauri::AppHandle) -> Result<(String, Vec<String>), String> {
    if let Ok(raw) = std::env::var("DSH_BIN") {
        return Ok(parse_dsh_bin(&raw));
    }
    if let Some(invocation) = resolve_path_dsh() {
        return Ok(invocation);
    }
    set_phase(app, "未检测到 dsh，正在准备内置运行时…");
    log_line("dsh not found on PATH; bootstrapping the pinned runtime");
    let runtime = runtime_dir();
    let (node, npm_cli) = ensure_node(app, &runtime)?;
    let bin_js = ensure_dsh(app, &runtime, &node, &npm_cli)?;
    Ok((node, vec![bin_js]))
}

/// Record the startup failure and keep it visible: the loading window stays
/// open with the reason instead of the process dying silently.
fn fail_startup(app: &tauri::AppHandle, message: &str) {
    if let Some(state) = app.try_state::<DesktopState>() {
        *state.error.lock().expect("desktop: error mutex poisoned") = Some(message.to_string());
    }
    set_phase(app, message);
    log_line(&format!("startup failed: {message}"));
}

/// Grow the loading window to its full size and open the surface on it. The
/// window must be resizable for the grow to take effect, and the phase title
/// set during loading must not stick on the surface.
fn open_surface(window: &WebviewWindow, url: &str) {
    let parsed = Url::parse(url).expect("desktop: readiness URL is not a valid URL");
    let _ = window.set_resizable(true);
    let _ = window.set_maximizable(true);
    if window.set_size(Size::Logical(LogicalSize::new(1280.0, 800.0))).is_err()
        || window.set_min_size(Some(LogicalSize::new(960.0, 600.0))).is_err()
        || window.center().is_err()
        || window.navigate(parsed).is_err() {
        log_line("failed to grow or navigate the main window")
    }
    let _ = window.set_title("DeepSeek Harness");
}

/// Advance the startup phase: stored state, window title (visible in the
/// taskbar without any page IPC), and the loading-page event.
fn set_phase(app: &tauri::AppHandle, phase: &str) {
    log_line(&format!("phase: {phase}"));
    if let Some(state) = app.try_state::<DesktopState>() {
        *state.phase.lock().expect("desktop: phase mutex poisoned") = phase.to_string();
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_title(&format!("DeepSeek Harness — {phase}"));
    }
    let _ = app.emit("startup-state", phase);
}

/// Extra `dsh` arguments from `DSH_DESKTOP_ARGS` (whitespace-split), for
/// example `--port 8080`.
fn extra_host_args() -> Vec<String> {
    std::env::var("DSH_DESKTOP_ARGS")
        .map(|raw| raw.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Spawn the resolved host invocation (`… web …`), stream its stderr into the
/// log, and open the surface when the readiness line arrives.
fn spawn_and_stream(app: &tauri::AppHandle, program: String, leading_args: Vec<String>) {
    let log_cmd = [program.clone(), leading_args.join(" "), "web".to_string(), extra_host_args().join(" ")]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    log_line(&format!("host command: {log_cmd}"));
    set_phase(app, "正在启动宿主进程…");
    let mut command = Command::new(program);
    command
        .args(leading_args)
        .arg("web")
        .args(extra_host_args())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // The host is a console-subsystem program (node): without the flag
        // Windows would open a black console window beside the app. Its
        // stderr is piped above and lands in the shell log instead.
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            fail_startup(app, &format!(
                "启动失败:无法启动宿主命令「{log_cmd}」: {error}。请检查 DSH_BIN 是否指向有效的 dsh 启动器。",
            ));
            return;
        }
    };
    let stdout = child.stdout.take().expect("desktop: host stdout is not piped");
    let stderr = child.stderr.take().expect("desktop: host stderr is not piped");
    if let Some(state) = app.try_state::<DesktopState>() {
        *state.spawned.lock().expect("desktop: spawned mutex poisoned") = true;
        *state.child.lock().expect("desktop: child mutex poisoned") = Some(child);
    }
    set_phase(app, "正在等待服务器就绪…");
    // Keep the host's recent stderr for failure messages.
    let stderr_app = app.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { continue };
            log_line(&format!("host stderr: {line}"));
            if let Some(state) = stderr_app.try_state::<DesktopState>() {
                let mut tail = state.stderr_tail.lock().expect("desktop: stderr mutex poisoned");
                tail.push_str(&line);
                tail.push('\n');
                if tail.len() > 4000 {
                    let cut = tail.len() - 2000;
                    let drained = tail.split_off(cut);
                    *tail = drained;
                }
            }
        }
    });
    // The stdout reader: readiness line → grow onto the surface.
    let stdout_app = app.clone();
    std::thread::spawn(move || {
        let mut published = false;
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { continue };
            log_line(&format!("host stdout: {line}"));
            if let Some(url) = parse_url_line(&line) {
                if !is_local_web_url(&url) {
                    continue;
                }
                published = true;
                if let Some(state) = stdout_app.try_state::<DesktopState>() {
                    *state.ready.lock().expect("desktop: ready mutex poisoned") = true;
                }
                if let Some(window) = stdout_app.get_webview_window("main") {
                    set_phase(&stdout_app, "正在打开界面…");
                    open_surface(&window, &url);
                }
                break;
            }
        }
        if !published {
            let ready = stdout_app.try_state::<DesktopState>()
                .map(|state| *state.ready.lock().expect("desktop: ready mutex poisoned"))
                .unwrap_or(false);
            if !ready {
                let tail = stdout_app.try_state::<DesktopState>()
                    .map(|state| state.stderr_tail.lock().expect("desktop: stderr mutex poisoned").clone())
                    .unwrap_or_default();
                fail_startup(&stdout_app, &format!(
                    "启动失败:宿主进程已退出且未发布服务器地址。{}",
                    if tail.is_empty() {
                        format!("详情见日志 {}。", log_path())
                    } else {
                        format!("宿主输出:\n{tail}")
                    },
                ));
            }
        }
    });
}

/// Start the host: resolve the invocation (which may bootstrap the runtime on
/// first run) on a worker thread, then stream readiness to the window. A
/// watchdog fails the startup visibly: the bootstrap phase gets its own
/// budget, and the host phase gets `ready_timeout` once the host has spawned.
fn spawn_host(app: &tauri::AppHandle) {
    set_phase(app, "正在启动宿主进程…");
    let watchdog_app = app.clone();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let spawned_at = loop {
            let Some(state) = watchdog_app.try_state::<DesktopState>() else { return };
            let ready = *state.ready.lock().expect("desktop: ready mutex poisoned");
            if ready {
                return;
            }
            let spawned = *state.spawned.lock().expect("desktop: spawned mutex poisoned");
            if spawned {
                break std::time::Instant::now();
            }
            if started.elapsed() > bootstrap_timeout() {
                fail_startup(&watchdog_app, &format!(
                    "启动超时:{} 秒内未能完成运行时下载/安装。请检查网络（默认走国内镜像），或查看日志 {}。",
                    bootstrap_timeout().as_secs(),
                    log_path(),
                ));
                return;
            }
            std::thread::sleep(Duration::from_secs(2));
        };
        std::thread::sleep(ready_timeout());
        if let Some(state) = watchdog_app.try_state::<DesktopState>() {
            if !*state.ready.lock().expect("desktop: ready mutex poisoned") {
                fail_startup(&watchdog_app, &format!(
                    "启动超时:宿主在 {} 秒内没有就绪（自 {} 秒前开始）。原因见宿主日志,或查看 {}。",
                    ready_timeout().as_secs(),
                    spawned_at.elapsed().as_secs(),
                    log_path(),
                ));
            }
        }
    });
    let resolve_app = app.clone();
    std::thread::spawn(move || {
        match resolve_host_invocation(&resolve_app) {
            Ok((program, leading_args)) => spawn_and_stream(&resolve_app, program, leading_args),
            Err(message) => fail_startup(&resolve_app, &message),
        }
    });
}

/// The startup progress snapshot the loading page polls: current phase,
/// recorded failure, the host's recent stderr, the Node download progress,
/// and the live npm install progress.
#[tauri::command]
fn startup_status(state: tauri::State<'_, DesktopState>) -> StartupStatus {
    StartupStatus {
        phase: state.phase.lock().expect("desktop: phase mutex poisoned").clone(),
        error: state.error.lock().expect("desktop: error mutex poisoned").clone(),
        stderr_tail: state.stderr_tail.lock().expect("desktop: stderr mutex poisoned").clone(),
        log_path: log_path(),
        progress: state.progress.lock().expect("desktop: progress mutex poisoned").clone(),
        npm_progress: state.npm_progress.lock().expect("desktop: npm progress mutex poisoned").clone(),
    }
}

/// The polling contract between the shell and the loading page.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct StartupStatus {
    phase: String,
    error: Option<String>,
    stderr_tail: String,
    log_path: String,
    progress: Option<DownloadProgress>,
    npm_progress: Option<NpmProgress>,
}

/// Build and run the desktop shell.
pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            log_line(&format!("shell starting, log at {}", log_path()));
            log_proxy_environment();
            app.manage(DesktopState {
                child: Mutex::new(None),
                phase: Mutex::new("正在启动…".to_string()),
                error: Mutex::new(None),
                stderr_tail: Mutex::new(String::new()),
                ready: Mutex::new(false),
                spawned: Mutex::new(false),
                progress: Mutex::new(None),
                installing: Mutex::new(false),
                npm_progress: Mutex::new(None),
            });
            spawn_host(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![startup_status])
        .build(tauri::generate_context!())
        .expect("error while building the desktop shell");
    app.run(|handle, event| {
        if let tauri::RunEvent::Exit = event {
            // An exit that skips managed-state drop (a force-killed window)
            // must not orphan the host server behind it. Drop on DesktopState
            // remains the ordinary path; this is the explicit fallback.
            if let Some(state) = handle.try_state::<DesktopState>() {
                if let Ok(mut guard) = state.child.lock() {
                    if let Some(mut child) = guard.take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_readiness_labels_and_drops_the_lan_suffix() {
        assert_eq!(
            parse_url_line("dsh web: http://127.0.0.1:3080 (LAN: http://192.168.1.5:3080)"),
            Some("http://127.0.0.1:3080".to_string()),
        );
        assert_eq!(
            parse_url_line("dsh desktop: http://localhost:4567"),
            Some("http://localhost:4567".to_string()),
        );
    }

    #[test]
    fn ignores_lines_outside_the_readiness_contract() {
        assert_eq!(parse_url_line("listening on http://127.0.0.1:3080"), None);
        assert_eq!(parse_url_line("dsh headless: http://127.0.0.1:3080"), None);
        assert_eq!(parse_url_line(""), None);
    }

    #[test]
    fn the_local_url_guard_accepts_loopback_only() {
        assert!(is_local_web_url("http://127.0.0.1:3080"));
        assert!(is_local_web_url("http://localhost:4567"));
        assert!(!is_local_web_url("http://192.168.1.5:3080"));
        assert!(!is_local_web_url("https://example.com"));
    }

    #[test]
    fn dsh_bin_splits_program_and_leading_arguments() {
        assert_eq!(
            parse_dsh_bin("node C:\\dsh\\apps\\cli\\lib\\bin.js"),
            ("node".to_string(), vec!["C:\\dsh\\apps\\cli\\lib\\bin.js".to_string()]),
        );
        assert_eq!(parse_dsh_bin("dsh"), ("dsh".to_string(), Vec::new()));
    }

    #[test]
    fn extract_keeps_only_the_node_binary_and_npm_bundle() {
        let top = node_archive_top_dir();
        assert_eq!(
            node_extract_relative(&format!("{top}/node.exe")),
            Some("node.exe".to_string()),
        );
        assert_eq!(
            node_extract_relative(&format!("{top}/node_modules/npm/bin/npm-cli.js")),
            Some("node_modules/npm/bin/npm-cli.js".to_string()),
        );
        assert_eq!(node_extract_relative(&format!("{top}/LICENSE")), None);
        assert_eq!(node_extract_relative(&format!("{top}/node_modules/npm")), None);
        assert_eq!(node_extract_relative("other/node.exe"), None);
    }

    #[test]
    fn path_resolution_finds_npm_cmd_shims() {
        // All halves mutate PATH, so they share one test to avoid racing.
        let root = std::env::temp_dir().join(format!("dsh-path-test-{}", std::process::id()));
        let bin_dir = root.join("node_modules").join(".bin");
        let lib_dir = root.join("node_modules").join("@deepseek-ai").join("dsh").join("lib");
        let npm_dir = root.join("node_modules").join("npm").join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&lib_dir).unwrap();
        fs::create_dir_all(&npm_dir).unwrap();
        fs::write(bin_dir.join("dsh.cmd"), "@echo off\n").unwrap();
        fs::write(lib_dir.join("bin.js"), "#!/usr/bin/env node\n").unwrap();
        // The real npm.cmd lives in the Node install dir, not in
        // node_modules/.bin; mirror that layout.
        fs::write(root.join("npm.cmd"), "@echo off\n").unwrap();
        fs::write(npm_dir.join("npm-cli.js"), "#!/usr/bin/env node\n").unwrap();
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let old_pathext = std::env::var_os("PATHEXT").unwrap_or_default();
        // Keep the real node dir on the test PATH so the shim resolution is
        // exercised against a real `node`.
        let node_dir = find_on_path("node").and_then(|path| path.parent().map(|dir| dir.to_path_buf()));
        let mut test_path = format!("C:\\Windows\\System32;{};{}", bin_dir.display(), root.display());
        if let Some(dir) = &node_dir {
            test_path.push(';');
            test_path.push_str(&dir.display().to_string());
        }
        std::env::set_var("PATH", &test_path);
        std::env::set_var("PATHEXT", ".COM;.EXE;.BAT;.CMD");
        let found = find_on_path("dsh");
        let had_node = command_on_path("node");
        let resolved = if cfg!(windows) { resolve_path_dsh() } else { None };
        let system_npm = resolve_system_npm();
        std::env::set_var("PATH", old_path);
        std::env::set_var("PATHEXT", old_pathext);
        fs::remove_dir_all(&root).ok();
        // PATHEXT yields the extension in its own case (dsh.CMD); compare
        // case-insensitively.
        let found_lower = found.as_ref().map(|path| path.to_string_lossy().to_lowercase());
        assert_eq!(
            found_lower,
            Some(bin_dir.join("dsh.cmd").to_string_lossy().to_lowercase()),
            "find_on_path returned {found:?}",
        );
        if cfg!(windows) && had_node {
            let expected_bin_js = bin_dir
                .join("..")
                .join("@deepseek-ai")
                .join("dsh")
                .join("lib")
                .join("bin.js");
            assert_eq!(
                resolved,
                Some(("node".to_string(), vec![expected_bin_js.display().to_string()])),
                "resolve_path_dsh returned {resolved:?} with node_dir {node_dir:?}",
            );
            // The npm `.cmd` shim resolves to its npm-cli.js; the bare `npm`
            // name is not spawnable by `Command`.
            assert_eq!(
                system_npm,
                Some(npm_dir.join("npm-cli.js").display().to_string()),
                "resolve_system_npm returned {system_npm:?}",
            );
        }
    }

    #[test]
    fn node_engine_compliance_parses_version_lines() {
        assert!(node_engine_compliant(parse_node_version("v22.19.0")));
        assert!(node_engine_compliant(parse_node_version("v22.20.3")));
        assert!(node_engine_compliant(parse_node_version("v24.3.0")));
        assert!(node_engine_compliant(parse_node_version("v25.0.0")));
        assert!(!node_engine_compliant(parse_node_version("v18.20.0")));
        assert!(!node_engine_compliant(parse_node_version("v22.18.0")));
        assert!(!node_engine_compliant(parse_node_version("v23.0.0")));
        assert!(!node_engine_compliant(parse_node_version("garbage")));
        assert!(!node_engine_compliant(None));
    }

    #[test]
    fn proxy_redaction_keeps_host_and_drops_credentials() {
        assert_eq!(
            redact_proxy("http://user:secret@127.0.0.1:7890"),
            "***@127.0.0.1:7890",
        );
        assert_eq!(redact_proxy("http://127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(redact_proxy("socks5://a:b@proxy.example:1080"), "***@proxy.example:1080");
    }

    #[test]
    fn size_and_directory_counting_helpers() {
        let root = std::env::temp_dir().join(format!("dsh-size-test-{}", std::process::id()));
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("@scope/one")).unwrap();
        fs::create_dir_all(root.join("@scope/two")).unwrap();
        fs::write(root.join("one.bin"), vec![1u8; 100]).unwrap();
        fs::write(root.join("a/two.bin"), vec![2u8; 250]).unwrap();
        fs::write(root.join("a/b/three.bin"), vec![3u8; 40]).unwrap();
        assert_eq!(dir_stats(&root).0, 390);
        assert_eq!(count_directories(&root), 4); // a + @scope + its two packages
        assert_eq!(count_directories(&root.join("a")), 1);
        assert_eq!(count_directories(&root.join("@scope")), 2);
        assert_eq!(dir_stats(&root.join("missing")).0, 0);
        assert_eq!(count_directories(&root.join("missing")), 0);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn package_version_reads_the_manifest_version_field() {
        let root = std::env::temp_dir().join(format!("dsh-version-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let manifest = root.join("package.json");
        fs::write(&manifest, "{\n  \"name\": \"@deepseek-ai/dsh\",\n  \"version\": \"0.1.0-rc.6\"\n}\n").unwrap();
        assert_eq!(package_version(&manifest).as_deref(), Some("0.1.0-rc.6"));
        assert_eq!(package_version(&root.join("missing.json")), None);
        fs::write(&manifest, "not json\n").unwrap();
        assert_eq!(package_version(&manifest), None);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn download_reports_monotonic_progress_and_verifies_checksum() {
        use std::io::Write as _;
        use std::net::{Shutdown, TcpListener};

        let payload = vec![7u8; 4 * 1024 * 1024];
        let payload_len = payload.len() as u64;
        let expected = sha256_hex(&payload);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server_payload = payload.clone();
        let server = std::thread::spawn(move || {
            // Serve two connections (happy path, then checksum mismatch)
            // within a deadline so a failed client cannot hang the test.
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {payload_len}\r\nConnection: close\r\n\r\n",
            );
            for _ in 0..2 {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(_) if std::time::Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => return,
                    }
                };
                // Consume the request; closing a socket with unread receive
                // data sends RST on Windows and kills the client mid-body.
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(headers.as_bytes());
                for chunk in server_payload.chunks(64 * 1024) {
                    let _ = stream.write_all(chunk);
                }
                let _ = stream.shutdown(Shutdown::Write);
            }
        });

        let mut seen = Vec::new();
        let body = download(&format!("http://{addr}/node.zip"), Some(&expected), |done, total| {
            seen.push((done, total));
        })
        .unwrap();

        assert_eq!(body, payload);
        assert!(!seen.is_empty());
        assert!(seen.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        assert_eq!(seen.last(), Some(&(payload.len() as u64, payload.len() as u64)));

        let bad = sha256_hex(b"other");
        let error = download(&format!("http://{addr}/node.zip"), Some(&bad), |_, _| {}).unwrap_err();
        assert!(error.contains("校验失败"), "unexpected error: {error}");

        // The server serves exactly the two connections above; join after
        // both downloads so it cannot block on a connection that never comes.
        server.join().unwrap();
    }
}
