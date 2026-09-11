//! MCP（Model Context Protocol）服务端 —— 让 Agent 直接接管远程终端。
//!
//! 两种传输，都不引入新依赖：
//! * **stdio**：一行一个 JSON-RPC（`rdterm --mcp`），Agent 直接 spawn 这个进程；
//! * **HTTP**：`rdterm --mcp-http 127.0.0.1:8787`，`POST /mcp` 收 JSON-RPC。
//!   手写 HTTP/1.1，够用就好。
//!
//! 约束：stdio 模式下 stdout 只准写 JSON-RPC，任何日志都必须走 stderr，
//! 否则会污染协议流。

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};

use serde_json::{json, Value};

use super::{identity, remote};

/// MCP 协议版本。客户端会在 initialize 里带上自己的版本，这里原样回一个能用的。
const PROTOCOL_VERSION: &str = "2024-11-05";

/// 一条命令最多等多久（毫秒）。
const DEFAULT_MAX_WAIT_MS: u64 = 5000;
/// 输出静默多久就认为命令跑完了（毫秒）。
const DEFAULT_QUIET_MS: u64 = 400;

// ---------------------------------------------------------------- 协议分发

/// 处理一个 JSON-RPC 请求。通知（没有 id）返回 `None`，也就是不回包。
fn handle_request(req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let is_notification = id.is_none() || req.get("id").map(Value::is_null).unwrap_or(false);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    let outcome: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(initialize_result()),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => tools_call(req.get("params")),
        "resources/list" => Ok(json!({ "resources": [] })),
        "prompts/list" => Ok(json!({ "prompts": [] })),
        "ping" => Ok(json!({})),
        other => Err((-32601, format!("method not found: {other}"))),
    };

    if is_notification {
        return None;
    }

    Some(match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": { "listChanged": false },
        },
        "serverInfo": {
            "name": "rdterm",
            "version": crate::VERSION,
        },
        "instructions": "rdterm 是纯终端的远程控制工具。先用 rdterm_identity 拿到本机 ID \
                         （给别人连你）；要接管别人就用 rdterm_connect 连过去拿到 session，\
                         然后 rdterm_exec 发命令、rdterm_read 取输出，用完 rdterm_close。\
                         注意：无密码模式，凭证就是那个每 12 小时轮换的随机 ID。",
    })
}

// ---------------------------------------------------------------- 工具定义

fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "rdterm_identity",
            "description": "查看本机 ID（别人用它连进来）以及还剩多久轮换。无密码模式，ID 就是凭证。",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "rdterm_connect",
            "description": "连到对端并打开一个远程终端，返回 session id。\
                             peer 可以是 12 位 ID（走 rendezvous 服务器），\
                             也可以是 IP 或 IP:端口（走直连，默认端口 21118，不走服务器）。\
                             同机或局域网优先用 IP，成功率更高。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer": { "type": "string", "description": "对端 ID，或 IP / IP:端口（如 192.168.1.20 或 192.168.1.20:21118）" },
                    "password": { "type": "string", "description": "一般留空；仅当对端仍启用密码时才需要" },
                    "rows": { "type": "integer", "description": "终端行数，默认 24" },
                    "cols": { "type": "integer", "description": "终端列数，默认 120" },
                },
                "required": ["peer"],
            },
        }),
        json!({
            "name": "rdterm_exec",
            "description": "在指定会话里执行一条命令，返回该命令的输出（已剥掉 ANSI 转义与颜色码）。\
                             结束时机会等到命令真正跑完（内部追加 echo 哨兵判定），\
                             所以不用担心拿到上一条命令的残留输出；超时会明确标注。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "integer", "description": "rdterm_connect 返回的会话 id" },
                    "command": { "type": "string", "description": "要执行的命令，换行由服务端补" },
                    "wait_ms": { "type": "integer", "description": "最多等待毫秒数，默认 5000" },
                },
                "required": ["session", "command"],
            },
        }),
        json!({
            "name": "rdterm_read",
            "description": "取走会话自上次读取以来累积的输出（不会等待）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "integer" },
                },
                "required": ["session"],
            },
        }),
        json!({
            "name": "rdterm_sessions",
            "description": "列出当前所有远程终端会话。",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "rdterm_resize",
            "description": "调整远端终端尺寸，避免输出被折行。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "integer" },
                    "rows": { "type": "integer" },
                    "cols": { "type": "integer" },
                },
                "required": ["session", "rows", "cols"],
            },
        }),
        json!({
            "name": "rdterm_close",
            "description": "关闭并释放一个远程终端会话。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "integer" },
                },
                "required": ["session"],
            },
        }),
    ]
}

