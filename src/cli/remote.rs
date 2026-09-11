//! 可复用的远程终端会话（给 MCP / 自动化用）。
//!
//! 交互模式（`console::run_client`）是「一把梭」：连上、接管 stdin/stdout、断开。
//! MCP 需要的是另一回事——一次连上，然后被反复地「写命令 / 读输出」，
//! 而且可以同时挂着好几个对端。所以这里把会话单独抽出来：
//!
//! * `RemoteSession` 持有一份 `Session<ConsoleHandler>`（缓冲模式），
//!   远端输出不打印，而是攒在内存里等 `take_output()` 取；
//! * 全局注册表按数字 id 管理会话，MCP 工具只认 id。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use hbb_common::config::LocalConfig;
use hbb_common::rendezvous_proto::ConnType;

use crate::ui_session_interface::{io_loop, Session};

use super::console::{ConsoleHandler, CONNECT_TIMEOUT, OPEN_TIMEOUT, TERMINAL_ID};

/// 会话注册表。id 从 1 开始自增。
fn registry() -> &'static Mutex<HashMap<u32, Arc<RemoteSession>>> {
    static REG: OnceLock<Mutex<HashMap<u32, Arc<RemoteSession>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_id() -> u32 {
    static NEXT: OnceLock<Mutex<u32>> = OnceLock::new();
    let mut n = NEXT.get_or_init(|| Mutex::new(0)).lock().unwrap();
    *n += 1;
    *n
}

/// 一个远端的远程终端。可以反复读写，直到 `close()`。
pub struct RemoteSession {
    pub id: u32,
    pub peer: String,
    session: Session<ConsoleHandler>,
    handler: ConsoleHandler,
}

impl RemoteSession {
    /// 连到对端并打开终端。成功即表示 PTY 已就绪，可以发命令了。
    pub fn connect(peer: &str, password: Option<&str>, rows: u32, cols: u32) -> Result<Arc<Self>, String> {
        let handler = ConsoleHandler::buffered();
        let session: Session<ConsoleHandler> = Session {
            password: password.unwrap_or_default().to_owned(),
            ui_handler: handler.clone(),
            ..Default::default()
        };
        session.lc.write().unwrap().initialize(
            peer.to_owned(),
            ConnType::TERMINAL,
            None,
            false,
            None,
            None,
            None,
        );
        LocalConfig::set_remote_id(peer);

        let round = session.connection_round_state.lock().unwrap().new_round();
        let loop_session = session.clone();
        std::thread::spawn(move || io_loop(loop_session, round));

        if !handler.wait_connected(CONNECT_TIMEOUT) {
            return Err(match handler.take_failure() {
                Some(reason) => format!("连接失败: {reason}"),
                None => format!("连接超时（{CONNECT_TIMEOUT:?}）"),
            });
        }

        session.open_terminal(TERMINAL_ID, rows, cols);
        if !handler.wait_opened(OPEN_TIMEOUT) {
            return Err(match handler.take_failure() {
                Some(reason) => format!("打开终端失败: {reason}"),
                None => format!("等待远端终端就绪超时（{OPEN_TIMEOUT:?}）"),
            });
        }

        let s = Arc::new(Self {
            id: next_id(),
            peer: peer.to_owned(),
            session,
            handler,
        });
        s.init_shell();
        registry().lock().unwrap().insert(s.id, s.clone());
        Ok(s)
    }

    /// 打开终端之后做一次 shell 初始化，让后续输出干净可解析。
    ///
    /// 关键一步是关掉 PowerShell 的 PSReadLine。它负责命令行编辑，会不断重绘
    /// 当前输入行，长命令下这些重绘片段会混进输出里，导致结果无法解析
    /// （实测表现为命令回显被切成好几段、中间夹着 `> `）。关掉之后回显退化成
    /// 朴素模式，一行命令一行回显。非 PowerShell 环境这条命令不存在，无害。
    fn init_shell(&self) {
        self.send("Remove-Module PSReadLine -Force -ErrorAction SilentlyContinue");
        self.send(ENTER);
        // 等 PSReadLine 卸载完成，别让它的输出混进第一条命令。
        std::thread::sleep(Duration::from_millis(400));
        let _ = self.take_output();
    }

