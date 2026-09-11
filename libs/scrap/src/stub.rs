//! A capture-free stand-in for this crate, selected by the `stub` feature.
//!
//! Short version: the terminal-only flavour of RustDesk never captures, encodes or
//! records a screen, but several modules still *compile against* the scrap API (the
//! video handler's format enum, the login handshake's supported-decoding probe, the
//! record path's state enum). Those call sites never run, so every function here
//! returns "nothing to do".
//!
//! Deliberately minimal: adding an item here should always mean some still-compiled
//! call site needs it. If a symbol is only reachable through video / display /
//! camera code, it does not belong here.

use std::os::raw::c_void;

use base::message_proto::{SupportedDecoding, SupportedEncoding};
use hbb_common::ResultType;

/// The video codecs the protocol knows about. Used as a format tag throughout the
/// client; here it can never be produced by an actual encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CodecFormat {
    #[default]
    VP8,
    VP9,
    AV1,
    H264,
    H265,
    Unknown,
}

/// Pixel layout of an RGB buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageFormat {
    #[default]
    Raw,
    ABGR,
    ARGB,
}

/// A decoded frame's pixels. Always empty here: nothing ever decodes.
#[derive(Clone, Default)]
pub struct ImageRgb {
    pub raw: Vec<u8>,
    pub w: usize,
    pub h: usize,
    pub fmt: ImageFormat,
    pub align: usize,
}

impl ImageRgb {
    pub fn new(fmt: ImageFormat, align: usize) -> Self {
        Self {
            raw: Vec::new(),
            w: 0,
            h: 0,
            fmt,
            align,
        }
    }

    #[inline]
    pub fn fmt(&self) -> ImageFormat {
        self.fmt
    }

    #[inline]
    pub fn align(&self) -> usize {
        self.align
    }

    #[inline]
    pub fn set_align(&mut self, align: usize) {
        self.align = align;
    }
}

/// A GPU texture handle. Always null here.
pub struct ImageTexture {
    pub texture: *mut c_void,
    pub w: usize,
    pub h: usize,
}

impl Default for ImageTexture {
    fn default() -> Self {
        Self {
            texture: std::ptr::null_mut(),
            w: 0,
            h: 0,
        }
    }
}

impl From<&base::message_proto::VideoFrame> for CodecFormat {
    fn from(_it: &base::message_proto::VideoFrame) -> Self {
        CodecFormat::Unknown
    }
}

/// A display. Always reports empty: nothing is ever captured, so there is
/// nothing to enumerate. Exists only so call sites that *name* `scrap::Display`
/// (e.g. `display_service::try_get_displays`) keep compiling — the methods
/// mirror the real `scrap::Display` API (`name`/`width`/`height`/`scale`/…).
#[derive(Clone, Debug, Default)]
pub struct Display {
    pub name: String,
    pub width: usize,
    pub height: usize,
    pub scale: f64,
    pub origin: (i32, i32),
    pub is_primary: bool,
}

impl Display {
    pub fn name(&self) -> String {
        self.name.clone()
    }
    pub fn width(&self) -> usize {
        self.width
    }
    pub fn height(&self) -> usize {
        self.height
    }
    pub fn scale(&self) -> f64 {
        self.scale
    }
    pub fn origin(&self) -> (i32, i32) {
        self.origin
    }
    pub fn is_primary(&self) -> bool {
        self.is_primary
    }
}

pub mod codec {
    use super::*;
    use base::message_proto::{Chroma, SupportedDecoding, SupportedEncoding};
    use std::collections::HashMap;

    /// Nothing to decode, so nothing switches codecs mid-session.
    pub const ENCODE_NEED_SWITCH: &'static str = "ENCODE_NEED_SWITCH";

    pub struct Decoder {
        format: CodecFormat,
    }

    impl Decoder {
        pub fn new(format: CodecFormat, _luid: Option<i64>) -> Self {
            Self { format }
        }

        pub fn format(&self) -> CodecFormat {
            self.format
        }

        /// No real decoder exists; report valid so the surrounding
        /// mark-unsupported bookkeeping doesn't trip on a missing method.
        pub fn valid(&self) -> bool {
            true
        }

        /// No decoder exists, so we advertise no decoding ability. The peer then
        /// falls back to whatever it can do; nothing on this side consumes video.
        pub fn supported_decodings(
            _id_for_perfer: Option<&str>,
            _use_texture_render: bool,
            _luid: Option<i64>,
            _mark_unsupported: &Vec<CodecFormat>,
        ) -> SupportedDecoding {
            SupportedDecoding::default()
        }

        pub fn handle_video_frame(
            &mut self,
            _frame: &base::message_proto::video_frame::Union,
            _rgb: &mut ImageRgb,
            _texture: &mut ImageTexture,
            _pixelbuffer: &mut bool,
            _chroma: &mut Option<Chroma>,
        ) -> ResultType<bool> {
            Ok(false)
        }
    }

    pub struct Encoder;

    impl Encoder {
        pub fn supported_encoding() -> SupportedEncoding {
            SupportedEncoding::default()
        }

        pub fn usable_encoding() -> Option<SupportedEncoding> {
            None
        }

        pub fn update(_update: EncodingUpdate) {}
    }

    #[derive(Debug, Clone)]
    pub enum EncodingUpdate {
        Update(i32, SupportedDecoding),
        Remove(i32),
        NewOnlyVP9(i32),
        Check,
    }

    pub fn enable_hwcodec_option() -> bool {
        false
    }

    pub fn test_av1() {}

    /// Kept because the real codec module is consulted at startup; nothing to probe.
    pub fn get_peer_codecs(_id: &str) -> SupportedDecoding {
        SupportedDecoding::default()
    }

    pub fn peer_codecs(_all: bool) -> HashMap<String, SupportedDecoding> {
        HashMap::new()
    }
}

pub mod record {
    use super::*;

    /// Screen recording states. Nothing generates them without a capturer.
    #[derive(Debug, Clone)]
    pub enum RecordState {
        NewFile(String),
        NewFrame,
        WriteTail,
        RemoveFile,
    }

    #[derive(Debug, Clone)]
    pub struct RecorderContext {
        pub server: bool,
        pub id: String,
        pub dir: String,
        pub display_idx: usize,
        pub camera: bool,
        pub tx: Option<std::sync::mpsc::Sender<RecordState>>,
    }

    pub struct Recorder;

    impl Recorder {
        pub fn new(_ctx: RecorderContext) -> ResultType<Self> {
            Err(hbb_common::anyhow::anyhow!(
                "screen recording is unavailable in this build"
            ))
        }

        pub fn write_frame(
            &mut self,
            _frame: &base::message_proto::video_frame::Union,
            _w: usize,
            _h: usize,
        ) -> ResultType<()> {
            Ok(())
        }
    }
}
