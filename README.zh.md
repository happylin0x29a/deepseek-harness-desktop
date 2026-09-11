# dsh-desktop — Tauri 外壳

[English](README.md) | 中文

DeepSeek Harness 的桌面外壳：一个 Tauri 2 应用，启动 `dsh web`、等待就绪行，然后在原生窗口中显示同一个 Web GUI。客户端还是网页的选择只是外壳选择——普通浏览器标签页通过同一个本地服务器显示同一个会话。

## 工作原理

1. 外壳立即打开一个小型加载窗口，解析宿主进程（顺序：`DSH_BIN` 环境变量 → `PATH` 上的 `dsh` → 首次运行引导），并把启动状态流式显示在窗口里（启动宿主 → 等待服务器 → 打开界面）；同一阶段也会同步到窗口标题栏，页面加载前就能看到进度。Windows 上子进程以 `CREATE_NO_WINDOW` 运行，应用旁不会出现控制台窗口。
2. web runtime 在其 Loader 树稳定后打印 `dsh web: http://127.0.0.1:PORT`；外壳解析该行。外壳自己就是那块界面，所以会加 `--no-open` 让宿主**不要**再往系统默认浏览器里塞一份——但只对声明了该开关的 `dsh` 加：0.1.5 之前的 `dsh-web-app` 没有这个选项（那些版本本来也不打开浏览器），而 `commander` 遇到未知选项会直接报错退出，因此外壳读的是**即将运行的那个 launcher 自己的** `dsh-web-app`，读不到就按"不支持"处理。
3. URL 到达后，加载窗口放大到完整尺寸并直接落在那条界面上——没有选择步骤，界面也不做 IPC。宿主启动失败、退出且未发布 URL、或超过就绪超时（`DSH_READY_TIMEOUT_MS`，默认 300 秒，首次引导可能较慢）时，原因（含宿主最近的 stderr）会保留在加载窗口上，完整时间线写入 `%TEMP%\dsh-desktop.log`。
4. **点窗口的关闭按钮（×）不会退出程序，而是缩到托盘**——窗口只是"界面"，不是"程序"。如果外壳自己启动过宿主，关掉进程就等于关掉那个引擎；而你可能在浏览器标签页里正用着同一个引擎，那个标签页会一起失效。所以 × 只隐藏窗口，引擎继续在托盘后面服务；**只有托盘菜单里的"退出（同时关闭引擎）"才真正结束进程**，并终止外壳**自己启动的**宿主。接入或复用的服务不属于外壳，两种退出方式都不会动它。托盘图标左键点回窗口，右键出菜单；窗口已不存在时（异常情况）不会留下没有界面的僵尸进程，那种情况按退出处理。
5. 退出时，被外壳启动的宿主进程会被终止（`Drop for DesktopState` 是常规路径，`RunEvent::Exit` 是兜底，防止跳过析构的退出把宿主变成孤儿）。

## 更新引擎（窗口标题栏左上角）

窗口改成**无边框**（`decorations: false`），标题栏由注入脚本自己画：左边是应用图标 + **更新引擎** 按钮，右边是最小化/最大化/关闭，整条可拖动、双击最大化。Windows 上原生标题栏里放不进 webview 内容（`titleBarStyle` 只对 macOS 有效，wry 也没暴露 WebView2 的标题栏覆盖能力），所以要让按钮出现在窗口左上角，只能整条自绘。

这段脚本注入到**窗口显示的每一个页面**（`Builder::on_page_load`）：引擎自己的页面、以及启动页——因为无边框窗口否则在加载期间既不能移动也不能关闭。只有引擎页才带更新按钮那一半。页面被下推 32px，不与标题栏重叠。

窗口的几个动作走的是**外壳自己的命令**（`window_drag` / `window_minimize` / `window_toggle_maximize` / `window_close`），而不是给引擎页开 `core:window:*` 权限——把一条命令加进 `AppManifest` 是比把窗口插件的整套权限面交给远程源窄得多的授权。关闭按钮走的还是原生那条路：隐藏到托盘，只有托盘菜单里的"退出"才结束进程。

点按钮后：查镜像的发布通道 → 比对版本 → 用 npm 更新 → **重启外壳自己启动的那个引擎**（杀掉并重新拉起，窗口落到新引擎上）。npm 不提供百分比，所以进度按"哪个信号在动"推断，显示 `正在获取包元数据… / 正在下载依赖包… / 正在解压安装…` 加已下载字节、网速、已安装包数，结果在按钮下方浮层里显示。

