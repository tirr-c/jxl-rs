use std::{
    ffi::{CStr, c_int, c_void},
    io::prelude::*,
};

use color_eyre::eyre::{Result, WrapErr, bail};
use jxl::{api::JxlColorType, image::OwnedRawImage};
use rusty_ffmpeg::ffi as ffmpeg;
use rusty_ffmpeg::ffi::av_err2str;

use super::FfmpegErrorExt;

trait BufTypeHelp: Copy {
    fn as_const(self) -> *const u8;
}

impl BufTypeHelp for *const u8 {
    fn as_const(self) -> *const u8 {
        self
    }
}

impl BufTypeHelp for *mut u8 {
    fn as_const(self) -> *const u8 {
        self as *const u8
    }
}

pub struct VideoContext<W> {
    size: (usize, usize),
    writer_ptr: *mut W,
    avio_ctx: *mut ffmpeg::AVIOContext,
    muxer_ctx: *mut ffmpeg::AVFormatContext,
    video_codec: *const ffmpeg::AVCodec,
    video_ctx: *mut ffmpeg::AVCodecContext,
    video_stream: *mut ffmpeg::AVStream,
    packet_ptr: *mut ffmpeg::AVPacket,
    source_frame_ptr: *mut ffmpeg::AVFrame,
    video_frame_ptr: *mut ffmpeg::AVFrame,
    filters: super::filter::VideoFilter,
    muxer_use_global_header: bool,
    pts: usize,
}

impl<W: Write + Seek> VideoContext<W> {
    const BUFFER_SIZE: usize = 4096;

    pub fn new(writer: W) -> Result<Self> {
        super::init_ffmpeg_log();

        let fmt_mp4 = unsafe {
            let mut it = std::ptr::null_mut();
            loop {
                let output_fmt = ffmpeg::av_muxer_iterate(&mut it);
                if output_fmt.is_null() {
                    bail!("output format mp4 not found");
                }
                let name = std::ffi::CStr::from_ptr((*output_fmt).name);
                if name == c"mp4" {
                    break output_fmt;
                }
            }
        };

        let muxer_use_global_header =
            unsafe { (*fmt_mp4).flags as u32 & ffmpeg::AVFMT_GLOBALHEADER != 0 };

        let mut output = Self {
            size: (0, 0),
            writer_ptr: std::ptr::null_mut(),
            avio_ctx: std::ptr::null_mut(),
            muxer_ctx: std::ptr::null_mut(),
            video_codec: std::ptr::null(),
            video_ctx: std::ptr::null_mut(),
            video_stream: std::ptr::null_mut(),
            packet_ptr: std::ptr::null_mut(),
            source_frame_ptr: std::ptr::null_mut(),
            video_frame_ptr: std::ptr::null_mut(),
            filters: super::filter::VideoFilter::new(),
            muxer_use_global_header,
            pts: 0,
        };

        let buffer = unsafe {
            let buffer = ffmpeg::av_malloc(Self::BUFFER_SIZE);
            if buffer.is_null() {
                panic!("cannot allocate memory of 4 KiB");
            }
            buffer
        };

        let writer = Box::new(writer);
        let writer_ptr = Box::into_raw(writer);
        output.writer_ptr = writer_ptr;

        let avio_ctx = unsafe {
            let ctx = ffmpeg::avio_alloc_context(
                buffer as *mut _,
                Self::BUFFER_SIZE as c_int,
                1,
                writer_ptr as *mut _,
                None,
                Some(Self::cb_write_packet),
                Some(Self::cb_seek),
            );
            if ctx.is_null() {
                ffmpeg::av_free(buffer as *mut _);
                bail!("failed to allocate avio context");
            }
            ctx
        };
        output.avio_ctx = avio_ctx;

        let muxer_ctx = unsafe {
            let ctx_ptr = ffmpeg::avformat_alloc_context();
            if ctx_ptr.is_null() {
                bail!("failed to allocate avformat context");
            }

            let ctx = &mut *ctx_ptr;
            // `oformat` is `*mut AVOutputFormat` in ffmpeg4.
            ctx.oformat = fmt_mp4.cast_mut();
            ctx.pb = avio_ctx;

            ctx_ptr
        };
        output.muxer_ctx = muxer_ctx;

        Ok(output)
    }

