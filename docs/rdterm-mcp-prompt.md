# rdterm MCP 接管提示词（供任意 Agent 直接使用）

> 用途：把下面整段内容作为系统提示词 / 任务说明注入给任何支持 MCP 的 Agent（Claude、WorkBuddy、Trae 等），
> 它即可完成 rdterm MCP 的配置并正确调用远程终端。

---

## 【提示词正文 —— 复制以下全部内容】

你具备通过 **rdterm MCP** 接管远程 Windows 终端的能力。rdterm 是 RustDesk 终端精简版：
无密码、**ID 即凭证**（12 位随机 ID，每 12 小时轮换），单 exe 同时提供被控端与 MCP Server。

### 一、MCP Server 配置

rdterm MCP 有两种传输方式，按场景二选一：

**方式 A：stdio（推荐，本机已装 rdterm.exe 时）**

```json
{
  "mcpServers": {
    "rdterm": {
      "command": "D:\\rdterm\\rdterm.exe",
      "args": ["--mcp"]
    }
  }
}
```

把 `command` 改成目标机器上 rdterm.exe 的实际路径。写入宿主的 MCP 配置文件
（如 Claude Desktop 的 `claude_desktop_config.json`、WorkBuddy 的 `~/.workbuddy/mcp.json` 的 `mcpServers` 节点），
保存后**重启宿主会话**并在连接器列表中信任/启用 `rdterm`。

**方式 B：HTTP（被控端与 Agent 不在同一台机器时）**

被控端先启动：`rdterm.exe --mcp-http 127.0.0.1:8787`（或 `--mcp --server` 同时开 MCP + 被控端）。
Agent 侧配置 HTTP 端点：`http://<被控端IP>:8787/mcp`（JSON-RPC 2.0，POST）；
探活：`GET http://<被控端IP>:8787/health`。
协议版本 `2024-11-05`，调用前需完成 `initialize` 握手 + `notifications/initialized`。

无第三方依赖；排障时可给 rdterm.exe 加 `--log` 看 stderr。

### 二、工具清单（7 个）

| 工具 | 必填参数 | 说明 |
|---|---|---|
| `rdterm_identity` | 无 | 本机 ID + 距下次 12h 轮换的剩余时间 |
| `rdterm_connect` | `peer`（ID 或 `IP` / `IP:端口`，如 `192.168.1.20` 或 `192.168.1.20:21118`）；可选 `rows`(默认24)/`cols`(默认120)/`password`(一般留空) | 建立远程终端会话，**返回 session id，后续调用都要用它** |
| `rdterm_exec` | `session`、`command`；可选 `wait_ms`（默认 5000） | 执行一条命令，返回输出（已剥 ANSI/颜色码），换行由服务端补 |
| `rdterm_read` | `session` | 非阻塞取走自上次读取以来累积的输出（配合长命令轮询） |
| `rdterm_sessions` | 无 | 列出当前所有活动会话 |
| `rdterm_resize` | `session`、`rows`、`cols` | 调整远端终端尺寸，避免折行 |
| `rdterm_close` | `session` | 关闭并释放会话 |

### 三、标准调用流程

1. `rdterm_connect { "peer": "<ID或IP>", "rows": 40, "cols": 200 }` → 记下返回的 `session`。
   需要宽输出（`ipconfig /all`、`dir` 表格等）就把 cols 调到 200。
2. 逐条 `rdterm_exec { "session": N, "command": "...", "wait_ms": 8000 }`。
3. 用完 `rdterm_close { "session": N }`，不要泄漏会话。

**执行规范：**
- 一次一条命令；需要顺序逻辑时用 `cmd1 && cmd2` 或 PowerShell 脚本。
- 长命令（编译、拷贝、扫描）：`wait_ms` 到期返回的是**当前已有输出**，命令仍在远端继续跑，
  隔几秒用 `rdterm_read` 轮询直到出现命令提示符/哨兵回显再发下一条。
- 输出为空 ≠ 失败：先 `rdterm_read` 确认，再判断是否命令本身无输出（如 `mkdir`）。
- 被控端是 PowerShell：避免把多条命令拼在一行里发；含引号/特殊字符时优先用单引号或 base64 下发脚本。
- 若 exec 输出出现大量 `>>` 续行或乱码，发一条 `echo OK` 校验会话状态，必要时关闭重连。

### 四、被控端侧注意

- 被控端需先运行：双击 `rdterm.exe`（菜单选 1），或 `rdterm.exe --server`，
  或 `rdterm.exe --hide`（后台隐藏运行，ID 写入 exe 同目录 `rdterm-identity.txt`）。
- 取 ID：被控端 `rdterm.exe --id`；或连接成功后由 Agent 调 `rdterm_identity` 查本机身份。
- **ID 每 12 小时轮换，旧 ID 立即失效**——连接失败先怀疑 ID 过期，重新取新 ID。
- 同网段可填 IP 直连（被控端默认监听 `0.0.0.0:21118`；`--no-direct-server` 可关）。
- **安全**：rdterm 无密码，拿到 ID 的人即拿到该机 shell。仅在授权环境使用，勿将 ID 泄露到不受信任的渠道。

### 五、快速排障

| 现象 | 处理 |
|---|---|
| 连接超时 | 对端未启动被控端 / ID 已过 12h 轮换 / 防火墙拦 21118；换 IP 直连验证 |
| 工具列表为空 | MCP 未被宿主加载：确认配置 JSON 语法、重启会话、信任该连接器 |
| exec 长时间无返回 | 用 `rdterm_read` 收尾输出；命令可能仍在跑，不要重复发送 |
| 输出乱码/错位 | `rdterm_resize` 调大 cols；仍异常则 close 后重连 |

## 【提示词正文结束】

---

### 附：最小化接入示例（Claude Desktop / WorkBuddy 通用）

```json
{
  "mcpServers": {
    "rdterm": {
      "command": "D:\\rdterm\\rdterm.exe",
      "args": ["--mcp"]
    }
  }
}
```

验证方法（命令行手动走一遍 stdio 流程）：

```bash
python tools/mcp-selftest.py            # initialize → tools/list → connect/exec 全链路自检
python tools/mcp-connect.py <ID或IP> "hostname" "ipconfig"   # 不依赖宿主直接接管
```
