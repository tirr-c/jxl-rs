use std::path::PathBuf;

use clap::Parser;
use color_eyre::eyre::{Result, WrapErr};
use jxl::{
    api::{
        JxlColorEncoding, JxlColorProfile, JxlColorType, JxlDecoder, JxlDecoderOptions,
        JxlOutputBuffer, ProcessingResult,
    },
    image::{OwnedRawImage, Rect},
};

struct LoadProgress {
    iter: usize,
    unit_step: usize,
    current_step: usize,
    bytes_read: usize,
    total_bytes: usize,
}

impl LoadProgress {
    fn new(mut unit_step: usize, total_bytes: usize) -> Self {
        if unit_step == 0 {
            // divide by 1500 (10% divided by 30 * 5), round up to unit of 100 bytes
            unit_step = total_bytes.div_ceil(150000) * 100;
        }

        Self {
            iter: 0,
            unit_step,
            current_step: unit_step,
            bytes_read: 0,
            total_bytes,
        }
    }

    fn slice_buf<'buf>(&self, buf: &'buf [u8]) -> &'buf [u8] {
        let end = self.total_bytes.min(self.bytes_read + self.current_step);
        &buf[self.bytes_read..end]
    }

    #[inline]
    fn add_iter(&mut self, bytes_read: usize) {
        self.bytes_read += bytes_read;
        self.iter += 1;
    }

    #[inline]
    fn try_increase_step(&mut self) -> Option<usize> {
        let progress_int = self.bytes_read * 100 / self.total_bytes;
        let multiplier = match progress_int {
            ..=9 => 1,
            10..=24 => 2,
            25..=49 => 4,
            _ => 8,
        };
        let new_step = (self.unit_step * multiplier).min(self.step_cap());
        if self.current_step != new_step {
            self.current_step = new_step;
            Some(new_step)
        } else {
            None
        }
    }

    #[inline]
    fn step_cap(&self) -> usize {
        self.total_bytes.div_ceil(self.unit_step * 100) * self.unit_step
    }

    #[inline]
    fn progress(&self) -> f64 {
        self.bytes_read as f64 / self.total_bytes as f64
    }
}

impl std::fmt::Display for LoadProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let step = self.current_step;
        let total_bytes = self.total_bytes;
        if total_bytes > 0 {
            let percentage = self.progress() * 100.0;
            if self.bytes_read >= total_bytes {
                write!(f, "{percentage:.2}\\% loaded\ntotal {total_bytes} bytes")
            } else {
                write!(
                    f,
                    "{percentage:.2}\\% loaded\n{step} bytes/frame, total {total_bytes} bytes"
                )
            }
        } else {
            write!(f, "{step} bytes/frame")
        }
    }
}

#[derive(Parser)]
struct Opt {
    /// Input JXL file
    input: PathBuf,
    /// Output video file
    output: PathBuf,
}

fn main() -> Result<()> {
    let opt = Opt::parse();
    let input = std::fs::read(&opt.input)
        .wrap_err_with(|| format!("Failed to read source image from {:?}", opt.input))?;

    let total_bytes = input.len();
    let mut progress = LoadProgress::new(0, total_bytes);

    let mut decoder_options = JxlDecoderOptions::default();
    decoder_options.progressive_mode = jxl::api::JxlProgressiveMode::Eager;

    let mut output = std::fs::File::create(&opt.output)
        .wrap_err_with(|| format!("Failed to output file at {:?}", opt.output))?;
    let mut encoder = jxl_progressive::video::Mp4FileEncoder::new(&mut output, "monospace")?;

    let mut initialized_decoder = JxlDecoder::<jxl::api::states::Initialized>::new(decoder_options);

    let mut decoder = loop {
        let mut buf = progress.slice_buf(&input);
        let input_size = buf.len();
        let result = initialized_decoder.process(&mut buf)?;
        progress.add_iter(input_size - buf.len());
        initialized_decoder = match result {
            ProcessingResult::Complete { result } => break result,
            ProcessingResult::NeedsMoreInput { fallback, .. } => fallback,
        };
        encoder.add_empty_frame(&progress)?;
        progress.try_increase_step();
    };

    let current_format = decoder.current_pixel_format().clone();
    let color_type = match current_format.color_type {
        JxlColorType::Grayscale | JxlColorType::GrayscaleAlpha => JxlColorType::Grayscale,
        _ => JxlColorType::Rgb,
    };
    let new_format = jxl::api::JxlPixelFormat {
        color_type,
        color_data_format: Some(jxl::api::JxlDataFormat::U8 { bit_depth: 8 }),
        extra_channel_format: vec![None; current_format.extra_channel_format.len()],
    };
    decoder.set_pixel_format(new_format);
    let samples_per_pixel = color_type.samples_per_pixel();

    let src_profile = decoder.output_color_profile();
    let dst_profile = JxlColorProfile::Simple(JxlColorEncoding::srgb(color_type.is_grayscale()));

    let src_profile = moxcms::ColorProfile::new_from_slice(&src_profile.as_icc())?;
    let dst_profile = moxcms::ColorProfile::new_from_slice(&dst_profile.as_icc())?;
    let layout = if color_type.is_grayscale() {
        moxcms::Layout::Gray
    } else {
        moxcms::Layout::Rgb
    };
    let transformer =
        src_profile.create_transform_8bit(layout, &dst_profile, layout, Default::default())?;

    let image_size = decoder.basic_info().size;
    encoder.init(image_size, color_type)?;

    let mut output = OwnedRawImage::new((image_size.0 * samples_per_pixel, image_size.1))?;

    let mut first = true;
    let mut decoder_frame = loop {
        let mut buf = if first {
            [].as_slice()
        } else {
            progress.slice_buf(&input)
        };
        let mut output_buf: [JxlOutputBuffer<'_>; 1] = {
            let rect = Rect {
                size: output.byte_size(),
                origin: (0, 0),
            };
            [JxlOutputBuffer::from_image_rect_mut(
                output.get_rect_mut(rect),
            )]
        };

        let input_size = buf.len();
        let result = decoder.process(&mut buf)?;
        if !first {
            progress.add_iter(input_size - buf.len());
        }
        decoder = match result {
            ProcessingResult::Complete { result } => break result,
            ProcessingResult::NeedsMoreInput { fallback, .. } => fallback,
        };
        decoder.flush_pixels(&mut output_buf)?;
        encoder.add_frame(&mut output, &progress, &*transformer)?;
        progress.try_increase_step();

        first = false;
    };

    let mut first = true;
    loop {
        let mut buf = if first {
            [].as_slice()
        } else {
            progress.slice_buf(&input)
        };
        let mut output_buf: [JxlOutputBuffer<'_>; 1] = {
            let rect = Rect {
                size: output.byte_size(),
                origin: (0, 0),
            };
            [JxlOutputBuffer::from_image_rect_mut(
                output.get_rect_mut(rect),
            )]
        };

        let input_size = buf.len();
        let result = decoder_frame.process(&mut buf, &mut output_buf)?;
        if !first {
            progress.add_iter(input_size - buf.len());
        }
        decoder_frame = match result {
            ProcessingResult::Complete { .. } => break,
            ProcessingResult::NeedsMoreInput { fallback, .. } => fallback,
        };
        decoder_frame.flush_pixels(&mut output_buf)?;
        encoder.add_frame(&mut output, &progress, &*transformer)?;
        progress.try_increase_step();

        first = false;
    }

    encoder.add_frame(&mut output, &progress, &*transformer)?;
    encoder.finalize()?;
    Ok(())
}
