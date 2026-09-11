//! `rdterm` —— RustDesk 的「终端精简版」。
//!
//! * 无图形界面（不编译 Flutter / Sciter / 托盘）
//! * 无远程屏幕采集（不编译 video / display / camera / 编解码）
//! * 不保留音频、剪贴板、文件传输、端口转发、打印机、隐私模式
//! * 只保留远程终端（`ConnType::TERMINAL`，Windows 上是 cmd.exe / PowerShell，通过 ConPTY）
//! * 纯绿色：配置与日志落在可执行文件所在目录
//!
//! 设计文档见 `docs/terminal-only/DESIGN.md`。

use std::io::Write;

mod console;
mod identity;
mod mcp;
mod remote;

use hbb_common::config;
use hbb_common::log;

/// 从命令行解析出的运行模式。
enum Mode {
    /// 交互式菜单：双击 exe 时用，不用记参数。
    Menu,
    /// 被控端：常驻，等待别人连进来开终端。
    Server {
        /// 启动之后怎么处置自己那个控制台窗口。
        win: WindowMode,
    },
    /// MCP 服务：让 Agent 直接接管远程终端。
    Mcp {
        /// `Some(addr)` 走 HTTP（`POST /mcp`），`None` 走 stdio。
        http: Option<String>,
        /// 是否同时起被控端（这样别人也能连进来）。
        server: bool,
    },
    /// 结束一个后台（隐藏/最小化）运行的被控端。
    Stop,
    /// 控制端：连到对端开一个远程终端。
    Connect {
        peer: String,
        password: Option<String>,
    },
    /// 只打印本机 ID / 密码后退出。
    Id,
    /// 设置固定（永久）密码，写进配置文件后退出。
    SetPassword(String),
    Help,
}

/// 被控端起来之后，自己那个控制台窗口怎么处理。
#[derive(Clone, Copy, PartialEq, Eq)]
enum WindowMode {
    /// 保持显示（默认）。
    Normal,
    /// 最小化到任务栏，点一下还能回来。
    Minimize,
    /// 完全隐藏，任务栏也看不到，只能在任务管理器里结束。
    Hidden,
}

const USAGE: &str = "\
rdterm —— RustDesk 终端精简版（无界面 / 无屏幕采集 / 仅远程终端 / 纯绿色）

用法:
  rdterm                               双击/无参数时进入交互式菜单
  rdterm --menu                        强制进入交互式菜单
  rdterm --server [--hide|--minimize]  以被控端模式运行，打印本机 ID 与密码
  rdterm --hide                        被控端运行并把窗口完全隐藏（后台无窗口）
  rdterm --minimize                    被控端运行并把窗口最小化到任务栏
  rdterm --stop                        结束后台运行的被控端（读 rdterm.pid）
  rdterm --id                          只打印本机 ID 后退出
  rdterm --connect <PEER_ID> [--password <PWD>]
                                       连到对端并打开一个远程终端
  rdterm --mcp                         以 MCP 服务运行（stdio，供 Agent 直接接管）
  rdterm --mcp-http <ADDR>             MCP over HTTP，默认 127.0.0.1:8787
  rdterm --mcp --server                MCP + 同时起被控端
  rdterm --set-password <PWD>          已废弃：本版本不校验密码
  rdterm --help                        显示本帮助

选项:
  --config-dir <DIR>  指定配置目录（默认：可执行文件所在目录）
  --hide / --minimize 只影响被控端自己的控制台窗口。隐藏后看不到 ID 了，
                      程序会把它写到配置目录下的 rdterm-identity.txt。
                      要停止隐藏的被控端：rdterm --stop，或任务管理器结束 rdterm.exe。
  --no-direct-server  关闭 IP 直连监听（默认开 21118，见下）
  --log[=LEVEL]       把 RustDesk 内部日志打到 stderr，排障用（默认 info）

ID 与密码:
  不校验任何密码，凭证就是 ID。
  ID 是随机生成的（不再由 MAC 派生），每 12 小时自动轮换一次，
  状态记在配置目录的 rdterm-id.json，轮换后原来那个 ID 立即失效。

