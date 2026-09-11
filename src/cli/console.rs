//! 控制端：`ConsoleHandler` 与本地 stdin/stdout ↔ 远端 PTY 的桥接。
//!
//! 设计要点：
//! * `ConsoleHandler` 实现 `InvokeUiSession`，`Session<ConsoleHandler>` 会自动获得
//!   `client::Interface`，所以只需要实现 UI 回调这一个 trait。
//! * 远端 PTY 的输出通过 `handle_terminal_response` 直接写 stdout；
//!   本地键盘输入由独立线程读 stdin，转 `send_terminal_input` 发过去。
//! * 连接状态用 `Arc<AtomicBool>` 在 handler 与主线程之间同步，避免引入额外通道。

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base::message_proto::*;
use hbb_common::config::LocalConfig;
use hbb_common::log;
use hbb_common::rendezvous_proto::ConnType;

use crate::client::QualityStatus;
use crate::ui_session_interface::{io_loop, InvokeUiSession, Session};

/// 本进程只用一个终端，编号固定为 1。
pub const TERMINAL_ID: i32 = 1;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(15);

/// handler 与主线程共享的状态。
#[derive(Default)]
struct Shared {
    /// 登录成功、io_loop 已进入主循环。
    connected: AtomicBool,
    /// 会话结束（对端关闭 / 出错 / 本地 stdin 关闭）。
    closed: AtomicBool,
    /// 远端 PTY 已创建成功。
    opened: AtomicBool,
    /// msgbox 传来的失败原因。
    failure: Mutex<Option<String>>,
    /// 输出的去向。
    ///
    /// * `None` —— 交互模式，直接写 stdout，人能看见；
    /// * `Some(buf)` —— 缓冲模式，累积起来由 `take_output()` 取走，
    ///   给 MCP / 自动化用（那种场景没人看屏幕）。
    sink: Option<Arc<Mutex<Vec<u8>>>>,
}

/// 控制台会话的 UI 回调实现。
///
/// 除终端输出、连接状态、错误提示之外全部是空实现——精简版没有画面、没有文件
/// 传输、没有语音，这些回调永远不会携带有效数据。
#[derive(Clone, Default)]
pub struct ConsoleHandler {
    shared: Arc<Shared>,
}

impl ConsoleHandler {
    /// 交互模式：远端输出直接写 stdout。
    pub fn interactive() -> Self {
        Self::default()
    }

    /// 缓冲模式：远端输出累积在内存里，`take_output()` 一次取走。
    pub fn buffered() -> Self {
        Self {
            shared: Arc::new(Shared {
                sink: Some(Arc::new(Mutex::new(Vec::new()))),
                ..Default::default()
            }),
        }
    }

    /// 取走并清空已缓冲的输出。交互模式永远返回空串。
    pub fn take_output(&self) -> String {
        match &self.shared.sink {
            Some(buf) => {
                let mut b = buf.lock().unwrap();
                let s = String::from_utf8_lossy(&b).into_owned();
                b.clear();
                s
            }
            None => String::new(),
        }
    }

    /// 缓冲了多少字节还没取走。
    pub fn pending_len(&self) -> usize {
        self.shared
            .sink
            .as_ref()
            .map(|b| b.lock().unwrap().len())
            .unwrap_or(0)
    }