**代价**：失去原生标题栏的一部分系统集成——Windows 11 最大化按钮悬停的"贴靠布局"面板不再出现（`Win`+方向键、拖到边缘贴靠仍可用），以及标题栏不再跟随系统主题。

两个刻意的设计：

- **不装 `@latest`**。这个包的 `latest` 故意停在一个**更旧**的 RC 上：实测 `latest = 0.1.5-rc.1`、`next = 0.1.5-rc.2`。照 `@latest` 装会把当前引擎**降级**。外壳读 `dist-tags`，在 `latest` 与 `next` 之间取 semver 更高者（两者都没有时退回到全部 tag 取最高），并且**只有严格更高才动手**——否则直接回"已是最新版本"，一个文件都不碰。
- **只重启自己拥有的引擎**。接入的服务不属于外壳（见上一条），杀掉它等于杀掉你在终端里起的进程；这种情况工具条显示"引擎由外部进程启动，请重启它以生效"，不擅自动手。想让桌面端接管，就先别在外部起引擎。

**安全边界**：引擎页面是 `http://127.0.0.1:…`，对 Tauri 而言是**远程源**，默认拿不到任何 IPC。`capabilities/engine-surface.json` 只给它三个外壳命令（`allow-engine-status` / `allow-engine-update` / `allow-toolbar-ready`），**不给任何 core 或插件权限**；这三个 `allow-*` 由 `build.rs` 的 `AppManifest::commands` 自动生成。本地启动页仍走 `default` 能力（`core:default` + `allow-startup-status`）。

## 首次运行：自动下载运行时

安装包只包含外壳本身，**不内置** Node.js 或 dsh。首次启动时，如果既没有 `dsh` 也没有 `DSH_BIN`，外壳会从国内镜像自动引导运行时：

1. **复用本机环境**：只要找得到满足 dsh 引擎要求的 node + npm（Node 22.19+ 的 22.x，或任意 24+），就**跳过 Node 下载**，直接用本机 node/npm。查找范围依次是：继承来的 `PATH` → 注册表中的机器级与用户级 `PATH`（展开 `%JAVA_HOME%` 之类的引用）→ 常见安装位置（官方安装器的 `nodejs` 目录、nvm-windows 的 `NVM_SYMLINK`、Volta、pnpm、`%APPDATA%\npm`）;
2. 否则下载固定的 Node.js（`22.19.0`，Windows x64 约 35 MB / macOS arm64 约 30 MB，SHA-256 校验）并解压到运行时目录；
3. 用 npm 从镜像 registry 安装固定版本 `@deepseek-ai/dsh@0.1.0-rc.6`（首次约 200 MB 磁盘占用）；
4. 以 `<node> <运行时>/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js web` 启动宿主；本机 node 合规时使用本机 node 的**绝对路径**。