    unsafe extern "C" fn cb_write_packet<Buf: BufTypeHelp>(
        opaque: *mut c_void,
        buf: Buf,
        buf_size: c_int,
    ) -> c_int {
        let buf = buf.as_const();
        let result = std::panic::catch_unwind(|| unsafe {
            let buf = std::slice::from_raw_parts(buf, buf_size as usize);
            let writer = &mut *(opaque as *mut W);
            writer.write_all(buf)
        });

        match result {
            Ok(Ok(_)) => 0,
            Ok(Err(e)) => e
                .raw_os_error()
                .map(|v| ffmpeg::AVERROR(v as u32))
                .unwrap_or(ffmpeg::AVERROR_UNKNOWN),
            Err(_) => ffmpeg::AVERROR_UNKNOWN,
        }
    }

    unsafe extern "C" fn cb_seek(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
        let result = std::panic::catch_unwind(|| unsafe {
            let writer = &mut *(opaque as *mut W);
            let pos = match whence as u32 {
                ffmpeg::SEEK_CUR => std::io::SeekFrom::Current(offset),
                ffmpeg::SEEK_SET => std::io::SeekFrom::Start(offset as u64),
                ffmpeg::SEEK_END => std::io::SeekFrom::End(offset),
                _ => return Err(std::io::ErrorKind::InvalidInput.into()),
            };
            writer.seek(pos)
        });

        match result {
            Ok(Ok(pos)) => pos as i64,
            Ok(Err(e)) => e
                .raw_os_error()
                .map(|v| ffmpeg::AVERROR(v as u32))
                .unwrap_or(ffmpeg::AVERROR_UNKNOWN) as i64,
            Err(_) => ffmpeg::AVERROR_UNKNOWN as i64,
        }
    }
}

impl<W> VideoContext<W> {
    #[allow(clippy::too_many_arguments)]
    pub fn init_video(
        &mut self,
        size: (usize, usize),
        color_type: JxlColorType,
        font: &CStr,
    ) -> Result<()> {
        assert!(self.video_ctx.is_null());

        let (width, height) = size;
        let video_width = (width.div_ceil(2) * 2) as i32;
        let video_height = (height.div_ceil(2) * 2) as i32;
        let pix_fmt = match color_type {
            JxlColorType::Grayscale | JxlColorType::GrayscaleAlpha => ffmpeg::AV_PIX_FMT_GRAY8,
            JxlColorType::Rgb | JxlColorType::Rgba => ffmpeg::AV_PIX_FMT_RGB24,
            JxlColorType::Bgr | JxlColorType::Bgra => ffmpeg::AV_PIX_FMT_BGR24,
        };
        self.size = size;

        // sRGB, rgb24 full range -> BT.709, yuv420p limited range
        let primaries = ffmpeg::AVCOL_PRI_BT709;
        let colorspace = ffmpeg::AVCOL_SPC_RGB;
        let trc = ffmpeg::AVCOL_TRC_BT709;
        let video_color_range = ffmpeg::AVCOL_RANGE_MPEG;
        let video_pix_fmt = ffmpeg::AV_PIX_FMT_YUV420P;

        let video_codec = unsafe {
            let codec = ffmpeg::avcodec_find_encoder_by_name(c"libx264".as_ptr());
            if codec.is_null() {
                bail!("codec libx264 not found");
            }
            codec
        };
        self.video_codec = video_codec;

        let video_ctx = unsafe {
            let ctx = ffmpeg::avcodec_alloc_context3(self.video_codec);
            if ctx.is_null() {
                bail!("failed to allocate avcodec context");
            }
            ctx
        };
        self.video_ctx = video_ctx;

        let video_stream = unsafe {
            let stream = ffmpeg::avformat_new_stream(self.muxer_ctx, self.video_codec);
            if stream.is_null() {
                bail!("failed to add stream to format");
            }
            stream
        };
        self.video_stream = video_stream;

        let packet_ptr = unsafe {
            let packet = ffmpeg::av_packet_alloc();
            if packet.is_null() {
                bail!("failed to allocate packet");
            }
            packet
        };
        self.packet_ptr = packet_ptr;

        self.filters.init(
            video_width,
            video_height,
            ffmpeg::AVCOL_SPC_RGB,
            pix_fmt,
            colorspace,
            video_pix_fmt,
            video_color_range,
            font,
        )?;

        unsafe {
            let video_stream = &mut *video_stream;
            let video = &mut *video_ctx;

            video_stream.id = 0;

            video.width = video_width;
            video.height = video_height;
            video.pix_fmt = video_pix_fmt;
            video.colorspace = colorspace;
            video.color_trc = trc;
            video.color_primaries = primaries;
            video.color_range = video_color_range;

            video.time_base = self.filters.time_base();
            video.framerate = self.filters.frame_rate();
            video_stream.time_base = video.time_base;
            video_stream.avg_frame_rate = video.framerate;
            video_stream.r_frame_rate = video.framerate;

            ffmpeg::av_opt_set(video.priv_data, c"preset".as_ptr(), c"slow".as_ptr(), 0);
            ffmpeg::av_opt_set(video.priv_data, c"crf".as_ptr(), c"18".as_ptr(), 0);

            if self.muxer_use_global_header {
                video.flags |= ffmpeg::AV_CODEC_FLAG_GLOBAL_HEADER as i32;
            }

            ffmpeg::avcodec_open2(video_ctx, self.video_codec, std::ptr::null_mut())
                .into_ffmpeg_result()?;

            ffmpeg::avcodec_parameters_from_context(video_stream.codecpar, video_ctx)
                .into_ffmpeg_result()?;
        }

        let source_frame_ptr = Self::init_frame(|frame| {
            frame.width = video_width;
            frame.height = video_height;
            frame.format = pix_fmt;
            frame.colorspace = ffmpeg::AVCOL_SPC_RGB;
            frame.color_range = ffmpeg::AVCOL_RANGE_JPEG;
            frame.color_primaries = primaries;
            frame.color_trc = trc;
        })?;
        self.source_frame_ptr = source_frame_ptr;

        let video_frame_ptr = Self::init_frame(|frame| {
            frame.width = video_width;
            frame.height = video_height;
            frame.format = video_pix_fmt;
            frame.colorspace = colorspace;
            frame.color_range = video_color_range;
            frame.color_primaries = primaries;
            frame.color_trc = trc;
        })?;
        self.video_frame_ptr = video_frame_ptr;

        unsafe {
            ffmpeg::avformat_write_header(self.muxer_ctx, std::ptr::null_mut())
                .into_ffmpeg_result()?;
        }

        Ok(())
    }

