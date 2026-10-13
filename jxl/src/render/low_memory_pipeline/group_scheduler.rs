// Copyright (c) the JPEG XL Project Authors. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::ops::Range;

use crate::error::Result;
use crate::image::Rect;
use crate::render::LowMemoryRenderPipeline;
use crate::render::buffer_splitter::BufferSplitter;
use crate::render::internal::{ChannelInfo, Stage};
use crate::util::tracing_wrappers::*;

// Finds a small set of rectangles that cover all the "true" values in `ready_mask`,
// and calls `f` on each such rectangle.
fn foreach_ready_rect(
    ready_mask: [bool; 9],
    mut f: impl FnMut(Range<u8>, Range<u8>) -> Result<()>,
) -> Result<()> {
    // x range in middle row
    let xrange = (1 - ready_mask[3] as u8)..(2 + ready_mask[5] as u8);
    let can_extend_top = xrange.clone().all(|x| ready_mask[x as usize]);
    let can_extend_bottom = xrange.clone().all(|x| ready_mask[6 + x as usize]);
    let yrange = (1 - can_extend_top as u8)..(2 + can_extend_bottom as u8);
    f(xrange.clone(), yrange)?;

    if !can_extend_top {
        if ready_mask[1] {
            let xrange = (1 - ready_mask[0] as u8)..(2 + ready_mask[2] as u8);
            f(xrange, 0..1)?;
        } else {
            if ready_mask[0] {
                f(0..1, 0..1)?;
            }
            if ready_mask[2] {
                f(2..3, 0..1)?;
            }
        }
    } else {
        if ready_mask[0] && !xrange.contains(&0) {
            f(0..1, 0..1)?;
        }
        if ready_mask[2] && !xrange.contains(&2) {
            f(2..3, 0..1)?;
        }
    }

    if !can_extend_bottom {
        if ready_mask[7] {
            let xrange = (1 - ready_mask[6] as u8)..(2 + ready_mask[8] as u8);
            f(xrange, 2..3)?;
        } else {
            if ready_mask[6] {
                f(0..1, 2..3)?;
            }
            if ready_mask[8] {
                f(2..3, 2..3)?;
            }
        }
    } else {
        if ready_mask[6] && !xrange.contains(&0) {
            f(0..1, 2..3)?;
        }
        if ready_mask[8] && !xrange.contains(&2) {
            f(2..3, 2..3)?;
        }
    }

    Ok(())
}

pub(super) fn align_down(p: usize, scale: usize, phase: usize) -> usize {
    let p = p as isize;
    let phase = (phase % scale) as isize;
    (phase + (p - phase).div_euclid(scale as isize) * scale as isize).max(0) as usize
}

pub(super) fn align_up(p: usize, scale: usize, phase: usize) -> usize {
    let phase = phase % scale;
    if p <= phase {
        phase
    } else {
        phase + (p - phase).div_ceil(scale) * scale
    }
}

#[allow(clippy::too_many_arguments)]
fn boundary_points(
    origin: usize,
    end: usize,
    is_first: bool,
    is_last: bool,
    border: usize,
    input_size: usize,
    scale: usize,
    crop_origin: usize,
) -> [usize; 4] {
    let p0 = if is_first {
        origin
    } else {
        align_down(origin.saturating_sub(border), scale, crop_origin)
    };
    let p1 = align_up(origin + border, scale, crop_origin).min(end);
    let p2 = align_down(end.saturating_sub(border), scale, crop_origin).max(p1);
    let p3 = if is_last {
        end
    } else {
        align_up(end + border, scale, crop_origin).min(input_size)
    };
    [p0, p1, p2, p3]
}

#[allow(clippy::too_many_arguments)]
fn ready_image_area(
    group_rect: Rect,
    group_position: (usize, usize),
    group_count: (usize, usize),
    input_size: (usize, usize),
    border_size: (usize, usize),
    scale: (usize, usize),
    crop_origin: (usize, usize),
    xrange: Range<u8>,
    yrange: Range<u8>,
) -> Option<Rect> {
    let (gx, gy) = group_position;
    let xs = boundary_points(
        group_rect.origin.0,
        group_rect.end().0,
        gx == 0,
        gx + 1 == group_count.0,
        border_size.0,
        input_size.0,
        scale.0,
        crop_origin.0,
    );
    let ys = boundary_points(
        group_rect.origin.1,
        group_rect.end().1,
        gy == 0,
        gy + 1 == group_count.1,
        border_size.1,
        input_size.1,
        scale.1,
        crop_origin.1,
    );

    let x0 = xs[xrange.start as usize];
    let x1 = xs[xrange.end as usize];
    let y0 = ys[yrange.start as usize];
    let y1 = ys[yrange.end as usize];

    (x1 > x0 && y1 > y0).then(|| Rect {
        origin: (x0, y0),
        size: (x1 - x0, y1 - y0),
    })
}

