//! `terminal-only` 桩服务（no-op replacements）。
//!
//! 精简版不采集屏幕、不采集声音。但 `server.rs` / `connection.rs` 里有大量针对
//! video / display / audio 服务的调用点，逐个 `cfg` 门控既啰嗦又容易漏。
//! 这里提供同名的**空实现桩模块**，让这些调用点原样编译通过，但：
//!
//! * 不会创建任何采集器（`Capturer`）
//! * 不注册任何真实的 video / audio 服务
//! * 不产生、不发送任何视频帧或音频帧
//!
//! 这样 `libs/scrap`（以及它背后的 vcpkg：ffmpeg / aom / libvpx / libyuv）就完全不参与编译。

use std::sync::Mutex;
use std::time::Instant;

use hbb_common::ResultType;

use super::service::GenericService;
use base::message_proto::{DisplayInfo, Message};

/// `scrap::camera` 的替代桩：永远报告「没有摄像头」。
pub mod camera {
    pub const PRIMARY_CAMERA_IDX: usize = 0;

    pub fn primary_camera_exists() -> bool {
        false
    }

    pub struct Cameras;

    impl Cameras {
        pub fn get_sync_cameras() -> Vec<base::message_proto::DisplayInfo> {
            Vec::new()
        }
    }
}

/// `video_service` 的替代桩。
pub mod video_service {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum VideoSource {
        Monitor,
        Camera,
    }

    impl VideoSource {
        pub fn service_name_prefix(&self) -> &'static str {
            "\0rdterm-no-video\0"
        }
    }

    pub const OPTION_REFRESH: &'static str = "refresh";

    pub fn get_service_name(_source: VideoSource, _idx: usize) -> String {
        "\0rdterm-no-video\0".to_owned()
    }

    pub fn new(source: VideoSource, idx: usize) -> GenericService {
        GenericService::new(get_service_name(source, idx), false)
    }

    /// 视频 QoS 的空实现。方法签名与 `video_qos::VideoQoS` 保持一致。
    pub struct VideoQoS;

    impl Default for VideoQoS {
        fn default() -> Self {
            Self
        }
    }

    impl VideoQoS {
        pub fn bitrate(&self) -> u32 {
            0
        }
        pub fn user_delay_response_elapsed(&mut self, _id: i32, _elapsed: u128) {}
        pub fn user_network_delay(&mut self, _id: i32, _delay: u32) {}
        pub fn user_auto_adjust_fps(&mut self, _id: i32, _fps: u32) {}
        pub fn user_record(&mut self, _id: i32, _v: bool) {}
        pub fn user_image_quality(&mut self, _id: i32, _image_quality: i32) {}
        pub fn user_custom_fps(&mut self, _id: i32, _fps: u32) {}
        pub fn on_connection_open(&mut self, _id: i32) {}
        pub fn on_connection_close(&mut self, _id: i32) {}
    }

    pub static VIDEO_QOS: Mutex<VideoQoS> = Mutex::new(VideoQoS);

    pub static IS_UAC_RUNNING: Mutex<bool> = Mutex::new(false);
    pub static IS_FOREGROUND_WINDOW_ELEVATED: Mutex<bool> = Mutex::new(false);

    pub fn qos_diag_verbose() -> bool {
        false
    }

    pub fn refresh() {}

    pub fn test_create_capturer(
        _privacy_mode_id: i32,
        _display_idx: usize,
        _timeout_millis: u64,
    ) -> String {
        "".to_owned()
    }

    pub fn notify_video_frame_fetched(_display_idx: usize, _conn_id: i32, _tm: Option<Instant>) {}

    pub fn notify_video_frame_fetched_by_conn_id(_conn_id: i32, _tm: Option<Instant>) {}

    pub fn make_display_changed_msg(
        _display_idx: usize,
        _opt_display: Option<DisplayInfo>,
        _source: VideoSource,
    ) -> Option<Message> {
        None
    }

    pub fn set_take_screenshot<T>(
        _source: VideoSource,
        _display_idx: usize,
        _sid: String,
        _tx: T,
    ) {
    }
}

/// `display_service` 的替代桩：报告「零个显示器」。
pub mod display_service {
    use super::*;

