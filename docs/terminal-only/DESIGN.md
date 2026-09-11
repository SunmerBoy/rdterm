# rdterm —— RustDesk 终端精简版 二次开发设计

> 基线：`rustdesk` v1.5.0，commit `65edf21`
> 目标：**去掉图形界面、去掉远程屏幕采集，仅保留远程终端（CMD/PowerShell/PTY），纯绿色安装，单程序双向**。

---

## 1. 目标与非目标

### 目标
| 编号 | 目标 | 说明 |
|---|---|---|
| G1 | 单 exe 双向 | 同一个可执行文件既能当**被控端**（只暴露终端），也能当**控制端**（命令行连对方、拿到一个可交互 shell） |
| G2 | 无 GUI | 不编译 Flutter，不编译 Sciter，不创建窗口，不驻留托盘 |
| G3 | 无屏幕采集 | 不编译 video_service / display_service / camera / 编解码；不采集、不传输、不渲染任何一帧图像 |
| G4 | 仅保留远程终端 | 保留 `ConnType::TERMINAL` 全链路（登录 → PTY 拉起 → 双向流 → resize/断开） |
| G5 | 纯绿色 | 配置、密钥、日志全部落在 exe 同目录；不写注册表、不装系统服务、不往 `%APPDATA%` 写任何东西 |

### 非目标（本阶段不做）
- 跨平台：先把 **Windows x86_64** 打通，Linux/macOS 后续再说。
- 文件传输、剪贴板同步、音频、端口转发、RDP、打印机、隐私模式、虚拟显示器 —— 全部裁掉。
- 上游同步：这是 fork，不追求能干净 rebase 上游。

---

## 2. 现状调研结论

### 2.1 入口链路
```
src/main.rs
  ├─ feature="flutter" ──► common::global_init() + flutter 引擎（GUI）
  └─ 否则 ──────────────► core_main::core_main() → Some(args) → ui::start(args)   // Sciter GUI
```
* `src/lib.rs` 里 `pub mod ui;` 已被 `#[cfg(not(feature="flutter"))]` 门控 —— 无 flutter 时走 Sciter，**仍然有 GUI**。这是 G2 要改的第一个点。

### 2.2 终端服务端（被控端，保留）
* `src/server/terminal_service.rs`（2191 行）—— PTY 会话管理、输出缓冲、重连回放、SIGWINCH 两阶段重绘。
* `src/server/terminal_helper.rs`（Windows）—— 默认 shell 选择：`pwsh.exe` → `powershell.exe` → `%COMSPEC%`（cmd.exe）；UTF-8 通过 `chcp 65001` 配置；另有提权 helper 管道。
* `src/server/connection.rs:1965` 附近：`let mut terminal = cfg!(not(android/ios));` 决定是否在特性位里声明支持终端。
* `src/server/connection.rs:2086`：`else if self.terminal { self.init_terminal_service().await; }`

### 2.3 终端客户端（控制端，需要新建）
* 会话类型：`ConnType::TERMINAL`（`src/flutter.rs:1283` 由 `is_terminal` 映射而来）。
* 会话对象：`ui_session_interface::Session<T: InvokeUiSession>`，`src/ui_session_interface.rs:61`。
* 消息循环：`client::io_loop::Remote::io_loop(&mut self, key, token, round)` —— `src/client/io_loop.rs:151`。
* 启动方式（Flutter 版，CLI 要照抄这段）：
  ```rust
  // src/flutter.rs:1265 session_add()
  let session: Session<FlutterHandler> = Session { password, ..Default::default() };
  session.lc.write().unwrap().initialize(id, conn_type, switch_uuid, force_relay, ...);
  sessions::insert_session(session_id, conn_type, session);
  // src/flutter.rs:1352 session_start_()
  std::thread::spawn(move || { let round = ...new_round(); io_loop(session, round); });
  ```
* **关键工作量**：`InvokeUiSession`（`src/ui_session_interface.rs:1679`）有约 40 个方法且**大多无默认实现**，
  `client::Interface`（`src/client.rs:4641`）也有十几个。CLI 必须提供一个 `ConsoleHandler` 把这两个 trait 都实现掉
  （绝大多数写成 no-op / 打到 stderr）。

