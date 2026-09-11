//! 随机 ID 与 12 小时生命周期。
//!
//! 去掉密码之后，ID 就是唯一的入网凭证，所以对它的要求变了：
//! * **不能再用 MAC 派生** —— 那样同一台机器永远是同一个 ID，克隆机/虚拟机还会撞车；
//! * **必须密码学随机** —— 9 位数字空间太小，无密码场景下会被扫出来，这里用 12 位；
//! * **12 小时一换** —— 就算 ID 泄露，窗口也有限。
//!
//! 状态落在配置目录的 `rdterm-id.json`，进程重启后沿用同一个 ID，直到过期。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use hbb_common::config::{self, Config};

/// ID 有效期：12 小时。
pub const ID_TTL_SECS: u64 = 12 * 3600;

const STATE_FILE: &str = "rdterm-id.json";
/// ID 的数字位数。12 位 ≈ 10^12，配上限速的失败重试已经足够难扫。
const ID_LEN_DIGITS: u32 = 12;

/// 当前 ID 与它的诞生时间。进程内缓存一份，避免每次都读盘。
static CURRENT: Mutex<Option<(String, u64)>> = Mutex::new(None);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn state_path() -> PathBuf {
    PathBuf::from(config::APP_DIR.read().unwrap().clone()).join(STATE_FILE)
}

/// 生成一个新的随机 ID（12 位数字，首位不为 0）。
fn gen_id() -> String {
    use hbb_common::rand::Rng;
    let min = 10u64.pow(ID_LEN_DIGITS - 1);
    let max = 10u64.pow(ID_LEN_DIGITS);
    hbb_common::rand::thread_rng()
        .gen_range(min..max)
        .to_string()
}

/// 从状态文件读 ID。文件不存在/损坏都当作「没有」。
fn load() -> Option<(String, u64)> {
    let s = std::fs::read_to_string(state_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    let id = v.get("id")?.as_str()?.to_owned();
    let created_at = v.get("created_at")?.as_u64()?;
    if id.is_empty() {
        return None;
    }
    Some((id, created_at))
}

fn save(id: &str, created_at: u64) {
    let v = serde_json::json!({ "id": id, "created_at": created_at });
    if let Err(e) = std::fs::write(state_path(), v.to_string()) {
        eprintln!("[rdterm] 写 ID 状态文件失败: {e}");
    }
}

/// 启用一个新 ID：写盘 + 交给 RustDesk 的 Config + 更新进程内缓存。
fn adopt(id: &str, created_at: u64) {
    Config::set_id(id);
    save(id, created_at);
    *CURRENT.lock().unwrap() = Some((id.to_owned(), created_at));
}

/// 启动时调用：保证 `Config::get_id()` 拿到的是一个有效（未过期）的随机 ID。
pub fn ensure() -> (String, u64) {
    let (id, created_at) = match load() {
        Some((id, created_at)) if now_secs() < created_at + ID_TTL_SECS => (id, created_at),
        _ => {
            let id = gen_id();
            let created_at = now_secs();
            (id, created_at)
        }
    };
    adopt(&id, created_at);
    (id, created_at)
}

/// 当前 ID（必须先 `ensure()`）。
pub fn current() -> (String, u64) {
    match CURRENT.lock().unwrap().clone() {
        Some(v) => v,
        None => (Config::get_id(), now_secs()),
    }
}

/// 当前 ID 还剩多少秒过期。
pub fn ttl_secs() -> u64 {
    let (_, created_at) = current();
    (created_at + ID_TTL_SECS).saturating_sub(now_secs())
}

/// 到期就换新 ID。返回新 ID 表示确实换了。
///
/// 换完之后要让 rendezvous 重新注册（否则别人还是按老 ID 找过来），
/// `restart()` 会让 mediator 的下一次循环重新读 `Config::get_id()`。
pub fn rotate_if_expired() -> Option<String> {
    if ttl_secs() > 0 {
        return None;
    }
    let id = gen_id();
    let created_at = now_secs();
    adopt(&id, created_at);
    hbb_common::log::info!("[rdterm] ID 已到期轮换，新 ID: {id}");
    println!("[rdterm] ID 已到期轮换，新 ID: {id}");
    // 重新注册，否则 rendezvous 那边还是老 ID。
    crate::rendezvous_mediator::RendezvousMediator::restart();
    Some(id)
}

/// 起一个后台线程，每分钟检查一次 ID 是否到期。
pub fn spawn_rotation_thread() {
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
        rotate_if_expired();
    });
}

/// 给 `--id` / 菜单 / MCP 用的一行摘要。
pub fn summary() -> String {
    let (id, _) = current();
    format!("{id}（{} 后轮换）", humanize(ttl_secs().max(1)))
}

fn humanize(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 {
        format!("{h} 小时 {m} 分")
    } else {
        format!("{m} 分")
    }
}
