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

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use tauri::{Emitter, LogicalSize, Manager, Size, Url, WebviewWindow};

/// Ask `CreateProcess` to attach no console window to the host child.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The shell's shared state: the spawned host process.
struct DesktopState {
    child: Mutex<Option<Child>>,
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

/// Publish one startup-progress state to the loading window.
fn emit_state(app: &tauri::AppHandle, state: &str) {
    let _ = app.emit("startup-state", state);
}

/// Grow the loading window to its full size and open the surface on it.
fn open_surface(window: &WebviewWindow, url: &str) {
    let parsed = Url::parse(url).expect("desktop: readiness URL is not a valid URL");
    if window.set_size(Size::Logical(LogicalSize::new(1280.0, 800.0))).is_err()
        || window.set_min_size(Some(LogicalSize::new(960.0, 600.0))).is_err()
        || window.center().is_err()
        || window.navigate(parsed).is_err() {
        eprintln!("desktop: failed to grow or navigate the main window")
    }
}

/// Spawn `dsh desktop` windowless and stream startup progress; once its
/// readiness URL arrives the window grows onto the surface, and a host that
/// exits without one keeps the failure reason on the loading window.
fn spawn_host(state: &DesktopState, app: tauri::AppHandle) {
    emit_state(&app, "正在启动宿主进程…");
    let (program, leading_args) = host_command();
    let mut command = Command::new(program);
    command
        .args(leading_args)
        .arg("desktop")
        .args(extra_host_args())
        .stdout(Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // The host is a console-subsystem program (node): without the flag
        // Windows would open a black console window beside the app. Its
        // stderr has nowhere useful to go windowless, so drop it.
        command.creation_flags(CREATE_NO_WINDOW);
        command.stderr(Stdio::null());
    }
    #[cfg(not(target_os = "windows"))]
    {
        command.stderr(Stdio::inherit());
    }
    let mut child = command.spawn().expect("desktop: failed to spawn the dsh host");
    let stdout = child.stdout.take().expect("desktop: host stdout is not piped");
    *state.child.lock().expect("desktop: child mutex poisoned") = Some(child);
    emit_state(&app, "正在等待服务器就绪…");
    std::thread::spawn(move || {
        let mut published = false;
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { continue };
            if let Some(url) = parse_url_line(&line) {
                if !is_local_web_url(&url) {
                    continue;
                }
                published = true;
                if let Some(window) = app.get_webview_window("main") {
                    emit_state(&app, "正在打开界面…");
                    open_surface(&window, &url);
                }
                break;
            }
        }
        if !published {
            emit_state(&app, "启动失败：请确认 dsh 已安装并位于 PATH（或设置 DSH_BIN 环境变量）后重试。");
        }
    });
}

/// Build and run the desktop shell.
pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let state = DesktopState { child: Mutex::new(None) };
            spawn_host(&state, app.handle().clone());
            app.manage(state);
            Ok(())
        })
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