### 2.4 需要裁掉的服务（服务端 `Server::new()`，`src/server.rs:105`）
```rust
server.add_service(Box::new(audio_service::new()));            // 删
server.add_service(Box::new(display_service::new()));          // 删（屏幕采集）
server.add_service(Box::new(clipboard_service::new(..)));      // 删
server.add_service(Box::new(input_service::new_cursor()));     // 删
server.add_service(Box::new(input_service::new_pos()));        // 删
server.add_service(Box::new(input_service::new_window_focus()));// 删
printer_service                                                // 删（本就 gated by flutter）
```

### 2.5 依赖瘦身：vcpkg 可以整个砍掉
`vcpkg.json` 里的依赖 **全是** 音视频/屏幕采集栈：
`aom`、`libjpeg-turbo`、`opus`、`libvpx`、`libyuv`、`mfx-dispatch`、`ffmpeg`。
裁掉视频/音频后 **不再需要 vcpkg**，`hbb_common` 侧只剩 `sodiumoxide`（cc 编译，MSVC cl 即可）和
`native-tls`（Windows 走 schannel，无 openssl）。
→ **构建前置从「Rust + vcpkg + 一堆 C 库」降为「Rust + MSVC」**，这是本次精简最大的工程收益。

---

## 3. 架构设计

### 3.1 Cargo feature：`terminal-only`
所有裁剪用 `#[cfg(feature = "terminal-only")]` 门控，**默认关闭**，保证原版仍可编译，便于对照回归。

```toml
# Cargo.toml
scrap       = { path = "libs/scrap", features = ["wayland"], optional = true }  # 改为 optional
magnum-opus = { git = "...", optional = true }                                  # 改为 optional

[features]
default       = ["use_dasp", "dep:scrap", "dep:magnum-opus"]
hwcodec       = ["scrap?/hwcodec"]        # 注意 ? 语法：不反向启用可选依赖
vram          = ["scrap?/vram"]
mediacodec    = ["scrap?/mediacodec"]
drm           = ["scrap?/drm"]
linux-pkg-config = ["magnum-opus/linux-pkg-config", "scrap?/linux-pkg-config"]
terminal-only = []
```

### 3.2 新二进制：`rdterm`
```toml
[[bin]]
name = "rdterm"
path = "src/cli.rs"
```
`src/cli.rs` 职责：
1. **最先**执行绿色化：把 `config::APP_DIR` 设为 `current_exe()` 所在目录，并把 `APP_NAME` 设为 `rdterm`（与已安装的 RustDesk 隔离）。
2. `common::global_init()`。
3. 解析参数，二选一：
   * 被控端：`rdterm [--server]` → `start_server(true, false)`，控制台打印 ID / 一次性密码 / 中继状态，Ctrl-C 退出。
   * 控制端：`rdterm --connect <PEER_ID> [--password <PWD>]` → 建 `ConnType::TERMINAL` 会话，`io_loop` 跑起来，
     把远端 PTY 输出写 stdout、本地 stdin 转发给远端。

### 3.3 目录/模块布局
```
src/cli.rs            入口 + 参数解析 + 绿色化
src/cli/
  mod.rs
  handler.rs          ConsoleHandler：实现 InvokeUiSession + client::Interface
  server_mode.rs      被控端前台模式
  client_mode.rs      控制端交互模式（stdin/stdout 桥接 PTY）
```

### 3.4 门控清单（第一版）
| 文件 | 位置 | 动作 |
|---|---|---|
| `src/lib.rs` | `pub mod ui;` | 追加 `and(not(feature="terminal-only"))` |
| `src/lib.rs` | `mod tray/updater/whiteboard/clipboard_file/privacy_mode/virtual_display_manager` | 同上 |
| `src/server.rs` | `pub mod audio_service/display_service/video_service/clipboard_service/input_service` | 同上 |
| `src/server.rs` | `Server::new()` 里的 `add_service` 调用 | 同上 |
| `src/server.rs` | `use scrap::camera;` / `use video_service::VideoSource;` | 同上 |
| `src/server.rs` | `start_server()` 里 `input_service::fix_key_down_timeout_loop()` | 同上 |
| `src/client.rs` | `AudioHandler` / `VideoHandler` / `start_audio_thread` | 同上 |
| `src/client/io_loop.rs` | `audio_sender:`、视频/音频帧处理分支 | 同上 |
| `src/ui_session_interface.rs` | `on_rgba` / `get_rgba` / `next_rgba`（签名带 `scrap::ImageRgb`） | 同上 |