impl LowMemoryRenderPipeline {
    fn aligned_render_area(&self, area: Rect) -> Rect {
        let origin = (
            area.origin.0 / self.render_align.0 * self.render_align.0,
            area.origin.1 / self.render_align.1 * self.render_align.1,
        );
        let end = (
            area.end()
                .0
                .next_multiple_of(self.render_align.0)
                .min(self.shared.input_size.0),
            area.end()
                .1
                .next_multiple_of(self.render_align.1)
                .min(self.shared.input_size.1),
        );
        Rect {
            origin,
            size: (end.0 - origin.0, end.1 - origin.1),
        }
    }

    /// Output ownership is in canvas coordinates. Edge jobs also own the background portion of
    /// blocks intersecting the current frame; pure-background jobs stop at these block boundaries.
    pub(super) fn extended_output_area(&self, area: Rect) -> Rect {
        let Stage::Extend(e) = &self.shared.stages[self.shared.extend_stage_index.unwrap()] else {
            unreachable!()
        };
        let mut origin = (
            (area.origin.0 as isize + e.frame_origin.0).clamp(0, e.image_size.0 as isize) as usize,
            (area.origin.1 as isize + e.frame_origin.1).clamp(0, e.image_size.1 as isize) as usize,
        );
        let mut end = (
            (area.end().0 as isize + e.frame_origin.0).clamp(0, e.image_size.0 as isize) as usize,
            (area.end().1 as isize + e.frame_origin.1).clamp(0, e.image_size.1 as isize) as usize,
        );
        if origin.0 == end.0 || origin.1 == end.1 {
            return Rect {
                origin,
                size: (0, 0),
            };
        }
        let scale = 1usize << self.save_downsample;
        if area.origin.0 == 0 {
            origin.0 = align_down(origin.0, scale, self.save_crop_origin.0);
        }
        if area.origin.1 == 0 {
            origin.1 = align_down(origin.1, scale, self.save_crop_origin.1);
        }
        if area.end().0 == self.shared.input_size.0 {
            end.0 = align_up(end.0, scale, self.save_crop_origin.0).min(e.image_size.0);
        }
        if area.end().1 == self.shared.input_size.1 {
            end.1 = align_up(end.1, scale, self.save_crop_origin.1).min(e.image_size.1);
        }
        Rect {
            origin,
            size: (end.0 - origin.0, end.1 - origin.1),
        }
    }

