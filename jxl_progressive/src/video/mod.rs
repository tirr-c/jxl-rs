#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::io::prelude::*;

use color_eyre::eyre::{Result, eyre};
use jxl::api::JxlColorType;
use jxl::image::OwnedRawImage;
use rusty_ffmpeg::ffi as ffmpeg;

mod context;
mod filter;

pub use context::VideoContext;

pub struct Mp4FileEncoder<'w, Writer> {
    inner: VideoContext<&'w mut Writer>,
    font: std::ffi::CString,
    inited: bool,
    buffered_empty_frames: Vec<String>,
}

impl<'w, Writer: Write + Seek> Mp4FileEncoder<'w, Writer> {
    pub fn new(writer: &'w mut Writer, font: &str) -> Result<Self> {
        let inner = VideoContext::new(writer)?;
        let c_font = format!("{font}\0");
        Ok(Self {
            inner,
            font: std::ffi::CString::from_vec_with_nul(c_font.into_bytes()).unwrap(),
            inited: false,
            buffered_empty_frames: Vec::new(),
        })
    }

    pub fn init(&mut self, size: (usize, usize), color_type: JxlColorType) -> Result<()> {
        self.inner.init_video(size, color_type, &self.font)?;
        self.inited = true;
        for desc in std::mem::take(&mut self.buffered_empty_frames) {
            self.add_empty_frame(desc)?;
        }
        Ok(())
    }

    pub fn add_empty_frame(&mut self, desc: impl std::fmt::Display) -> Result<()> {
        if !self.inited {
            self.buffered_empty_frames.push(desc.to_string());
            return Ok(());
        }

        self.inner.write_empty_frame(desc)
    }

    pub fn add_frame(
        &mut self,
        image: &mut OwnedRawImage,
        desc: impl std::fmt::Display,
        transformer: &moxcms::Transform8BitExecutor,
    ) -> Result<()> {
        self.inner.write_frame(image, desc, transformer)
    }

    pub fn finalize(&mut self) -> Result<()> {
        self.inner.finalize(false)
    }

    pub fn finalize_skip_still(&mut self) -> Result<()> {
        self.inner.finalize(true)
    }
}

trait FfmpegErrorExt {
    fn into_ffmpeg_result(self) -> Result<()>;
}

impl FfmpegErrorExt for c_int {
    #[inline]
    fn into_ffmpeg_result(self) -> Result<()> {
        if self < 0 {
            Err(eyre!("{}", ffmpeg::av_err2str(self)))
        } else {
            Ok(())
        }
    }
}

#[allow(improper_ctypes_definitions)]
unsafe extern "C" fn ffmpeg_log(
    avcl: *mut c_void,
    level: c_int,
    fmt: *const c_char,
    vl: va_list::VaList<'static>,
) {
    let mut out = vec![0u8; 65536];
    unsafe {
        let vsnprintf = std::mem::transmute::<
            unsafe extern "C" fn(*mut c_char, std::ffi::c_ulong, *const c_char, _) -> i32,
            unsafe extern "C" fn(_, _, _, va_list::VaList<'static>) -> i32,
        >(ffmpeg::vsnprintf);
        vsnprintf(out.as_mut_ptr() as *mut c_char, 65536, fmt, vl);
    }

    let len = out.iter().position(|&v| v == 0).unwrap();
    let line = String::from_utf8_lossy(&out[..len]);
    let line = line.trim_end();

    let log_level = match level {
        ..=16 => 1,
        17..=24 => 2,
        25..=32 => 3,
        33..=40 => 4,
        _ => return,
    };

    let avcl = avcl as *mut *const ffmpeg::AVClass;
    let avc = if avcl.is_null() {
        std::ptr::null()
    } else {
        unsafe { *avcl }
    };

    let header = if !avc.is_null() {
        let item_name = unsafe {
            let item_name_fn = (*avc).item_name.unwrap_or(ffmpeg::av_default_item_name);
            let name = CStr::from_ptr(item_name_fn(avcl as *mut _));
            name.to_string_lossy()
        };
        format!("[{item_name}] ")
    } else {
        String::new()
    };

    match log_level {
        1 => eprintln!("ffmpeg: error: {header}{line}"),
        2 => eprintln!("ffmpeg:  warn: {header}{line}"),
        3 => eprintln!("ffmpeg:  info: {header}{line}"),
        _ => eprintln!("ffmpeg: debug: {header}{line}"),
    }
}

fn init_ffmpeg_log() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let cb = std::mem::transmute::<unsafe extern "C" fn(_), unsafe extern "C" fn(*const c_void)>(ffmpeg::av_log_set_callback);
        cb(ffmpeg_log as *const _);
    });
}
