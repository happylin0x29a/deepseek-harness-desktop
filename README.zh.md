# dsh-desktop — Tauri 外壳

[English](README.md) | 中文

DeepSeek Harness 的桌面外壳：一个 Tauri 2 应用，启动 `dsh desktop`、等待就绪行，然后在原生窗口中显示同一个 Web GUI。客户端还是网页的选择只是外壳选择——普通浏览器标签页通过同一个本地服务器显示同一个会话。

## 工作原理

1. 外壳立即打开一个小型加载窗口，启动 `dsh desktop`（二进制来自 `DSH_BIN`，默认 `dsh`；额外参数来自 `DSH_DESKTOP_ARGS`），并把启动状态流式显示在窗口里（启动宿主 → 等待服务器 → 打开界面）。Windows 上子进程以 `CREATE_NO_WINDOW` 运行，应用旁不会出现控制台窗口。
2. web runtime 在其 Loader 树稳定后打印 `dsh web: http://127.0.0.1:PORT`；外壳解析该行（两种标签均可）。
3. URL 到达后，加载窗口放大到完整尺寸并直接落在那条界面上——没有选择步骤，也没有 IPC。宿主退出且未发布 URL 时，失败原因会保留在加载窗口上。
4. 外壳退出时，被启动的宿主进程会被终止。

## 图标

鲸鱼图标由仓库 favicon（`apps/web/public/favicon.svg`）渲染进 `src-tauri/icons`；用 `pnpm tauri icon whale-source.png` 重新生成。

## 前置条件

- Node.js 和可用的 `dsh` 安装（`npm i -g @deepseek-ai/dsh`），或已构建的源码检出（在仓库根目录 `pnpm run build`）。
- Rust stable 以及 [tauri.app](https://tauri.app/start/prerequisites/) 所列平台前置条件（Windows 上的 WebView2，Linux 上的 webkit2gtk）。
- pnpm，用于 Tauri CLI 开发依赖：在本目录执行 `pnpm install`（首次需要网络）。在仓库检出内请使用 `pnpm install --ignore-workspace`——否则父级 pnpm 工作区会把安装吸收到仓库根目录。

## 运行

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

desktop profile 是 `dsh-app-boot` 中的 `desktop` 模板（`dsh-base` → `dsh-web-app` → `dsh-desktop-bundle`）；其提示词分节与浏览器标记见 [`packages/desktop/desktop-bundle`](../packages/desktop/desktop-bundle/README.md) 与 [`packages/client/desktop`](../packages/client/desktop/README.md)。

## 已知限制

- **没有代码签名、自动更新或通知**——产物未签名；mac 分发需要 `tauri icon` 生成的 `icons/icon.icns`。
- **没有单实例保护**——启动两次外壳会在同一端口上再起一个宿主；请通过 `DSH_DESKTOP_ARGS` 传入 `--port`。
- **Windows 上的进程树终止是尽力而为**——终止 Node 子进程可能残留孙进程；普通情况下宿主会随外壳退出。