    pub(super) fn render_with_new_group(
        &self,
        g: usize,
        buffer_splitter: &BufferSplitter,
    ) -> Result<()> {
        let buf = self.input_buffers.get(g);
        let (gx, gy) = self.shared.group_position(g);
        debug!("new data ready for group {gx},{gy}");

        // Prepare output buffers for the group.
        let (origin, size) = if let Some(e) = self.shared.extend_stage_index {
            let Stage::Extend(e) = &self.shared.stages[e] else {
                unreachable!("extend stage is not an extend stage");
            };
            (e.frame_origin, e.image_size)
        } else {
            ((0, 0), self.shared.input_size)
        };
        let gsz = 1 << self.shared.log_group_size;
        let group_rect = Rect {
            size: (gsz, gsz),
            origin: (gsz * gx, gsz * gy),
        }
        .clip(self.shared.input_size);

        if self.border_size != (0, 0) {
            for c in 0..self.shared.num_channels() {
                if !self.shared.channel_is_used[c] {
                    continue;
                }
                let (bx, by) = self.border_size;
                let ChannelInfo {
                    ty,
                    downsample: (dx, dy),
                } = self.shared.channel_info[0][c];
                let bx = bx >> dx;
                let by = by >> dy;
                if bx == 0 && by == 0 {
                    continue;
                }
                let data = buf.data[c].try_read().unwrap();
                let input = data.as_ref().unwrap();
                let (sx, sy) = input.byte_size();
                if by != 0 {
                    let mut topbottom =
                        if let Some(b) = buf.topbottom[c].try_write().unwrap().take() {
                            b
                        } else {
                            let height = 4 * by;
                            let width = (1 << self.shared.log_group_size) * ty.size();
                            self.shared
                                .buffer_recycler
                                .get_raw_buffer((width, height), false)?
                        };
                    for y in 0..(2 * by).min(sy) {
                        topbottom.row_mut(y)[..sx].copy_from_slice(input.row(y));
                        topbottom.row_mut(4 * by - 1 - y)[..sx]
                            .copy_from_slice(input.row(sy - y - 1));
                    }
                    *buf.topbottom[c].try_write().unwrap() = Some(topbottom);
                }
                if bx != 0 {
                    let mut leftright =
                        if let Some(b) = buf.leftright[c].try_write().unwrap().take() {
                            b
                        } else {
                            let height = 1 << self.shared.log_group_size;
                            let width = 4 * bx * ty.size();
                            self.shared
                                .buffer_recycler
                                .get_raw_buffer((width, height), false)?
                        };
                    let cs = (bx * 2 * ty.size()).min(sx);
                    for y in 0..sy {
                        let row_out = leftright.row_mut(y);
                        let row_in = input.row(y);
                        row_out[..cs].copy_from_slice(&row_in[..cs]);
                        row_out[4 * bx * ty.size() - cs..].copy_from_slice(&row_in[sx - cs..]);
                    }
                    *buf.leftright[c].try_write().unwrap() = Some(leftright);
                }
            }
        }

        let ready_mask = self.input_buffers.mark_ready(g);

        let mut data = self.per_thread_data.get();
        data.ensure_populated(self)?;

        foreach_ready_rect(ready_mask, |xrange, yrange| {
            let Some(image_area) = ready_image_area(
                group_rect,
                (gx, gy),
                self.shared.group_count,
                self.shared.input_size,
                self.base_border_size,
                self.save_align,
                (
                    (self.save_crop_origin.0 as isize - origin.0)
                        .rem_euclid(self.save_align.0 as isize) as usize,
                    (self.save_crop_origin.1 as isize - origin.1)
                        .rem_euclid(self.save_align.1 as isize) as usize,
                ),
                xrange,
                yrange,
            ) else {
                return Ok(());
            };

            let extended_area =
                if self.save_downsample > 0 && self.shared.extend_stage_index.is_some() {
                    Some(self.extended_output_area(image_area))
                } else {
                    None
                };
            let infos: Vec<_> = self
                .save_buffer_info
                .iter()
                .map(|info| info.filter(|i| extended_area.is_none() || i.save_downsample == 0))
                .collect();
            let mut local_buffers = buffer_splitter.get_local_buffers(
                &infos,
                image_area,
                false,
                self.shared.input_size,
                size,
                origin,
            );
            if let Some(area) = extended_area {
                let infos: Vec<_> = self
                    .save_buffer_info
                    .iter()
                    .map(|info| info.filter(|i| i.save_downsample > 0))
                    .collect();
                let mut outputs =
                    buffer_splitter.get_local_buffers(&infos, area, true, size, size, (0, 0));
                for (dst, src) in local_buffers.iter_mut().zip(outputs.iter_mut()) {
                    if src.is_some() {
                        *dst = src.take();
                    }
                }
            }
            let render_area = self.aligned_render_area(image_area);
            self.render_group(
                &mut data,
                (gx, gy),
                render_area,
                image_area,
                extended_area,
                &mut local_buffers,
            )?;
            Ok(())
        })?;

        self.input_buffers.mark_done(g, &self.shared, |image| {
            self.shared.buffer_recycler.recycle_raw_buffer(image);
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_foreach_ready_rect() {
        for i in 0..512 {
            let mut ready_mask = [false; 9];
            for j in 0..9 {
                if (i >> j) & 1 == 1 {
                    ready_mask[j] = true;
                }
            }
            if !ready_mask[4] {
                continue;
            }

            let mut covered = [false; 9];
            foreach_ready_rect(ready_mask, |xr, yr| {
                for y in yr {
                    for x in xr.clone() {
                        let idx = (y as usize) * 3 + (x as usize);
                        assert!(
                            ready_mask[idx],
                            "Covered not ready index {} in mask {:?} (x={}, y={})",
                            idx, ready_mask, x, y
                        );
                        assert!(
                            !covered[idx],
                            "Double coverage of index {} in mask {:?}",
                            idx, ready_mask
                        );
                        covered[idx] = true;
                    }
                }
                Ok(())
            })
            .unwrap();

            for j in 0..9 {
                if ready_mask[j] {
                    assert!(
                        covered[j],
                        "Failed to cover index {} in mask {:?}",
                        j, ready_mask
                    );
                }
            }
        }
    }

    #[test]
    fn test_ready_image_area_clips_tiny_edge_group() {
        let area = ready_image_area(
            Rect {
                origin: (512, 512),
                size: (8, 8),
            },
            (1, 1),
            (2, 2),
            (520, 520),
            (18, 18),
            (1, 1),
            (0, 0),
            0..1,
            0..1,
        );
        assert_eq!(
            area,
            Some(Rect {
                origin: (494, 494),
                size: (26, 26),
            })
        );
    }

    // The last group of a 513-pixel-wide image is 1 pixel wide, narrower than
    // the border, so its interior band starts past the right edge of the image.
    #[test]
    fn test_ready_image_area_interior_band_past_image_edge() {
        let area = ready_image_area(
            Rect {
                origin: (512, 0),
                size: (1, 256),
            },
            (2, 0),
            (3, 2),
            (513, 321),
            (3, 3),
            (1, 1),
            (0, 0),
            1..3,
            0..2,
        );
        assert_eq!(area, None);
    }
}
