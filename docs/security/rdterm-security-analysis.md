# rdterm 组网安全性论证报告

> 版本：v1.0.0（对应提交 `b447dda` / Release v1.0.0）
> 方法：源码审查（Rust 实现，含 libsodium 调用点）+ 可复现的黑盒实测（MITM 抓包、零凭据接入、握手日志取证、ID 随机性抽样）
> 判定原则：**只对已验证的事实下结论**；未经实测的推断一律标注为「代码层结论，未做网络层实证」

---

## 1. 结论摘要

rdterm 的组网安全性可以概括为一句话：**加密是可靠的，鉴权被简化到了「知道 ID 就能进来」，而直连模式把这条边界退化成「网络能通就能进来」。**

| # | 结论 | 等级 | 实证 |
|---|------|------|------|
| G1 | 通过 rendezvous（ID）建链时，会话数据面是端到端加密的（XSalsa20-Poly1305），身份由 Ed25519 签名校验 | ✅ 正面 | T4 握手日志 `punch secure_connection ok` + 代码 |
| G2 | 客户端↔ID 服务器通道是加密的，ID、用户名、主机名、产品串均不以明文出现在链路上 | ✅ 正面 | T3 抓包 1742B 全密文 |
| G3 | ID 为 12 位密码学随机数（≈39.7 bit），12 小时轮换，且注册到 ID 服务器的确实是这个随机 ID（非 MAC 派生） | ✅ 正面 | S1 30 样本 / S2 按 ID 回连成功 |
| G4 | 无密码存储，不存在可离线爆破的密码哈希 | ✅ 正面 | 配置目录审查 |
| **R1** | **以 IP 直连（`IP:21118`）时，终端会话明文传输**：命令、输出、用户名、主机名、平台、版本、终端 ID 全部可被同链路嗅探 | 🔴 严重 | **T1 抓包命中哨兵明文（双向）** |
| **R2** | **零凭据可取得交互式 shell**：免密设计下，任何能连上 21118 的主体（无需知道 ID）即可完全控制主机 | 🔴 严重 | **T2 全新配置目录直连即得 `meyapico\14448`** |
| **R3** | 12 位 ID（39.7 bit）是唯一凭证，且经公共 ID 服务器对外可达；在线枚举一旦命中即等于完全控制 | 🟠 高 | S1 熵估算 + 架构分析 |
| **R4** | MCP 的 HTTP 传输（`--mcp-http`）无任何鉴权，绑定到非回环地址时等于把命令执行接口公开 | 🟠 高 | 代码级（`mcp.rs` 仅 `TcpListener::bind`） |
| **R5** | 启动瞬间配置层会生成 **MAC 派生的 29 bit 旧式 ID**；若随机 ID 注入失败（写盘异常等）存在退化为固定弱 ID 的风险 | 🟡 中 | T4 直连日志 `Generated id 195605026` |
| **R6** | ID 以明文落盘（`rdterm-id.json` / `rdterm-identity.txt`），同机其他用户或离线取证者可直接读取 | 🟡 中 | 配置目录审查 |

**适用边界（运维红线）**：rdterm 只应部署在**可信网络**内（本机、受控局域网、点对点 VPN 隧道）。不要把 21118 暴露到公网；跨不可信网络时用 `--no-direct-server` 强制走 rendezvous（此时数据面加密生效），并考虑自建 ID/中继服务器。

---

## 2. 组网架构与信任边界

rdterm 继承 RustDesk 的三条建链路径，三条路径的**加密与鉴权强度完全不同** —— 这是全部分析的关键。

```
                        ┌──────────────────────────────┐
                        │      12 位随机 ID（凭证）      │
                        └───────────────┬──────────────┘
                                        │
        ┌───────────────────────────────┼───────────────────────────────┐
        │                               │                               │
   路径 A：rendezvous              路径 B：IP 直连                路径 C：relay 中继
   （ID 服务器牵线）              （LAN/本机，默认开）           （打洞失败时兜底）
        │                               │                               │
  1. 客户端→ID 服务器：           客户端直接 connect 21118       两端都连公网 relay
     secretbox 加密通道           无任何身份交换                 数据面同为会话密钥加密
  2. ID 服务器返回对端 pk            ⇒ 无会话密钥 ⇒ 明文
     （Ed25519 签名，根公钥固定）
  3. 对端身份校验通过后协商
     会话密钥（X25519+secretbox）
        │                               │                               │
   数据面：加密 ✅                  数据面：明文 ❌                数据面：加密 ✅（代码结论）
   鉴权：ID + 签名                 鉴权：无凭据 ❌                鉴权：ID + 签名
```