> **为什么不能只看 `PATH`**：进程的 `PATH` 是**启动它的那一刻**的环境快照。从桌面或开始菜单快捷方式启动时，外壳继承的是 Explorer 在读取注册表时缓存的副本——在这之后新增到 `PATH` 的目录，在用户注销重新登录之前对该进程都是不可见的。因此只探测继承的 `PATH` 会在明明装了 Node.js 的机器上报“未找到 Node”，白白下载 35 MB 运行时。注册表 + 常见目录这两级回退就是为了消除这种误判。

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
- **提示 `credentials-local` 的 `version` 不是字符串**：这是旧版 dsh 留下的 `~/.dsh/.credentials.yaml` 格式。桌面外壳会在启动宿主前自动迁移为新版格式，并保留原文件为 `.credentials.yaml.legacy-v1`；若仍失败，请查看 `%TEMP%\dsh-desktop.log`。
- **启动时报 `Cannot find package '@deepseek-ai/…'`，看起来像依赖没装全**：其实一个依赖都不缺。dsh 的 profile 插件**不是**从自身 `node_modules` 解析的，而是从 `~/.dsh/profiles/node_modules` 这个由 dsh 生成的「扁平回退目录」解析（`boot()` 把 Loader 锚在 profile 目录，且 `bareModuleBaseUrl` 未设置，裸包名只能走 Node 的父目录查找）。该目录**被所有 dsh 安装共用**：谁启动 dsh，谁就按**自己的依赖闭包**重建一遍软链接，别人留下的链接既不改也不删。

  所以关键在于：**外壳必须和拥有这个目录的那个安装是同一个**。宿主解析顺序是 `DSH_BIN`（开发覆盖）→ `PATH` 上的 `dsh` → 首次引导下载内置运行时；`PATH` 上已有 dsh 时一律用它，正是为了让版本与链接归属保持一致。一旦那份安装被移动、重建、卸载，或正被 npm 重装（npm 会先删 `node_modules`），残留链接就指向不存在的目标——**对 Node 而言等于不存在**，于是每个插件都报 `Cannot find package`，表现为一段特定时间窗内的偶发启动失败。外壳会在启动前释放这类**无法服务的**外来链接（目标已消失，或目标虽在却读不出 `package.json`）——只删链接、绝不触碰目标，仍能正常解析的外来链接一律保留。日志会打印 `profile fallback: released …`。

  另一种成因是**整棵目录被瞬时独占**：该回退目录由大量 junction（重解析点）组成，安全软件扫描时会持有它若干秒，期间 Node 看到的每个插件都像不存在——但链接一条都没坏（这也是删除单条 junction 要花近一秒的同一现象）。所以宿主因解析失败**或毫无输出地退出**时，外壳会**先等待目录恢复可读（最多 15 秒），再退避重试，总共 3 次**，而不是立即重试一次。日志会依次打印 `profile fallback readable before prepare: …` 与 `… retrying (attempt n/3)`。

  **要让共享 `~/.dsh` 稳定成立，机器上只留一份 dsh 最省事**：`npm i -g @deepseek-ai/dsh` 后外壳会直接复用它（Windows 上也能正确识别 npm 全局 shim，不会再退回去下载内置运行时），版本与链接归属自然一致；会话、凭据、profile 也都在同一个 `~/.dsh` 里。

## 已知限制

- **没有代码签名、自动更新或通知**——产物未签名；mac 分发需要 `tauri icon` 生成的 `icons/icon.icns`。
- **启动时端口已被占用**：外壳启动前会探测配置端口（默认 3080），分四种情况处理。
  - **那里是一个 dsh 服务，且不带 token 就能打开** → 直接复用，不另起宿主（被复用的服务不属于外壳，关窗不会杀它）。
  - **那里是一个带 `?token=` 保护的 dsh** → **接上去**：外壳从 `~/.dsh/.credentials.yaml` 的 `records["client-connection/browser-session"]` 里取出本机共用的签名密钥，按 dsh 的 cookie 格式（名字 `dsh-auth-<sha256(authority)>`，值 `v1.<base64url body>.<base64url HMAC-SHA256>`）自己签一个，**先用 HTTP 验证它能过鉴权**，再写进窗口的 cookie 存储并打开那个服务。
    这是唯一能避免 `SessionAlreadyOwnedError` 的做法：两个引擎会列出同一批会话，但一个会话同时只能有一个写句柄，在第二个引擎里打开正在使用的会话就会报 `already owned by an active write handle`。桌面窗口和你自己跑的 `dsh web` 是**同一个引擎**时，这个问题不存在。
    签名密钥读不到、cookie 被服务拒绝、或写不进窗口，都会**自动退回**"用空闲端口起自己的宿主"，绝不会把窗口停在鉴权页上。
  - **那里是别的东西** → 改用空闲端口（`--port 0`）自己起一个宿主。这一步必须在启动宿主**之前**做：bind 失败的宿主**一行输出都不打印**、也永远不会就绪，事后靠 `EADDRINUSE` 兜底是接不住的。
  - 日志会依次打印 `attached to the running dsh on port …` / `port … is taken; starting the host on a free port instead`。
  - 想让桌面端跑一个**独立**引擎（刻意不接入已有的），把端口指到一个空闲端口即可：`DSH_DESKTOP_ARGS=--port 8080`。
- **没有单实例保护**——启动两次外壳会各起一个宿主（第二个自动落到空闲端口）；要在两者之间固定端口，请通过 `DSH_DESKTOP_ARGS` 传入 `--port`。首次引导期间请勿并行启动两个实例，避免同时写入运行时目录。
- **Windows 上的进程树终止是尽力而为**——终止 Node 子进程可能残留孙进程；普通情况下宿主会随外壳退出。
- **首次启动需要网络**——下载 Node 与 dsh 来自国内镜像；无法访问镜像时请预先安装 `@deepseek-ai/dsh`（`npm i -g @deepseek-ai/dsh`）或设置 `DSH_BIN`。
