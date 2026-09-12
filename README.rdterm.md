# rdterm —— RustDesk 终端精简版

> 基于 RustDesk v1.5.0（commit `65edf21`）二次开发。
> **无图形界面、无屏幕采集，仅保留远程终端；单 exe 双向（被控端 + 控制端 + MCP Server）；纯绿色，免安装。**

设计文档见 [`docs/terminal-only/DESIGN.md`](docs/terminal-only/DESIGN.md)，Agent 接管配套脚本见 [`tools/`](tools/)。

---

## 特性

| 特性 | 说明 |
|---|---|
| 单 exe 双向 | 同一个 `rdterm.exe` 既可当被控端（只暴露终端），也可当控制端（拿一个可交互 shell），还内置 MCP Server |
| 无 GUI / 无采集 | 不编译 Flutter/Sciter，不采集、不传输任何一帧图像/音频（`terminal-only` feature 全量裁剪） |
| 纯绿色 | 配置、密钥、日志全部落在 exe 同目录；不写注册表、不装服务、不写 `%APPDATA%` |
| 免密登录 | 去掉密码校验，**凭证就是 ID**（服务端短路放行 + 控制端跳过密码弹框） |
| 随机 ID + 12h 轮换 | 12 位密码学随机 ID（非 MAC 派生），存 `rdterm-id.json`，每 12 小时自动轮换并重新注册，旧 ID 立即失效 |
| IP 直连 | 默认监听 `0.0.0.0:21118`，`rdterm_connect` 填 IP 可绕过中继/ID 服务器直连 |
| MCP 接管 | 手写 JSON-RPC 2.0，零新增依赖；stdio（`--mcp`）与 HTTP（`--mcp-http`）双传输，Agent 可直接接管远端 shell |
| 体积极小 | release 单文件约 11 MB，不需要 vcpkg / ffmpeg / aom 等音视频依赖，构建前置降为「Rust + MSVC」 |

---

## 快速开始

### 被控端

```bat
rdterm.exe                       :: 双击/无参数 → 交互式菜单
rdterm.exe --server              :: 被控端模式，前台运行，打印本机 ID
rdterm.exe --hide                :: 被控端 + 完全隐藏窗口（后台运行）
rdterm.exe --minimize            :: 被控端 + 最小化到任务栏
rdterm.exe --stop                :: 结束后台被控端（读 rdterm.pid）
rdterm.exe --id                  :: 只打印本机 ID 后退出
```

> `--hide` 后看不到 ID，程序会写到配置目录的 `rdterm-identity.txt`。

### 控制端

```bat
rdterm.exe --connect <PEER_ID>            :: 连到对端，打开交互式远程终端
rdterm.exe --connect 192.168.1.10         :: 填 IP 走 21118 直连
```

### MCP 接管（Agent / 自动化）

被控端机器运行 `rdterm.exe --server` 后，控制侧：

```bat
:: 方式一：stdio（本脚本内置拉起 rdterm.exe --mcp）
python tools\mcp-connect.py <ID或IP>              :: 进入交互式远程终端
python tools\mcp-connect.py <ID或IP> "hostname" "ipconfig /all"

:: 方式二：HTTP
rdterm.exe --mcp-http 127.0.0.1:8787              :: POST /mcp 走 JSON-RPC，GET /health 探活
```

接入 Claude / WorkBuddy 等 MCP 宿主（stdio），参考 [`tools/mcp-example.json`](tools/mcp-example.json)：

```json
{ "mcpServers": { "rdterm": { "command": "D:\\path\\to\\rdterm.exe", "args": ["--mcp"] } } }
```

---

## 命令行参数一览

| 参数 | 作用 |
|---|---|
| （无参数）/ `--menu` | 交互式菜单 |
| `--server` / `-s` | 被控端模式（打印 ID，Ctrl-C 退出） |
| `--hide` / `--minimize` | 被控端窗口完全隐藏 / 最小化（仅影响自身控制台） |
| `--stop` | 结束后台被控端 |
| `--id` | 打印本机 ID |
| `--connect <PEER_ID> [--password <PWD>]` | 控制端连接并开远程终端 |
| `--mcp` | MCP Server（stdio） |
| `--mcp-http [ADDR]` | MCP Server（HTTP，默认 `127.0.0.1:8787`） |
| `--mcp --server` | MCP + 同时起被控端 |
| `--no-direct-server` | 关闭 21118 直连监听 |
| `--config-dir <DIR>` | 指定配置目录（默认 exe 所在目录） |
| `--log[=LEVEL]` | 内部日志打到 stderr，排障用 |
| `--set-password <PWD>` | 已废弃（本版本不校验密码） |