---

## 4. 绿色安装方案

**问题**：`hbb_common::config::Config::path()`（`libs/hbb_common/src/config.rs:780`）在桌面端只认
`directories_next::ProjectDirs`（Windows → `%APPDATA%\<APP_NAME>\config`），`APP_DIR` 只在 Android/iOS 分支生效。

**方案**：`libs/hbb_common` 是 git submodule，改起来别扭且推不上去（本机 github 被墙）。
→ **把 `libs/hbb_common` 从 submodule 内联（vendor）进主仓库**，然后给 `Config::path()` 加一个早退分支：

```rust
pub fn path<P: AsRef<Path>>(p: P) -> PathBuf {
    // 绿色模式：APP_DIR 一旦被设置，一切配置/日志都落在它下面
    {
        let dir = APP_DIR.read().unwrap();
        if !dir.is_empty() {
            let mut path: PathBuf = dir.clone().into();
            path.push(p);
            return path;
        }
    }
    ... 原逻辑
}
```
同理 `Config::log_path()`。
`src/cli.rs` 启动时：
```rust
let exe_dir = std::env::current_exe()?.parent().unwrap().to_path_buf();
*hbb_common::config::APP_DIR.write().unwrap() = exe_dir.to_string_lossy().into_owned();
*hbb_common::config::APP_NAME.write().unwrap() = "rdterm".into();
```

**配套**：`--server` 不注册系统服务、不写 `HKEY_*`；日志目录、`.toml` 配置、密钥对全部落在 exe 同目录。

---

## 5. 实施步骤与当前状态

| 步骤 | 内容 | 状态 |
|---|---|---|
| S0 | 环境：github 镜像（ghproxy）+ Rust 1.98.1 + cargo `git-fetch-with-cli` + hbb_common 子模块 + MSVC 生成工具 | ✅ 完成 |
| S2 | Cargo.toml：`terminal-only` feature、`scrap`/`magnum-opus` 转 optional、新增 `[[bin]] rdterm` | ✅ 完成 |
| S3 | `src/cli/mod.rs` + `src/rdterm.rs`：参数解析、绿色化、被控端模式 | ✅ 骨架完成 |
| S6a | lib.rs 门控：`ui` / `tray` / `updater` / `whiteboard` / `clipboard` / `clipboard_file` / `privacy_mode` / `virtual_display_manager` | ✅ 完成 |
| S6b | 服务端桩：`src/server/no_service.rs` 提供 `video_service` / `display_service` / `audio_service` / `camera` 空实现 | ✅ 完成 |
| S1 | **confy 已从 hbb_common 移除**（用 `toml` 库原地替换读写），并给 `Config::path()/log_path()` 加桌面端 `APP_DIR` 早退 → 真正绿色；config.rs 已改 | ✅ 完成 |
| S7 | 客户端门控：`client.rs`（`VideoHandler` / `AudioHandler` / `Recorder` / `update_supported_decodings`）、`io_loop.rs`（视频线程、音频线程） | ✅ 基本完成（io_loop 视频线程最终 gate 待收尾） |
| S4 | `ConsoleHandler`：实现 `InvokeUiSession`（约 40 个方法）+ `client::Interface` | ✅ 完成（`src/cli/console.rs`） |
| S5 | 控制端模式：stdin/stdout ↔ 远端 PTY 桥接，窗口大小同步（SIGWINCH） | ✅ 完成（`src/cli/remote.rs`） |
| S8 | 发布：`cargo build --release --no-default-features --features terminal-only --bin rdterm` | ✅ 完成（单 exe ≈ 11.3 MB） |
| S9 | 被控端窗口隐藏：`--hide` / `--minimize` / `--stop`（`GetConsoleWindow` + `ShowWindow`，仅独立控制台时隐藏否则最小化），ID 写 `rdterm-identity.txt` | ✅ 完成 |
| S10 | 免密 + 随机 ID：服务端 `connection.rs` 无密码短路放行（不预置 `authorized`，否则 `send_logon_response_and_keep_alive` 直接 return 吞掉登录响应）、控制端 `client.rs` 空密码直连；`src/cli/identity.rs` 12 位密码学随机 ID + 12h 轮换 | ✅ 完成 |
| S11 | IP 直连：`Config::set_option("direct-server","Y")` 默认开启，监听 `0.0.0.0:21118`，`--no-direct-server` 可关；`peer` 填 IP 走 `connect_tcp_local` | ✅ 完成 |
| S12 | MCP Server：`src/cli/mcp.rs` 手写 JSON-RPC 2.0（零新依赖），stdio（`--mcp`）+ HTTP（`--mcp-http`，`POST /mcp`、`GET /health`）双传输；7 工具 `rdterm_identity/connect/exec/read/sessions/resize/close` | ✅ 完成（本机 + 跨机实测通过） |
| S13 | 接管脚本与文档：`tools/`（mcp-connect.py / remote-ps.py / mcp-selftest.py / takeover-demo.py）、`README.rdterm.md`、本设计文档状态更新 | ✅ 完成 |