两种连法:
  1) ID：rdterm_connect {\"peer\":\"<12位ID>\"}        —— 需要 rendezvous 服务器可达
  2) IP：rdterm_connect {\"peer\":\"192.168.1.20\"}      —— 直连 21118，不经过服务器
     同机实测用 127.0.0.1，局域网填内网 IP，端口不对就写 IP:端口。
     被控端默认开着直连监听，注意这意味着能连到该 IP 的人都能开终端。

MCP 工具（Agent 用）:
  rdterm_identity   查看本机 ID 与剩余有效期
  rdterm_connect    连到对端开远程终端，返回 session id
  rdterm_exec       在会话里执行命令并返回输出
  rdterm_read       取走会话尚未读取的输出
  rdterm_sessions   列出当前所有会话
  rdterm_resize     调整远端终端尺寸
  rdterm_close      关闭会话
";

/// 窗口隐藏后，ID / 密码只能从这里看。
const IDENTITY_FILE: &str = "rdterm-identity.txt";
/// 后台被控端的 PID，`--stop` 用它。
const PID_FILE: &str = "rdterm.pid";

/// Windows 控制台默认用 GBK 代码页，Rust 的 `println!` 输出的是 UTF-8，
/// 直接打印中文会变成乱码。启动时把输入/输出代码页都切成 65001（UTF-8）。
#[cfg(windows)]
fn init_console_utf8() {
    use winapi::um::wincon::{SetConsoleCP, SetConsoleOutputCP};
    const CP_UTF8: u32 = 65001;
    unsafe {
        SetConsoleOutputCP(CP_UTF8);
        SetConsoleCP(CP_UTF8);
    }
}

#[cfg(not(windows))]
fn init_console_utf8() {}

/// 只在需要排查时才打开的日志开关：`--log` / `--log=debug` / 环境变量 `RDTERM_LOG`。
///
/// RustDesk 的 log 初始化挂在 GUI 路径上，terminal-only 下从没跑过，
/// 于是所有 `log::info!` 都被静默丢弃。这里补一个打到 stderr 的极简 logger——
/// stdio 模式下 stdout 是 JSON-RPC 专用通道，日志只能走 stderr。
fn init_logger(argv: &[String]) {
    let spec = argv
        .iter()
        .find_map(|a| match a.strip_prefix("--log=") {
            Some(s) => Some(s.to_owned()),
            None => (a == "--log").then(|| "info".to_owned()),
        })
        .or_else(|| std::env::var("RDTERM_LOG").ok());
    let Some(spec) = spec else { return };
    let level = match spec.as_str() {
        "error" => log::LevelFilter::Error,
        "warn" => log::LevelFilter::Warn,
        "debug" | "trace" => log::LevelFilter::Debug,
        _ => log::LevelFilter::Info,
    };
    static LOGGER: StderrLogger = StderrLogger;
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(level);
        eprintln!("[rdterm] 日志已开启（level={level}），输出到 stderr");
    }
}

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _m: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        eprintln!("[{}] [{}] {}", r.level(), r.target(), r.args());
    }
    fn flush(&self) {}
}

pub fn run() {
    init_console_utf8();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mode = parse_args(&argv);
    // RustDesk 只在 GUI 里初始化 logger，精简版从来没初始化过，
    // 于是所有 log::info 都被丢掉，出问题根本没法查。`--log` 补上这个口子。
    init_logger(&argv);

    // 必须在任何配置读取之前完成，否则 ID/密钥会落到 %APPDATA% 里。
    if let Err(e) = init_portable(&argv) {
        eprintln!("[rdterm] 绿色化失败: {e}");
    }
    // 随机 ID（12 小时一换）要赶在 RustDesk 任何地方读 `Config::get_id()` 之前落定，
    // 否则 rendezvous 会拿 MAC 派生的老 ID 去注册。
    identity::ensure();

    if !crate::common::global_init() {
        eprintln!("[rdterm] global_init 失败");
        return;
    }
    crate::common::load_custom_client();
    #[cfg(windows)]
    if !crate::platform::windows::bootstrap() {
        eprintln!("[rdterm] windows bootstrap 失败");
        return;
    }

    match mode {
        Mode::Help => print!("{}", USAGE),
        Mode::Menu => run_menu(),
        Mode::Id => print_identity(),
        // 密码已经不用了，留着这个参数是为了不让人输错命令时一头雾水。
        Mode::SetPassword(_) => explain_password(),
        Mode::Server { win } => run_server(win),
        Mode::Mcp { http, server } => run_mcp(http.as_deref(), server),
        Mode::Stop => stop_server(),
        Mode::Connect { peer, password } => run_client(&peer, password.as_deref()),
    }

    crate::common::global_clean();
}