    /// 是不是交互模式（决定要不要往屏幕上打提示）。
    fn is_interactive(&self) -> bool {
        self.shared.sink.is_none()
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    pub fn mark_closed(&self) {
        self.shared.closed.store(true, Ordering::SeqCst);
    }

    pub fn take_failure(&self) -> Option<String> {
        self.shared.failure.lock().unwrap().take()
    }

    /// 阻塞等待连接建立，超时或已关闭返回 false。
    pub fn wait_connected(&self, timeout: Duration) -> bool {
        self.wait_until(timeout, |s| s.connected.load(Ordering::SeqCst))
    }

    /// 阻塞等待远端 PTY 就绪。
    pub fn wait_opened(&self, timeout: Duration) -> bool {
        self.wait_until(timeout, |s| s.opened.load(Ordering::SeqCst))
    }

    fn wait_until(&self, timeout: Duration, cond: impl Fn(&Shared) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if cond(&self.shared) {
                return true;
            }
            if self.is_closed() || Instant::now() >= deadline {
                return cond(&self.shared);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl InvokeUiSession for ConsoleHandler {
    fn set_cursor_data(&self, _cd: CursorData) {}
    fn set_cursor_id(&self, _id: String) {}
    fn set_cursor_position(&self, _cp: CursorPosition) {}
    fn set_display(&self, _x: i32, _y: i32, _w: i32, _h: i32, _cursor_embedded: bool, _scale: f64) {
    }
    fn switch_display(&self, _display: &SwitchDisplay) {}

    fn set_peer_info(&self, peer_info: &PeerInfo) {
        log::info!(
            "对端: {} / {} 个显示器",
            peer_info.hostname,
            peer_info.displays.len()
        );
    }

    fn set_displays(&self, _displays: &Vec<DisplayInfo>) {}
    fn set_platform_additions(&self, _data: &str) {}

    fn on_connected(&self, _conn_type: ConnType) {
        self.shared.connected.store(true, Ordering::SeqCst);
        if self.is_interactive() {
            println!("[rdterm] 已连接，正在打开远程终端…");
        }
    }

    fn update_privacy_mode(&self) {}
    fn set_permission(&self, _name: &str, _value: bool) {}

    fn close_success(&self) {
        self.mark_closed();
    }

    fn update_quality_status(&self, _qs: QualityStatus) {}

    fn set_connection_type(&self, _is_secured: bool, direct: bool, _stream_type: &str) {
        if !self.is_interactive() {
            return;
        }
        if direct {
            println!("[rdterm] 连接方式: 直连 (P2P)");
        } else {
            println!("[rdterm] 连接方式: 中继 (relay)");
        }
    }

    fn set_fingerprint(&self, _fingerprint: String) {}

    fn job_error(&self, _id: i32, _err: String, _file_num: i32) {}
    fn job_done(&self, _id: i32, _file_num: i32) {}
    fn clear_all_jobs(&self) {}

    fn new_message(&self, msg: String) {
        println!("[rdterm] 对端消息: {msg}");
    }

    fn update_transfer_list(&self) {}
    fn load_last_job(&self, _cnt: i32, _job_json: &str, _auto_start: bool) {}

    fn update_folder_files(
        &self,
        _id: i32,
        _entries: &Vec<FileEntry>,
        _path: String,
        _is_local: bool,
        _only_count: bool,
    ) {
    }

    fn confirm_delete_files(&self, _id: i32, _i: i32, _name: String) {}

    fn override_file_confirm(
        &self,
        _id: i32,
        _file_num: i32,
        _to: String,
        _is_upload: bool,
        _is_identical: bool,
    ) {
    }

    fn update_block_input_state(&self, _on: bool) {}
    fn job_progress(&self, _id: i32, _file_num: i32, _speed: f64, _finished_size: f64) {}
    fn adapt_size(&self) {}

    fn on_rgba(&self, _display: usize, _rgba: &mut scrap::ImageRgb) {}

    /// 连接失败 / 密码错误 / 被拒等都走这里。打印原因并结束会话。
    fn msgbox(&self, msgtype: &str, title: &str, text: &str, _link: &str, _retry: bool) {
        let reason = format!("{title}: {text}");
        if self.is_interactive() {
            eprintln!("\r\n[rdterm] {msgtype} —— {reason}");
        }
        *self.shared.failure.lock().unwrap() = Some(reason);
        self.mark_closed();
    }

    fn cancel_msgbox(&self, _tag: &str) {}
    fn switch_back(&self, _id: &str) {}
    fn portable_service_running(&self, _running: bool) {}
    fn on_voice_call_started(&self) {}
    fn on_voice_call_closed(&self, _reason: &str) {}
    fn on_voice_call_waiting(&self) {}
    fn on_voice_call_incoming(&self) {}

    fn get_rgba(&self, _display: usize) -> *const u8 {
        std::ptr::null()
    }

    fn next_rgba(&self, _display: usize) {}

    fn set_multiple_windows_session(&self, _sessions: Vec<WindowsSession>) {}
    fn set_current_display(&self, _disp_idx: i32) {}
    fn update_record_status(&self, _start: bool) {}
    fn printer_request(&self, _id: i32, _path: String) {}
    fn handle_screenshot_resp(&self, _sid: String, _msg: String) {}

    /// 远端 PTY 的输出落到这里 —— 这是控制端的核心。
    fn handle_terminal_response(&self, response: TerminalResponse) {
        use base::message_proto::terminal_response::Union;

        match response.union {
            Some(Union::Opened(opened)) => {
                if opened.success {
                    self.shared.opened.store(true, Ordering::SeqCst);
                } else {
                    if self.is_interactive() {
                        eprintln!("\r\n[rdterm] 远端打开终端失败: {}", opened.message);
                    }
                    self.mark_closed();
                }
            }
            Some(Union::Data(data)) => {
                // 输出量大时服务端会压缩，这里必须还原，否则会打印乱码。
                let out = if data.compressed {
                    hbb_common::compress::decompress(&data.data)
                } else {
                    data.data.to_vec()
                };
                match &self.shared.sink {
                    // 缓冲模式：攒着，等 MCP 的 read / exec 来取。
                    Some(buf) => buf.lock().unwrap().extend_from_slice(&out),
                    None => {
                        let stdout = std::io::stdout();
                        let mut lock = stdout.lock();
                        lock.write_all(&out).ok();
                        lock.flush().ok();
                    }
                }
            }
            Some(Union::Closed(closed)) => {
                if self.is_interactive() {
                    eprintln!("\r\n[rdterm] 远端终端已关闭（退出码 {}）", closed.exit_code);
                }
                self.mark_closed();
            }
            Some(Union::Error(e)) => {
                if self.is_interactive() {
                    eprintln!("\r\n[rdterm] 远端终端错误: {}", e.message);
                }
                self.mark_closed();
            }
            // 未来协议新增的分支不应该让客户端崩掉，记日志即可。
            other => log::debug!("未处理的终端响应: {other:?}"),
        }
    }
}

/// 控制端入口：连到对端并打开一个远程终端。
pub fn run_client(peer: &str, password: Option<&str>) {
    let handler = ConsoleHandler::default();

    let session: Session<ConsoleHandler> = Session {
        password: password.unwrap_or_default().to_owned(),
        ui_handler: handler.clone(),
        ..Default::default()
    };
    session.lc.write().unwrap().initialize(
        peer.to_owned(),
        ConnType::TERMINAL,
        None, // switch_uuid
        false, // force_relay
        None, // adapter_luid
        None, // shared_password
        None, // conn_token
    );
    LocalConfig::set_remote_id(peer);

    println!("[rdterm] 正在连接 {peer} …");

    // io_loop 自带 current_thread tokio runtime，直接丢进线程即可。
    let round = session.connection_round_state.lock().unwrap().new_round();
    let loop_session = session.clone();
    std::thread::spawn(move || io_loop(loop_session, round));

    if !handler.wait_connected(CONNECT_TIMEOUT) {
        match handler.take_failure() {
            Some(reason) => eprintln!("[rdterm] 连接失败: {reason}"),
            None => eprintln!("[rdterm] 连接超时（{CONNECT_TIMEOUT:?}）"),
        }
        return;
    }

    let (cols, rows) = terminal_size();
    session.open_terminal(TERMINAL_ID, rows, cols);

    if !handler.wait_opened(OPEN_TIMEOUT) {
        match handler.take_failure() {
            Some(reason) => eprintln!("[rdterm] 打开终端失败: {reason}"),
            None => eprintln!("[rdterm] 等待远端终端就绪超时（{OPEN_TIMEOUT:?}）"),
        }
        return;
    }

    // 切原始模式：关闭行缓冲与回显，让每一次按键立刻发到远端，
    // 由远端 shell 决定回显，这样 tab 补全、方向键、Ctrl-C 才正常。
    let _raw = enable_raw_mode();
    println!("[rdterm] 远程终端已就绪（Ctrl-C / exit 退出）\r");

    let stdin_session = session.clone();
    let stdin_handler = handler.clone();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1024];
        while !stdin_handler.is_closed() {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let input = String::from_utf8_lossy(&buf[..n]).to_string();
                    stdin_session.send_terminal_input(TERMINAL_ID, input);
                }
                Err(e) => {
                    log::debug!("stdin 读取结束: {e}");
                    break;
                }
            }
        }
        stdin_handler.mark_closed();
    });

    // 主线程只负责等会话结束；输出到 stdout 是在 io_loop 线程里完成的。
    while !handler.is_closed() {
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 从 stdin 读一行（去掉首尾空白）。返回 `None` 表示 stdin 已关闭。
pub fn prompt_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(line.trim().to_string()),
        Err(_) => None,
    }
}