> **2026-09-10 构建环境重要修正（踩坑）**：
> 本机 shell（Git Bash）下 `cmd //c "cargo ..."` **不会真正执行 cargo**（vcvars 重设 PATH 把 cargo 冲掉 / `//c` 被改写导致 cmd 进交互模式），
> 之前两次"BUILD_EXIT=0"均为假象（日志里根本没有 cargo 的 Checking/Finished 输出，`Cargo.lock` 里 confy 也未被裁剪）。
> **正确做法**：在 bash 里直接 `export` MSVC 的 `PATH`/`INCLUDE`/`LIB` 后运行 cargo（不经过 cmd）：
> ```
> export PATH="$PATH:/c/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/VC/Tools/MSVC/14.44.35207/bin/Hostx64/x64:/c/Program Files (x86)/Windows Kits/10/bin/10.0.26100.0/x64"
> export INCLUDE="C:/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/VC/Tools/MSVC/14.44.35207/include;C:/Program Files (x86)/Windows Kits/10/Include/10.0.26100.0/um;C:/Program Files (x86)/Windows Kits/10/Include/10.0.26100.0/shared;C:/Program Files (x86)/Windows Kits/10/Include/10.0.26100.0/winrt;C:/Program Files (x86)/Windows Kits/10/Include/10.0.26100.0/cppwinrt"
> export LIB="C:/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/VC/Tools/MSVC/14.44.35207/lib/x64;C:/Program Files (x86)/Windows Kits/10/Lib/10.0.26100.0/um/x64;C:/Program Files (x86)/Windows Kits/10/Lib/10.0.26100.0/ucrt/x64"
> cargo build --release --no-default-features --features terminal-only --bin rdterm
> ```
> 验证：`cargo check --no-default-features --features terminal-only --bin rdterm`（输出写入 check4.log）。当前 check4 正在后台验证整条 terminal-only 链路能否编译。

> **构建命令必须带 `--no-default-features`**：`default` 里含 `dep:scrap`、`dep:magnum-opus`，
> 不带这个开关它们会被重新拉回，vcpkg 就白省了。

### S7 的推荐做法（已确认可行路径）
`scrap` 在 Windows 上的 `build.rs` **无条件**要求 vcpkg（ffmpeg / aom / libvpx / libyuv），无法靠 feature 关掉，
所以「不装 vcpkg」与「彻底移除 scrap」是同一件事，必须做到编译期移除。

服务端已用 `no_service.rs` 桩模块解决。客户端侧（`src/client.rs`、`src/client/io_loop.rs`）建议**直接门控**而不是打桩，
因为精简版的控制端永远不会收到视频/音频帧，需要处理的点集中在：
* `src/client.rs`：`use scrap::{...}`（L80）、`VideoHandler`（L2432 起）、`AudioHandler`（L2050 起）、
  `record_screen`、`update_supported_decodings`（L3682）