fn tools_call(params: Option<&Value>) -> Result<Value, (i64, String)> {
    let p = params.ok_or((-32602, "missing params".to_owned()))?;
    let name = p
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or((-32602, "missing name".to_owned()))?;
    let args = p.get("arguments").cloned().unwrap_or_else(|| json!({}));

    match call_tool(name, &args) {
        Ok(text) => Ok(json!({
            "content": [ { "type": "text", "text": text } ],
            "isError": false,
        })),
        Err(e) => Ok(json!({
            "content": [ { "type": "text", "text": e } ],
            "isError": true,
        })),
    }
}

fn arg_u32(args: &Value, key: &str, default: u32) -> u32 {
    args.get(key)
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(default)
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

/// 取会话：找不到就报错，比在每个工具里重复判断省事。
fn need_session(args: &Value) -> Result<std::sync::Arc<remote::RemoteSession>, String> {
    let id = arg_u32(args, "session", 0);
    remote::get(id).ok_or_else(|| format!("没有会话 {id}，先用 rdterm_connect 连一个"))
}

fn call_tool(name: &str, args: &Value) -> Result<String, String> {
    match name {
        "rdterm_identity" => {
            let (id, _) = identity::current();
            Ok(json!({
                "id": id,
                "ttl_seconds": identity::ttl_secs(),
                "version": crate::VERSION,
                "pid": std::process::id(),
                "hint": "把这个 ID 给对方，对方用 rdterm_connect 连进来（无需密码）",
            })
            .to_string())
        }
        "rdterm_connect" => {
            let peer = arg_str(args, "peer").ok_or("缺少 peer（对端 ID）")?;
            let rows = arg_u32(args, "rows", 24);
            let cols = arg_u32(args, "cols", 120);
            let s = remote::RemoteSession::connect(peer, arg_str(args, "password"), rows, cols)?;
            Ok(json!({
                "session": s.id,
                "peer": s.peer,
                "status": "ready",
            })
            .to_string())
        }
        "rdterm_exec" => {
            let s = need_session(args)?;
            let command = arg_str(args, "command").ok_or("缺少 command")?;
            let wait = args
                .get("wait_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_MAX_WAIT_MS);
            let r = s.exec(command, wait.max(200), DEFAULT_QUIET_MS);
            if s.is_closed() {
                if let Some(reason) = s.take_failure() {
                    return Err(format!("会话已断开: {reason}\n{}", r.output).trim().to_owned());
                }
            }
            if r.complete {
                Ok(r.output)
            } else {
                // 没等到哨兵：要么命令还在跑，要么它进了交互模式（python / ssh 之类）。
                Ok(format!(
                    "{}\n[rdterm] 未在 {}ms 内等到命令结束标记，输出可能不完整；\
                     若命令仍在等待输入，请用 rdterm_close 结束会话重开。",
                    r.output, wait
                ))
            }
        }
        "rdterm_read" => {
            let s = need_session(args)?;
            Ok(remote::strip_ansi(&s.take_output()))
        }
        "rdterm_sessions" => Ok(json!({ "sessions": remote::list() }).to_string()),
        "rdterm_resize" => {
            let s = need_session(args)?;
            let rows = arg_u32(args, "rows", 24);
            let cols = arg_u32(args, "cols", 120);
            s.resize(rows, cols);
            Ok(format!("已把会话 {} 的终端改成 {rows}x{cols}", s.id))
        }
        "rdterm_close" => {
            let s = need_session(args)?;
            let id = s.id;
            s.close();
            Ok(format!("会话 {id} 已关闭"))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

// ---------------------------------------------------------------- stdio 传输

/// stdio 模式：stdin 收一行 JSON，stdout 回一行 JSON。
pub fn serve_stdio() {
    // 日志别往 stdout 走，会串进协议里。
    eprintln!("[rdterm] MCP stdio 已就绪（protocol {PROTOCOL_VERSION}）");

    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("[rdterm] 读 stdin 失败: {e}");
                break;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[rdterm] 不是合法 JSON，跳过: {e}");
                continue;
            }
        };
        if let Some(resp) = handle_request(&req) {
            let out = std::io::stdout();
            let mut lock = out.lock();
            let _ = writeln!(lock, "{resp}");
            let _ = lock.flush();
        }
    }
}

// ---------------------------------------------------------------- HTTP 传输

/// HTTP 模式：`POST /mcp` 收 JSON-RPC，`GET /health` 探活。
pub fn serve_http(addr: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    println!("[rdterm] MCP 已监听 http://{addr}/mcp （POST JSON-RPC）");
    println!("[rdterm] 探活: http://{addr}/health");
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                std::thread::spawn(move || handle_http_conn(s));
            }
            Err(e) => eprintln!("[rdterm] accept 失败: {e}"),
        }
    }
    Ok(())
}