fn parse_args(argv: &[String]) -> Mode {
    let mut it = argv.iter();
    // 被控端可以晚点定：先收齐 `--server / --hide / --minimize` 再统一构造，
    // 这样 `--server --hide` 和 `--hide --server` 两种写法都认。
    let mut server = false;
    let mut win = WindowMode::Normal;
    let mut mcp = false;
    let mut mcp_http: Option<String> = None;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => return Mode::Help,
            "--menu" | "-m" => return Mode::Menu,
            "--server" | "-s" => server = true,
            "--mcp" => mcp = true,
            "--mcp-http" => {
                mcp = true;
                mcp_http = Some(
                    it.next()
                        .cloned()
                        .unwrap_or_else(|| "127.0.0.1:8787".to_owned()),
                );
            }
            "--hide" => {
                server = true;
                win = WindowMode::Hidden;
            }
            "--minimize" => {
                server = true;
                win = WindowMode::Minimize;
            }
            "--stop" => return Mode::Stop,
            "--id" => return Mode::Id,
            "--set-password" => {
                let pwd = it.next().cloned().unwrap_or_default();
                if pwd.is_empty() {
                    eprintln!("[rdterm] --set-password 后面缺少密码");
                    return Mode::Help;
                }
                return Mode::SetPassword(pwd);
            }
            "--connect" | "-c" => {
                let peer = it.next().cloned().unwrap_or_default();
                if peer.is_empty() {
                    eprintln!("[rdterm] --connect 后面缺少对端 ID");
                    return Mode::Help;
                }
                let mut password = None;
                while let Some(next) = it.next() {
                    if next == "--password" {
                        password = it.next().cloned();
                    }
                }
                return Mode::Connect { peer, password };
            }
            // 全局选项：真正的处理在别处，这里只是别把它们当未知参数。
            "--config-dir" => {
                it.next();
            }
            "--no-direct-server" | "--log" => {}
            other if other.starts_with("--log=") => {}
            other => {
                // 关键：不认识就报错，绝不能默默退化成「被控端」。
                // 历史上就吃过这个亏 —— RustDesk 内部会 spawn `SelfExe --cm`，
                // 那时 `--cm` 落到这里会起第二个 server，抢同一个 ID 把会话踢掉。
                eprintln!("[rdterm] 无法识别的参数: {other}");
                return Mode::Help;
            }
        }
    }
    if mcp {
        return Mode::Mcp {
            http: mcp_http,
            server,
        };
    }
    if server {
        return Mode::Server { win };
    }
    // 双击 exe 时会走到这里。有人盯着看的控制台就给菜单，
    // 被脚本/管道调用（stdin 不是控制台）时退回原来的「无参数=被控端」行为。
    if is_interactive_console() {
        Mode::Menu
    } else {
        Mode::Server {
            win: WindowMode::Normal,
        }
    }
}

/// stdin 是不是真正的控制台。取不到就当作非交互，避免脚本里卡在菜单上。
#[cfg(windows)]
fn is_interactive_console() -> bool {
    use winapi::um::consoleapi::GetConsoleMode;
    use winapi::um::processenv::GetStdHandle;
    use winapi::um::winbase::STD_INPUT_HANDLE;
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode = 0u32;
        GetConsoleMode(handle, &mut mode) != 0
    }
}

#[cfg(not(windows))]
fn is_interactive_console() -> bool {
    false
}

/// 绿色化：把配置/日志根目录钉到可执行文件所在目录，并换掉 APP_NAME，
/// 免得跟机器上安装过的 RustDesk 抢同一份配置。
fn init_portable(argv: &[String]) -> std::io::Result<()> {
    let dir = argv
        .iter()
        .position(|a| a == "--config-dir")
        .and_then(|i| argv.get(i + 1))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .unwrap_or_default()
        });

    std::fs::create_dir_all(&dir)?;
    *config::APP_DIR.write().unwrap() = dir.to_string_lossy().into_owned();
    *config::APP_NAME.write().unwrap() = "rdterm".to_owned();
    Ok(())
}