* `src/client/io_loop.rs`：`use scrap::CodecFormat`（L62）、`audio_sender` / `start_audio_thread`、
  视频线程创建（约 L2480）、`video_format` 相关分支（L384 / L1391）
* 其余 `scrap::codec::Encoder::*` / `test_av1()` / `scrap::record::RecordState` 调用点逐条 `cfg` 门控

> 备选方案：写一个 `src/scrap_stub.rs` 提供同名类型（`CodecFormat` / `ImageRgb` / `ImageFormat` / `Recorder` …），
> 再在每个用到 `scrap::` 的文件顶部加 `#[cfg(feature="terminal-only")] use crate::scrap_stub as scrap;`。
> 改动点更少，但要精确对齐 `ImageRgb::new()`、`CodecFormat::from(&VideoFrame)` 等签名。

---

## 6. 风险与未决

1. **`InvokeUiSession` 40 个方法**：改动上游签名时会牵连 CLI handler，属于可接受维护成本；后续可考虑给 trait 加默认实现反向上游。
2. ~~**终端登录鉴权**~~：已在 S10 解决 —— 去掉密码校验，ID 即凭证（随机 12 位 + 12h 轮换）。
3. **Windows ConPTY 与本地控制台**：已解决 —— 控制端不下发 ANSI 重绘，`Remove-Module PSReadLine` 消除 PowerShell 回显碎片；`exec()` 采用「先命令后哨兵」两步发送 + 整行砍掉哨兵回显；ENTER 仅发 `\r`。
4. ~~**vcpkg 是否已完全不需要**~~：已确认不需要，`scrap` 整体不参与编译（optional 未启用 + 服务端 `no_service.rs` 桩）。

---

## 7. 实施记录补充（2026-09-12）

### 7.1 关键踩坑

| 坑 | 现象 | 修法 |
|---|---|---|
| `self.authorized=true` 预置 | 服务端打印 "accept login without password" 但客户端永远超时：`send_logon_response_and_keep_alive()` 第一行 `if self.authorized {return true}` 直接返回，登录响应根本没发出去 | 免密短路里**不预置** `authorized`，保持默认 false，让正常登录流程继续 |
| LNK1104 | 链接失败：exe 被残留的 `mcp-connect.py` 控制进程占用 | 编译前 `Get-Process rdterm \| Stop-Process -Force` |
| exec 输出错位 | 命令与哨兵一起发被 ConPTY 拼乱；`\r\n` 多出的 `\n` 触发 PowerShell 续行符 `>>` | 两步发送：先发命令等静默，再发 `echo <marker>\r`；ENTER 仅发 `\r` |
| PSReadLine 重绘碎片 | 长命令回显被 ANSI 重绘切成碎片 | 控制端 `init_shell()` 下发 `Remove-Module PSReadLine -Force` |
| 假构建成功 | Git Bash 下 `cmd //c "cargo ..."` 不真正执行 cargo，`BUILD_EXIT=0` 是假象 | 见第 5 节 2026-09-10 修正：直接在 bash 里 export 后跑 cargo，不经过 cmd |
| `RmDir` 别名冲突 | 自定义 PS 函数名被 `RmDir` 别名抢先解析，静默失败 | 用 `Remove-Item -LiteralPath` 逐项操作并验证 |

### 7.2 仓库形态

- `libs/hbb_common` 已从 git submodule **内联（vendor）进主仓库**（含 confy 移除 + `Config::path()/log_path()` 的 `APP_DIR` 绿色化早退），`.gitmodules` 删除 —— 仓库自包含，clone 即可编译。
- `libs/portable-pty` 为 vendored PTY 依赖。
- 配套接管脚本收敛至 `tools/`，使用文档见根目录 `README.rdterm.md`。
5. **ID 稳定性**：绿色版每次换目录即换配置 → 换 ID。如需固定 ID，允许 `--config-dir` 指定或把配置写回同目录（默认已如此）。
