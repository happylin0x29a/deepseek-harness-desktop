//! DeepSeek Harness desktop shell.
//!
//! The shell opens a small loading window immediately, spawns `dsh desktop`
//! as a child process, and streams startup states to the window while it
//! waits for the readiness line the web runtime prints once its Loader tree
//! settles (`dsh web: http://127.0.0.1:PORT`, possibly with a LAN suffix).
//! Once the URL arrives the window grows to its full size and navigates
//! directly onto the surface — no choice step, and no IPC from the surface.
//! If the host dies without publishing a URL, the failure reason stays on
//! the loading window. On Windows the host child runs windowless
//! (`CREATE_NO_WINDOW`), so no console window appears beside the app.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{Emitter, LogicalSize, Manager, Size, Url, WebviewWindow};

/// Ask `CreateProcess` to attach no console window to the host child.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How long the shell waits for the host's readiness line before giving up.
fn ready_timeout() -> Duration {
    let millis = std::env::var("DSH_READY_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(120_000);
    Duration::from_millis(millis)
}

/// The shell's shared state: the spawned host process, the current startup
/// phase, the recorded failure, and the host's recent stderr (kept so the
/// loading page can read everything without racing the event stream).
struct DesktopState {
    child: Mutex<Option<Child>>,
    phase: Mutex<String>,
    error: Mutex<Option<String>>,
    stderr_tail: Mutex<String>,
    ready: Mutex<bool>,
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

/// The host binary: `DSH_BIN` for development (a wrapper script or the built
/// `apps/cli/lib/bin.js` under `node`), plain `dsh` for installed deployments.
fn host_bin() -> String {
    std::env::var("DSH_BIN").unwrap_or_else(|_| "dsh".to_string())
}

/// Extra `dsh` arguments from `DSH_DESKTOP_ARGS` (whitespace-split), for
/// example `--port 8080`.
fn extra_host_args() -> Vec<String> {
    std::env::var("DSH_DESKTOP_ARGS")
        .map(|raw| raw.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// The host invocation as program plus leading arguments. `DSH_BIN` may name
/// a single executable (`dsh`, a wrapper script) or a program with arguments
/// (`node <path>/apps/cli/lib/bin.js`); paths containing whitespace need a
/// wrapper script instead.
fn host_command() -> (String, Vec<String>) {
    let raw = host_bin();
    let mut parts = raw.split_whitespace();
    let program = parts.next().unwrap_or("dsh").to_string();
    (program, parts.map(str::to_string).collect())
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

/// Grow the loading window to its full size and open the surface on it.
fn open_surface(window: &WebviewWindow, url: &str) {
    let parsed = Url::parse(url).expect("desktop: readiness URL is not a valid URL");
    if window.set_size(Size::Logical(LogicalSize::new(1280.0, 800.0))).is_err()
        || window.set_min_size(Some(LogicalSize::new(960.0, 600.0))).is_err()
        || window.center().is_err()
        || window.navigate(parsed).is_err() {
        log_line("failed to grow or navigate the main window")
    }
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

/// Spawn `dsh desktop` windowless and stream startup progress; once its
/// readiness URL arrives the window grows onto the surface. A spawn failure,
/// a host that exits without publishing, or a readiness timeout keeps the
/// reason (plus the host's recent stderr) on the loading window.
fn spawn_host(state: &DesktopState, app: tauri::AppHandle) {
    set_phase(&app, "正在启动宿主进程…");
    log_line(&format!("host command: {} desktop {}", host_bin(), extra_host_args().join(" ")));
    let (program, leading_args) = host_command();
    let mut command = Command::new(program);
    command
        .args(leading_args)
        .arg("desktop")
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
            fail_startup(&app, &format!(
                "启动失败:找不到 dsh。请先安装 @deepseek-ai/dsh(npm i -g @deepseek-ai/dsh),或设置 DSH_BIN 环境变量指向它。({error})",
            ));
            return;
        }
    };
    let stdout = child.stdout.take().expect("desktop: host stdout is not piped");
    let stderr = child.stderr.take().expect("desktop: host stderr is not piped");
    *state.child.lock().expect("desktop: child mutex poisoned") = Some(child);
    set_phase(&app, "正在等待服务器就绪…");
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
    // Readiness watchdog: after the timeout, a silent host fails visibly.
    let timeout_app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(ready_timeout());
        if let Some(state) = timeout_app.try_state::<DesktopState>() {
            let ready = *state.ready.lock().expect("desktop: ready mutex poisoned");
            if !ready {
                fail_startup(&timeout_app, &format!(
                    "启动超时:宿主在 {} 秒内没有就绪。原因见宿主日志,或查看 {}。",
                    ready_timeout().as_secs(),
                    log_path(),
                ));
            }
        }
    });
    // The stdout reader: readiness line → grow onto the surface.
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
                if let Some(state) = app.try_state::<DesktopState>() {
                    *state.ready.lock().expect("desktop: ready mutex poisoned") = true;
                }
                if let Some(window) = app.get_webview_window("main") {
                    set_phase(&app, "正在打开界面…");
                    open_surface(&window, &url);
                }
                break;
            }
        }
        if !published {
            let ready = app.try_state::<DesktopState>()
                .map(|state| *state.ready.lock().expect("desktop: ready mutex poisoned"))
                .unwrap_or(false);
            if !ready {
                let tail = app.try_state::<DesktopState>()
                    .map(|state| state.stderr_tail.lock().expect("desktop: stderr mutex poisoned").clone())
                    .unwrap_or_default();
                fail_startup(&app, &format!(
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

/// The startup progress snapshot the loading page polls: current phase,
/// recorded failure, and the host's recent stderr.
#[tauri::command]
fn startup_status(state: tauri::State<'_, DesktopState>) -> StartupStatus {
    StartupStatus {
        phase: state.phase.lock().expect("desktop: phase mutex poisoned").clone(),
        error: state.error.lock().expect("desktop: error mutex poisoned").clone(),
        stderr_tail: state.stderr_tail.lock().expect("desktop: stderr mutex poisoned").clone(),
        log_path: log_path(),
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
}

/// Build and run the desktop shell.
pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            log_line(&format!("shell starting, log at {}", log_path()));
            let state = DesktopState {
                child: Mutex::new(None),
                phase: Mutex::new("正在启动…".to_string()),
                error: Mutex::new(None),
                stderr_tail: Mutex::new(String::new()),
                ready: Mutex::new(false),
            };
            spawn_host(&state, app.handle().clone());
            app.manage(state);
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
    fn host_command_splits_program_and_leading_arguments() {
        std::env::set_var("DSH_BIN", "node C:\\dsh\\apps\\cli\\lib\\bin.js");
        assert_eq!(
            host_command(),
            ("node".to_string(), vec!["C:\\dsh\\apps\\cli\\lib\\bin.js".to_string()]),
        );
        std::env::remove_var("DSH_BIN");
        assert_eq!(host_command(), ("dsh".to_string(), Vec::new()));
    }
}