## MCP 工具（7 个）

| 工具 | 说明 |
|---|---|
| `rdterm_identity` | 返回本机 ID / 轮换剩余 TTL 等身份信息 |
| `rdterm_connect` | 连接对端（ID 或 IP），参数 `peer` / `rows` / `cols`，返回 `session` |
| `rdterm_exec` | 在会话里执行一条命令并等待结果（哨兵判定 + ANSI 清洗）。注意：外层是 PowerShell，含 `$` 变量或嵌套引号的复杂命令推荐 `powershell -NoProfile -EncodedCommand <UTF-16LE base64>` |
| `rdterm_read` | 读取会话当前缓冲输出（交互式场景） |
| `rdterm_sessions` | 列出当前活动会话 |
| `rdterm_resize` | 调整远端 PTY 行列 |
| `rdterm_close` | 关闭会话 |

---

## 构建方法

前置：Rust（MSVC toolchain）+ Visual Studio 2022 BuildTools + Win10 SDK；**不需要 vcpkg**。

```bash
# bash（Git Bash）下直接 export，不要经过 cmd
export PATH="/c/Users/<你>/.cargo/bin:$PATH"
export INCLUDE="<VS>/VC/Tools/MSVC/<ver>/include;<SDK>/Include/{um,shared,winrt,cppwinrt}"
export LIB="<VS>/VC/Tools/MSVC/<ver>/lib/x64;<SDK>/Lib/{um,ucrt}/x64"

cargo build --release --no-default-features --features terminal-only --bin rdterm
```

> 必须带 `--no-default-features`：default feature 会把 scrap / magnum-opus（音视频栈）拉回来。

## 目录结构

```
src/rdterm.rs            rdterm 二进制入口
src/cli/mod.rs           参数解析 / 菜单 / 窗口隐藏 / direct-server / 绿色化
src/cli/console.rs       ConsoleHandler（实现 InvokeUiSession + client::Interface）
src/cli/identity.rs      随机 ID + 12h 轮换线程
src/cli/remote.rs        RemoteSession（控制端会话层：exec 哨兵 / strip_ansi / PSReadLine 治理）
src/cli/mcp.rs           MCP Server（stdio + HTTP 双传输，手写 JSON-RPC 2.0）
src/server/no_service.rs 音视频/采集服务端空桩
libs/hbb_common/         已从 submodule 内联（含绿色化 APP_DIR 修改）
libs/portable-pty/       vendored PTY 库
tools/                   Agent 接管配套脚本（mcp-connect.py / remote-ps.py / ...）
docs/terminal-only/      设计文档
```

## tools/ 配套脚本

| 脚本 | 用途 |
|---|---|
| `mcp-connect.py` | 主力接管入口：`python mcp-connect.py <ID或IP> [cmd...]`，内部拉起 `rdterm.exe --mcp` 走 stdio |
| `remote-ps.py` | 通用远程 PowerShell 执行器（脚本 base64 下发，绕开引号转义） |
| `mcp-selftest.py` | MCP stdio 全流程自检（initialize → tools/list → connect/exec） |
| `takeover-demo.py` | 端到端接管演示 |
| `mcp-example.json` | MCP 宿主接入配置示例 |

## 安全须知（务必阅读）

- **本版本去掉了密码校验，ID 即唯一凭证**：拿到 ID 的任何人即可取得该机 shell。
  ID 每 12 小时轮换可缩小暴露窗口，但**请不要把 ID 泄露到不受信任的渠道**。
- 默认开启 `0.0.0.0:21118` 直连监听；同网段设备可直接连。不需要时加 `--no-direct-server`。
- 仅供授权环境下的远程运维 / 取证 / 自动化使用，请遵守当地法律法规。

## 已知限制 / 待办

- S7 客户端 `io_loop` 视频线程的最终 gate、控制端窗口 resize 与远端 PTY 同步、噪音日志清理、`--stop` 在隐藏模式下的健壮性。
- 目前仅支持 Windows x86_64；Linux/macOS 未适配。
- 上游为 fork 快照，不保证能干净 rebase rustdesk 上游。
- 2026-09-12 已通过 22 项实测矩阵（`tools/mcp-test-matrix.py`，对真实远端）：命令时序、长输出、多会话隔离、resize、中文、长命令轮询等全部通过；回归可直接 `python tools/mcp-test-matrix.py <ID或IP>`。
