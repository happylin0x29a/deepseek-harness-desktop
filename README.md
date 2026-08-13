# dsh-desktop — the Tauri shell

English | [中文](README.zh.md)

The desktop shell for DeepSeek Harness: a Tauri 2 application that spawns `dsh desktop`, waits for the readiness line, and shows the same Web GUI in a native window. The client-or-web choice is a shell choice — a regular browser tab shows the same session over the same local server.

## How it works

1. The shell opens a small loading window immediately, spawns `dsh desktop` (binary from `DSH_BIN`, default `dsh`; extra args from `DSH_DESKTOP_ARGS`) and streams startup states into it (starting the host → waiting for the server → opening the surface). On Windows the child runs with `CREATE_NO_WINDOW`, so no console window appears beside the app.
2. The web runtime prints `dsh web: http://127.0.0.1:PORT` once its Loader tree settles; the shell parses it (either label).
3. Once the URL arrives the loading window grows to its full size and opens directly on that surface — no choice step, and no IPC. A host that exits without publishing a URL keeps the failure reason on the loading window.
4. When the shell exits, the spawned host is killed.

## Icons

The whale icon is rendered from the repository favicon (`apps/web/public/favicon.svg`) into `src-tauri/icons`; regenerate with `pnpm tauri icon whale-source.png`.

## Prerequisites

- Node.js and a working `dsh` installation (`npm i -g @deepseek-ai/dsh`), or a built source checkout (`pnpm run build` in the repository root).
- Rust stable and the platform prerequisites from [tauri.app](https://tauri.app/start/prerequisites/) (WebView2 on Windows, webkit2gtk on Linux).
- pnpm, for the Tauri CLI devDependency: `pnpm install` in this directory (needs network the first time). Inside the repository checkout, use `pnpm install --ignore-workspace` — the parent pnpm workspace would otherwise absorb the install into the repo root.

## Run

```sh
# installed deployment: `dsh` on PATH
pnpm tauri dev

# source checkout: point the shell at the built CLI (relative to this directory)
$env:DSH_BIN = "node <deepseek-harness-checkout>\apps\cli\lib\bin.js"
$env:DSH_DESKTOP_ARGS = "--port 8080"   # optional
pnpm tauri dev

# production bundle
pnpm tauri build
```

The desktop profile is the `desktop` template in `dsh-app-boot` (`dsh-base` → `dsh-web-app` → `dsh-desktop-bundle`); its prompt section and browser marker live in [`packages/desktop/desktop-bundle`](../packages/desktop/desktop-bundle/README.md) and [`packages/client/desktop`](../packages/client/desktop/README.md).

## Known limitations

- **No code signing, auto-update, or notifications** — the bundle is unsigned; mac distributions need an `icons/icon.icns` generated with `tauri icon`.
- **No single-instance guard** — launching the shell twice spawns a second host on the same port; pass `--port` through `DSH_DESKTOP_ARGS`.
- **Process-tree kill is best-effort on Windows** — killing the spawned Node process may leave grandchildren; the host exits with the shell in ordinary cases.