    /// 往远端 PTY 写数据（不自动补换行，调用方自己决定）。
    pub fn send(&self, data: &str) {
        self.session.send_terminal_input(TERMINAL_ID, data.to_owned());
    }

    /// 取走自上次读取以来累积的输出。
    pub fn take_output(&self) -> String {
        self.handler.take_output()
    }

    /// 还没被取走的输出字节数。
    pub fn pending_len(&self) -> usize {
        self.handler.pending_len()
    }

    pub fn is_closed(&self) -> bool {
        self.handler.is_closed()
    }

    /// 会话当前的失败原因（取一次就没了）。
    pub fn take_failure(&self) -> Option<String> {
        self.handler.take_failure()
    }

    /// 调整远端 PTY 尺寸，让 `ls` 之类的输出不至于被折成奇怪的宽度。
    pub fn resize(&self, rows: u32, cols: u32) {
        self.session.resize_terminal(TERMINAL_ID, rows, cols);
    }

    /// 关闭远端终端并从注册表摘掉。
    pub fn close(&self) {
        self.session.close_terminal(TERMINAL_ID);
        self.session.close();
        self.handler.mark_closed();
        registry().lock().unwrap().remove(&self.id);
    }

    /// 执行一条命令：丢掉历史输出 → 下发 → 等哨兵回显 → 返回本次输出。
    ///
    /// 光靠「输出静默 N 毫秒」判定结束是不行的：PowerShell 启动慢，常常是
    /// 第一条命令的输出在下一条命令打完之后才回来，调用方就会把 A 的输出当成 B 的。
    /// 所以这里在命令后面追加一条 `echo <哨兵>`，等到远端把哨兵吐回来才认定这条跑完了；
    /// 万一超时或命令进了交互模式（比如直接敲 `python`），就把已有输出返回并标记未完成。
    pub fn exec(&self, command: &str, max_wait_ms: u64, quiet_ms: u64) -> ExecResult {
        // 先把上一条残留清掉，再给远端 shell 一点时间回到提示符。
        // 刚连上时 PowerShell 还在打印横幅，此时灌命令会被拆散。
        self.take_output();
        std::thread::sleep(Duration::from_millis(SETTLE_MS));
        self.take_output();

        let deadline = Instant::now() + Duration::from_millis(max_wait_ms);
        self.send(command);
        if !command.ends_with('\n') {
            self.send(ENTER);
        }

        // 等这条命令自己先跑安静了再发哨兵。
        // 两条命令一起灌进去会被 ConPTY 拼成乱序输入（实测会出现 PowerShell 的续行符 `>>`）。
        let mut acc = self.wait_quiet(quiet_ms, deadline);
        if self.is_closed() {
            return ExecResult {
                output: tidy_lines(&strip_ansi(&acc)),
                complete: false,
            };
        }

        let marker = new_marker();
        self.send(&format!("echo {marker}{ENTER}"));
        loop {
            std::thread::sleep(Duration::from_millis(50));
            acc.push_str(&self.take_output());
            let clean = strip_ansi(&acc);
            if let Some(pos) = clean.find(&marker) {
                // 哨兵第一次出现是在「echo <哨兵>」这条命令的回显行里，
                // 把它所在的那一行整行砍掉，剩下的就是上一条命令的回显 + 输出。
                let cut = clean[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
                return ExecResult {
                    output: tidy_lines(&clean[..cut]),
                    complete: true,
                };
            }
            if Instant::now() >= deadline || self.is_closed() {
                break;
            }
        }
        ExecResult {
            output: tidy_lines(&strip_ansi(&acc)),
            complete: false,
        }
    }

    /// 一直收输出，直到连续 `quiet_ms` 毫秒没有新字节（或到 deadline）。
    fn wait_quiet(&self, quiet_ms: u64, deadline: Instant) -> String {
        let mut acc = String::new();
        let mut last_len = 0usize;
        let mut last_change = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(50));
            acc.push_str(&self.take_output());
            if acc.len() != last_len {
                last_len = acc.len();
                last_change = Instant::now();
                continue;
            }
            if last_change.elapsed() >= Duration::from_millis(quiet_ms) && !acc.is_empty() {
                break;
            }
            if Instant::now() >= deadline || self.is_closed() {
                break;
            }
        }
        acc
    }
}

