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
//! already on `PATH`, then a first-run bootstrap that reuses the system
//! `node`+`npm` when they satisfy the `dsh` engine range, and otherwise
//! downloads a pinned Node.js runtime and installs `@deepseek-ai/dsh` from
//! the configured mirrors (domestic China mirrors by default) into the
//! app-data runtime directory, and runs the host from there.
//!
//! `node`, `npm`, and `dsh` are looked up across [`search_dirs`] — the
//! inherited `PATH` plus the machine and user `PATH` read from the registry,
//! plus the well-known install locations. The inherited `PATH` on its own is
//! a snapshot taken when the *launcher* started, so trusting it alone made
//! the shell download a runtime on machines that already had Node.js.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
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
/// The body a dsh service returns when it is reached without its browser
/// cookie (`writeUnauthorized` in `@deepseek-ai/dsh-client-connection`). It
/// names a running dsh this shell cannot attach to, as opposed to a foreign
/// program holding the port — both must send the host to a free port, but only
/// this one is worth explaining in the log.
const AUTH_REQUIRED_MARKER: &str = "dsh web authentication required";
/// Lifetime of the cookie the shell mints to attach to a running dsh. dsh
/// signs cookies valid for at most `cookieMaxAgeDays` (30 by default), and
/// `isAuthenticated` rejects a payload whose lifetime exceeds that cap, so this
/// stays comfortably under it — the cookie only has to outlive one window.
const COOKIE_LIFETIME_MILLIS: u64 = 12 * 60 * 60 * 1000;
/// How much of a loopback response to buffer while classifying a port. dsh's
/// own page is ~28 KB; the cap only bounds a foreign server that answers
/// without ever closing.
const LOOPBACK_READ_LIMIT: usize = 256 * 1024;

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
    port_retried: Mutex<bool>,
    /// How many times the host was restarted because it died before it
    /// published an address — a transient resolution failure on the shared
    /// profile fallback. Capped by `MAX_RESOLUTION_RETRIES`.
    resolution_attempts: Mutex<u32>,
    /// Where the engine's own `@deepseek-ai/dsh` package lives, recorded when a
    /// host is resolved so the toolbar's update rewrites exactly the
    /// installation the running engine came from.
    engine_install: Mutex<Option<EngineInstall>>,
    /// The engine update the toolbar drives: idle, running, or finished.
    engine_update: Mutex<EngineUpdateState>,
}

/// The npm prefix owning one `@deepseek-ai/dsh` installation.
#[derive(Clone)]
struct EngineInstall {
    /// `<prefix>` such that `<prefix>/node_modules/@deepseek-ai/dsh` is the
    /// package. npm rewrites this tree, and its bin shims live here too when
    /// the install is global.
    prefix: PathBuf,
    /// Whether npm must treat `prefix` as a *global* prefix. The bundled
    /// runtime is a plain local prefix; anything else is the machine's global
    /// one, whose bin shims sit at the prefix root rather than in
    /// `node_modules/.bin`.
    global: bool,
    bin_js: PathBuf,
}

/// What the toolbar shows about an engine update.
#[derive(serde::Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct EngineUpdateState {
    running: bool,
    /// Human-readable phase, e.g. `正在下载依赖包…`.
    phase: String,
    /// Live npm progress while `running`.
    npm: Option<NpmProgress>,
    /// Set once a run ends: what happened, or why it failed.
    result: Option<String>,
    failed: bool,
    /// Whether the engine the shell is showing was started by the shell. Only
    /// an owned engine can be restarted here — an attached one belongs to
    /// whoever started it.
    owned: bool,
}

/// Restart budget for a host that dies before readiness. The shared profile
/// fallback is a directory of junction points that security software can hold
/// for several seconds, so one retry is not always enough to outlive it.
const MAX_RESOLUTION_RETRIES: u32 = 3;

/// How long each retry waits for the fallback to become readable before
/// spawning the host again.
const RESOLUTION_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// Backoff applied after the readability wait, for the 1st, 2nd and 3rd
/// retry. The wait is best-effort (a still-unreadable tree is retried anyway),
/// so the explicit backoff also covers a release that lands later.
const RESOLUTION_BACKOFF: [std::time::Duration; 3] = [
    std::time::Duration::from_millis(1500),
    std::time::Duration::from_millis(4000),
    std::time::Duration::from_millis(8000),
];

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

/// Apply the Windows no-console flag to a child command. Even short-lived
/// version probes can flash a console when launched from a GUI application.
fn hide_console(command: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

/// Keep the directory holding the resolved program first on a child's `PATH`.
/// `dsh` and its plugins shell out to a bare `node`, and lifecycle scripts do
/// the same, so a child started from an absolute path still needs to *find*
/// `node` by name.
fn prepend_program_dir(command: &mut Command, program: &str) {
    let Some(dir) = Path::new(program)
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
    else {
        return;
    };
    let mut search = vec![dir.to_path_buf()];
    search.extend(std::env::split_paths(&std::env::var("PATH").unwrap_or_default()));
    if let Ok(joined) = std::env::join_paths(search) {
        command.env("PATH", joined);
    }
}

/// Whether a program with this name can be spawned. The bare name is tried
/// first (it resolves through the inherited `PATH`), then the absolute path
/// from [`find_program`], so a launcher that handed the shell a stripped
/// `PATH` does not turn a present `node` into a missing one.
fn command_on_path(name: &str) -> bool {
    if probe_program(name) {
        return true;
    }
    find_program(name)
        .map(|path| probe_program(&path.to_string_lossy()))
        .unwrap_or(false)
}

/// Spawn a program once to see whether it exists and runs.
fn probe_program(program: &str) -> bool {
    let mut command = Command::new(program);
    hide_console(&mut command);
    command
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// Resolve a directly spawnable program to its absolute path. Windows only
/// needs this for `node.exe`: `CreateProcess` cannot run npm's `.cmd` shims,
/// and `node` is exactly the program the shell later re-spawns as the host,
/// so an absolute path must survive even when `PATH` did not.
fn find_program(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    for dir in search_dirs() {
        let path = dir.join(&file);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Append a directory to a search list, skipping blanks and duplicates. The
/// comparison is case-insensitive on Windows, where `D:\Node` and `d:\node`
/// are the same directory.
fn push_search_dir(dirs: &mut Vec<PathBuf>, dir: PathBuf) {
    if dir.as_os_str().is_empty() {
        return;
    }
    let duplicate = dirs.iter().any(|existing| {
        if cfg!(windows) {
            existing
                .to_string_lossy()
                .eq_ignore_ascii_case(&dir.to_string_lossy())
        } else {
            existing == &dir
        }
    });
    if !duplicate {
        dirs.push(dir);
    }
}

/// Every directory probed for `node`, `npm`, and `dsh`, in priority order:
/// the inherited `PATH`, then the machine and user `PATH` read straight from
/// the registry, then the well-known install locations.
///
/// The registry pass is what keeps a machine's real toolchain visible. The
/// inherited `PATH` is a snapshot the launcher took when *it* started —
/// Explorer hands every child the environment block captured the last time it
/// read the registry, so a directory added afterwards stays invisible to an
/// app started from a desktop or Start-menu shortcut until the user signs
/// out. Probing only the inherited list therefore reported "no Node" on a
/// machine that has one, and started the 35 MB runtime download instead of
/// reusing it.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for dir in std::env::split_paths(&std::env::var("PATH").unwrap_or_default()) {
        push_search_dir(&mut dirs, dir);
    }
    #[cfg(target_os = "windows")]
    for dir in registry_search_dirs() {
        push_search_dir(&mut dirs, dir);
    }
    for dir in well_known_dirs() {
        push_search_dir(&mut dirs, dir);
    }
    dirs
}

/// The machine and user `PATH` as the registry holds them, with
/// `%NAME%` references resolved.
#[cfg(target_os = "windows")]
fn registry_search_dirs() -> Vec<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    const MACHINE_ENV: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
    let machine = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(MACHINE_ENV).ok();
    let user = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Environment").ok();

    // `Path` is stored as REG_EXPAND_SZ (`%JAVA_HOME%\bin`, `%PNPM_HOME%`, …),
    // and the registry read returns it unexpanded. Resolve it against the
    // registry's own variables: the process environment is exactly the
    // snapshot we are working around and may not define them at all.
    let lookup = |name: &str| -> Option<String> {
        if let Ok(value) = std::env::var(name) {
            return Some(value);
        }
        for key in [machine.as_ref(), user.as_ref()].into_iter().flatten() {
            if let Ok(value) = key.get_value::<String, _>(name) {
                return Some(value);
            }
        }
        None
    };

    let mut dirs = Vec::new();
    for key in [user.as_ref(), machine.as_ref()].into_iter().flatten() {
        let Ok(raw) = key.get_value::<String, _>("Path") else {
            continue;
        };
        for entry in expand_percent_vars(&raw, &lookup).split(';') {
            if !entry.trim().is_empty() {
                dirs.push(PathBuf::from(entry.trim()));
            }
        }
    }
    dirs
}

/// Expand `%NAME%` references, leaving undefined names literal — the same
/// outcome `CreateProcess` produces.
fn expand_percent_vars(value: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut expanded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        expanded.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            expanded.push('%');
            rest = after;
            continue;
        };
        let name = &after[..end];
        match lookup(&name.to_uppercase()) {
            Some(replacement) if !name.is_empty() => expanded.push_str(&replacement),
            _ => {
                expanded.push('%');
                expanded.push_str(name);
                expanded.push('%');
            }
        }
        rest = &after[end + 1..];
    }
    expanded.push_str(rest);
    expanded
}

/// Standard install locations, probed after `PATH` so a custom install always
/// wins. Covers the official installers, nvm-windows, Volta, and pnpm.
fn well_known_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let var = |name: &str| std::env::var(name).ok().map(PathBuf::from);
    if cfg!(windows) {
        for name in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(root) = var(name) {
                dirs.push(root.join("nodejs"));
            }
        }
        if let Some(local) = var("LOCALAPPDATA") {
            dirs.push(local.join("Programs").join("nodejs"));
            dirs.push(local.join("Volta").join("bin"));
        }
        if let Some(roaming) = var("APPDATA") {
            dirs.push(roaming.join("npm"));
        }
        if let Some(pnpm) = var("PNPM_HOME") {
            dirs.push(PathBuf::from(pnpm));
        }
        if let Some(nvm_symlink) = var("NVM_SYMLINK") {
            dirs.push(PathBuf::from(nvm_symlink));
        }
        if let Some(volta) = var("VOLTA_HOME") {
            dirs.push(PathBuf::from(volta).join("bin"));
        }
    } else {
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        if let Some(home) = var("HOME") {
            dirs.push(home.join(".volta").join("bin"));
            dirs.push(home.join(".local").join("bin"));
        }
    }
    dirs
}

