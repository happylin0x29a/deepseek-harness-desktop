# dsh-desktop — the Tauri shell

English | [中文](README.zh.md)

The desktop shell for DeepSeek Harness: a Tauri 2 application that spawns `dsh web`, waits for the readiness line, and shows the same Web GUI in a native window. The client-or-web choice is a shell choice — a regular browser tab shows the same session over the same local server.

## How it works

1. The shell opens a small loading window immediately, resolves the host process (`DSH_BIN` env var → `dsh` on `PATH` → first-run bootstrap) and streams startup states into it (starting the host → waiting for the server → opening the surface); the same phase is mirrored in the window title, so progress is visible even before the page loads. On Windows the child runs with `CREATE_NO_WINDOW`, so no console window appears beside the app.
2. The web runtime prints `dsh web: http://127.0.0.1:PORT` once its Loader tree settles; the shell parses that line. The shell *is* that surface, so it passes `--no-open` to stop the host from also handing the URL to the system browser — but only to a `dsh` that declares the flag: pre-0.1.5 `dsh-web-app` builds have no such option (they never open a browser either), and `commander` exits on an unknown option. The shell therefore reads the `dsh-web-app` of the launcher that is **about to run**, and treats an unreadable one as unsupported.
3. Once the URL arrives the loading window grows to its full size and opens directly on that surface — no choice step, and no IPC from the surface. A host that fails to spawn, exits without publishing a URL, or misses the readiness timeout (`DSH_READY_TIMEOUT_MS`, default 300 s; the first-run bootstrap can be slow) keeps the reason — including the host's recent stderr — on the loading window, and a full timeline is appended to `%TEMP%\dsh-desktop.log`.
4. When the shell exits, the spawned host is killed.

## First run: runtime bootstrap

The installer ships the shell only — **neither Node.js nor dsh is bundled**. On first launch, when neither `dsh` on `PATH` nor `DSH_BIN` exists, the shell bootstraps the runtime from the configured mirrors:

1. **Reuse the local environment**: when a `node`+`npm` satisfying the dsh engine range (22.19+ on the 22 line, or any 24+) is on `PATH`, the Node download is skipped and the local node/npm are used.
2. Otherwise, downloads a pinned Node.js (`22.19.0`; ~35 MB win-x64 / ~30 MB macOS arm64, SHA-256 verified) and unpacks it into the runtime directory;
3. Installs the pinned `@deepseek-ai/dsh@0.1.0-rc.6` from the mirror registry with npm (~200 MB on disk);
4. Starts the host as `node <runtime>/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js web` (the local node when it is compliant).

Runtime directory: `%LOCALAPPDATA%\dsh-desktop\runtime` on Windows; `~/Library/Application Support/dsh-desktop/runtime` on macOS. Later launches reuse the installed runtime.

Mirrors and versions are overridable through environment variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `DSH_NODE_MIRROR` | `https://npmmirror.com/mirrors/node` | Node.js distribution mirror |
| `DSH_NPM_REGISTRY` | `https://registry.npmmirror.com` | npm registry mirror |
| `DSH_NODE_VERSION` | `22.19.0` | Node version (checksum verification is skipped when overridden) |
| `DSH_BIN` | — | Host command for development, e.g. `node ..\apps\cli\lib\bin.js`; skips the bootstrap |
| `DSH_DESKTOP_ARGS` | — | Extra arguments appended to `dsh web`, e.g. `--port 8080` |
| `DSH_READY_TIMEOUT_MS` | `300000` | Host readiness timeout in milliseconds once the host has spawned |
| `DSH_BOOTSTRAP_TIMEOUT_MS` | `1200000` | Runtime download/install timeout in milliseconds (20 min default) |

## Icons

The whale icon is rendered from the repository favicon (`apps/web/public/favicon.svg`) into `src-tauri/icons`; regenerate with `pnpm tauri icon whale-source.png`.

## Prerequisites