/// 下发命令之前先空等一会儿，让远端 shell 回到提示符。
const SETTLE_MS: u64 = 150;

/// 送进 PTY 的「回车」。
///
/// 只发 `\r`，千万别发 `\r\n`：远端会把多出来的 `\n` 当成一个空行，
/// PowerShell 于是吐一个续行提示符 `>>` 出来，白白污染输出。
const ENTER: &str = "\r";

/// 一次 `exec` 的结果。
pub struct ExecResult {
    /// 净化后的输出（已剥掉 ANSI、已去掉哨兵回显行）。
    pub output: String,
    /// 是否等到了哨兵。false 表示超时或命令可能还停在交互模式里，输出不完整。
    pub complete: bool,
}

/// 生成一次性的结束标记。带进程内自增序号，避免同一会话里前后两条命令串味。
fn new_marker() -> String {
    static N: OnceLock<Mutex<u64>> = OnceLock::new();
    let mut n = N.get_or_init(|| Mutex::new(0)).lock().unwrap();
    *n += 1;
    format!("RDTERM_DONE_{n}")
}

/// 剥掉 ANSI 转义序列。
///
/// 远端是 ConPTY，PowerShell 会给提示符上色、会发清屏和光标定位指令，
/// 原样丢给 Agent 基本没法读。这里处理三类：
/// CSI（`ESC [ ... 字母`）、OSC（`ESC ] ... BEL` 或 `ESC \`）、以及两字符序列。
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match it.peek() {
            Some('[') => {
                it.next();
                // 参数与中间字节直到遇到最终字节（@ 到 ~）。
                while let Some(&c2) = it.peek() {
                    it.next();
                    if ('@'..='~').contains(&c2) {
                        break;
                    }
                }
            }
            Some(']') => {
                it.next();
                // 字符串序列，以 BEL 或 ST（ESC \）结束。
                while let Some(&c2) = it.peek() {
                    it.next();
                    if c2 == '\x07' {
                        break;
                    }
                    if c2 == '\x1b' {
                        if it.peek() == Some(&'\\') {
                            it.next();
                        }
                        break;
                    }
                }
            }
            Some('(') | Some(')') | Some('#') => {
                // 字符集选择 / 其他三字符序列：ESC ( B 这种，连吃两个。
                it.next();
                it.next();
            }
            Some(_) => {
                it.next(); // 其他两字符序列，吃掉后随那个字符
            }
            None => {}
        }
    }
    out
}

/// 收尾整理：CRLF 统一成 LF，去掉每行尾部空白，压缩连续空行，去掉首尾空行。
fn tidy_lines(s: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in s.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
        let l = line.trim_end();
        if l.is_empty() && out.last().map(|p: &String| p.is_empty()).unwrap_or(false) {
            continue;
        }
        out.push(l.to_owned());
    }
    while out.first().map(|s| s.is_empty()).unwrap_or(false) {
        out.remove(0);
    }
    while out.last().map(|s| s.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out.join("\n")
}

/// 取一个还活着的会话。
pub fn get(id: u32) -> Option<Arc<RemoteSession>> {
    registry().lock().unwrap().get(&id).cloned()
}

/// 列出所有会话的摘要（给 `tools/call` 的 sessions 用）。
pub fn list() -> Vec<serde_json::Value> {
    registry()
        .lock()
        .unwrap()
        .values()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "peer": s.peer,
                "closed": s.is_closed(),
                "pending_bytes": s.pending_len(),
            })
        })
        .collect()
}