/// Resolve `name` across [`search_dirs`], probing the `PATHEXT` extensions
/// (`dsh` → `dsh.exe`, `dsh.cmd`, …). Rust's `Command` does not probe
/// extensions, so npm's `.cmd` shims are invisible to it. On Windows the bare
/// name is skipped: npm also drops an extensionless POSIX shim (`dsh`) next
/// to `dsh.cmd`, and neither it nor a bare `dsh` can be spawned by `Command`.
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
    for dir in search_dirs() {
        for candidate in &candidates {
            let path = dir.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// Expand `%dp0%` / `%~dp0%` in a shim line against the shim's own directory.
/// npm writes `dp0` as `%~dp0`, which carries a trailing separator; keeping it
/// is harmless, and undefined-looking `%NAME%` references are left alone.
fn expand_shim_dir(text: &str, dir: &Path) -> String {
    let lower = text.to_ascii_lowercase();
    let replacement = dir.display().to_string();
    let mut expanded = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let rest = &lower[index..];
        let matched = if rest.starts_with("%~dp0%") {
            Some(6)
        } else if rest.starts_with("%dp0%") {
            Some(5)
        } else {
            None
        };
        match matched {
            Some(len) => {
                expanded.push_str(&replacement);
                index += len;
            }
            None => {
                let ch = text[index..].chars().next().expect("desktop: shim text is not empty");
                expanded.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    expanded
}

/// The script paths an npm `.cmd`/`.bat` shim launches, read from the shim's
/// own text with `%dp0%` resolved against the shim's directory.
///
/// npm generates every shim from one template whose last line runs
/// `"%_prog%" "<script>" %*`, where `<script>` is `%dp0%` plus the target's
/// path *relative to the shim's directory*. That relative path is
/// layout-dependent:
///
/// * a **global** install keeps the shim at the npm prefix root, so it names
///   `node_modules\@deepseek-ai\dsh\lib\bin.js`;
/// * a **local** install keeps it in `node_modules\.bin`, so it names
///   `..\@deepseek-ai\dsh\lib\bin.js`.
///
/// Reading the shim instead of guessing one layout is what keeps the shell
/// coupled to the machine's own installation. Guessing the local layout made
/// the shell blind to a globally installed `dsh`; it then bootstrapped its own
/// pinned runtime, and since both installations share `$DSH_HOME`, the
/// profile's plugin fallback resolved the shell's host out of the *other*
/// installation's `node_modules` — at a different dsh version, and fatally so
/// whenever that installation was mid-reinstall.
fn shim_scripts(shim: &Path) -> Vec<PathBuf> {
    let Some(dir) = shim.parent() else { return Vec::new() };
    let Ok(text) = fs::read_to_string(shim) else {
        // A binary on `PATH` can share the name; only text shims are parsed.
        return Vec::new();
    };
    let mut scripts: Vec<PathBuf> = Vec::new();
    for token in text.split('"').flat_map(str::split_whitespace) {
        // The interpreter (`node.exe`) and `%_prog%` are quoted on the same
        // line; npm's entry point is the only `.js` reference.
        if !token.to_ascii_lowercase().ends_with(".js") {
            continue;
        }
        let expanded = expand_shim_dir(token, dir);
        let path = PathBuf::from(&expanded);
        let path = if path.is_absolute() { path } else { dir.join(path) };
        if !scripts.contains(&path) {
            scripts.push(path);
        }
    }
    scripts
}

/// Resolve an npm-shimmed launcher to a concrete script file: whatever the
/// shim itself names first, then each `fallbacks` layout in order. Every
/// fallback entry is a path relative to the shim's own directory.
fn shim_script(shim: &Path, fallbacks: &[&[&str]]) -> Option<PathBuf> {
    if let Some(found) = shim_scripts(shim).into_iter().find(|path| path.is_file()) {
        return Some(found);
    }
    let dir = shim.parent()?;
    fallbacks.iter().find_map(|parts| {
        let candidate = parts.iter().fold(dir.to_path_buf(), |path, part| path.join(part));
        candidate.is_file().then_some(candidate)
    })
}

/// The `bin.js` of the launcher a host invocation will actually run: the
/// resolved leading argument, or the `dsh` program followed to its real file
/// (a `PATH` symlink points into the same `…/@deepseek-ai/dsh/lib/bin.js`).
/// `None` when neither names that launcher.
fn launcher_bin_js(program: &str, leading_args: &[String]) -> Option<PathBuf> {
    let named = leading_args
        .first()
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| fs::canonicalize(program).ok());
    let named = named?;
    let tail: Vec<String> = named
        .components()
        .rev()
        .take(4)
        .map(|part| part.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    // `<scope>/dsh/lib/bin.js`, read backwards.
    (tail.as_slice() == ["bin.js", "lib", "dsh", "@deepseek-ai"]).then_some(named)
}

/// The `@deepseek-ai/dsh-web-app` package the launcher resolves, following
/// Node's parent-directory walk for a bare specifier: every ancestor
/// directory's `node_modules`. This covers npm's nested layout (the package
/// under `dsh/node_modules`) and a hoisted one (the package beside `dsh`).
fn resolved_web_app_dir(bin_js: &Path) -> Option<PathBuf> {
    // `<install>/node_modules/@deepseek-ai/dsh/lib/bin.js` → `…/@deepseek-ai/dsh`.
    let package_dir = bin_js.parent()?.parent()?;
    package_dir.ancestors().find_map(|dir| {
        let candidate = dir.join("node_modules").join("@deepseek-ai").join("dsh-web-app");
        candidate.is_dir().then_some(candidate)
    })
}

/// Whether the launcher's own `dsh-web-app` accepts `--no-open`.
///
/// The flag belongs to that plugin, not to the launcher: the `web` command is
/// registered when the profile's `dsh-web-app` row boots, and the profile
/// resolves its plugins out of the *launcher's* installation once its boot has
/// healed the shared fallback. So the module read here is the one that will
/// parse the command line — reading the shared fallback instead would answer
/// for whoever owns it right now, which is exactly wrong in the case that
/// matters: the bundled `0.1.0-rc.6` has no `--no-open`, so a stale fallback
/// owned by a newer installation would report support and the flag would then
/// kill the host. Asking the launcher directly means booting its whole plugin
/// tree (measured ~8 s).
///
/// Anything unreadable answers `false`: `commander` rejects an unknown option
/// and the host would die before serving anything, while a needless browser
/// tab is only a nuisance.
fn web_app_declares_no_open(bin_js: &Path) -> bool {
    let Some(package_dir) = resolved_web_app_dir(bin_js) else { return false };
    let Ok(entries) = fs::read_dir(package_dir.join("lib")) else { return false };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("js") {
            return false;
        }
        fs::read_to_string(&path).is_ok_and(|text| text.contains("--no-open"))
    })
}

/// Whether the host may be told `--no-open`, i.e. whether the launcher that is
/// about to run declares the flag.
fn host_supports_no_open(program: &str, leading_args: &[String]) -> bool {
    launcher_bin_js(program, leading_args)
        .is_some_and(|bin_js| web_app_declares_no_open(&bin_js))
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
    let shim = find_on_path("dsh")?;
    log_line(&format!("path dsh shim: {}", shim.display()));
    let extension = shim.extension().and_then(|ext| ext.to_str()).map(str::to_lowercase);
    if !matches!(extension.as_deref(), Some("cmd" | "bat")) {
        log_line(&format!("path dsh shim extension rejected: {extension:?}"));
        return None;
    }
    let bin_js = shim_script(
        &shim,
        &[
            // Global install: the shim sits at the npm prefix root.
            &["node_modules", "@deepseek-ai", "dsh", "lib", "bin.js"],
            // Local install: `node_modules/.bin` sits one level down.
            &["..", "@deepseek-ai", "dsh", "lib", "bin.js"],
        ],
    );
    let node = resolve_system_node();
    log_line(&format!(
        "path dsh bin.js: {}, node found: {}",
        bin_js
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "not found".to_string()),
        node.as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "none".to_string()),
    ));
    let bin_js = bin_js?;
    // Hand the host the absolute node path. It is spawned later, and a
    // relative `node` would be resolved against a `PATH` that either lost the
    // entry or was never complete to begin with.
    let node = node?;
    Some((node.display().to_string(), vec![bin_js.display().to_string()]))
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

/// The absolute path of the `node` the shell should use, resolved across
/// [`search_dirs`] rather than the inherited `PATH` alone.
fn resolve_system_node() -> Option<PathBuf> {
    find_program("node")
}

/// Whether that `node` satisfies the `dsh` engine range.
fn system_node_compliant(node: &Path) -> bool {
    let mut command = Command::new(node);
    hide_console(&mut command);
    let Ok(output) = command
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
    let npm_cli = shim_script(
        &shim,
        &[
            // The official Windows installer keeps npm inside the Node dir.
            &["node_modules", "npm", "bin", "npm-cli.js"],
            &["..", "node_modules", "npm", "bin", "npm-cli.js"],
        ],
    )?;
    Some(npm_cli.display().to_string())
}

/// Ensure a usable Node runtime: the system `node`+`npm` when one can be
/// resolved and satisfies the `dsh` engine range, otherwise the pinned mirror
/// download inside the runtime dir. Returns the node program and the npm CLI
/// to run (the system `npm` name or the downloaded npm-cli.js path).
///
/// A resolvable system node always wins over the download: it costs the user
/// nothing, keeps their toolchain authoritative, and works offline.
fn ensure_node(app: &tauri::AppHandle, runtime: &Path) -> Result<(String, String), String> {
    if let Some(node) = resolve_system_node() {
        if system_node_compliant(&node) {
            if let Some(npm_cli) = resolve_system_npm() {
                log_line(&format!(
                    "using system node/npm (dsh engine compliant): {}",
                    node.display()
                ));
                return Ok((node.display().to_string(), npm_cli));
            }
            log_line(&format!(
                "system node {} satisfies the dsh engine range but npm is unusable; downloading the pinned runtime",
                node.display()
            ));
        } else {
            log_line(&format!(
                "system node {} does not satisfy the dsh engine range; downloading the pinned runtime",
                node.display()
            ));
        }
    } else {
        log_line("no system node found in the inherited PATH, the registry PATH, or the well-known install locations; downloading the pinned runtime");
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

/// Return the user's Harness home, matching `@deepseek-ai/dsh-home-paths`.
/// `DSH_HOME` wins; otherwise use the native user-profile environment.
fn dsh_home_dir() -> Option<PathBuf> {
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
    } else {
        std::env::var_os("HOME")
    }?;
    let home = PathBuf::from(home);
    if let Some(raw) = std::env::var_os("DSH_HOME") {
        let raw = raw.to_string_lossy();
        if !raw.trim().is_empty() {
            let expanded = if raw == "~" {
                home.clone()
            } else if raw.starts_with("~/") || raw.starts_with("~\\") {
                home.join(&raw[2..])
            } else {
                PathBuf::from(raw.as_ref())
            };
            return Some(if expanded.is_absolute() {
                expanded
            } else {
                std::env::current_dir().ok()?.join(expanded)
            });
        }
    }
    Some(home.join(".dsh"))
}

/// Flatten the v1 credentials wrapper (`version: 1` + `refs:`) used by older
/// dsh releases into the current strict mapping format. This intentionally
/// accepts only the known legacy shape; unknown documents are left untouched
/// so a malformed credentials file still produces the host's precise error.
fn flatten_legacy_credentials(text: &str) -> Option<String> {
    let mut saw_version = false;
    let mut saw_refs = false;
    let mut in_refs = false;
    let mut entry_indent = None;
    let mut entries = Vec::new();

    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !in_refs {
            if trimmed == "version: 1" {
                saw_version = true;
                continue;
            }
            if trimmed == "refs:" {
                saw_refs = true;
                in_refs = true;
                continue;
            }
            return None;
        }

        // Legacy refs are an indented mapping. Require every entry to use the
        // same indentation so nested/multiline YAML is left untouched rather
        // than being flattened incorrectly.
        let indent = raw_line.len() - raw_line.trim_start().len();
        if indent == 0 || raw_line[..indent].contains('\t') {
            return None;
        }
        match entry_indent {
            Some(expected) if indent != expected => return None,
            None => entry_indent = Some(indent),
            _ => {}
        }
        let (key, value) = trimmed.split_once(':')?;
        let key = key.trim();
        if key.is_empty()
            || !key
                .bytes()
                .enumerate()
                .all(|(index, byte)| byte == b'_' || byte.is_ascii_alphanumeric() && (index > 0 || byte.is_ascii_alphabetic() || byte == b'_'))
        {
            return None;
        }
        let value = value.trim_start();
        if value.is_empty() {
            return None;
        }
        entries.push(format!("{key}: {value}"));
    }

    if !saw_version || !saw_refs {
        return None;
    }
    Some(if entries.is_empty() {
        String::new()
    } else {
        format!("{}\n", entries.join("\n"))
    })
}

/// Migrate a legacy credentials document before the host's Loader tree reads
/// it. The original is copied beside the document for recovery, and the
/// migration is logged without ever logging credential values.
fn migrate_legacy_credentials_file(path: &Path) -> Result<bool, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("无法读取凭据文件 {}: {error}", path.display())),
    };
    let Some(migrated) = flatten_legacy_credentials(&text) else {
        return Ok(false);
    };

    let backup = path.with_file_name(format!(
        "{}.legacy-v1",
        path.file_name().and_then(|name| name.to_str()).unwrap_or(".credentials.yaml")
    ));
    if !backup.exists() {
        fs::copy(path, &backup).map_err(|error| {
            format!("无法备份旧凭据文件 {}: {error}", backup.display())
        })?;
    }
    let temp = path.with_file_name(format!(
        ".{}.migrating-{}",
        path.file_name().and_then(|name| name.to_str()).unwrap_or("credentials.yaml"),
        std::process::id()
    ));
    fs::write(&temp, migrated)
        .map_err(|error| format!("无法写入迁移中的凭据文件 {}: {error}", temp.display()))?;
    // `credentials-local` requires owner-only permissions on POSIX. A plain
    // `fs::write` follows the process umask (often 0644), so tighten the
    // staging file before it is atomically moved into the live location.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&temp)
            .map_err(|error| format!("无法检查迁移中的凭据文件 {}: {error}", temp.display()))?
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&temp, permissions).map_err(|error| {
            let _ = fs::remove_file(&temp);
            format!("无法保护迁移中的凭据文件 {}: {error}", temp.display())
        })?;
    }
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).map_err(|error| {
            let _ = fs::remove_file(&temp);
            format!("无法替换旧凭据文件 {}: {error}", path.display())
        })?;
    }
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(format!("无法完成凭据文件迁移 {}: {error}", path.display()));
    }
    log_line(&format!(
        "migrated legacy credentials format: {} (backup: {})",
        path.display(),
        backup.display()
    ));
    Ok(true)
}

/// Repair known credentials formats before spawning either PATH or bundled dsh.
fn repair_legacy_credentials() -> Result<(), String> {
    let Some(home) = dsh_home_dir() else { return Ok(()) };
    let path = home.join(".credentials.yaml");
    let _ = migrate_legacy_credentials_file(&path)?;
    Ok(())
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
    prepend_program_dir(&mut command, node);
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

/// Compare two paths for ownership purposes: separators unified, the NT
/// `\\?\` prefix stripped, case folded (Windows paths are case-insensitive).
fn normalized_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    let text = text.strip_prefix("\\\\?\\").unwrap_or(&text).to_string();
    text.trim_end_matches('\\').to_ascii_lowercase()
}

/// Remove a symlink/junction without ever touching its target. A Windows
/// junction is a reparse-point *directory*, so it needs `remove_dir`; a file
/// symlink needs `remove_file`.
fn remove_link(path: &Path) -> std::io::Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(directory_error) => fs::remove_file(path).map_err(|_| directory_error),
    }
}

/// Whether a fallback link's target belongs to a different installation than
/// `owned` (already normalised): only absolute targets are judged, because
/// relative ones are npm `.bin` shims inside a profile's own `node_modules`.
fn is_foreign_link(target: &Path, owned: &str) -> bool {
    target.is_absolute() && !normalized_path(target).starts_with(owned)
}