    fn init_frame(config_fn: impl FnOnce(&mut ffmpeg::AVFrame)) -> Result<*mut ffmpeg::AVFrame> {
        let mut frame_ptr = unsafe {
            let frame_ptr = ffmpeg::av_frame_alloc();
            if frame_ptr.is_null() {
                bail!("failed to allocate frame");
            }
            frame_ptr
        };

        unsafe {
            let frame = &mut *frame_ptr;
            config_fn(frame);

            if let Err(e) = ffmpeg::av_frame_get_buffer(frame_ptr, 0).into_ffmpeg_result() {
                ffmpeg::av_frame_free(&mut frame_ptr);
                return Err(e);
            }
        }

        Ok(frame_ptr)
    }

    fn push_frame(&mut self) -> Result<()> {
        unsafe {
            ffmpeg::av_frame_make_writable(self.video_frame_ptr).into_ffmpeg_result()?;

            self.filters
                .filter(self.source_frame_ptr as *const _, self.video_frame_ptr)?;

            (*self.video_frame_ptr).pts = (*self.source_frame_ptr).pts;
            self.send_frame(self.video_frame_ptr)?;
        }

        Ok(())
    }

    fn repeat_frame(&mut self) -> Result<()> {
        unsafe {
            (*self.video_frame_ptr).pts = self.pts as i64;
            self.send_frame(self.video_frame_ptr)?;
        }

        Ok(())
    }

    unsafe fn send_frame(&mut self, frame_ptr: *mut ffmpeg::AVFrame) -> Result<()> {
        unsafe {
            ffmpeg::avcodec_send_frame(self.video_ctx, frame_ptr).into_ffmpeg_result()?;

            loop {
                let ret = ffmpeg::avcodec_receive_packet(self.video_ctx, self.packet_ptr);
                if ret == ffmpeg::AVERROR(ffmpeg::EAGAIN) || ret == ffmpeg::AVERROR_EOF {
                    break;
                } else if ret < 0 {
                    bail!("{}", av_err2str(ret));
                }

                ffmpeg::av_packet_rescale_ts(
                    self.packet_ptr,
                    (*self.video_ctx).time_base,
                    (*self.video_stream).time_base,
                );
                (*self.packet_ptr).stream_index = (*self.video_stream).index;

                ffmpeg::av_write_frame(self.muxer_ctx, self.packet_ptr).into_ffmpeg_result()?;
            }
        }

        Ok(())
    }