/// 读一行密码，输入过程不回显。
///
/// Windows 下临时摘掉 `ENABLE_ECHO_INPUT` 再恢复；密码本身不过渡处理，
/// 只去掉行尾换行，避免用户设的密码里有空格被吃掉。
pub fn prompt_password(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();

    #[cfg(windows)]
    let saved = {
        use winapi::shared::minwindef::DWORD;
        use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
        use winapi::um::processenv::GetStdHandle;
        use winapi::um::winbase::STD_INPUT_HANDLE;
        use winapi::um::wincon::ENABLE_ECHO_INPUT;

        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode: DWORD = 0;
            if GetConsoleMode(handle, &mut mode) != 0 {
                SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT);
                Some((handle, mode))
            } else {
                None
            }
        }
    };

    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);

    #[cfg(windows)]
    if let Some((handle, mode)) = saved {
        use winapi::um::consoleapi::SetConsoleMode;
        unsafe {
            SetConsoleMode(handle, mode);
        }
    }

    // 回显被关掉了，换行得自己补。
    println!();
    let _ = std::io::stdout().flush();

    match read {
        Ok(0) => None,
        Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
        Err(_) => None,
    }
}

/// 查询当前控制台尺寸，返回 `(列数, 行数)`。取不到时回落到 80x24。
#[cfg(windows)]
fn terminal_size() -> (u32, u32) {
    use winapi::um::processenv::GetStdHandle;
    use winapi::um::winbase::STD_OUTPUT_HANDLE;
    use winapi::um::wincon::{GetConsoleScreenBufferInfo, CONSOLE_SCREEN_BUFFER_INFO};

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(handle, &mut info) != 0 {
            let cols = (info.srWindow.Right - info.srWindow.Left + 1).max(1) as u32;
            let rows = (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as u32;
            return (cols, rows);
        }
    }
    (80, 24)
}

#[cfg(not(windows))]
fn terminal_size() -> (u32, u32) {
    (80, 24)
}

/// 把 stdin 切成原始模式，并在 Drop 时恢复。
#[cfg(windows)]
fn enable_raw_mode() -> Option<ConsoleModeGuard> {
    use winapi::shared::minwindef::DWORD;
    use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
    use winapi::um::processenv::GetStdHandle;
    use winapi::um::winbase::STD_INPUT_HANDLE;
    use winapi::um::wincon::{ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT};

    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode: DWORD = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return None;
        }
        let raw = mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
        if SetConsoleMode(handle, raw) == 0 {
            return None;
        }
        Some(ConsoleModeGuard { handle, mode })
    }
}

#[cfg(not(windows))]
fn enable_raw_mode() -> Option<()> {
    None
}

#[cfg(windows)]
struct ConsoleModeGuard {
    handle: winapi::um::winnt::HANDLE,
    mode: winapi::shared::minwindef::DWORD,
}

#[cfg(windows)]
impl Drop for ConsoleModeGuard {
    fn drop(&mut self) {
        use winapi::um::consoleapi::SetConsoleMode;
        unsafe {
            SetConsoleMode(self.handle, self.mode);
        }
    }
}