/// 打印本机 ID。
///
/// 本版本已经去掉密码校验：入网凭证就是那个随机 ID，12 小时一换。
fn print_identity() {
    let (id, _) = identity::current();
    println!("ID       : {id}");
    println!(
        "有效期   : 还剩 {}（到期自动轮换，见 rdterm-id.json）",
        humanize_ttl(identity::ttl_secs())
    );
    println!("密码     : 无 —— 本版本不校验密码，ID 就是凭证");
    let _ = std::io::stdout().flush();
}

/// 秒数转「x 小时 y 分」。
fn humanize_ttl(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 {
        format!("{h} 小时 {m} 分")
    } else {
        format!("{m} 分")
    }
}

/// 密码相关：本版本已经不校验密码了，留个说明免得白设。
fn explain_password() {
    println!("[rdterm] 本版本已去掉密码校验，不需要也不能设置密码。");
    println!("[rdterm] 入网凭证是随机 ID（12 小时轮换），连的时候只填 ID 就行。");
}

fn print_banner() {
    println!("rdterm {} —— 终端精简版（无界面 / 无屏幕采集）", crate::VERSION);
    println!("配置文件: {}", config::APP_DIR.read().unwrap());
}

/// 把 ID 写进配置目录。窗口一旦隐藏/最小化，屏幕上就看不着了。
fn write_identity_file() -> std::io::Result<std::path::PathBuf> {
    let (id, _) = identity::current();
    let mut s = String::new();
    s.push_str(&format!("ID       : {id}\n"));
    s.push_str(&format!(
        "有效期   : 还剩 {}（到期自动轮换）\n",
        humanize_ttl(identity::ttl_secs())
    ));
    s.push_str("密码     : 无 —— 本版本不校验密码\n");
    let path = std::path::Path::new(&config::APP_DIR.read().unwrap().clone()).join(IDENTITY_FILE);
    std::fs::write(&path, s)?;
    Ok(path)
}

/// 记下自己的 PID，`--stop` 靠它找到后台那个被控端。
fn write_pid_file() -> std::io::Result<std::path::PathBuf> {
    let path = std::path::Path::new(&config::APP_DIR.read().unwrap().clone()).join(PID_FILE);
    std::fs::write(&path, std::process::id().to_string())?;
    Ok(path)
}