fn handle_http_conn(mut stream: TcpStream) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];

    // 读到 "\r\n\r\n" 就够解析头部了。
    let header_end = loop {
        match stream.read(&mut tmp) {
            Ok(0) => break None,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(pos + 4);
                }
            }
            Err(_) => break None,
        }
    };
    let Some(header_end) = header_end else {
        return;
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut content_length = 0usize;
    for line in lines {
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }

    // 把可能已经读进来的 body 部分补上。
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }

    let response_body = match (method.as_str(), path.as_str()) {
        ("OPTIONS", _) => None,
        // 探活两种方法都认，省得客户端用 POST 打过来收到一个 204 不知所措。
        ("GET", "/health") | ("POST", "/health") => {
            Some(json!({ "ok": true, "service": "rdterm-mcp" }).to_string())
        }
        ("GET", _) => Some(
            json!({
                "service": "rdterm-mcp",
                "protocolVersion": PROTOCOL_VERSION,
                "endpoint": "POST /mcp  (JSON-RPC 2.0)",
                "tools": tools().iter().map(|t| t["name"].clone()).collect::<Vec<_>>(),
            })
            .to_string(),
        ),
        ("POST", _) => {
            let text = String::from_utf8_lossy(&body).to_string();
            match serde_json::from_str::<Value>(text.trim()) {
                Ok(req) => handle_request(&req).map(|v| v.to_string()),
                Err(e) => Some(
                    json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": { "code": -32700, "message": format!("parse error: {e}") },
                    })
                    .to_string(),
                ),
            }
        }
        _ => Some(json!({ "error": "use POST /mcp" }).to_string()),
    };

    let _ = write_http_response(&mut stream, response_body.as_deref());
}

fn write_http_response(stream: &mut TcpStream, body: Option<&str>) -> std::io::Result<()> {
    // CORS 放开，方便浏览器里的 MCP 客户端 / 调试页面直接连。
    let head = match body {
        None => "HTTP/1.1 204 No Content\r\n\
                 Access-Control-Allow-Origin: *\r\n\
                 Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
                 Access-Control-Allow-Headers: Content-Type\r\n\
                 Connection: close\r\n\r\n"
            .to_string(),
        Some(b) => format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json; charset=utf-8\r\n\
             Content-Length: {}\r\n\
             Access-Control-Allow-Origin: *\r\n\
             Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
             Access-Control-Allow-Headers: Content-Type\r\n\
             Connection: close\r\n\r\n",
            b.len()
        ),
    };
    stream.write_all(head.as_bytes())?;
    if let Some(b) = body {
        stream.write_all(b.as_bytes())?;
    }
    stream.flush()
}