/// Strip *unserviceable* foreign links out of the shared flat module fallback
/// `$DSH_HOME/profiles/node_modules`.
///
/// dsh resolves every in-box plugin from that directory, not from the
/// installation's own `node_modules`: `boot()` anchors the Loader at the
/// profile directory and leaves `bareModuleBaseUrl` unset (only
/// `profile-boot` calls `boot()`, and the CLI exposes no flag for it), so
/// Node's parent-directory walk is the only way a bare specifier such as
/// `@deepseek-ai/dsh-llm` can be found. The fallback is *shared*:
/// `healProfilesModuleFallback` rebuilds one symlink per package in the
/// dependency closure of **whichever installation is booting**, and leaves
/// every other installation's links in place. dsh documents what happens to
/// those leftovers: "a stale link to a vanished package stays until its name
/// is reused (dangling links are invisible to resolution)". While the owner
/// is merely being reinstalled or rebuilt its links dangle for the duration,
/// and every plugin that resolves through the fallback then fails as
/// `Cannot find package` — which reads like an incomplete npm install while
/// not a single dependency is missing.
///
/// Only *unserviceable* foreign links are released: a link whose target is
/// gone, or whose target exists but no longer yields a readable
/// `package.json` (security software holding the reparse point, or npm
/// mid-reify having created the directory ahead of its files). A foreign link
/// that still resolves is left alone: another installation may be using it,
/// and dsh re-points every name it owns on its next boot anyway. Removing a
/// link never touches its target. Real directories are left alone too — dsh
/// refuses to manage over one, so that decision stays with the operator.
///
/// Ownership is derived from the running `bin.js`
/// (`<install>/node_modules/@deepseek-ai/dsh/lib/bin.js`), so this works for
/// the bundled runtime and for a `PATH`/`DSH_BIN` launcher alike.
/// @returns how many unserviceable foreign links were removed.
fn repair_profile_module_fallback(bin_js: &Path) -> usize {
    let Some(owned) = bin_js.ancestors().nth(4) else { return 0 };
    if owned.file_name() != Some(std::ffi::OsStr::new("node_modules")) {
        return 0;
    }
    let Some(home) = dsh_home_dir() else { return 0 };
    let farm = home.join("profiles").join("node_modules");
    if !farm.is_dir() {
        return 0;
    }
    let owned = normalized_path(owned);
    let mut removed = 0;
    let mut links = Vec::new();
    for entry in fs::read_dir(&farm).into_iter().flatten().flatten() {
        let path = entry.path();
        // A `@scope` directory holds the real links; everything else at the
        // top level is a link itself.
        if path.is_dir() && !path.is_symlink() && entry.file_name().to_string_lossy().starts_with('@') {
            for inner in fs::read_dir(&path).into_iter().flatten().flatten() {
                links.push(inner.path());
            }
        } else {
            links.push(path);
        }
    }
    for link in links {
        let Ok(target) = fs::read_link(&link) else { continue };
        if !is_foreign_link(&target, &owned) {
            continue;
        }
        let Some(reason) = unserviceable_reason(&link, &target) else { continue };
        match remove_link(&link) {
            Ok(()) => {
                removed += 1;
                log_line(&format!(
                    "profile fallback: released {} ({reason}; target {})",
                    link.display(),
                    target.display(),
                ));
            }
            Err(error) => log_line(&format!(
                "profile fallback: could not release {}: {error}",
                link.display(),
            )),
        }
    }
    removed
}

/// Why a link in the shared fallback cannot serve its package, or `None` when
/// it still can and must be left alone.
///
/// "Dangling" is only the loud half of the failure. A junction whose target
/// exists but answers nothing — a reparse point security software is holding,
/// or a directory npm has created ahead of the files it reifies into it —
/// resolves no package for any installation while looking perfectly healthy to
/// `exists()`. Read one manifest *through* the link, the same way Node would,
/// and release it when that fails: the link is worthless to its owner too, and
/// dsh rebuilds every name it owns on the next boot.
fn unserviceable_reason(link: &Path, target: &Path) -> Option<&'static str> {
    if !target.exists() {
        return Some("its target is gone");
    }
    if fs::read(link.join("package.json")).is_err() {
        return Some("its manifest is unreadable");
    }
    None
}

/// Whether Node would be able to resolve the profile's in-box plugins right
/// now. The fallback is a directory of links, so "unreadable" is the failure
/// that matters: security software can hold that tree — junction reparse
/// points in particular, which is also why deleting one takes the better part
/// of a second here — long enough for every plugin to look missing, which
/// surfaces as one `Cannot find package` per entry even though the install is
/// untouched. Reading one manifest *through* the farm exercises the same link
/// resolution Node performs, so this is a faithful, read-only probe.
fn profile_fallback_readable() -> bool {
    let Some(home) = dsh_home_dir() else { return true };
    let scope = home.join("profiles").join("node_modules").join("@deepseek-ai");
    let Ok(entries) = fs::read_dir(&scope) else { return false };
    // A handful of manifests is enough: a locked tree fails on the first.
    for (checked, entry) in entries.flatten().enumerate() {
        if fs::read(entry.path().join("package.json")).is_ok() {
            return true;
        }
        if checked >= 8 {
            break;
        }
    }
    false
}

/// Wait for the profile fallback to become readable, polling at a fixed
/// interval, and report whether it did before the budget elapsed.
fn wait_for_profile_fallback(budget: std::time::Duration) -> bool {
    let started = std::time::Instant::now();
    loop {
        if profile_fallback_readable() {
            return true;
        }
        if started.elapsed() >= budget {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Ask dsh to prepare the selected profile before starting the long-lived
/// server. The launcher heals `$DSH_HOME/profiles/node_modules` here; doing it
/// as a separate, hidden process avoids a first-boot race where Loader starts
/// resolving profile imports before that fallback is visible to Node. That
/// probe only serialises *our* two processes, so the fallback is also cleared
/// of links written by a different dsh installation first.
fn prepare_host_profile(
    app: &tauri::AppHandle,
    program: &str,
    leading_args: &[String],
) -> Result<(), String> {
    if let Some(bin_js) = leading_args.first() {
        let released = repair_profile_module_fallback(Path::new(bin_js));
        if released > 0 {
            log_line(&format!(
                "profile fallback: released {released} unserviceable link(s) left by another dsh installation",
            ));
        }
    }
    // Recorded so a later failure can be told apart from a locked tree.
    log_line(&format!(
        "profile fallback readable before prepare: {}",
        profile_fallback_readable()
    ));
    set_phase(app, "正在准备 dsh Profile 依赖…");
    let mut command = Command::new(program);
    hide_console(&mut command);
    prepend_program_dir(&mut command, program);
    command
        .args(leading_args)
        .arg("web")
        .arg("--dump-default-config")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let output = command
        .output()
        .map_err(|error| format!("无法准备 dsh Profile：{error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if detail.is_empty() {
        format!("dsh Profile 准备失败（exit {:?}）。", output.status.code())
    } else {
        format!(
            "dsh Profile 准备失败（exit {:?}）：{}",
            output.status.code(),
            detail
        )
    })
}

/// Resolve the host invocation as program plus leading arguments; the `web`
/// mode argument is appended by the spawner. `DSH_BIN` wins (development),
/// then a `dsh` on `PATH`, then the first-run bootstrap.
fn resolve_host_invocation(app: &tauri::AppHandle) -> Result<(String, Vec<String>), String> {
    // Older dsh releases wrote a wrapped credentials document. Repair it
    // before any host invocation (including DSH_BIN/PATH overrides) so the
    // bundled and system launchers observe the same compatible format.
    set_phase(app, "正在检查本地凭据配置…");
    repair_legacy_credentials()?;
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

/// The fingerprint of a dsh web response: the injected boot manifest that
/// only the dsh host serves.
fn looks_like_dsh(response_head: &str) -> bool {
    response_head.contains("__DSH_BOOT__")
}

/// What already answers on the loopback port the host would bind. The cases
/// need different reactions, and collapsing them into a boolean is what left
/// the shell sending a host into a port it could never have.
#[derive(Debug, PartialEq, Eq)]
enum PortOwner {
    /// Nothing is listening: the host may bind the port.
    Free,
    /// A dsh service that serves the shell without a token — reuse it, because
    /// a second host would die on EADDRINUSE.
    Reusable,
    /// A dsh service behind its browser-session token. Attaching needs a
    /// cookie, and a cookie can be minted from the secret both processes share
    /// through `$DSH_HOME`.
    Protected,
    /// Anything else: another program, or a dsh speaking a protocol this shell
    /// does not know. Spawning here cannot succeed, and dsh says so silently —
    /// a host that loses the bind prints nothing at all and never becomes
    /// ready — so the caller must move to a free port instead.
    Taken,
}

/// One loopback `GET /`. `Err` means nothing accepted the connection at all,
/// which is what separates a free port from an occupied one; `Ok` carries the
/// response read until the peer closes, bounded by [`LOOPBACK_READ_LIMIT`].
///
/// The response must be read past its headers: dsh's boot manifest sits at the
/// end of a ~28 KB document, so a single 4 KiB read lands a few dozen bytes
/// short and reports a perfectly good dsh service as "not dsh".
fn loopback_get(port: u16, cookie: Option<&str>) -> Result<String, ()> {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(2),
    )
    .map_err(|_| ())?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    // The authority the service signs cookies against is the `Host` header, so
    // it has to carry the port the browser would send.
    let cookie = cookie.map(|value| format!("Cookie: {value}\r\n")).unwrap_or_default();
    let request =
        format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{cookie}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).map_err(|_| ())?;
    let mut response: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            // `Connection: close`: the peer closing is the end of the response.
            Ok(0) => break,
            Ok(read) => {
                response.extend_from_slice(&buf[..read]);
                if response.len() >= LOOPBACK_READ_LIMIT {
                    break;
                }
            }
            // A read timeout or reset still leaves a usable prefix.
            Err(_) => break,
        }
    }
    Ok(String::from_utf8_lossy(&response).into_owned())
}

/// Classify the port by connecting and reading the response head.
fn probe_port(port: u16) -> PortOwner {
    let Ok(head) = loopback_get(port, None) else {
        return PortOwner::Free;
    };
    if looks_like_dsh(&head) {
        return PortOwner::Reusable;
    }
    if head.contains(AUTH_REQUIRED_MARKER) {
        return PortOwner::Protected;
    }
    PortOwner::Taken
}

/// base64url without padding, the only encoding dsh's cookie layer writes.
fn encode_base64_url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn decode_base64_url(value: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value).ok()
}

/// The cookie name `cookieName` derives: `dsh-auth-` plus the base64url SHA-256
/// of the request authority (`127.0.0.1:3080`, port included).
fn browser_cookie_name(authority: &str) -> String {
    use sha2::{Digest as _, Sha256};
    format!("dsh-auth-{}", encode_base64_url(&Sha256::digest(authority.as_bytes())))
}

/// The `v1.<body>.<signature>` value `encodeCookie` produces. The signature
/// covers the base64url *body string*, not the JSON behind it.
fn browser_cookie_value(
    authority: &str,
    secret: &[u8],
    issued_at: u64,
    expires_at: u64,
) -> String {
    use hmac::{Hmac, Mac as _};
    use sha2::Sha256;
    let body = encode_base64_url(
        format!(
            "{{\"version\":1,\"authority\":\"{authority}\",\"issuedAt\":{issued_at},\"expiresAt\":{expires_at}}}"
        )
        .as_bytes(),
    );
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret).expect("desktop: HMAC accepts any key length");
    mac.update(body.as_bytes());
    format!("v1.{body}.{}", encode_base64_url(&mac.finalize().into_bytes()))
}

/// Pull `records["client-connection/browser-session"].payload.secret` out of
/// the credentials document.
///
/// The shell reads one value from a file it does not own, so the block is
/// walked by indentation rather than by pulling in a YAML parser. Every
/// failure answers `None`, which degrades to "spawn our own host" — never to a
/// window stuck on an authentication page.
fn credentials_record_secret(text: &str) -> Option<Vec<u8>> {
    const RECORD: &str = "client-connection/browser-session:";
    let mut record_indent: Option<usize> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match record_indent {
            None => {
                if trimmed.starts_with(RECORD) {
                    record_indent = Some(indent);
                }
            }
            Some(record) => {
                // The record's block ended without carrying a secret.
                if indent <= record {
                    return None;
                }
                if let Some(value) = trimmed.strip_prefix("secret:") {
                    return decode_base64_url(value.trim().trim_matches(['"', '\'']));
                }
            }
        }
    }
    None
}

/// The Harness home's browser-session signing secret.
///
/// This is what lets a second process authenticate to a `dsh web` the user
/// started themselves: the launch token in that service's printed URL is
/// random per process and never persisted, while every cookie it hands out is
/// signed with this one secret, which `initializeSecret` stores in the
/// credentials document and every activation of the same home therefore
/// shares.
fn browser_session_secret() -> Option<Vec<u8>> {
    let home = dsh_home_dir()?;
    let text = fs::read_to_string(home.join(".credentials.yaml")).ok()?;
    let secret = credentials_record_secret(&text)?;
    // `canonicalSecret` accepts exactly 32 bytes; anything else is a document
    // this shell does not understand.
    (secret.len() == 32).then_some(secret)
}