/// `rdterm --stop`：结束后台（隐藏/最小化）运行的被控端。
fn stop_server() {
    let dir = std::path::PathBuf::from(config::APP_DIR.read().unwrap().clone());
    let pid_path = dir.join(PID_FILE);
    let pid = std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    match pid {
        Some(pid) => {
            println!("[rdterm] 正在结束被控端进程 PID={pid}");
            let ok = std::process::Command::new("taskkill")
                .args(["/f", "/pid", &pid.to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                let _ = std::fs::remove_file(&pid_path);
                println!("[rdterm] 已停止");
            } else {
                eprintln!("[rdterm] taskkill 失败，进程可能已经退出了");
            }
        }
        None => println!(
            "[rdterm] 没找到 {}，没有记录在案的后台被控端。\n\
             要结束所有 rdterm 进程：taskkill /f /im rdterm.exe",
            pid_path.display()
        ),
    }
}

/// 真正动窗口：隐藏或者最小化。
///
/// 只有在「这个控制台是我们独占的」时候才敢 `SW_HIDE`。
/// 从已有的 cmd / PowerShell 里启动时，控制台是共享的，
/// 隐藏会连人家的窗口一起藏掉，那时候退化成最小化。
#[cfg(windows)]
fn apply_window_mode(win: WindowMode) {
    use winapi::um::wincon::{GetConsoleProcessList, GetConsoleWindow};
    use winapi::um::winuser::{ShowWindow, SW_HIDE, SW_MINIMIZE};

    if win == WindowMode::Normal {
        return;
    }
    unsafe {
        let hwnd = GetConsoleWindow();
        if hwnd.is_null() {
            // 没有自己的控制台窗口（服务/计划任务/管道里跑的），无从隐藏，直接算了。
            eprintln!("[rdterm] 取不到控制台窗口，跳过隐藏/最小化");
            return;
        }
        let mut pids = [0u32; 32];
        let n = GetConsoleProcessList(pids.as_mut_ptr(), 32);
        let exclusive = n == 1;
        let cmd = match (win, exclusive) {
            (WindowMode::Hidden, true) => SW_HIDE,
            (WindowMode::Hidden, false) => {
                eprintln!("[rdterm] 当前控制台是与宿主共享的，改用最小化而不是隐藏");
                SW_MINIMIZE
            }
            (WindowMode::Minimize, _) => SW_MINIMIZE,
            (WindowMode::Normal, _) => return,
        };
        ShowWindow(hwnd, cmd);
    }
}

#[cfg(not(windows))]
fn apply_window_mode(_win: WindowMode) {}

/// 被控端模式：常驻。`start_server` 内部自带 tokio runtime，会一直阻塞。
/// 打开直连监听（默认 21118/TCP）。
///
/// 开了之后控制端可以用 `rdterm --connect <IP>` 或 MCP `rdterm_connect {"peer":"<IP>"}`
/// 直接建链，不需要 rendezvous 服务器中转——本机、局域网、以及任何能直连到这台机器
/// 的场景都能用。关掉它加 `--no-direct-server`。
fn enable_direct_server() {
    if !direct_server_enabled() {
        return;
    }
    hbb_common::config::Config::set_option("direct-server".to_owned(), "Y".to_owned());
    let port = hbb_common::config::Config::get_option("direct-access-port")
        .parse::<i32>()
        .unwrap_or(0);
    let port = if port > 0 { port } else { 21116 + 2 };
    eprintln!("[rdterm] 已开启 IP 直连监听 0.0.0.0:{port}（--no-direct-server 可关闭）");
}

/// 命令行里有没有显式关掉直连。
fn direct_server_enabled() -> bool {
    !std::env::args().any(|a| a == "--no-direct-server")
}

fn run_server(win: WindowMode) {
    print_banner();
    print_identity();
    // ID 到期自动换，换完重新注册。
    identity::spawn_rotation_thread();
    // 开直连监听（默认 21118）。rdterm 无密码、ID 又随机，走 rendezvous 需要公网，
    // 而同一台机器 / 局域网里用 IP 直连最省事，所以默认打开。
    enable_direct_server();

    if win != WindowMode::Normal {
        match write_identity_file() {
            Ok(p) => println!("[rdterm] ID 已写入 {}", p.display()),
            Err(e) => eprintln!("[rdterm] 写身份文件失败: {e}"),
        }
        if let Err(e) = write_pid_file() {
            eprintln!("[rdterm] 写 PID 文件失败: {e}");
        }
        let what = if win == WindowMode::Hidden {
            "隐藏本窗口"
        } else {
            "最小化本窗口"
        };
        println!(
            "[rdterm] 3 秒后{what}。停止请执行 `rdterm --stop`，或在任务管理器里结束 rdterm.exe"
        );
        let _ = std::io::stdout().flush();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            apply_window_mode(win);
        });
    }

    // start_server 会阻塞，另起一个线程在它起来之后补一次状态输出。
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        println!("[rdterm] 正在等待连接…（Ctrl-C 退出）");
        let _ = std::io::stdout().flush();
    });

    crate::start_server(true, false);
}