- None at runtime: the first run downloads Node.js and dsh (needs network; npmmirror by default).
- To build: Rust stable and the platform prerequisites from [tauri.app](https://tauri.app/start/prerequisites/) (WebView2 on Windows, Xcode command-line tools on macOS), plus pnpm for the Tauri CLI devDependency.

## Platform support

- **Windows x64** (NSIS/MSI) and **macOS Apple Silicon / Intel** (DMG/app): the first run downloads the matching Node.js archive (win-x64 zip / darwin-arm64 / darwin-x64 tar.gz, SHA-256 verified) and installs dsh — no prerequisites.
- **macOS note**: the runtime lives in `~/Library/Application Support/dsh-desktop/runtime`; the downloaded Node is an ordinary user-space file and needs no admin rights.
- **Other platforms (Linux, …)**: no installers and no runtime auto-download; install Node.js and `@deepseek-ai/dsh` yourself, or point `DSH_BIN` at an existing dsh launcher.

## Run

```sh
# installed deployment: nothing to install up front; the first run bootstraps the runtime
pnpm tauri dev

# source checkout: point the shell at the built CLI (relative to this directory)
$env:DSH_BIN = "node ..\apps\cli\lib\bin.js"
$env:DSH_DESKTOP_ARGS = "--port 8080"   # optional
pnpm tauri dev

# production bundle
pnpm tauri build
```

The shell boots the standard `web` profile (equivalent to `dsh web` / `dsh --profile web`) — the desktop window and a browser tab show exactly the same surface.

## Troubleshooting

- **Stuck on "等待网络响应" (waiting for network)**: a system proxy (Clash or similar in TUN/system-proxy mode) intercepts connections to the mirror, and proxy nodes often stall on npmmirror. Turn the proxy off, or add `npmmirror.com` and `registry.npmmirror.com` to the proxy bypass list. The shell log (`%TEMP%\dsh-desktop.log`) records detected proxy environment variables (credentials redacted).
- **Network unreachable**: verify `https://registry.npmmirror.com` is reachable; switch mirrors via `DSH_NPM_REGISTRY` / `DSH_NODE_MIRROR`.
- **Closing the window mid-install**: an already-installed dsh (version-verified) is reused on the next launch without reinstalling; an unfinished install resumes from the npm cache (`<runtime>\npm-cache`).
- **`credentials-local` says `version` must be a string**: an older dsh left `~/.dsh/.credentials.yaml` in the wrapped legacy format. Before spawning the host, the shell migrates it to the current format and keeps the original as `.credentials.yaml.legacy-v1`; if startup still fails, inspect `%TEMP%\dsh-desktop.log`.
- **`Cannot find package '@deepseek-ai/…'` on startup, as if dependencies were missing**: none are. dsh resolves a profile's plugins from `~/.dsh/profiles/node_modules`, a generated *flat fallback*, not from its own `node_modules` (`boot()` anchors the Loader at the profile directory and leaves `bareModuleBaseUrl` unset, so a bare specifier can only be found through Node's parent-directory walk). That directory is **shared by every dsh installation**: each boot rebuilds one symlink per package in *its own* dependency closure and leaves everyone else's links in place.

  What matters is therefore that **the shell and the installation owning that directory are one and the same**. The host resolution order is `DSH_BIN` (development override) → a `dsh` on `PATH` → the first-run bootstrap of the pinned runtime, and a `dsh` on `PATH` always wins: that is what keeps the version and the link ownership aligned. Once that installation is moved, rebuilt, uninstalled, or merely mid-reinstall (npm deletes `node_modules` first), its leftover links point at nothing — and dsh documents dangling links as *invisible to resolution* — so every plugin fails as `Cannot find package`. The shell releases such **unserviceable** foreign links before booting — a link whose target is gone, or whose target exists yet yields no readable `package.json`. Links only; targets are never touched, and a foreign link that still resolves is left alone so another installation keeps working. Each release is logged as `profile fallback: released …`.

  The other cause is **the tree being held for a few seconds**: that fallback is a directory of junction points, and security software scanning it can make every plugin look missing while not a single link is broken — the same effect that makes deleting one junction take the better part of a second. So when the host exits with a resolution failure or with no output at all, the shell **waits for the tree to become readable (up to 15 s) and then backs off before retrying, three attempts in total**, instead of one immediate retry. The log records `profile fallback readable before prepare: …` and `… retrying (attempt n/3)`.

  **Sharing one `~/.dsh` works best with one dsh on the machine**: after `npm i -g @deepseek-ai/dsh` the shell reuses it directly (including npm's global Windows shim, instead of falling back to its own runtime download), so version and link ownership agree by construction — and sessions, credentials, and profiles all live in that same `~/.dsh`.

## Known limitations

- **No code signing, auto-update, or notifications** — the bundle is unsigned; mac distributions need an `icons/icon.icns` generated with `tauri icon`.
- **The port is already taken at launch**: the shell probes the configured port (3080 by default) and handles four cases.
  - **A dsh service that opens without a token** → reused as-is, with no second host (a reused service is not owned, so closing the window leaves it running).
  - **A dsh behind its `?token=`** → the shell **attaches**: it takes the machine-wide signing secret from `records["client-connection/browser-session"]` in `~/.dsh/.credentials.yaml`, mints a cookie in dsh's own format (name `dsh-auth-<sha256(authority)>`, value `v1.<base64url body>.<base64url HMAC-SHA256>`), **proves over HTTP that the server accepts it**, and only then stores it in the window's cookie jar and opens that service.
    This is the only way to avoid `SessionAlreadyOwnedError`: two engines list the same sessions, but a session admits one write handle, so opening a session that is live in the other engine fails with `already owned by an active write handle`. When the desktop window and your own `dsh web` are **one engine**, that cannot happen.
    A missing secret, a rejected cookie, or a cookie the window will not store all **fall back** to spawning a host on a free port — never to an authentication page in the desktop window.
  - **Anything else on the port** → the shell starts its own host on a free port (`--port 0`). That decision has to happen *before* spawning: a host that loses the bind prints **nothing at all** and never becomes ready, so a reactive `EADDRINUSE` fallback never fires.
  - The log records `attached to the running dsh on port …` and `port … is taken; starting the host on a free port instead`.
  - To deliberately keep the desktop app on its **own** engine, point it at a free port: `DSH_DESKTOP_ARGS=--port 8080`.
- **No single-instance guard** — launching the shell twice gives each instance its own host (the second lands on a free port); pass `--port` through `DSH_DESKTOP_ARGS` to pin it. Do not run two instances concurrently during the first-run bootstrap, which writes the runtime directory.
- **Process-tree kill is best-effort on Windows** — killing the spawned Node process may leave grandchildren; the host exits with the shell in ordinary cases.
- **First run needs network** — Node and dsh come from the mirrors; if the mirrors are unreachable, install `@deepseek-ai/dsh` yourself (`npm i -g @deepseek-ai/dsh`) or set `DSH_BIN`.