**信任根**：客户端内置固定根公钥 `RS_PUB_KEY`（`libs/hbb_common/src/config.rs:118`），用于验证 ID 服务器签发的对端公钥。因此 **ID 服务器是身份信任根** —— 这是上游 RustDesk 的设计，rdterm 未改动，也未引入新的信任假设。恶意或被控的 ID 服务器理论上可替换对端公钥实施 MITM；自建服务器（`custom-rendezvous-server`）可消除这一依赖。

**rdterm 引入的改动**（相对上游）：

| 改动 | 位置 | 安全含义 |
|------|------|----------|
| 免密登录（terminal-only 构建特性） | `src/server/connection.rs:2889-2904` | 鉴权从「密码」退化为「ID」，直连场景进一步退化为「网络可达」 |
| 12 位随机 ID + 12h 轮换 | `src/cli/identity.rs` | 凭证强度从 9 位（≈30 bit，且 MAC 派生）提升到 12 位（≈39.7 bit，CSPRNG） |
| 默认开启直连监听 21118 | `src/cli/mod.rs`（`enable_direct_server`） | 便利性换取了暴露面：见 R1/R2 |
| MCP Server（stdio + HTTP） | `src/cli/mcp.rs` | 新增一条「Agent → 远端 shell」的通路，HTTP 模式无鉴权（R4） |

---

## 3. 密码学实现审查

实现全部基于 libsodium（`sodiumoxide 0.2`，`libs/hbb_common/Cargo.toml:34`），未自造密码学。

| 用途 | 算法 | 代码位置 | 评价 |
|------|------|----------|------|
| 会话数据面加密 | XSalsa20-Poly1305（`secretbox`），每方向单调计数器 nonce | `libs/hbb_common/src/tcp.rs:296-321` | ✅ nonce 由 `FramedStream::get_nonce(计数器)` 生成，单向递增不重用；密钥每会话全新 |
| 会话密钥封装 | X25519（`crypto_box`）+ 临时密钥对 | `src/common.rs:2164-2171`、`src/server.rs:264-269` | ✅ 全零 nonce 但发送方密钥每次随机生成，等价 libsodium `crypto_box_seal` 构造，无 nonce 重用风险 |
| 对端身份 | Ed25519 签名（`sign::verify`） | `src/common.rs:2085-2092`、`src/client.rs:1592-1612` | ✅ 校验签名归属根公钥、且签名中的 ID 必须等于目标 ID，不匹配则拒绝 |
| 客户端↔ID 服务器通道 | 同上：KeyExchange 用根公钥验签后协商 `secretbox` 密钥 | `src/common.rs:2069-2100` | ✅ 实测全密文（T3）；`wss://` 时跳过冗余加密（该情形由 TLS 承担） |
| 会话内敏感值（如密码存储） | `secretbox` + 版本前缀 | `libs/hbb_common/src/password_security.rs` | rdterm 免密模式下不涉及 |

**结论**：密码学调用方式正确，没有发现自造的 IV/nonce 复用、静态密钥协商或降级到明文的设计缺陷。**唯一的「明文」来自路径 B 的设计选择**（客户端拿到的是裸 IP，没有可校验的身份，因此上游直接走 non-secure 分支，见 `src/client.rs:424-442`、`src/client.rs:1630` 注释）。

---

## 4. 实测验证

全部测试脚本位于 `tools/security/`，可一键复现。

### T1 直连路径明文性 —— 🔴 命中

```
代理 127.0.0.1:21199 → 127.0.0.1:21118，客户端以 IP:port 建链
抓包：296 字节(客户端→服务端) + 961 字节(服务端→客户端)
哨兵明文出现方向：['c2s', 's2c']
```

抓包可读内容（节选，`.` 表示二进制不可打印字节）：

```
### client->server (296 bytes)
.127.0.0.1:21199".613986318402*.14448P..........Z.1.5.0b.j.Windows....
Remove-Module PSReadLine -Force -ErrorAction SilentlyContinue
echo RDTERM_PLAINTEXT_PROBE_9F4C1; echo RDTERM_DONE_1

### server->client (961 bytes)
...Terminal opened .j*'ts_9f3697b4-9de9-4e79-8dc4-6e7ef612663f.....
: C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe
```

即：**登录请求、对端地址、用户名、平台、版本、终端会话 ID、shell 路径、命令与命令输出全部明文**。同一链路上的嗅探者或中间设备可完整还原终端操作。

### T2 零凭据访问 —— 🔴 命中

清空配置目录（无任何凭据、无任何历史信任状态）后直连：

```
配置目录已清空重建: D:\rdterm\_sec_ctl_t2
connect 结果: {"peer":"127.0.0.1:21118","session":1,"status":"ready"}
exec 输出: 'whoami; hostname; echo RDTERM_DONE_1\nmeyapico\\14448\nMEYAPICO'
```