/// MCP 模式：把远程终端暴露给 Agent。
///
/// `http` 为 `None` 时走 stdio，此时 stdout 归 JSON-RPC 所有，
/// 任何提示都只能走 stderr。
fn run_mcp(http: Option<&str>, with_server: bool) {
    let say = |s: String| match http {
        Some(_) => println!("{s}"),
        None => eprintln!("{s}"),
    };
    say(format!(
        "rdterm {} —— MCP 服务（{}）",
        crate::VERSION,
        match http {
            Some(a) => format!("HTTP http://{a}/mcp"),
            None => "stdio".to_owned(),
        }
    ));
    let (id, _) = identity::current();
    say(format!("[rdterm] 本机 ID: {id}"));

    if with_server {
        if http.is_none() {
            // RustDesk 内部有些启动日志是直接 println 的，走 stdio 会串进 JSON-RPC 流里。
            say("[rdterm] 警告：stdio 模式下被控端的内部日志可能混入协议流，\
                 建议改用 --mcp-http"
                .to_owned());
        }
        say("[rdterm] 同时启动被控端（后台）".to_owned());
        identity::spawn_rotation_thread();
        enable_direct_server();
        std::thread::spawn(|| crate::start_server(true, false));
        // 等一下，让 server 先把 ID 注册上去。
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }

    match http {
        Some(addr) => {
            if let Err(e) = mcp::serve_http(addr) {
                eprintln!("[rdterm] MCP HTTP 启动失败: {e}");
            }
        }
        None => mcp::serve_stdio(),
    }
}

/// 控制端模式：连到对端开一个远程终端。
///
/// 实现在 `cli::console`：`ConsoleHandler` 负责把远端 PTY 输出写 stdout，
/// 独立线程读 stdin 转发给远端。启动时会把控制台切成原始模式，
/// 退出时自动恢复。
fn run_client(peer: &str, password: Option<&str>) {
    console::run_client(peer, password);
}

/// 起被控端之前问一句窗口怎么办。直接回车就是保持显示。
fn ask_window_mode() -> WindowMode {
    println!();
    println!("本窗口如何处理？（隐藏后仍可从 rdterm-identity.txt 看到 ID/密码）");
    println!("  1) 保持显示");
    println!("  2) 最小化到任务栏");
    println!("  3) 完全隐藏（后台无窗口，需 rdterm --stop 才能停）");
    print!("> ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => WindowMode::Normal,
        Ok(_) => match line.trim() {
            "2" => WindowMode::Minimize,
            "3" => WindowMode::Hidden,
            _ => WindowMode::Normal,
        },
    }
}

/// 交互式菜单：双击 exe 时用，不用背命令行参数。
fn run_menu() {
    print_banner();
    loop {
        println!();
        println!("请选择操作：");
        println!("  1) 作为被控端运行   —— 打印本机 ID，等待别人连进来");
        println!("  2) 连接到远程主机   —— 本机当控制端，进入远程终端");
        println!("  3) 查看本机 ID");
        println!("  4) 关于密码");
        println!("  5) 帮助");
        println!("  6) 停止后台运行的被控端");
        println!("  7) 启动 MCP 服务     —— 让 Agent 通过 MCP 接管");
        println!("  0) 退出");
        print!("> ");
        let _ = std::io::stdout().flush();

        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            // stdin 关了（管道结束 / 控制台断开）就退出。
            // 这里必须判 Ok(0)：否则空行会被当成「无效选择」，菜单会无限重刷。
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }

        match line.trim() {
            "1" => {
                // 会一直阻塞到被控端停止（正常情况就是 Ctrl-C）。
                run_server(ask_window_mode());
                return;
            }
            "2" => {
                let peer = match console::prompt_line("对端 ID: ") {
                    Some(v) if !v.is_empty() => v,
                    _ => {
                        println!("[rdterm] 已取消（未输入 ID）");
                        continue;
                    }
                };
                // 本版本不校验密码，直接连。
                console::run_client(&peer, None);
            }
            "3" => print_identity(),
            "4" => explain_password(),
            "5" => print!("{}", USAGE),
            "6" => stop_server(),
            "7" => {
                let addr = console::prompt_line("监听地址（回车用 127.0.0.1:8787）: ")
                    .unwrap_or_default();
                let addr = if addr.is_empty() {
                    "127.0.0.1:8787".to_owned()
                } else {
                    addr
                };
                println!("[rdterm] 启动 MCP（HTTP {addr}），Ctrl-C 结束");
                let _ = std::io::stdout().flush();
                if let Err(e) = mcp::serve_http(&addr) {
                    eprintln!("[rdterm] MCP 启动失败: {e}");
                }
            }
            "0" | "q" | "quit" => return,
            // 直接回车：安静地重刷菜单，不报错。
            "" => {}
            other => println!("[rdterm] 无效选择: {other}"),
        }
    }
}