/// Attach the window to a running dsh that sits behind its browser-session
/// token, by minting the cookie that service's own browser holds.
///
/// The desktop shell and a `dsh web` the user started are meant to be one
/// engine over one Harness home. Two engines both list every session, but only
/// one can hold a session's write handle, so opening a session that is live in
/// the other one fails with `SessionAlreadyOwnedError` — attaching removes the
/// second engine instead of explaining the error.
///
/// The cookie is proven over HTTP *before* the window navigates, so a rejected
/// guess leaves an ordinary "spawn a host" startup behind rather than an
/// authentication page in the desktop window.
fn attach_browser_session(app: &tauri::AppHandle, port: u16) -> bool {
    let Some(secret) = browser_session_secret() else {
        log_line("no browser-session secret is stored; starting a host instead of attaching");
        return false;
    };
    let authority = format!("127.0.0.1:{port}");
    let name = browser_cookie_name(&authority);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let value = browser_cookie_value(&authority, &secret, now, now + COOKIE_LIFETIME_MILLIS);
    log_line(&format!(
        "attach diagnostics: authority={authority} cookie={name} secret={} issuedAt={now}",
        sha256_hex(&secret)
    ));
    // Prove the cookie before the window navigates: a rejected guess must leave
    // an ordinary "spawn a host" startup behind, never an authentication page.
    let accepted = match loopback_get(port, Some(&format!("{name}={value}"))) {
        Err(()) => {
            log_line(&format!("attach probe to port {port}: nothing accepted the connection"));
            false
        }
        Ok(head) => {
            let accepted = looks_like_dsh(&head);
            if !accepted {
                log_line(&format!(
                    "attach probe to port {port}: {} bytes back, authentication still required = {}",
                    head.len(),
                    head.contains(AUTH_REQUIRED_MARKER),
                ));
            }
            accepted
        }
    };
    if !accepted {
        log_line("the minted browser-session cookie was rejected; starting a host instead");
        return false;
    }
    let Some(window) = app.get_webview_window("main") else { return false };
    // Never logged: the value is a live credential for the running service.
    let cookie = tauri::webview::Cookie::build((name, value))
        .domain("127.0.0.1")
        .path("/")
        .build();
    if let Err(error) = window.set_cookie(cookie) {
        log_line(&format!("could not store the browser-session cookie: {error}"));
        return false;
    }
    set_phase(app, "检测到正在运行的 dsh 服务，正在连接…");
    log_line(&format!("attached to the running dsh on port {port}"));
    // The attached engine's launcher was never resolved — only its port was
    // found. The `dsh` on `PATH` is what a user starts by hand, so that is the
    // installation its update targets.
    if let Some(install) = resolve_path_dsh()
        .and_then(|(_, leading)| leading.first().map(PathBuf::from))
        .and_then(|bin_js| engine_install_of(&bin_js))
    {
        if let Some(state) = app.try_state::<DesktopState>() {
            *state.engine_install.lock().expect("desktop: install mutex poisoned") = Some(install);
        }
    }
    if let Some(state) = app.try_state::<DesktopState>() {
        *state.ready.lock().expect("desktop: ready mutex poisoned") = true;
    }
    // The attached service is not owned: nothing is registered as the child, so
    // closing the window leaves it running.
    open_surface(&window, &format!("http://127.0.0.1:{port}"));
    true
}

/// The port `dsh web` will listen on: `--port` from `DSH_DESKTOP_ARGS`, else
/// the dsh default (3080).
fn configured_port() -> u16 {
    let args = std::env::var("DSH_DESKTOP_ARGS").unwrap_or_default();
    let parts: Vec<&str> = args.split_whitespace().collect();
    if let Some(position) = parts.iter().position(|part| *part == "--port") {
        if let Some(raw) = parts.get(position + 1) {
            if let Ok(port) = raw.parse::<u16>() {
                return port;
            }
        }
    }
    3080
}

