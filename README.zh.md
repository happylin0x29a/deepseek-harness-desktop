# dsh-desktop — Tauri 外壳

[English](README.md) | 中文

DeepSeek Harness 的桌面外壳：一个 Tauri 2 应用，启动 `dsh web`、等待就绪行，然后在原生窗口中显示同一个 Web GUI。客户端还是网页的选择只是外壳选择——普通浏览器标签页通过同一个本地服务器显示同一个会话。

## 工作原理

1. 外壳立即打开一个小型加载窗口，解析宿主进程（顺序：`DSH_BIN` 环境变量 → `PATH` 上的 `dsh` → 首次运行引导），并把启动状态流式显示在窗口里（启动宿主 → 等待服务器 → 打开界面）；同一阶段也会同步到窗口标题栏，页面加载前就能看到进度。Windows 上子进程以 `CREATE_NO_WINDOW` 运行，应用旁不会出现控制台窗口。
2. web runtime 在其 Loader 树稳定后打印 `dsh web: http://127.0.0.1:PORT`；外壳解析该行。
3. URL 到达后，加载窗口放大到完整尺寸并直接落在那条界面上——没有选择步骤，界面也不做 IPC。宿主启动失败、退出且未发布 URL、或超过就绪超时（`DSH_READY_TIMEOUT_MS`，默认 300 秒，首次引导可能较慢）时，原因（含宿主最近的 stderr）会保留在加载窗口上，完整时间线写入 `%TEMP%\dsh-desktop.log`。
4. 外壳退出时，被启动的宿主进程会被终止。

## 首次运行：自动下载运行时

安装包只包含外壳本身，**不内置** Node.js 或 dsh。首次启动时，如果 `PATH` 上既没有 `dsh` 也没有 `DSH_BIN`，外壳会从国内镜像自动引导运行时：

1. **复用本机环境**：如果 `PATH` 上有满足 dsh 引擎要求的 node + npm（Node 22.19+ 的 22.x，或任意 24+），则**跳过 Node 下载**，直接用本机 node/npm；
2. 否则下载固定的 Node.js（`22.19.0`，Windows x64 约 35 MB / macOS arm64 约 30 MB，SHA-256 校验）并解压到运行时目录；
3. 用 npm 从镜像 registry 安装固定版本 `@deepseek-ai/dsh@0.1.0-rc.6`（首次约 200 MB 磁盘占用）；
4. 以 `node <运行时>/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js web` 启动宿主（本机 node 合规时使用本机 node）。

运行时目录：Windows `%LOCALAPPDATA%\dsh-desktop\runtime`；macOS `~/Library/Application Support/dsh-desktop/runtime`。再次启动时直接复用已装好的运行时。

镜像与版本可用环境变量覆盖：

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `DSH_NODE_MIRROR` | `https://npmmirror.com/mirrors/node` | Node.js 发行版镜像 |
| `DSH_NPM_REGISTRY` | `https://registry.npmmirror.com` | npm registry 镜像 |
| `DSH_NODE_VERSION` | `22.19.0` | Node 版本（覆盖后跳过校验和验证） |
| `DSH_BIN` | — | 宿主命令（开发用），例如 `node ..\apps\cli\lib\bin.js`；设置后跳过引导 |
| `DSH_DESKTOP_ARGS` | — | 追加给 `dsh web` 的额外参数，例如 `--port 8080` |
| `DSH_READY_TIMEOUT_MS` | `300000` | 宿主启动后就绪超时（毫秒） |
| `DSH_BOOTSTRAP_TIMEOUT_MS` | `1200000` | 首次下载/安装运行时的超时（毫秒，默认 20 分钟） |

## 图标

鲸鱼图标由仓库 favicon（`apps/web/public/favicon.svg`）渲染进 `src-tauri/icons`；用 `pnpm tauri icon whale-source.png` 重新生成。

## 前置条件

- 无：首次运行会自动下载 Node.js 与 dsh（需要网络）。国内用户默认走 npmmirror 镜像。
- 构建需要：Rust stable、[tauri.app](https://tauri.app/start/prerequisites/) 所列平台前置条件（Windows 上的 WebView2，macOS 的 Xcode 命令行工具），以及 pnpm（Tauri CLI 开发依赖）。

## 平台支持

- **Windows x64**（安装包 NSIS/MSI）与 **macOS Apple Silicon / Intel**（DMG/app）：首次启动自动下载对应架构的 Node.js（win-x64 zip / darwin-arm64 / darwin-x64 tar.gz，SHA-256 校验）并安装 dsh，无需任何前置。
- **macOS 提示**：运行时目录为 `~/Library/Application Support/dsh-desktop/runtime`；下载的 Node 是普通用户态文件，不需要管理员权限。
- **其他平台（Linux 等）**：不提供安装包，也不支持自动下载运行时；请自行安装 Node.js 与 `@deepseek-ai/dsh`，或用 `DSH_BIN` 指向已有的 dsh 启动器。

## 运行

```sh
# 已安装部署：无需任何前置，首次启动自动下载运行时
pnpm tauri dev

# 源码检出：用 DSH_BIN 指向已构建的 CLI（相对本目录）
$env:DSH_BIN = "node ..\apps\cli\lib\bin.js"
$env:DSH_DESKTOP_ARGS = "--port 8080"   # optional
pnpm tauri dev

# production bundle
pnpm tauri build
```

外壳启动的就是标准 `web` profile（等价于 `dsh web` / `dsh --profile web`）——桌面窗口与浏览器标签页看到的是同一个界面，行为完全一致。

## 故障排查

- **一直"等待网络响应"**：系统代理（Clash 等工具的 TUN/系统代理模式）会拦截到镜像的连接，且代理节点访问 npmmirror 常常卡住。关闭代理或把 `npmmirror.com` 与 `registry.npmmirror.com` 加入代理工具的绕过列表即可。启动日志（`%TEMP%\dsh-desktop.log`）会记录检测到的代理环境变量（凭据已脱敏）。
- **网络不可达**：确认能访问 `https://registry.npmmirror.com`；可通过 `DSH_NPM_REGISTRY` / `DSH_NODE_MIRROR` 换成其他镜像源。
- **首次安装中途关窗**：已安装的 dsh（版本校验通过）会在下次启动时直接复用，不会重装；未完成的安装会从 npm 缓存（`<runtime>\npm-cache`）继续。

## 已知限制

- **没有代码签名、自动更新或通知**——产物未签名；mac 分发需要 `tauri icon` 生成的 `icons/icon.icns`。
- **没有单实例保护**——启动两次外壳会在同一端口上再起一个宿主；请通过 `DSH_DESKTOP_ARGS` 传入 `--port`。首次引导期间请勿并行启动两个实例，避免同时写入运行时目录。
- **Windows 上的进程树终止是尽力而为**——终止 Node 子进程可能残留孙进程；普通情况下宿主会随外壳退出。
- **首次启动需要网络**——下载 Node 与 dsh 来自国内镜像；无法访问镜像时请预先安装 `@deepseek-ai/dsh`（`npm i -g @deepseek-ai/dsh`）或设置 `DSH_BIN`。