**无需任何凭据即取得交互式 shell**。结合「默认监听 `0.0.0.0:21118`」，实际访问控制完全由网络可达性决定（防火墙/路由），应用层不提供任何门槛。

### T3 客户端↔ID 服务器通道 —— ✅ 密文

把 `custom-rendezvous-server` 指向本地代理（上游 `rs-ny.rustdesk.com:21116`），观察被控端注册流量：

```
抓包: 1100+642 字节，连接数 14
敏感串明文检索：ID(743141554060)=0 处、14448=0 处、meyapico=0 处、Windows=0 处、RustDesk=0 处
```

链路上只有不可解析的密文，与 `secure_tcp_impl` 的 `secretbox` 通道实现一致。

### T4 握手协商对照取证

| 路径 | 客户端日志关键行 | 结论 |
|------|------------------|------|
| ID 路径 | `#1 TCP punch attempt with 192.168.31.98:58303, id: 804049596673`<br>`UDP+WebRTC Hole Punched 804049596673 = 116.10.166.135:45238`<br>`[DEBUG] UDP+WebRTC punch secure_connection ok` | 协商了安全握手（身份验签 + 会话密钥），数据面加密 |
| 直连 IP | `[WARN] hbb_common::config ID is invalid, generating new one`<br>`[INFO] Generated id 195605026`（**MAC 派生的 9 位 ID**）<br>全程无 `secure_connection` 相关日志 | 未协商会话密钥 ⇒ 明文（与 T1 互证） |

### S1 ID 随机性抽样（30 次，每次全新目录）

```
位数分布: {12: 30}        唯一: 30 / 30（无重复）
首位分布: {'1':2,'2':2,'3':6,'4':3,'5':1,'6':7,'7':4,'8':3,'9':2}  （首位 0 出现 0 次）
数字频率(0-9): [36,30,28,44,37,29,41,36,40,39]  期望≈36.0
空间: 9×10^11 ≈ 39.7 bit
```

分布与均匀随机一致，确认使用 CSPRNG（而非 MAC 派生/时间派生）。

### S2 注册凭证判定 —— ✅ 登记的确实是随机 ID

```
被控端 rdterm-id.json 中的 ID = 814208037923（12 位）
按 12 位 ID 走 rendezvous 连接: {"peer":"814208037923","session":1,"status":"ready"}
取到输出: 'hostname; ...\nMEYAPICO'
```

说明 rendezvous 上登记的是我们注入的 12 位随机 ID，R5 描述的 MAC 派生 ID 只出现在启动瞬间的配置加载阶段，未成为注册凭证。

---

## 5. 风险清单与缓解

### R1 直连路径明文传输（🔴 严重）

- **影响**：终端会话内容、命令与输出、用户名/主机名/平台/版本、终端会话 ID 在链路上明文可读；同网段 ARP 欺骗、交换机镜像、上游网络设备均可获取。
- **复现**：`python tools/security/security-tests.py t1`
- **现有缓解**：无（默认即开启直连；仅当用 IP 直连时命中）。
- **建议**：
  1. 只在可信链路使用 IP 直连；跨网段/无线/共享网络一律改用 ID（rendezvous）路径或点对点 VPN；
  2. 需要强制时：被控端 `--no-direct-server`（禁用 21118 直连），全部走 rendezvous 的加密通道；
  3. 上游可改造项：为直连路径引入「一次性预共享密钥（PSK）+ box 握手」，恢复直连场景的机密性。

### R2 零凭据取 shell（🔴 严重）

- **影响**：任何能访问 21118 的主体（含同机低权进程、同网段主机、被入侵的内网设备）无需密码、无需知道 ID 即可获得该用户权限的交互式 shell。
- **复现**：`python tools/security/security-tests.py t2`
- **现有缓解**：ID 轮换与免密设计不覆盖此路径；21118 默认监听所有网卡。
- **建议**：
  1. **Windows 防火墙入站规则限制 21118 的来源网段**（最小必要）；
  2. 非必要不开直连（`--no-direct-server`）；
  3. 代码层：给直连路径加「连接确认/白名单（`--allow-peer`）」或恢复可选密码，使应用层不再把访问控制完全外包给网络。

### R3 单一凭证 + 39.7 bit 熵（🟠 高）

- **影响**：ID 即凭证，且经公共 ID 服务器可被任意互联网主体尝试连接；若服务端限速不足，长期在线枚举存在命中概率；命中即完全控制（无二次验证）。
- **复现**：`python tools/security/id-analysis.py 30 sample`
- **现有缓解**：12 位（≈39.7 bit，远优于上游 9 位≈30 bit）、12 小时轮换、公共服务器有限速。
- **建议**：
  1. ID 长度可配置（≥16 位十进制≈53 bit，或改用 base32 字母数字以提升每字符熵）；
  2. 接入失败尝试限速与告警（hbbs 侧或客户端侧）；
  3. 高价值目标改用自建 ID 服务器（不对外公开 ID 空间）。