/// Extra `dsh` arguments from `DSH_DESKTOP_ARGS` (whitespace-split), plus a
/// random-port override when the host retries after EADDRINUSE.
fn extra_host_args(retry_port: bool) -> Vec<String> {
    let mut args: Vec<String> = std::env::var("DSH_DESKTOP_ARGS")
        .map(|raw| raw.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    if retry_port {
        args.extend(["--port".to_string(), "0".to_string()]);
    }
    args
}

/// Every argument that follows the `web` mode word. The desktop shell *is* the
/// surface, so a host able to hand the URL to the system browser is told not
/// to — otherwise every launch also opens a browser tab beside the window.
/// The flag only goes to a launcher that declares it: `commander` rejects an
/// unknown option, and the bundled `0.1.0-rc.6` has no `--no-open` (it never
/// opens a browser in the first place).
fn web_mode_args(program: &str, leading_args: &[String], retry_port: bool) -> Vec<String> {
    let mut args = Vec::new();
    if host_supports_no_open(program, leading_args) {
        args.push("--no-open".to_string());
    }
    args.extend(extra_host_args(retry_port));
    args
}

/// Spawn the resolved host invocation (`… web …`), stream its stderr into the
/// log, and open the surface when the readiness line arrives. With
/// `retry_port` the host is started on a random port (after an EADDRINUSE).
fn spawn_and_stream(app: &tauri::AppHandle, program: String, leading_args: Vec<String>, retry_port: bool) {
    // Record where this engine's `@deepseek-ai/dsh` lives, so the toolbar's
    // update rewrites the installation actually in use rather than a guess.
    if let Some(install) = leading_args
        .first()
        .and_then(|bin_js| engine_install_of(Path::new(bin_js)))
    {
        if let Some(state) = app.try_state::<DesktopState>() {
            *state.engine_install.lock().expect("desktop: install mutex poisoned") = Some(install);
        }
    }
    let mode_args = web_mode_args(&program, &leading_args, retry_port);
    let log_cmd = [program.clone(), leading_args.join(" "), "web".to_string(), mode_args.join(" ")]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    log_line(&format!("host command: {log_cmd}"));
    set_phase(app, "正在启动宿主进程…");
    let mut command = Command::new(&program);
    prepend_program_dir(&mut command, &program);
    command
        .args(leading_args)
        .arg("web")
        .args(mode_args)
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
                // Another instance grabbed the port between the reuse probe
                // and the bind: restart the host on a random port instead of
                // failing the startup.
                let already_retried = stdout_app.try_state::<DesktopState>()
                    .map(|state| *state.port_retried.lock().expect("desktop: port_retried mutex poisoned"))
                    .unwrap_or(true);
                if !already_retried && tail.contains("EADDRINUSE") {
                    if let Some(state) = stdout_app.try_state::<DesktopState>() {
                        *state.port_retried.lock().expect("desktop: port_retried mutex poisoned") = true;
                        *state.stderr_tail.lock().expect("desktop: stderr mutex poisoned") = String::new();
                    }
                    set_phase(&stdout_app, "端口被占用，正在改用随机端口…");
                    log_line("EADDRINUSE; retrying host with --port 0");
                    let retry_app = stdout_app.clone();
                    std::thread::spawn(move || {
                        match resolve_host_invocation(&retry_app) {
                            Ok((program, leading_args)) => {
                                if let Err(message) =
                                    prepare_host_profile(&retry_app, &program, &leading_args)
                                {
                                    fail_startup(&retry_app, &message);
                                } else {
                                    spawn_and_stream(&retry_app, program, leading_args, true);
                                }
                            }
                            Err(message) => fail_startup(&retry_app, &message),
                        }
                    });
                    return;
                }
                // A host that dies on the shared profile fallback reports one
                // `Cannot find package` per plugin entry; a host killed
                // mid-boot (its fallback held by something else) can report
                // nothing at all. Both are transient — the usual trigger is
                // security software holding the junction farm for a few
                // seconds — so wait for the tree to answer again and retry a
                // few times before reporting the failure.
                let attempts = stdout_app.try_state::<DesktopState>()
                    .map(|state| *state.resolution_attempts.lock().expect("desktop: resolution_attempts mutex poisoned"))
                    .unwrap_or(MAX_RESOLUTION_RETRIES);
                let resolution_failed = tail.contains("Cannot find package")
                    || tail.contains("ERR_MODULE_NOT_FOUND")
                    || tail.trim().is_empty();
                if attempts < MAX_RESOLUTION_RETRIES && resolution_failed {
                    let attempt = attempts + 1;
                    if let Some(state) = stdout_app.try_state::<DesktopState>() {
                        *state.resolution_attempts.lock().expect("desktop: resolution_attempts mutex poisoned") = attempt;
                        *state.stderr_tail.lock().expect("desktop: stderr mutex poisoned") = String::new();
                    }
                    set_phase(&stdout_app, &format!(
                        "依赖解析失败，正在等待文件可访问后重试（第 {attempt}/{MAX_RESOLUTION_RETRIES} 次）…"
                    ));
                    log_line(&format!(
                        "host exited before readiness; waiting for the profile module fallback and retrying (attempt {attempt}/{MAX_RESOLUTION_RETRIES})"
                    ));
                    let retry_app = stdout_app.clone();
                    std::thread::spawn(move || {
                        if !wait_for_profile_fallback(RESOLUTION_WAIT) {
                            log_line("profile module fallback still unreadable after waiting; retrying anyway");
                        }
                        std::thread::sleep(RESOLUTION_BACKOFF[attempt as usize - 1]);
                        match resolve_host_invocation(&retry_app) {
                            Ok((program, leading_args)) => {
                                if let Err(message) =
                                    prepare_host_profile(&retry_app, &program, &leading_args)
                                {
                                    fail_startup(&retry_app, &message);
                                } else {
                                    spawn_and_stream(&retry_app, program, leading_args, true);
                                }
                            }
                            Err(message) => fail_startup(&retry_app, &message),
                        }
                    });
                    return;
                }
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

/// Start the host: reuse an already-running dsh service on the configured
/// port when one answers, otherwise resolve the invocation (which may
/// bootstrap the runtime on first run) on a worker thread and stream
/// readiness to the window. A watchdog fails the startup visibly: the
/// bootstrap phase gets its own budget, and the host phase gets
/// `ready_timeout` once the host has spawned.
fn spawn_host(app: &tauri::AppHandle) {
    set_phase(app, "正在启动宿主进程…");
    // Reuse instead of duplicate: a dsh service already answering on the
    // configured port without a token is the same surface, and spawning a
    // second host would die on EADDRINUSE. The reused service is not owned, so
    // closing the window must not kill it.
    let port = configured_port();
    let retry_port = match probe_port(port) {
        PortOwner::Reusable => {
            set_phase(app, "检测到正在运行的 dsh 服务，正在连接…");
            log_line(&format!("reusing existing dsh service on port {port}"));
            if let Some(state) = app.try_state::<DesktopState>() {
                *state.ready.lock().expect("desktop: ready mutex poisoned") = true;
            }
            if let Some(window) = app.get_webview_window("main") {
                open_surface(&window, &format!("http://127.0.0.1:{port}"));
            }
            return;
        }
        PortOwner::Free => false,
        // dsh, but behind its browser-session token. Attaching keeps the
        // desktop window and the user's own `dsh web` on one engine — and one
        // engine is what keeps a live session's single write handle out of the
        // way.
        PortOwner::Protected => {
            if attach_browser_session(app, port) {
                return;
            }
            log_line(&format!(
                "port {port} serves a token-protected dsh that could not be attached to; starting a host on a free port instead"
            ));
            set_phase(app, &format!("端口 {port} 已被占用，正在改用空闲端口…"));
            if let Some(state) = app.try_state::<DesktopState>() {
                *state.port_retried.lock().expect("desktop: port_retried mutex poisoned") = true;
            }
            true
        }
        // A host sent here is doomed and says nothing about it, so the shell
        // would sit on an indefinite "waiting for the server" screen until the
        // readiness timeout. Take a free port up front instead of after a
        // bind failure — and record the retry, because a host already started
        // with `--port 0` cannot hit EADDRINUSE.
        PortOwner::Taken => {
            log_line(&format!(
                "port {port} is taken; starting the host on a free port instead"
            ));
            set_phase(app, &format!("端口 {port} 已被占用，正在改用空闲端口…"));
            if let Some(state) = app.try_state::<DesktopState>() {
                *state.port_retried.lock().expect("desktop: port_retried mutex poisoned") = true;
            }
            true
        }
    };
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
            Ok((program, leading_args)) => {
                if let Err(message) =
                    prepare_host_profile(&resolve_app, &program, &leading_args)
                {
                    fail_startup(&resolve_app, &message);
                } else {
                    spawn_and_stream(&resolve_app, program, leading_args, retry_port);
                }
            }
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
/// Compare two `major.minor.patch[-prerelease]` versions the way semver does.
fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let split = |value: &str| {
        let (core, pre) = value.split_once('-').unwrap_or((value, ""));
        let numbers: Vec<u64> = core.split('.').map(|part| part.parse().unwrap_or(0)).collect();
        (numbers, pre.to_string())
    };
    let (left_numbers, left_pre) = split(left);
    let (right_numbers, right_pre) = split(right);
    for index in 0..left_numbers.len().max(right_numbers.len()) {
        let order = left_numbers
            .get(index)
            .copied()
            .unwrap_or(0)
            .cmp(&right_numbers.get(index).copied().unwrap_or(0));
        if order != Ordering::Equal {
            return order;
        }
    }
    // A prerelease ranks below the release it precedes.
    match (left_pre.is_empty(), right_pre.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => compare_prerelease(&left_pre, &right_pre),
    }
}

/// Prerelease ordering: dot-separated identifiers, numeric ones compared
/// numerically and ranking below alphanumeric ones, and fewer identifiers
/// losing when every shared one is equal.
fn compare_prerelease(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let left: Vec<&str> = left.split('.').collect();
    let right: Vec<&str> = right.split('.').collect();
    for index in 0..left.len().max(right.len()) {
        match (left.get(index), right.get(index)) {
            (Some(a), Some(b)) => {
                let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                    (Ok(a), Ok(b)) => a.cmp(&b),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => a.cmp(b),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => break,
        }
    }
    Ordering::Equal
}

/// The newest version dsh publishes to a release channel.
///
/// `@latest` is *not* the answer for this package: it is kept at an older RC
/// than `next` (measured `latest = 0.1.5-rc.1`, `next = 0.1.5-rc.2`), so
/// installing `@latest` would silently downgrade a current engine. Both
/// channels are read and the higher wins, with the remaining tags as a
/// fallback for a package that publishes only one.
fn newest_engine_version(node: &str, npm_cli: &str) -> Result<String, String> {
    let mut command = if npm_cli == "npm" {
        let mut command = Command::new("npm");
        command.arg("view");
        command
    } else {
        let mut command = Command::new(node);
        command.arg(npm_cli).arg("view");
        command
    };
    command
        .arg("@deepseek-ai/dsh")
        .arg("dist-tags")
        .arg("--json")
        .arg("--registry").arg(npm_registry())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console(&mut command);
    prepend_program_dir(&mut command, node);
    let output = command
        .output()
        .map_err(|error| format!("无法查询最新版本: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "查询最新版本失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let tags: std::collections::BTreeMap<String, String> = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("无法解析版本信息: {error}"))?;
    ["latest", "next"]
        .iter()
        .filter_map(|tag| tags.get(*tag))
        .max_by(|left, right| compare_versions(left, right))
        .or_else(|| tags.values().max_by(|left, right| compare_versions(left, right)))
        .cloned()
        .ok_or_else(|| "镜像没有返回任何版本信息。".to_string())
}

/// The npm prefix owning `bin_js`, when it has the dsh launcher's shape
/// (`<prefix>/node_modules/@deepseek-ai/dsh/lib/bin.js`).
fn engine_install_of(bin_js: &Path) -> Option<EngineInstall> {
    let modules = bin_js.ancestors().nth(4)?;
    if modules.file_name() != Some(std::ffi::OsStr::new("node_modules")) {
        return None;
    }
    let prefix = bin_js.ancestors().nth(5)?.to_path_buf();
    // The bundled runtime is a plain local prefix; anything else is the
    // machine's global one, whose bin shims live at the prefix root.
    let global = normalized_path(&prefix) != normalized_path(&runtime_dir().join("dsh"));
    Some(EngineInstall { prefix, global, bin_js: bin_js.to_path_buf() })
}

/// The installation the toolbar's update should rewrite: whatever the host was
/// resolved from, else the `dsh` on `PATH`, else the bundled runtime.
///
/// An *attached* service is the one case this cannot know for certain — the
/// shell never resolved its launcher, only found its port. The `dsh` on `PATH`
/// is the installation a user starts by hand, so that is the answer here.
fn current_engine_install(state: &DesktopState) -> Option<EngineInstall> {
    if let Some(install) = state.engine_install.lock().ok().and_then(|guard| guard.clone()) {
        return Some(install);
    }
    if let Some((_, leading)) = resolve_path_dsh() {
        if let Some(install) = leading.first().and_then(|bin_js| engine_install_of(Path::new(bin_js))) {
            return Some(install);
        }
    }
    engine_install_of(&runtime_dir().join("dsh").join(DSH_BIN_REL))
}

/// The `version` field of the engine's own manifest.
fn engine_version(install: &EngineInstall) -> String {
    package_version(&install.prefix.join("node_modules/@deepseek-ai/dsh/package.json"))
        .unwrap_or_default()
}

/// What the toolbar polls.
#[derive(serde::Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct EngineStatus {
    /// The installed version at the update target, empty when unknown.
    version: String,
    /// The prefix npm rewrites, for the toolbar's tooltip.
    prefix: String,
    running: bool,
    phase: String,
    npm: Option<NpmProgress>,
    result: Option<String>,
    failed: bool,
    /// Whether the shell started the engine it is showing. Only then can it
    /// restart it; an attached engine belongs to whoever started it.
    owned: bool,
}

/// The toolbar's view of the engine: its version, where it lives, and how the
/// last update run went.
#[tauri::command]
fn engine_status(state: tauri::State<'_, DesktopState>) -> EngineStatus {
    let install = current_engine_install(&state);
    let update = state
        .engine_update
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    let owned = state.spawned.lock().map(|guard| *guard).unwrap_or(false);
    EngineStatus {
        version: install.as_ref().map(engine_version).unwrap_or_default(),
        prefix: install
            .as_ref()
            .map(|install| install.prefix.display().to_string())
            .unwrap_or_default(),
        running: update.running,
        phase: update.phase,
        npm: update.npm,
        result: update.result,
        failed: update.failed,
        owned,
    }
}

/// The toolbar announces itself. The injected script runs inside a page the
/// *engine* serves, so this log line is the only proof from outside that the
/// injection happened and that the IPC bridge reached it.
#[tauri::command]
fn toolbar_ready(version: String) {
    log_line(&format!("engine toolbar ready on the engine page (dsh {version})"));
}

/// The window is undecorated, so its chrome is drawn inside the page and driven
/// through these. They exist instead of granting the page `core:window:*`:
/// adding an app command to the manifest is a narrower grant than handing a
/// remote origin the window plugin's whole permission surface.
fn window_error(error: tauri::Error) -> String {
    error.to_string()
}

#[tauri::command]
fn window_drag(window: WebviewWindow) -> Result<(), String> {
    window.start_dragging().map_err(window_error)
}

#[tauri::command]
fn window_minimize(window: WebviewWindow) -> Result<(), String> {
    window.minimize().map_err(window_error)
}

#[tauri::command]
fn window_toggle_maximize(window: WebviewWindow) -> Result<(), String> {
    if window.is_maximized().unwrap_or(false) {
        window.unmaximize().map_err(window_error)
    } else {
        window.maximize().map_err(window_error)
    }
}

/// Closing goes through the same path as the native button: the shell hides to
/// the tray and only the tray's quit item ends the process.
#[tauri::command]
fn window_close(window: WebviewWindow) -> Result<(), String> {
    window.close().map_err(window_error)
}

/// Replace a field of the update view, ignoring a poisoned mutex.
fn set_engine_update(state: &DesktopState, edit: impl FnOnce(&mut EngineUpdateState)) {
    if let Ok(mut guard) = state.engine_update.lock() {
        edit(&mut guard);
    }
}

/// What one update run did. The difference matters twice over: an unchanged
/// installation must not bounce a running engine, and it must not tell the user
/// to restart one either.
enum UpdateOutcome {
    /// The newest published version was already installed; nothing was written.
    Current(String),
    /// npm replaced the installation with a newer version.
    Updated(String),
}

/// Install the newest `@deepseek-ai/dsh` into the installation the engine is
/// running from, reporting npm's progress into the toolbar's state.
fn perform_engine_update(app: &tauri::AppHandle) -> Result<UpdateOutcome, String> {
    let state = app.state::<DesktopState>();
    let install = current_engine_install(&state)
        .ok_or_else(|| "找不到 dsh 的安装位置，无法更新。".to_string())?;
    let before = engine_version(&install);
    let node = resolve_system_node().ok_or_else(|| "未找到 node，无法运行 npm 更新。".to_string())?;
    let node = node.display().to_string();
    let npm_cli = resolve_system_npm().ok_or_else(|| "未找到可用的 npm，无法更新引擎。".to_string())?;

    set_engine_update(&state, |update| update.phase = "正在检查最新版本…".to_string());
    let newest = newest_engine_version(&node, &npm_cli)?;
    if compare_versions(&newest, &before) != std::cmp::Ordering::Greater {
        // Nothing to do, and nothing to touch: a matching or older channel tag
        // must never rewrite a working installation.
        return Ok(UpdateOutcome::Current(format!("已是最新版本（dsh {before}）。")));
    }

    let cache_dir = runtime_dir().join("npm-cache");
    let modules_dir = install.prefix.join("node_modules");
    log_line(&format!(
        "engine update: npm install{} --prefix {} @deepseek-ai/dsh@{newest} (launcher {}, registry {})",
        if install.global { " -g" } else { "" },
        install.prefix.display(),
        install.bin_js.display(),
        npm_registry(),
    ));
    set_engine_update(&state, |update| update.phase = format!("正在更新到 dsh {newest}…"));

    let mut command = if npm_cli == "npm" {
        let mut command = Command::new("npm");
        command.arg("install");
        command
    } else {
        let mut command = Command::new(&node);
        command.arg(&npm_cli).arg("install");
        command
    };
    if install.global {
        command.arg("-g");
    }
    command
        .arg("--prefix").arg(&install.prefix)
        .arg("--cache").arg(&cache_dir)
        .arg("--registry").arg(npm_registry())
        .arg("--no-audit").arg("--no-fund")
        .arg("--no-update-notifier")
        .arg("--ignore-scripts")
        .arg("--loglevel").arg("error")
        .arg(format!("@deepseek-ai/dsh@{newest}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    prepend_program_dir(&mut command, &node);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|error| format!("无法启动 npm: {error}"))?;
    let stdout = child.stdout.take().expect("desktop: npm stdout is not piped");
    let stderr = child.stderr.take().expect("desktop: npm stderr is not piped");
    if let Some(state) = app.try_state::<DesktopState>() {
        *state.child.lock().expect("desktop: child mutex poisoned") = Some(child);
    }

    // npm reports no percentage, so the phase is inferred from which signal is
    // moving — the same readout the first-run installer shows.
    let monitor_app = app.clone();
    let monitor_modules = modules_dir.clone();
    let monitor = std::thread::spawn(move || {
        let (mut last_bytes, mut last_files) = dir_stats(&cache_dir);
        let mut last_packages = count_directories(&monitor_modules);
        let mut last_time = std::time::Instant::now();
        let mut smoothed = 0u64;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let Some(state) = monitor_app.try_state::<DesktopState>() else { return };
            let running = state
                .engine_update
                .lock()
                .map(|guard| guard.running)
                .unwrap_or(false);
            if !running {
                return;
            }
            let now = std::time::Instant::now();
            let (bytes, files) = dir_stats(&cache_dir);
            let dt = now.duration_since(last_time).as_secs_f64().max(0.001);
            let instant = ((bytes as f64 - last_bytes as f64) / dt).max(0.0) as u64;
            smoothed = (smoothed + instant) / 2;
            let packages = count_directories(&monitor_modules);
            let phase = if packages < last_packages {
                "正在校验已安装的文件…"
            } else if packages > last_packages {
                "正在解压安装…"
            } else if bytes.saturating_sub(last_bytes) >= 64 * 1024 {
                "正在下载依赖包…"
            } else if files > last_files {
                "正在获取包元数据…"
            } else {
                "正在等待网络响应…"
            };
            if let Ok(mut guard) = monitor_app.state::<DesktopState>().engine_update.lock() {
                guard.phase = phase.to_string();
                guard.npm = Some(NpmProgress {
                    phase: phase.to_string(),
                    bytes,
                    speed: smoothed,
                    packages,
                });
            }
            last_bytes = bytes;
            last_files = files;
            last_packages = packages;
            last_time = now;
        }
    });

    let out_buf = std::sync::Arc::new(Mutex::new(Vec::new()));
    let err_buf = std::sync::Arc::new(Mutex::new(Vec::new()));
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
        None => return Err("应用已退出，更新已中止。".to_string()),
    };
    let _ = monitor.join();

    let out = out_buf.lock().expect("desktop: npm out mutex poisoned").clone();
    let err = err_buf.lock().expect("desktop: npm err mutex poisoned").clone();
    let mut tail = String::new();
    for (text, label) in [(&out, "npm out"), (&err, "npm err")] {
        let text = String::from_utf8_lossy(text);
        for line in text.lines() {
            log_line(&format!("engine update {label}: {line}"));
        }
        if !text.trim().is_empty() {
            tail.push_str(&format!("{label}: {}\n", text.trim()));
        }
    }
    if !status.success() {
        return Err(format!(
            "更新失败（npm exit {:?}）。{}详情见日志 {}。",
            status.code(),
            if tail.is_empty() { String::new() } else { format!("npm 输出:\n{tail}") },
            log_path(),
        ));
    }
    let after = engine_version(&install);
    if after == before {
        // npm ran but the installation already carried this version.
        return Ok(UpdateOutcome::Current(format!("已是最新版本（dsh {after}）。")));
    }
    if after.is_empty() {
        return Err(format!("npm 退出成功，但 {} 读不到版本。", DSH_BIN_REL));
    }
    Ok(UpdateOutcome::Updated(format!("引擎已更新：dsh {before} → {after}。")))
}

/// One update run: install, then restart the engine the shell owns.
fn run_engine_update(app: tauri::AppHandle) {
    let outcome = perform_engine_update(&app);
    let Some(state) = app.try_state::<DesktopState>() else { return };
    let owned = state.spawned.lock().map(|guard| *guard).unwrap_or(false);
    // Completion is owned *here*, not by the install path: an "already current"
    // run returns before npm ever starts, so clearing `running` only on the npm
    // path left the toolbar rendering its last phase and the button disabled
    // forever. Whatever the outcome, this is the one place that ends the run.
    set_engine_update(&state, |update| {
        update.running = false;
        update.npm = None;
    });
    match outcome {
        // Nothing was written, so there is nothing to restart and nothing to
        // tell the user to restart.
        Ok(UpdateOutcome::Current(message)) => {
            log_line(&format!("engine update finished: {message}"));
            set_engine_update(&state, |update| {
                update.failed = false;
                update.result = Some(message);
                update.owned = owned;
            });
        }
        Ok(UpdateOutcome::Updated(message)) => {
            log_line(&format!("engine update finished: {message}"));
            set_engine_update(&state, |update| {
                update.failed = false;
                update.result = Some(if owned {
                    format!("{message}正在重启引擎…")
                } else {
                    format!("{message}引擎由外部进程启动，请重启它以生效。")
                });
                update.owned = owned;
            });
            if owned {
                restart_owned_host(&app);
            }
        }
        Err(message) => {
            log_line(&format!("engine update failed: {message}"));
            set_engine_update(&state, |update| {
                update.failed = true;
                update.result = Some(message);
                update.owned = owned;
            });
        }
    }
}

/// Start an engine update in the background. Refuses a second concurrent run.
#[tauri::command]
fn engine_update(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<DesktopState>();
    {
        let mut update = state
            .engine_update
            .lock()
            .map_err(|_| "引擎更新状态不可用。".to_string())?;
        if update.running {
            return Err("更新已经在进行中。".to_string());
        }
        update.running = true;
        update.failed = false;
        update.result = None;
        update.npm = None;
        update.phase = "正在准备更新…".to_string();
    }
    log_line("engine update requested from the toolbar");
    std::thread::spawn(move || run_engine_update(app));
    Ok(())
}

/// Restart the host the shell started, so the freshly installed version is what
/// runs. An attached service is deliberately left alone: it is not the shell's
/// process to stop, and its owner restarts it.
fn restart_owned_host(app: &tauri::AppHandle) {
    let Some(state) = app.try_state::<DesktopState>() else { return };
    if !state.spawned.lock().map(|guard| *guard).unwrap_or(false) {
        return;
    }
    if let Ok(mut guard) = state.child.lock() {
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    if let Ok(mut ready) = state.ready.lock() {
        *ready = false;
    }
    if let Ok(mut spawned) = state.spawned.lock() {
        *spawned = false;
    }
    if let Ok(mut attempts) = state.resolution_attempts.lock() {
        *attempts = 0;
    }
    if let Ok(mut retried) = state.port_retried.lock() {
        *retried = false;
    }
    // Give the port back before respawning, so the fresh host takes the same
    // one instead of the shell attaching to the corpse it just killed.
    let port = configured_port();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if matches!(probe_port(port), PortOwner::Free) {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    log_line("restarting the engine after the update");
    spawn_host(app);
}

/// The window chrome and the engine button, drawn into whichever page the
/// window is showing.
///
/// The window is **undecorated** (`decorations: false`): Windows draws nothing
/// into a native caption that a webview can host, and neither Tauri nor wry
/// exposes the WebView2 title-bar overlay, so the only way to put a control at
/// the window's top-left — beside the app icon — is to draw the whole caption.
/// This bar is that caption: icon and the engine button on the left, the
/// minimize / maximize / close buttons on the right, drag on the strip and
/// double-click to maximize, matching what the native bar did.
///
/// It rides on both pages. The splash needs it too, or an undecorated window
/// could not be moved or closed while it loads. `window.__dshEnginePage`, set
/// by the injector, says whether the engine half applies.
///
/// The window commands are the shell's own — see `window_drag` and friends —
/// so the remote engine page never needs a `core:window:*` permission.
///
/// Every failure inside this script is contained: if the IPC bridge is missing
/// the popover says so and the page keeps working, because the shell never
/// depends on the page it decorates.
const ENGINE_TOOLBAR_SCRIPT: &str = r##"
(() => {
  const isEnginePage = window.__dshEnginePage === true;
  const invoke = (cmd, args) => {
    const core = window.__TAURI__ && window.__TAURI__.core;
    if (core && core.invoke) return core.invoke(cmd, args);
    const internals = window.__TAURI_INTERNALS__;
    if (internals && internals.invoke) return internals.invoke(cmd, args);
    return Promise.reject(new Error('桌面外壳的 IPC 通道不可用'));
  };
  if (window.__dshChrome) { window.__dshChrome.poll(); return; }

  const HEIGHT = 32;
  const style = document.createElement('style');
  style.textContent = [
    'html{height:100%;}',
    'body{height:calc(100% - ' + HEIGHT + 'px) !important;margin-top:' + HEIGHT + 'px !important;}',
    '#dsh-titlebar{position:fixed;top:0;left:0;right:0;height:' + HEIGHT + 'px;display:flex;align-items:center;',
    'z-index:2147483647;user-select:none;background:#1f2430;color:#e8eaf0;border-bottom:1px solid #2c3342;',
    'font:12px/1 -apple-system,BlinkMacSystemFont,"Segoe UI","Microsoft YaHei",sans-serif;}',
    '#dsh-titlebar .dsh-logo{width:16px;height:16px;margin:0 8px 0 10px;flex:none;}',
    '#dsh-titlebar .dsh-caption{opacity:.8;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;}',
    '#dsh-titlebar .dsh-grow{flex:1;min-width:12px;align-self:stretch;}',
    '#dsh-titlebar button{-webkit-appearance:none;appearance:none;font:inherit;border:0;background:transparent;',
    'color:inherit;cursor:default;}',
    '#dsh-engine-btn{display:inline-flex;align-items:center;gap:5px;flex:none;height:22px;margin-right:8px;',
    'padding:0 9px;border:1px solid rgba(232,234,240,.45) !important;border-radius:6px;cursor:pointer !important;',
    'opacity:.9;}',
    '#dsh-engine-btn:hover{background:rgba(255,255,255,.12) !important;opacity:1;}',
    '#dsh-engine-btn:disabled{opacity:.5;}',
    '#dsh-engine-btn .dsh-spin{display:inline-block;width:10px;height:10px;border:2px solid currentColor;',
    'border-top-color:transparent;border-radius:50%;animation:dsh-spin .8s linear infinite;}',
    '@keyframes dsh-spin{to{transform:rotate(360deg)}}',
    '#dsh-titlebar .dsh-sys{width:46px;height:' + HEIGHT + 'px;display:flex;align-items:center;justify-content:center;',
    'font-family:"Segoe Fluent Icons","Segoe MDL2 Assets",sans-serif;font-size:10px;cursor:default !important;}',
    '#dsh-titlebar .dsh-sys:hover{background:rgba(255,255,255,.12) !important;}',
    '#dsh-titlebar .dsh-sys.dsh-close:hover{background:#c42b1c !important;}',
    '#dsh-engine-pop{position:fixed;z-index:2147483647;max-width:min(340px,52vw);padding:7px 10px;box-sizing:border-box;',
    'font:12px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI","Microsoft YaHei",sans-serif;color:#e8eaf0;',
    'background:#1f2430;border:1px solid #2c3342;border-radius:8px;box-shadow:0 8px 24px rgba(0,0,0,.34);',
    'white-space:pre-wrap;}',
    '#dsh-engine-pop[hidden]{display:none;}',
    '#dsh-engine-pop .dsh-ok{color:#6ee7a8;}',
    '#dsh-engine-pop .dsh-bad{color:#ff9a9a;}'
  ].join('');
  document.documentElement.appendChild(style);

  const bar = document.createElement('div');
  bar.id = 'dsh-titlebar';
  const logo = document.createElement('img');
  logo.className = 'dsh-logo';
  logo.alt = '';
  logo.src = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(
    '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="7" fill="#111"/>'
    + '<path d="M6 19c2.4-4.2 5.6-6.3 9.6-6.3 2.6 0 4.6.9 6.1 2.6l4.3-2.2-1.9 4.4c.3 1.2.4 2.4.3 3.5H6z" fill="#fff"/>'
    + '<circle cx="12.4" cy="16.2" r="1.3" fill="#111"/></svg>');
  if (isEnginePage) {
    const button = document.createElement('button');
    button.id = 'dsh-engine-btn';
    button.type = 'button';
    button.textContent = '更新引擎';
    bar.append(logo, button);
  } else {
    bar.appendChild(logo);
  }
  const caption = document.createElement('span');
  caption.className = 'dsh-caption';
  caption.textContent = 'DeepSeek Harness';
  const grow = document.createElement('span');
  grow.className = 'dsh-grow';
  bar.append(caption, grow);

  const sysButton = (glyph, label, className) => {
    const element = document.createElement('button');
    element.className = 'dsh-sys' + (className === undefined ? '' : ' ' + className);
    element.type = 'button';
    element.textContent = glyph;
    element.title = label;
    element.setAttribute('aria-label', label);
    return element;
  };
  const minimize = sysButton('\uE921', '最小化');
  const maximize = sysButton('\uE922', '最大化');
  const close = sysButton('\uE8BB', '关闭', 'dsh-close');
  bar.append(minimize, maximize, close);
  document.body.appendChild(bar);

  const pop = document.createElement('div');
  pop.id = 'dsh-engine-pop';
  pop.hidden = true;
  pop.addEventListener('click', () => { pop.hidden = true; });
  document.body.appendChild(pop);

  const placePopup = () => {
    pop.style.left = '10px';
    pop.style.top = HEIGHT + 6 + 'px';
  };

  minimize.addEventListener('click', () => { invoke('window_minimize').catch(() => {}); });
  close.addEventListener('click', () => { invoke('window_close').catch(() => {}); });
  maximize.addEventListener('click', () => {
    invoke('window_toggle_maximize').catch(() => {}).then(() => {});
  });
  bar.addEventListener('mousedown', (event) => {
    if (event.button !== 0 || event.target.closest('button') !== null) return;
    invoke('window_drag').catch(() => {});
  });
  bar.addEventListener('dblclick', (event) => {
    if (event.target.closest('button') !== null) return;
    invoke('window_toggle_maximize').catch(() => {});
  });

  const engineButton = document.getElementById('dsh-engine-btn');
  const formatSpeed = (bytesPerSecond) => bytesPerSecond >= 1048576
    ? (bytesPerSecond / 1048576).toFixed(1) + ' MB/s'
    : Math.max(0, Math.round(bytesPerSecond / 1024)) + ' KB/s';

  const readout = (status) => {
    const parts = [status.phase];
    const npm = status.npm;
    if (npm) {
      if (npm.phase.indexOf('下载依赖包') >= 0) {
        parts.push('已下载 ' + Math.round(npm.bytes / 1048576) + ' MB');
        parts.push('网速 ' + formatSpeed(npm.speed));
      }
      if (npm.phase.indexOf('解压安装') >= 0) parts.push('已安装 ' + npm.packages + ' 个包');
    }
    return parts.join(' · ');
  };

  let announced = false;
  async function poll() {
    if (!isEnginePage || engineButton === null) return;
    try {
      const status = await invoke('engine_status');
      engineButton.disabled = !!status.running;
      bar.title = (status.version ? 'dsh ' + status.version : 'dsh')
        + (status.prefix ? '\n安装位置：' + status.prefix : '');
      engineButton.textContent = '';
      if (status.running) {
        const spinner = document.createElement('span');
        spinner.className = 'dsh-spin';
        engineButton.appendChild(spinner);
        engineButton.appendChild(document.createTextNode('更新中'));
        placePopup();
        pop.className = '';
        pop.textContent = readout(status);
        pop.hidden = false;
      } else {
        engineButton.appendChild(document.createTextNode('更新引擎'));
        caption.textContent = status.version ? 'DeepSeek Harness · dsh ' + status.version : 'DeepSeek Harness';
        if (status.result) {
          placePopup();
          pop.className = status.failed ? 'dsh-bad' : 'dsh-ok';
          pop.textContent = status.result + '\n（点击关闭）';
          pop.hidden = false;
        }
      }
      if (!announced) {
        announced = true;
        invoke('toolbar_ready', { version: status.version || 'unknown' }).catch(() => {});
      }
    } catch (error) {
      placePopup();
      pop.className = 'dsh-bad';
      pop.textContent = '无法读取引擎状态：' + String(error);
      pop.hidden = false;
    }
    timer = setTimeout(poll, 1200);
  }

  let timer = null;
  if (engineButton !== null) {
    engineButton.addEventListener('click', async () => {
      engineButton.disabled = true;
      placePopup();
      pop.className = '';
      pop.textContent = '正在启动更新…';
      pop.hidden = false;
      try {
        await invoke('engine_update');
      } catch (error) {
        engineButton.disabled = false;
        pop.className = 'dsh-bad';
        pop.textContent = String(error);
      }
    });
  }

  window.__dshChrome = { poll: () => { if (timer !== null) clearTimeout(timer); poll(); } };
  poll();
})();
"##;

/// Tray menu ids.
const TRAY_SHOW: &str = "tray-show";
const TRAY_QUIT: &str = "tray-quit";

/// Bring the shell's window back from the tray.
fn show_main_window(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("main") else { return };
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
}

/// Give the shell a tray icon, so hiding the window does not hide the process.
///
/// The window is the *surface*, not the application. Once the shell has spawned
/// a host, ending the process ends the engine serving the window — and if the
/// user is mid-conversation in a browser tab on that engine, that tab dies with
/// it. So the close button only hides the shell; the engine keeps running
/// behind the tray, and the tray's quit item is the single deliberate exit —
/// the path where [`tauri::RunEvent::Exit`] hands the owned host to its kill.
///
/// A service the shell only *attached to* is not owned and is left running
/// either way, which is why quitting from the tray is safe there too.
fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, TRAY_SHOW, "显示窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, TRAY_QUIT, "退出（同时关闭引擎）", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("DeepSeek Harness")
        .menu(&menu)
        // Windows convention: left click returns to the window, right click
        // opens the menu that holds the quit item.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            TRAY_SHOW => show_main_window(app),
            TRAY_QUIT => {
                log_line("tray: quit requested; stopping the owned host");
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

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
                port_retried: Mutex::new(false),
                resolution_attempts: Mutex::new(0),
                engine_install: Mutex::new(None),
                engine_update: Mutex::new(EngineUpdateState::default()),
            });
            spawn_host(app.handle());
            if let Err(error) = build_tray(app) {
                // Without a tray there is no way back from a hidden window, so
                // say so loudly and keep the close button meaning "exit".
                log_line(&format!("tray unavailable ({error}); the close button will end the shell"));
            }
            Ok(())
        })
        // Draw the window chrome on every page the window shows. The window is
        // undecorated, so the splash needs it too — otherwise it could neither
        // be moved nor closed while it loads. Only the engine's own page gets
        // the engine half of the bar, and that page is a *remote* origin to
        // Tauri, so `capabilities/engine-surface.json` is what lets its IPC
        // through.
        .on_page_load(|webview, payload| {
            if !matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                return;
            }
            let engine_page = is_local_web_url(payload.url().as_str());
            let script =
                format!("window.__dshEnginePage = {engine_page};\n{ENGINE_TOOLBAR_SCRIPT}");
            if let Err(error) = webview.eval(script) {
                log_line(&format!("could not inject the window chrome: {error}"));
            }
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // The window is the surface, not the application: hiding keeps
                // a host this shell spawned — and any browser tab pointed at
                // it — alive. Ending the process is the tray's quit item.
                api.prevent_close();
                let _ = window.hide();
                log_line("window closed to the tray; the shell is still running");
            }
        })
        .invoke_handler(tauri::generate_handler![
            startup_status,
            engine_status,
            engine_update,
            toolbar_ready,
            window_drag,
            window_minimize,
            window_toggle_maximize,
            window_close
        ])
        .build(tauri::generate_context!())
        .expect("error while building the desktop shell");
    app.run(|handle, event| match event {
        // Hiding the window can leave the platform thinking every window is
        // gone, which asks for an exit with no code. That is not the user
        // quitting — keep serving from the tray. Only a programmatic exit
        // (the tray item) carries a code, and that one is allowed through. A
        // shell whose window no longer exists has nothing left to show, so it
        // is allowed through as well rather than lingering as a zombie.
        tauri::RunEvent::ExitRequested { api, code, .. } => {
            if code.is_none() && handle.get_webview_window("main").is_some() {
                api.prevent_exit();
            }
        }
        tauri::RunEvent::Exit => {
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
        _ => {}
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
        std::env::set_var("PATH", &old_path);
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
            let (program, args) = resolved
                .unwrap_or_else(|| panic!("resolve_path_dsh returned None with node_dir {node_dir:?}"));
            // The host is spawned later, so the launcher must hand it an
            // absolute `node`: a bare name would be resolved against a `PATH`
            // that a stripped launcher could have left without node.
            assert!(
                Path::new(&program).is_absolute(),
                "resolve_path_dsh returned a non-absolute node program: {program}",
            );
            assert_eq!(
                Path::new(&program)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_lowercase()),
                Some(if cfg!(windows) { "node.exe".into() } else { "node".into() }),
                "resolve_path_dsh returned {program}",
            );
            assert_eq!(args, vec![expected_bin_js.display().to_string()]);
            // The npm `.cmd` shim resolves to its npm-cli.js; the bare `npm`
            // name is not spawnable by `Command`.
            assert_eq!(
                system_npm,
                Some(npm_dir.join("npm-cli.js").display().to_string()),
                "resolve_system_npm returned {system_npm:?}",
            );
        }
        // Regression: a *global* npm install puts the shim at the prefix root
        // and names `%dp0%\node_modules\@deepseek-ai\dsh\lib\bin.js` — no `..`.
        // Deriving the launcher as `<shim dir>\..\@deepseek-ai\dsh\lib\bin.js`
        // therefore found nothing on a machine that had dsh installed, so the
        // shell bootstrapped its own pinned runtime beside it and then shared
        // `$DSH_HOME` with the installation it had failed to see.
        if cfg!(windows) && had_node {
            let prefix = root.join("npm-global");
            let global_lib = prefix
                .join("node_modules")
                .join("@deepseek-ai")
                .join("dsh")
                .join("lib");
            fs::create_dir_all(&global_lib).unwrap();
            fs::write(global_lib.join("bin.js"), "#!/usr/bin/env node\n").unwrap();
            fs::write(
                prefix.join("dsh.cmd"),
                npm_shim(r"%dp0%\node_modules\@deepseek-ai\dsh\lib\bin.js"),
            )
            .unwrap();
            // Only the global prefix is on the probe PATH, so the local-layout
            // shim the block above created cannot answer for it.
            let saved_path = std::env::var_os("PATH").unwrap_or_default();
            let saved_pathext = std::env::var_os("PATHEXT").unwrap_or_default();
            let mut test_path = format!("C:\\Windows\\System32;{}", prefix.display());
            if let Some(dir) = &node_dir {
                test_path.push(';');
                test_path.push_str(&dir.display().to_string());
            }
            std::env::set_var("PATH", &test_path);
            std::env::set_var("PATHEXT", ".COM;.EXE;.BAT;.CMD");
            let resolved = if cfg!(windows) { resolve_path_dsh() } else { None };
            std::env::set_var("PATH", &saved_path);
            std::env::set_var("PATHEXT", &saved_pathext);
            let (program, args) = resolved
                .unwrap_or_else(|| panic!("resolve_path_dsh returned None for a global npm prefix"));
            assert_eq!(
                Path::new(&program)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_lowercase()),
                Some("node.exe".into()),
                "resolve_path_dsh returned {program}",
            );
            assert_eq!(args, vec![global_lib.join("bin.js").display().to_string()]);
        }
        // Regression: a shell started from a desktop or Start-menu shortcut
        // inherits the environment snapshot its launcher captured, so `node`
        // can be invisible even though the machine has it. The registry and
        // well-known passes must still resolve it, otherwise the shell
        // downloads a ~35 MB runtime it does not need. PATH stays mutated
        // inside this test only, because it is the one test that owns PATH.
        if cfg!(windows) {
            std::env::set_var("PATH", r"C:\Windows\System32");
            let without_inherited_path = resolve_system_node();
            std::env::set_var("PATH", old_path);
            if let Some(node) = without_inherited_path {
                assert!(node.is_absolute(), "resolve_system_node returned {node:?}");
                // The fallback must land on a real install, never on a stub
                // that happens to sit in System32.
                assert!(
                    !node.to_string_lossy().to_lowercase().contains(r"\system32"),
                    "resolve_system_node found {node:?} on a stripped PATH",
                );
                assert!(
                    system_node_compliant(&node),
                    "{node:?} is not dsh-engine compliant",
                );
            }
        }
    }

    #[test]
    fn percent_variables_expand_against_the_supplied_lookup() {
        let lookup = |name: &str| match name {
            "JAVA_HOME" => Some(r"C:\Java".to_string()),
            "PNPM_HOME" => Some(r"D:\pnpm".to_string()),
            _ => None,
        };
        assert_eq!(expand_percent_vars(r"%JAVA_HOME%\bin", &lookup), r"C:\Java\bin");
        assert_eq!(
            expand_percent_vars(r"%PNPM_HOME%;%JAVA_HOME%\bin", &lookup),
            r"D:\pnpm;C:\Java\bin",
        );
        // Undefined variables stay literal, exactly as CreateProcess leaves them.
        assert_eq!(
            expand_percent_vars(r"%JAVA_HOME%\bin;%NOT_SET%\x", &lookup),
            r"C:\Java\bin;%NOT_SET%\x",
        );
        // Names are matched case-insensitively; a lone `%` is not a reference.
        assert_eq!(expand_percent_vars(r"%java_home%", &lookup), r"C:\Java");
        assert_eq!(expand_percent_vars("100%", &lookup), "100%");
        assert_eq!(expand_percent_vars("%%", &lookup), "%%");
    }

    #[test]
    fn search_dirs_keep_order_and_drop_duplicates() {
        let mut dirs = Vec::new();
        push_search_dir(&mut dirs, PathBuf::from(r"C:\first"));
        push_search_dir(&mut dirs, PathBuf::new());
        push_search_dir(&mut dirs, PathBuf::from(r"C:\second"));
        // Same directory in different case: Windows treats them as one.
        push_search_dir(&mut dirs, PathBuf::from(r"c:\FIRST"));
        assert_eq!(dirs, vec![PathBuf::from(r"C:\first"), PathBuf::from(r"C:\second")]);
        // The inherited PATH always comes first, so a user's own install wins
        // over the well-known locations.
        let search = search_dirs();
        let inherited = std::env::split_paths(&std::env::var("PATH").unwrap_or_default())
            .find(|dir| !dir.as_os_str().is_empty());
        if let Some(first) = inherited {
            assert_eq!(search.first(), Some(&first));
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
    fn dsh_fingerprint_matches_only_the_boot_manifest() {
        assert!(looks_like_dsh("<!doctype html><script>window.__DSH_BOOT__ = {\"rev\":\"abc\"}"));
        assert!(looks_like_dsh("__DSH_BOOT__"));
        assert!(!looks_like_dsh("<html><head><title>Other App</title>"));
        assert!(!looks_like_dsh(""));
    }

    #[test]
    fn configured_port_reads_dsh_desktop_args() {
        let old = std::env::var_os("DSH_DESKTOP_ARGS");
        std::env::set_var("DSH_DESKTOP_ARGS", "--port 8080");
        assert_eq!(configured_port(), 8080);
        std::env::set_var("DSH_DESKTOP_ARGS", "web --port=8081");
        assert_eq!(configured_port(), 3080); // only the `--port <n>` form
        std::env::remove_var("DSH_DESKTOP_ARGS");
        assert_eq!(configured_port(), 3080);
        if let Some(old) = old {
            std::env::set_var("DSH_DESKTOP_ARGS", old);
        }
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
    fn legacy_credentials_are_flattened_and_backed_up() {
        let root = std::env::temp_dir().join(format!(
            "dsh-credentials-migration-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join(".credentials.yaml");
        fs::write(
            &path,
            "version: 1\nrefs:\n  DEEPSEEK_API_KEY: sk-test\n  OPENAI_API_KEY: 'sk:with-colon'\n",
        )
        .unwrap();

        assert_eq!(
            flatten_legacy_credentials(&fs::read_to_string(&path).unwrap()).as_deref(),
            Some("DEEPSEEK_API_KEY: sk-test\nOPENAI_API_KEY: 'sk:with-colon'\n"),
        );
        assert!(migrate_legacy_credentials_file(&path).unwrap());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "DEEPSEEK_API_KEY: sk-test\nOPENAI_API_KEY: 'sk:with-colon'\n",
        );
        assert!(path.with_file_name(".credentials.yaml.legacy-v1").is_file());
        assert!(!migrate_legacy_credentials_file(&path).unwrap());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unknown_credentials_documents_are_not_rewritten() {
        assert_eq!(
            flatten_legacy_credentials("version: 2\nrefs:\n  DEEPSEEK_API_KEY: sk-test\n"),
            None,
        );
        assert_eq!(
            flatten_legacy_credentials("DEEPSEEK_API_KEY: sk-test\n"),
            None,
        );
        assert_eq!(
            flatten_legacy_credentials("version: 1\nrefs:\n  DEEPSEEK_API_KEY: |\n    sk-test\n"),
            None,
        );
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

    #[test]
    fn normalized_path_folds_case_and_strips_the_nt_prefix() {
        assert_eq!(
            normalized_path(Path::new(r"\\?\C:\Users\Linfe\dsh-desktop\runtime")),
            normalized_path(Path::new(r"c:\users\linfe\dsh-desktop\runtime")),
        );
        assert_eq!(
            normalized_path(Path::new("C:/Users/linfe/.dsh/")),
            normalized_path(Path::new(r"C:\Users\linfe\.dsh")),
        );
    }

    #[test]
    fn is_foreign_link_only_judges_absolute_targets_outside_the_installation() {
        let owned = normalized_path(Path::new(r"C:\Users\linfe\dsh-desktop\runtime\dsh\node_modules"));
        // Owned by the running installation: one unscoped and one scoped link.
        assert!(!is_foreign_link(
            Path::new(r"C:\Users\linfe\dsh-desktop\runtime\dsh\node_modules\accepts"),
            &owned,
        ));
        assert!(!is_foreign_link(
            Path::new(r"C:\Users\linfe\dsh-desktop\runtime\dsh\node_modules\@deepseek-ai\dsh-llm"),
            &owned,
        ));
        // Written by a second installation: must be released.
        assert!(is_foreign_link(
            Path::new(r"D:\codetools\npm-global\node_modules\@deepseek-ai\dsh\node_modules\@deepseek-ai\dsh-authorization"),
            &owned,
        ));
        // npm's `.bin` shims are relative and belong to a profile, never to
        // the shared fallback's ownership model.
        assert!(!is_foreign_link(Path::new(r"..\@deepseek-ai\dsh\lib\bin.js"), &owned));
    }

    /// npm generates every shim from one template; only the last line's target
    /// differs per install layout. Both are reproduced verbatim here.
    fn npm_shim(target: &str) -> String {
        format!(
            "@ECHO off\nGOTO start\n:find_dp0\nSET dp0=%~dp0\nEXIT /b\n:start\nSETLOCAL\nCALL :find_dp0\n\nIF EXIST \"%dp0%\\node.exe\" (\n  SET \"_prog=%dp0%\\node.exe\"\n) ELSE (\n  SET \"_prog=node\"\n  SET PATHEXT=%PATHEXT:;.JS;=;%\n)\n\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"{target}\" %*\n"
        )
    }

    #[test]
    fn shim_scripts_read_both_npm_install_layouts() {
        let root = std::env::temp_dir().join(format!("dsh-shim-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // The local layout keeps a literal `..` in the path, so compare the
        // files each side actually names.
        let real = |path: &Path| fs::canonicalize(path).expect("desktop: test path exists");

        // A global install: the shim sits at the npm prefix root.
        let global = root.join("npm-global");
        let global_bin = global
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib")
            .join("bin.js");
        fs::create_dir_all(global_bin.parent().expect("desktop: bin.js has a parent")).unwrap();
        fs::write(&global_bin, "#!/usr/bin/env node\n").unwrap();
        fs::write(
            global.join("dsh.CMD"),
            npm_shim(r"%dp0%\node_modules\@deepseek-ai\dsh\lib\bin.js"),
        )
        .unwrap();
        assert_eq!(
            shim_scripts(&global.join("dsh.CMD")).iter().map(|path| real(path)).collect::<Vec<_>>(),
            vec![real(&global_bin)],
        );
        assert_eq!(
            shim_script(&global.join("dsh.CMD"), &[]).map(|path| real(&path)),
            Some(real(&global_bin)),
        );

        // A local install: `node_modules\.bin` sits one level down.
        let local = root.join("project");
        let local_bin = local
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib")
            .join("bin.js");
        fs::create_dir_all(local_bin.parent().expect("desktop: bin.js has a parent")).unwrap();
        fs::create_dir_all(local.join("node_modules").join(".bin")).unwrap();
        fs::write(&local_bin, "#!/usr/bin/env node\n").unwrap();
        fs::write(
            local.join("node_modules").join(".bin").join("dsh.cmd"),
            npm_shim(r"%dp0%\..\@deepseek-ai\dsh\lib\bin.js"),
        )
        .unwrap();
        assert_eq!(
            shim_scripts(&local.join("node_modules").join(".bin").join("dsh.cmd"))
                .iter()
                .map(|path| real(path))
                .collect::<Vec<_>>(),
            vec![real(&local_bin)],
        );

        // A shim that names nothing still resolves through the static layouts,
        // which is what a hand-written shim relies on.
        let hand_written = root.join("handwritten");
        let hand_lib = hand_written
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib");
        fs::create_dir_all(&hand_lib).unwrap();
        fs::write(hand_lib.join("bin.js"), "#!/usr/bin/env node\n").unwrap();
        fs::create_dir_all(hand_written.join("node_modules").join(".bin")).unwrap();
        fs::write(
            hand_written.join("node_modules").join(".bin").join("dsh.cmd"),
            "@echo off\n",
        )
        .unwrap();
        // A local shim sits one level inside the prefix, hence the `..`.
        assert_eq!(
            shim_script(
                &hand_written.join("node_modules").join(".bin").join("dsh.cmd"),
                &[&["..", "@deepseek-ai", "dsh", "lib", "bin.js"]],
            )
            .map(|path| real(&path)),
            Some(real(&hand_lib.join("bin.js"))),
        );
        // A global shim sits at the prefix root and needs no `..`.
        fs::write(hand_written.join("dsh.cmd"), "@echo off\n").unwrap();
        assert_eq!(
            shim_script(
                &hand_written.join("dsh.cmd"),
                &[&["node_modules", "@deepseek-ai", "dsh", "lib", "bin.js"]],
            )
            .map(|path| real(&path)),
            Some(real(&hand_lib.join("bin.js"))),
        );
        // Nothing to read: no candidates, no panic.
        assert!(shim_scripts(&root.join("missing.cmd")).is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn no_open_support_follows_the_launcher_that_will_run() {
        let root = std::env::temp_dir().join(format!("dsh-no-open-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let write = |path: &Path, text: &str| {
            fs::create_dir_all(path.parent().expect("desktop: probe file has a parent")).unwrap();
            fs::write(path, text).unwrap();
        };

        // npm's nested layout: the web app lives under `dsh/node_modules`.
        let nested_dsh = root.join("nested").join("node_modules").join("@deepseek-ai").join("dsh");
        let nested_bin = nested_dsh.join("lib").join("bin.js");
        write(&nested_bin, "#!/usr/bin/env node\n");
        let nested_web_app = nested_dsh
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh-web-app");
        write(
            &nested_web_app.join("lib").join("startup.js"),
            ".option(\"--no-open\", \"do not open the Web UI\")\n",
        );
        assert_eq!(resolved_web_app_dir(&nested_bin), Some(nested_web_app));
        assert!(web_app_declares_no_open(&nested_bin));

        // A hoisted layout: the web app sits beside `dsh` instead.
        let hoisted = root.join("hoisted");
        let hoisted_bin = hoisted
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib")
            .join("bin.js");
        write(&hoisted_bin, "#!/usr/bin/env node\n");
        let hoisted_web_app = hoisted.join("node_modules").join("@deepseek-ai").join("dsh-web-app");
        write(
            &hoisted_web_app.join("lib").join("startup.js"),
            ".option(\"--host <host>\", \"bind host\")\n",
        );
        assert_eq!(resolved_web_app_dir(&hoisted_bin), Some(hoisted_web_app));
        assert!(
            !web_app_declares_no_open(&hoisted_bin),
            "a launcher without --no-open must not be given the flag: commander rejects it",
        );

        // No web app at all, and a launcher that is not the dsh one: no claim.
        let bare_bin = root
            .join("bare")
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib")
            .join("bin.js");
        write(&bare_bin, "#!/usr/bin/env node\n");
        assert_eq!(resolved_web_app_dir(&bare_bin), None);
        assert!(!web_app_declares_no_open(&bare_bin));
        let other = root.join("other").join("some-tool.js");
        write(&other, "#!/usr/bin/env node\n");
        assert!(!web_app_declares_no_open(&other));

        // The launcher is recognised from its resolved leading argument, which
        // is what both the `PATH` shim and the bundled runtime hand the shell.
        assert_eq!(
            launcher_bin_js("node.exe", &[nested_bin.display().to_string()]),
            Some(nested_bin.clone()),
        );
        assert!(host_supports_no_open("node.exe", &[nested_bin.display().to_string()]));
        assert!(!host_supports_no_open("node.exe", &[hoisted_bin.display().to_string()]));
        // A program that is not the launcher, and a launcher that is not the
        // dsh shape: neither claims support.
        assert!(!host_supports_no_open("dsh-not-a-real-program-xyz", &[]));
        assert_eq!(launcher_bin_js(&other.display().to_string(), &[]), None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn port_probe_tells_a_reusable_dsh_from_a_taken_port() {
        use std::net::TcpListener;

        // Nothing listening: the host may bind it.
        let free = {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("desktop: bind a probe port");
            listener.local_addr().expect("desktop: probe addr").port()
        };
        assert_eq!(probe_port(free), PortOwner::Free);

        // A dsh that serves the boot manifest: reusable. The marker sits behind
        // a prelude larger than one naive 4 KiB read, exactly as it does in
        // dsh's real ~28 KB document — a truncated read reports a healthy
        // service as "not dsh", which silently costs the reuse.
        let reusable = TcpListener::bind(("127.0.0.1", 0)).expect("desktop: bind a probe server");
        let reusable_port = reusable.local_addr().expect("desktop: probe addr").port();
        let responder = std::thread::spawn(move || {
            if let Ok((mut socket, _)) = reusable.accept() {
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request);
                let prelude = "x".repeat(8000);
                let _ = socket.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\n\r\n<!--{prelude}--><script>window.__DSH_BOOT__={{}}</script>"
                    )
                    .as_bytes(),
                );
            }
        });
        assert_eq!(probe_port(reusable_port), PortOwner::Reusable);
        responder.join().ok();

        // A token-protected dsh: it is dsh, but the shell holds no cookie for
        // it, so spawning here would hang forever with no output.
        let protected = TcpListener::bind(("127.0.0.1", 0)).expect("desktop: bind a probe server");
        let protected_port = protected.local_addr().expect("desktop: probe addr").port();
        let responder = std::thread::spawn(move || {
            if let Ok((mut socket, _)) = protected.accept() {
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request);
                let _ = socket.write_all(
                    format!(
                        "HTTP/1.1 401 Unauthorized\r\ncontent-type: text/plain; charset=utf-8\r\n\r\n{AUTH_REQUIRED_MARKER}; reopen the URL printed by dsh web.\n"
                    )
                    .as_bytes(),
                );
            }
        });
        assert_eq!(probe_port(protected_port), PortOwner::Protected);
        responder.join().ok();

        // A foreign program holding the port: taken as well, for the same
        // reason — the host cannot bind and would never say so.
        let foreign = TcpListener::bind(("127.0.0.1", 0)).expect("desktop: bind a probe server");
        let foreign_port = foreign.local_addr().expect("desktop: probe addr").port();
        let responder = std::thread::spawn(move || {
            if let Ok((mut socket, _)) = foreign.accept() {
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request);
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\r\nhello\n");
            }
        });
        assert_eq!(probe_port(foreign_port), PortOwner::Taken);
        responder.join().ok();
    }

    #[test]
    fn browser_session_cookie_matches_the_dsh_wire_format() {
        // Golden values computed from `@deepseek-ai/dsh-client-connection`'s own
        // `cookieName` / `encodeCookie` (independent Node implementation), for
        // authority `127.0.0.1:3080`, the 32-byte secret 0x00..0x1f, and a fixed
        // issue/expiry pair. A drift here means the running service answers 401
        // and the shell silently falls back to spawning its own host.
        let authority = "127.0.0.1:3080";
        let secret: Vec<u8> = (0u8..32).collect();
        assert_eq!(encode_base64_url(&secret), "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8");
        assert_eq!(
            decode_base64_url("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8").as_deref(),
            Some(secret.as_slice()),
        );
        assert_eq!(
            browser_cookie_name(authority),
            "dsh-auth-VPhEEcLKeqRDBoBalzN2Nm7CnfxKhLE00pKIDWxt1sw",
        );
        assert_eq!(
            browser_cookie_value(authority, &secret, 1_700_000_000_000, 1_700_003_600_000),
            "v1.eyJ2ZXJzaW9uIjoxLCJhdXRob3JpdHkiOiIxMjcuMC4wLjE6MzA4MCIsImlzc3VlZEF0IjoxNzAwMDAwMDAwMDAwLCJleHBpcmVzQXQiOjE3MDAwMDM2MDAwMDB9.B0v5JPOdGQ5Kj6dKuTEbtVltd8tYJxj2llBTMDhV9Sw",
        );
        // The default `cookieMaxAgeDays` (30) caps the signed lifetime.
        assert!(COOKIE_LIFETIME_MILLIS < 30 * 24 * 60 * 60 * 1000);
    }

    #[test]
    fn credentials_record_secret_walks_only_the_browser_session_block() {
        let document = "version: 1\nrefs:\n  DEEPSEEK_API_KEY: sk-other\nrecords:\n  client-connection/browser-session:\n    kind: grant\n    payload:\n      version: 1\n      secret: AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8\n  other/record:\n    secret: QUJD\n";
        assert_eq!(
            credentials_record_secret(document).as_deref(),
            Some((0u8..32).collect::<Vec<u8>>().as_slice()),
        );
        // A quoted scalar is still a scalar.
        let quoted = "records:\n  client-connection/browser-session:\n    payload:\n      secret: \"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8\"\n";
        assert_eq!(
            credentials_record_secret(quoted).as_deref(),
            Some((0u8..32).collect::<Vec<u8>>().as_slice()),
        );
        // No such record, an empty block, and a block that ends before the
        // secret all answer `None` rather than borrowing another record's.
        assert_eq!(credentials_record_secret("version: 1\nrefs: {}\n"), None);
        assert_eq!(
            credentials_record_secret("records:\n  other/record:\n    secret: QUJD\n"),
            None,
        );
        assert_eq!(
            credentials_record_secret("records:\n  client-connection/browser-session:\n    kind: grant\n  other/record:\n    secret: QUJD\n"),
            None,
        );
        // Not decodable as base64url: refuse rather than guess.
        assert_eq!(
            credentials_record_secret("records:\n  client-connection/browser-session:\n    secret: not*base64\n"),
            None,
        );
    }

    #[test]
    fn version_comparison_never_mistakes_a_downgrade_for_an_update() {
        use std::cmp::Ordering;
        // The measured channel state: `latest` trails `next`, so `@latest`
        // would look "newer" to npm while actually moving backwards.
        assert_eq!(compare_versions("0.1.5-rc.2", "0.1.5-rc.1"), Ordering::Greater);
        assert_eq!(compare_versions("0.1.5-rc.1", "0.1.5-rc.2"), Ordering::Less);
        assert_eq!(compare_versions("0.1.5-rc.2", "0.1.5-rc.2"), Ordering::Equal);
        // A release outranks the prereleases it precedes.
        assert_eq!(compare_versions("0.1.5", "0.1.5-rc.2"), Ordering::Greater);
        assert_eq!(compare_versions("0.1.5-rc.2", "0.1.5"), Ordering::Less);
        // Numeric identifiers compare numerically, not lexically.
        assert_eq!(compare_versions("0.1.5-rc.10", "0.1.5-rc.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.1.10", "0.1.9"), Ordering::Greater);
        // Alphanumeric identifiers rank above numeric ones.
        assert_eq!(compare_versions("0.2.0-alpha", "0.2.0-1"), Ordering::Greater);
        // Fewer identifiers lose when every shared one is equal.
        assert_eq!(compare_versions("0.2.0-rc", "0.2.0-rc.1"), Ordering::Less);
        // Missing components read as zero, and an unparsable core is not fatal.
        assert_eq!(compare_versions("0.2", "0.2.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.0.0", "0.9.9"), Ordering::Greater);
    }

    #[test]
    fn engine_install_reads_the_prefix_out_of_the_launcher_path() {
        let bundled_bin = runtime_dir().join("dsh").join(DSH_BIN_REL);
        match engine_install_of(&bundled_bin) {
            Some(install) => {
                assert!(
                    !install.global,
                    "the bundled runtime is a local prefix, not a global one",
                );
                assert_eq!(install.prefix, runtime_dir().join("dsh"));
            }
            None => panic!("the bundled launcher path must resolve to an install"),
        }
        let global_bin = Path::new(r"D:\npm-global\node_modules\@deepseek-ai\dsh\lib\bin.js");
        let install = engine_install_of(global_bin).expect("a global launcher resolves");
        assert!(install.global, "anything outside the runtime dir is a global prefix");
        assert_eq!(install.prefix, Path::new(r"D:\npm-global"));
        // Not the launcher's shape: refuse rather than guess a prefix.
        assert!(engine_install_of(Path::new(r"D:\x\lib\other.js")).is_none());
    }

    #[test]
    fn unserviceable_reason_separates_dangling_from_unreadable() {
        let root = std::env::temp_dir().join(format!("dsh-farm-reason-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();

        // A directory without a manifest resolves nothing even though it
        // exists — the case `exists()` alone cannot see.
        assert_eq!(
            unserviceable_reason(&target, &target),
            Some("its manifest is unreadable"),
        );

        // With the manifest in place, read through the same path Node walks,
        // the link is healthy and must be left alone for its owner.
        fs::write(target.join("package.json"), "{}").unwrap();
        assert_eq!(unserviceable_reason(&target, &target), None);

        // A vanished target is still the loudest reason.
        assert_eq!(
            unserviceable_reason(&target, &root.join("gone")),
            Some("its target is gone"),
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn profile_fallback_readable_needs_a_resolvable_manifest() {
        let saved = std::env::var_os("DSH_HOME");
        let root = std::env::temp_dir().join(format!("dsh-farm-probe-{}", std::process::id()));
        let scope = root.join("profiles").join("node_modules").join("@deepseek-ai");
        std::env::set_var("DSH_HOME", &root);

        // No fallback at all: there is nothing for Node to resolve.
        let _ = fs::remove_dir_all(&root);
        assert!(!profile_fallback_readable());

        // A package directory without a manifest is not resolvable either.
        fs::create_dir_all(scope.join("dsh-base")).expect("create probe package dir");
        assert!(!profile_fallback_readable());

        // With the manifest in place the same probe succeeds.
        fs::write(scope.join("dsh-base").join("package.json"), "{}").expect("write probe manifest");
        assert!(profile_fallback_readable());

        match saved {
            Some(value) => std::env::set_var("DSH_HOME", value),
            None => std::env::remove_var("DSH_HOME"),
        }
        let _ = fs::remove_dir_all(&root);
    }
}