    pub fn new() -> GenericService {
        GenericService::new("\0rdterm-no-display\0".to_owned(), false)
    }

    /// 返回 true，这样 `Server::new()` 就不会去注册鼠标/键盘输入服务。
    pub fn capture_cursor_embedded() -> bool {
        true
    }

    pub fn is_inited_msg() -> Option<Message> {
        None
    }

    pub async fn update_get_sync_displays_on_login() -> ResultType<(Vec<DisplayInfo>, usize)> {
        Ok((Vec::new(), 0))
    }

    pub fn get_sync_displays() -> Vec<DisplayInfo> {
        Vec::new()
    }

    pub fn try_get_displays() -> ResultType<Vec<scrap::Display>> {
        Ok(Vec::new())
    }

    pub fn set_last_changed_resolution(
        _display_name: &str,
        _original: (i32, i32),
        _changed: (i32, i32),
    ) {
    }

    pub fn restore_resolutions() {}

    /// 隐私模式（放大镜方案）依赖屏幕采集，本版本不支持。
    pub fn is_privacy_mode_mag_supported() -> bool {
        false
    }
}

/// `portable_service` 的替代桩。
///
/// 真实的 `portable_service` 是 Windows 上「走共享内存投递桌面画面」的提权采集
/// 方案，整个模块建立在 `scrap::Capturer` / `Display` / `PixelBuffer` 之上。
/// 本版本没有屏幕采集，所以：
/// * `running()` 永远为 false → 所有提权/共享内存分支都不会被触发
/// * 输入注入（`handle_mouse` / `handle_pointer` / `handle_key`）直接走本进程的
///   `input_service`，不再经过提权 helper
/// * `start_portable_service()` / `run_portable_service()` 为空操作
#[cfg(windows)]
pub mod portable_service {
    use super::*;

    pub mod client {
        use base::message_proto::{KeyEvent, MouseEvent};

        pub enum StartPara {
            Direct,
            Logon(String, String),
        }

        pub fn set_quick_support(_v: bool) {}

        /// 提权采集服务在本版本里不存在，启动必然失败；调用方只记录日志。
        pub fn start_portable_service(_para: StartPara) -> hbb_common::ResultType<()> {
            hbb_common::bail!("portable service is unavailable in the terminal-only build")
        }

        pub fn running() -> bool {
            false
        }

        pub fn get_cursor_info(pci: winapi::um::winuser::PCURSORINFO) -> winapi::shared::minwindef::BOOL {
            unsafe { winapi::um::winuser::GetCursorInfo(pci) }
        }

        pub fn handle_mouse(
            evt: &MouseEvent,
            conn: i32,
            username: String,
            argb: u32,
            simulate: bool,
            show_cursor: bool,
        ) {
            crate::input_service::handle_mouse_(evt, conn, username, argb, simulate, show_cursor);
        }

        pub fn handle_pointer(evt: &base::message_proto::PointerDeviceEvent, conn: i32) {
            crate::input_service::handle_pointer_(evt, conn);
        }

        pub fn handle_key(evt: &KeyEvent) {
            crate::input_service::handle_key_(evt);
        }
    }

    pub mod server {
        /// 没有提权采集服务可跑。
        pub fn run_portable_service() {}
    }

    pub fn portable_service_shmem_arg(_name: &str) -> String {
        String::new()
    }

    pub fn portable_service_shmem_name_from_args() -> Option<String> {
        None
    }

    pub fn has_portable_service_shmem_arg() -> bool {
        false
    }
}

/// `audio_service` 的替代桩：无声音输入、无语音通话。
pub mod audio_service {
    use super::*;

    pub const NAME: &'static str = "\0rdterm-no-audio\0";
    pub const AUDIO_DATA_SIZE_U8: usize = 0;

    pub fn new() -> GenericService {
        GenericService::new(NAME.to_owned(), false)
    }

    pub fn restart() {}

    pub fn set_voice_call_input_device(_device: Option<String>, _is_default: bool) {}

    pub fn get_voice_call_input_device() -> Option<String> {
        None
    }

    #[cfg(target_os = "macos")]
    pub fn is_screen_capture_kit_available() -> bool {
        false
    }
}