### R4 MCP HTTP 传输无鉴权（🟠 高）

- **影响**：`--mcp-http 0.0.0.0:8787` 时，任何可达该地址者可直接调用 `rdterm_exec` 执行命令；该进程同时具备控制端能力（可再跳板到其他主机）。
- **位置**：`src/cli/mcp.rs:315` 直接 `TcpListener::bind(addr)`，无 token/来源校验。
- **建议**：
  1. 代码层拒绝非回环绑定（默认 `127.0.0.1`），或引入 `--mcp-token` 并要求 `Authorization` 头；
  2. 使用方只在本机回环上使用 HTTP 模式，跨机一律用 stdio（由 Agent 宿主管理）。

### R5 MAC 派生旧式 ID 的残留路径（🟡 中）

- **影响**：配置层 `Config::load()` 在 ID 无效时会用 `get_auto_id()` 生成 **MAC 派生的 29 bit ID**（`libs/hbb_common/src/config.rs:1067-1092`，日志 `Generated id <9位>`）。正常路径下随即被 `identity::ensure()` 的 12 位随机 ID 覆盖（S2 已验证），但若写盘失败/初始化顺序变化，存在退化注册固定弱 ID 的风险；同时该 ID 与网卡硬件绑定，具备跨重装的可追踪性（隐私）。
- **复现**：`python tools/security/security-tests.py t4`（观察直连客户端日志）
- **建议**：在 terminal-only 构建中短路 `get_auto_id()`（改为 CSPRNG 或直接返回 `None` 并强制走 `identity::ensure()`），消除退化路径。

### R6 凭证明文落盘（🟡 中）

- **影响**：`rdterm-id.json`、`rdterm-identity.txt`（含 ID 明文与有效期）、`rdterm.pid` 位于配置目录，权限继承当前用户；同机其他用户、备份/同步工具、离线取证均可获取 ID，进而（在网络可达时）直接接管。
- **建议**：收紧文件 ACL（仅当前用户）；高安全场景用 DPAPI 保护或改为仅内存持有（不落盘，重启即换新 ID）。

### 未覆盖范围（诚实声明）

- 未做协议 fuzzing 与畸形报文测试；
- 未审计上游 RustDesk 全部代码，只覆盖与鉴权/加密/组网相关的路径；
- **路径 C（relay 中继）的密文性为代码层结论**（与 A 路径共用同一会话密钥体系），未做网络层抓包实证；
- 未验证 ID 服务器被完全控制时的 MITM 可行性与影响面（信任根假设未做攻击验证）；
- 未做内存取证/进程转储分析（运行中进程内存里必然存在 ID 与会话密钥）。

---

## 6. 复现指引

```bat
:: 依赖：D:\rdterm\rdterm.exe（或设置 RDTERM_EXE 指向其他构建产物）
cd tools\security

:: T1 直连明文性（会临时起被控端与 MITM 代理，抓包落盘到 security\out\）
python security-tests.py t1

:: T2 零凭据访问（会临时起被控端）
python security-tests.py t2

:: T3 客户端↔ID 服务器通道（经本地代理观察，需能访问 rs-ny.rustdesk.com:21116）
python security-tests.py t3

:: T4 握手协商对照（目标可传任意在线 12 位 ID；缺省用本地被控端 ID）
python security-tests.py t4 [目标ID]

:: S1/S2 ID 随机性与注册凭证
python id-analysis.py 30 all
```

证据文件：`security/out/capture-*.bin`（原始字节）、`capture-*-strings.txt`（可读化视图）、`t4-*-client.log`（握手日志）。

---

## 7. 总体评价

- **密码学与协议实现**：达到上游 RustDesk 的水准，未发现实现层缺陷；加密（XSalsa20-Poly1305 + X25519 + Ed25519）在 rendezvous 路径上提供有效的端到端机密性与对端真实性。
- **鉴权模型**：为了「免密 + Agent 可直接接管」的易用性，牺牲了纵深防御 —— 应用层不再有第二道门槛，安全性被完全外移到「凭证保密（ID）」与「网络可达性（防火墙）」两件事上。
- **可接受性判断**：在**受控局域网/取证现场作业**场景下可以接受，前提是：限制 21118 来源、不使用不可信网络直连、凭证按 12 小时窗口管理；在**公网暴露或不可信内网**场景下**不可接受**，必须先落实 R1/R2/R4 的加固项。