    pub fn write_frame(
        &mut self,
        image: &OwnedRawImage,
        description: impl std::fmt::Display,
        transformer: &moxcms::Transform8BitExecutor,
    ) -> Result<()> {
        if self.video_ctx.is_null() {
            bail!("video context not initialized");
        }

        let description = format!("{description}\0");
        let c_description =
            CStr::from_bytes_with_nul(description.as_bytes()).wrap_err("invalid description")?;

        let (_width, height) = self.size;
        let frame_ptr = self.source_frame_ptr;
        let (data, linesize) = unsafe {
            ffmpeg::av_frame_make_writable(frame_ptr).into_ffmpeg_result()?;

            (*frame_ptr).pts = self.pts as i64;
            ((*frame_ptr).data, (*frame_ptr).linesize)
        };

        let data_ptr = data[0];
        let stride = linesize[0];

        for y in 0..height {
            let output_row = unsafe {
                let base_ptr = if stride < 0 {
                    data_ptr.offset((y + 1) as isize * stride as isize)
                } else {
                    data_ptr.offset(y as isize * stride as isize)
                };
                // SAFETY: pointer is aligned by FFmpeg.
                std::slice::from_raw_parts_mut(base_ptr, stride as usize)
            };
            let image_row = image.row(y);
            transformer.transform(image_row, &mut output_row[..image_row.len()])?;
        }

        self.filters.update_text(c_description)?;
        self.push_frame()?;
        self.pts += 1;
        Ok(())
    }

    pub fn write_empty_frame(&mut self, description: impl std::fmt::Display) -> Result<()> {
        if self.video_ctx.is_null() {
            bail!("video context not initialized");
        }

        let description = format!("{description}\0");
        let c_description =
            CStr::from_bytes_with_nul(description.as_bytes()).wrap_err("invalid description")?;

        let frame_ptr = self.source_frame_ptr;
        let channels = unsafe {
            match (*frame_ptr).format {
                ffmpeg::AV_PIX_FMT_GRAY8 => 1,
                _ => 3,
            }
        };
        let video_width = unsafe { (*frame_ptr).width } as usize;
        let video_height = unsafe { (*frame_ptr).height } as usize;

        let (data, linesize) = unsafe {
            ffmpeg::av_frame_make_writable(frame_ptr).into_ffmpeg_result()?;

            (*frame_ptr).pts = self.pts as i64;
            ((*frame_ptr).data, (*frame_ptr).linesize)
        };

        let data_ptr = data[0];
        let stride = linesize[0];
        for y in 0..video_height {
            unsafe {
                let base_ptr = if stride < 0 {
                    data_ptr.offset((y + 1) as isize * stride as isize)
                } else {
                    data_ptr.offset(y as isize * stride as isize)
                };
                base_ptr.write_bytes(0, video_width * channels);
            }
        }

        self.filters.update_text(c_description)?;
        self.push_frame()?;
        self.pts += 1;
        Ok(())
    }

    pub fn finalize(&mut self, skip_still: bool) -> Result<()> {
        if !skip_still {
            for _ in 0..60 {
                self.repeat_frame()?;
                self.pts += 1;
            }
        }

        unsafe {
            self.send_frame(std::ptr::null_mut())?;
            ffmpeg::av_write_trailer(self.muxer_ctx).into_ffmpeg_result()?;
        }
        Ok(())
    }
}

impl<W> Drop for VideoContext<W> {
    fn drop(&mut self) {
        unsafe {
            ffmpeg::avcodec_free_context(&mut self.video_ctx);

            if !self.muxer_ctx.is_null() {
                ffmpeg::avformat_free_context(self.muxer_ctx);
                self.muxer_ctx = std::ptr::null_mut();
            }

            if !self.avio_ctx.is_null() {
                let buffer = (*self.avio_ctx).buffer;
                ffmpeg::av_free(buffer as *mut _);
                ffmpeg::avio_context_free(&mut self.avio_ctx);
            }

            if !self.writer_ptr.is_null() {
                let writer = Box::from_raw(self.writer_ptr);
                self.writer_ptr = std::ptr::null_mut();
                drop(writer);
            }

            ffmpeg::av_frame_free(&mut self.source_frame_ptr);
            ffmpeg::av_frame_free(&mut self.video_frame_ptr);
            ffmpeg::av_packet_free(&mut self.packet_ptr);
        }
    }
}
