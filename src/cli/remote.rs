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
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use hbb_common::config::LocalConfig;
use hbb_common::rendezvous_proto::ConnType;

use crate::ui_session_interface::{io_loop, Session};

use super::console::{ConsoleHandler, CONNECT_TIMEOUT, OPEN_TIMEOUT};

/// 会话注册表。id 从 1 开始自增。
fn registry() -> &'static Mutex<HashMap<u32, Arc<RemoteSession>>> {
    static REG: OnceLock<Mutex<HashMap<u32, Arc<RemoteSession>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 对端 shell 方言。它决定 exec 把哨兵拼进同一行时用什么分隔符：
/// PowerShell 只认 `;`，cmd 只认 `&`，给错了就是一条 ParserError。
const DIALECT_UNKNOWN: u8 = 0;
const DIALECT_PS: u8 = 1;
const DIALECT_CMD: u8 = 2;

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
    /// 远端 PTY 编号。每个会话必须不同：服务端对已存在的 terminal_id
    /// 会走「重连既有终端」分支，两个会话就会共用同一个 shell。
    pub terminal_id: i32,
    /// 对端 shell 方言（决定 exec 拼接哨兵时用 `;` 还是 `&`，未知时另起一行下发）。
    dialect: AtomicU8,
    /// 上一次 exec 超时时检测到 PowerShell 续行符（>>），下一条命令前要先 Ctrl-C 重同步。
    needs_resync: std::sync::atomic::AtomicBool,
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

        // 每个会话一个独立 terminal_id（1、2、3…）。服务端对已存在的 id 会
        // 重连同一个 PTY，写死常量会让两个会话共享一个 shell。
        let terminal_id = next_id() as i32;
        session.open_terminal(terminal_id, rows, cols);
        if !handler.wait_opened(OPEN_TIMEOUT) {
            return Err(match handler.take_failure() {
                Some(reason) => format!("打开终端失败: {reason}"),
                None => format!("等待远端终端就绪超时（{OPEN_TIMEOUT:?}）"),
            });
        }

        let s = Arc::new(Self {
            id: terminal_id as u32,
            peer: peer.to_owned(),
            terminal_id,
            dialect: AtomicU8::new(DIALECT_UNKNOWN),
            needs_resync: std::sync::atomic::AtomicBool::new(false),
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
    ///
    /// 顺便探测对端 shell 方言：PowerShell 会执行 `echo PROBE_$((6*7))` 得到
    /// PROBE_42，cmd 只会把 `$((6*7))` 当普通字符原样回显，据此区分哨兵拼接
    /// 用的分隔符（`;` vs `&`）。两种证据都拿不到时判为 Unknown —— 这时 exec
    /// 会退化成「哨兵另起一行」，不依赖任何分隔符，两种方言都不会报错。
    fn init_shell(&self) {
        self.send("Remove-Module PSReadLine -Force -ErrorAction SilentlyContinue");
        self.send(ENTER);
        // 等 PSReadLine 卸载完成，别让它的输出混进第一条命令。
        std::thread::sleep(Duration::from_millis(400));
        let mut intro = self.take_output();
        self.send("echo PROBE_$((6*7))");
        self.send(ENTER);

        // 不靠固定 sleep 收结果：慢机器 / 高延迟链路上 800ms 未必回得来，
        // 一漏判就会退回 cmd 的 `&`，在 PowerShell 5.1 上每条命令都报错。
        // 轮询到拿到证据为止，最多等 PROBE_TIMEOUT_MS。
        let probe_deadline = Instant::now() + Duration::from_millis(PROBE_TIMEOUT_MS);
        let mut probe_hit = false;
        loop {
            std::thread::sleep(Duration::from_millis(100));
            intro.push_str(&self.take_output());
            if intro.contains("PROBE_42") {
                probe_hit = true;
                break;
            }
            if Instant::now() >= probe_deadline {
                break;
            }
        }
        intro.push_str(&self.take_output());

        // "PS C:\...>" 提示符或算术探测任一命中即认定 PowerShell。
        let ps_prompt = intro.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with("PS ") && t.contains(':') && t.contains('>')
        });
        // 只看输入回显、没有求值结果，说明这是个不会算 `$()` 的 shell（cmd）。
        let cmd_echo = !probe_hit && intro.contains("PROBE_$((6*7))");
        let dialect = if probe_hit || ps_prompt {
            DIALECT_PS
        } else if cmd_echo {
            DIALECT_CMD
        } else {
            DIALECT_UNKNOWN
        };
        self.dialect.store(dialect, Ordering::SeqCst);
    }

    /// 当前认定的 shell 方言。
    fn dialect(&self) -> u8 {
        self.dialect.load(Ordering::SeqCst)
    }

    /// 往远端 PTY 写数据（不自动补换行，调用方自己决定）。
    pub fn send(&self, data: &str) {
        self.session.send_terminal_input(self.terminal_id, data.to_owned());
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
        self.session.resize_terminal(self.terminal_id, rows, cols);
    }

    /// 关闭远端终端并从注册表摘掉。
    pub fn close(&self) {
        self.session.close_terminal(self.terminal_id);
        self.session.close();
        self.handler.mark_closed();
        registry().lock().unwrap().remove(&self.id);
    }

    /// 执行一条命令：丢掉历史输出 → 下发 → 等哨兵输出 → 返回本次输出。
    ///
    /// 2026-09-12 重构（实测 16/20 测试矩阵暴露的竞态）：旧实现把哨兵作为
    /// 第二条命令单独下发，而「输入回显完成」≠「命令执行完成」——ConPTY 对
    /// 输入的回显是即时的，`hostname` 这种瞬时命令的**输出**还没到，静默判定
    /// 就已通过、哨兵就已下发并被检测到，真实输出反而落在哨兵后面被裁掉。
    ///
    /// 新做法：把哨兵拼进**同一条输入行**（PowerShell 用 `cmd; echo MARK`、
    /// cmd 用 `cmd & echo MARK`），shell 一定在命令跑完后才会输出 MARK。
    /// 于是 MARK 出现两次：第 1 次在输入回显行里（已知），第 2 次是 shell 的
    /// 执行输出；两次出现之间的内容就是这条命令的完整输出，天然免竞态。
    ///
    /// 方言未知时（探测没拿到证据）不赌分隔符：命令和哨兵分两行下发，两种
    /// shell 都会在前一条跑完后才执行 `echo MARK`，结果一样，但没有任何
    /// 方言假设可错。
    ///
    /// 超时兜底：若输出里出现 PowerShell 续行符 `>>`（引号没闭合进了多行模式），
    /// 标记会话需要重同步，下次 exec 前先发 Ctrl-C；若只是命令还在跑，则返回
    /// 已有输出并标注未完成。
    ///
    /// 2026-09-24：探测万一判成 cmd 而对面其实是 PowerShell 5.1，`&` 会直接
    /// 抛 ParserError。这里检测到该错误就把方言翻成 PowerShell 重试一次，
    /// 不让整个会话一直错下去。
    pub fn exec(&self, command: &str, max_wait_ms: u64, quiet_ms: u64) -> ExecResult {
        let mut r = self.exec_once(command, max_wait_ms, quiet_ms);
        if !r.complete && self.dialect() != DIALECT_PS && looks_like_amp_error(&r.output) {
            self.dialect.store(DIALECT_PS, Ordering::SeqCst);
            // ParserError 不占用 stdin，但保险起见先把提示符拉干净再重发。
            self.send("\x03");
            std::thread::sleep(Duration::from_millis(300));
            self.take_output();
            r = self.exec_once(command, max_wait_ms, quiet_ms);
        }
        r
    }

    /// `exec` 的单次尝试：下发 → 等哨兵 → 返回本次输出。
    fn exec_once(&self, command: &str, max_wait_ms: u64, quiet_ms: u64) -> ExecResult {
        // 先把上一条残留清掉，再给远端 shell 一点时间回到提示符。
        // 刚连上时 PowerShell 还在打印横幅，此时灌命令会被拆散。
        self.take_output();
        std::thread::sleep(Duration::from_millis(SETTLE_MS));
        self.take_output();

        // 上一条超时时若卡在续行模式，这里先 Ctrl-C 拉回提示符。
        if self
            .needs_resync
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.send("\x03");
            std::thread::sleep(Duration::from_millis(300));
            self.take_output();
            self.needs_resync
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }

        let deadline = Instant::now() + Duration::from_millis(max_wait_ms);
        let marker = new_marker();

        if command.contains('\n') {
            // 多行输入（脚本粘贴）：老两步走——先灌命令，静默后再发哨兵行。
            self.send(command);
            if !command.ends_with('\n') {
                self.send(ENTER);
            }
            let _ = self.wait_quiet(quiet_ms.max(200), deadline);
            self.send(&format!("echo {marker}{ENTER}"));
        } else {
            // 单行命令（绝大多数场景）：哨兵拼进同一行，无输入时序竞态。
            match self.dialect() {
                DIALECT_PS => self.send(&format!("{command}; echo {marker}{ENTER}")),
                DIALECT_CMD => self.send(&format!("{command} & echo {marker}{ENTER}")),
                // 方言未知：分两行下发，cmd 与 PowerShell 都不会因为分隔符报错。
                _ => {
                    self.send(command);
                    self.send(ENTER);
                    std::thread::sleep(Duration::from_millis(SETTLE_MS));
                    self.send(&format!("echo {marker}{ENTER}"));
                }
            }
        }

        let mut acc = String::new();
        let mut echo_cut: Option<(usize, usize)> = None; // (哨兵回显行起点, 该次匹配结束位置)
        loop {
            std::thread::sleep(Duration::from_millis(50));
            acc.push_str(&self.take_output());
            let clean = strip_ansi(&acc);

            // 第一阶段：等哨兵在输入回显行里出现，记下裁剪起点。
            if echo_cut.is_none() {
                if let Some(pos) = clean.find(&marker) {
                    let line_start = clean[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
                    echo_cut = Some((line_start, pos + marker.len()));
                }
            }
            // 第二阶段：等 shell 真正输出哨兵（第一次出现之后的第二次出现）。
            if let Some((line_start, first_end)) = echo_cut {
                if let Some(second) = clean[first_end..].find(&marker) {
                    // 第二次哨兵出现所在行的行首，其之前的内容即本命令的回显+输出。
                    let abs = first_end + second;
                    let out_end = clean[..abs].rfind('\n').map(|i| i + 1).unwrap_or(abs);
                    let output = &clean[line_start..out_end];
                    return ExecResult {
                        output: tidy_output(output),
                        complete: true,
                    };
                }
            }

            if Instant::now() >= deadline || self.is_closed() {
                // 超时：若卡在续行模式（输出里行尾有 `>>`），标记重同步。
                if strip_ansi(&acc).lines().any(|l| l.trim_end().ends_with(">>")) {
                    self.needs_resync
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                let output = match echo_cut {
                    Some((line_start, _)) => tidy_output(&clean[line_start..]),
                    None => tidy_lines(&strip_ansi(&acc)),
                };
                return ExecResult {
                    output,
                    complete: false,
                };
            }
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

/// shell 方言探测最多等这么久（轮询，命中即提前返回）。
const PROBE_TIMEOUT_MS: u64 = 3000;

/// 输出里有没有 PowerShell 把 `&` 当分隔符时的解析错误。
///
/// 5.1 的原文是 `The token '&' is not a valid statement separator in this
/// version.`，中文语言包是「标记“&”不是此版本中的有效语句分隔符。」。
/// 只认这几个特征串，避免把命令自己的正常输出误判成解析错误。
fn looks_like_amp_error(s: &str) -> bool {
    s.contains("ParserError")
        || s.contains("statement separator")
        || s.contains("有效语句分隔符")
        || s.contains("The token '&'")
}

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

/// 判断一整行是不是 shell 提示符（`PS C:\Users\x>` / `C:\Windows>`）。
fn is_prompt_line(line: &str) -> bool {
    let t = line.trim();
    let t = t.strip_prefix("PS ").unwrap_or(t);
    // 盘符路径开头 + 以 `>` 收尾（后面没有命令内容）。
    let bytes = t.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2] == b'\\'
        && t.ends_with('>')
}

/// exec 结果整理：在 `tidy_lines` 基础上剥掉提示符。
///
/// * 整行只有提示符的（首行、输出结束后的尾行）直接丢掉；
/// * 首行是「提示符 + 命令回显」的，只去掉提示符前缀，保留回显。
fn tidy_output(s: &str) -> String {
    let mut lines: Vec<String> = tidy_lines(s)
        .lines()
        .map(|l| l.to_owned())
        .collect();
    if let Some(first) = lines.first_mut() {
        // 首行剥提示符前缀："PS C:\Users\x> hostname" → "hostname"。
        let t = first.as_str();
        let t = t.strip_prefix("PS ").unwrap_or(t);
        if let Some(gt) = t.find('>') {
            let head = &t[..gt];
            if head.len() >= 3
                && head.as_bytes()[0].is_ascii_alphabetic()
                && head.as_bytes()[1] == b':'
                && head.as_bytes()[2] == b'\\'
            {
                *first = t[gt + 1..].trim_start().to_owned();
            }
        }
    }
    // 输出尾部若挂着孤立的提示符行（哨兵输出前的那次提示），去掉。
    while lines.last().map(|l| is_prompt_line(l)).unwrap_or(false) {
        lines.pop();
    }
    // 首行被剥空（命令回显为空）时也丢掉。
    if lines.first().map(|l| l.is_empty()).unwrap_or(false) {
        lines.remove(0);
    }
    lines.join("\n")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amp_parser_error_is_detected() {
        let en = "At line:1 char:12\r\n+ hostname & echo RDTERM_DONE_1\r\n+            ~\r\nThe token '&' is not a valid statement separator in this version.\r\n    + CategoryInfo          : ParserError: (:) [], ParentContainsErrorRecordException";
        let zh = "标记“&”不是此版本中的有效语句分隔符。";
        assert!(looks_like_amp_error(en));
        assert!(looks_like_amp_error(zh));
        assert!(looks_like_amp_error("ParserError"));
    }

    #[test]
    fn normal_output_is_not_an_amp_error() {
        assert!(!looks_like_amp_error("DESKTOP-ABC123"));
        assert!(!looks_like_amp_error("cmdlet 未找到 & 相关命令"));
        assert!(!looks_like_amp_error(""));
    }
}
