// Copyright (c) the JPEG XL Project Authors. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::sync::{Mutex, MutexGuard};

use crate::{
    error::Result,
    image::{Image, ImageDataType},
    render::{RenderPipelineInputStage, RenderPipelineStage},
};

pub struct SaveStage<T: ImageDataType> {
    buf: Mutex<Image<T>>,
    channel: usize,
}

#[allow(unused)]
impl<T: ImageDataType> SaveStage<T> {
    pub(crate) fn new(channel: usize, size: (usize, usize)) -> Result<SaveStage<T>> {
        Ok(SaveStage {
            channel,
            buf: Mutex::new(Image::new(size)?),
        })
    }

    pub(crate) fn new_with_buffer(channel: usize, img: Image<T>) -> SaveStage<T> {
        SaveStage {
            channel,
            buf: Mutex::new(img),
        }
    }

    pub(crate) fn buffer(&self) -> MutexGuard<'_, Image<T>> {
        self.buf.lock().unwrap()
    }

    pub(crate) fn into_buffer(self) -> Image<T> {
        self.buf.into_inner().unwrap()
    }
}

impl<T: ImageDataType> std::fmt::Display for SaveStage<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "save channel {} (type {:?})",
            self.channel,
            T::DATA_TYPE_ID
        )
    }
}

impl<T: ImageDataType> RenderPipelineStage for SaveStage<T> {
    type Type = RenderPipelineInputStage<T>;

    fn uses_channel(&self, c: usize) -> bool {
        c == self.channel
    }

    fn process_row_chunk(&self, position: (usize, usize), xsize: usize, row: &mut [&[T]]) {
        let input = &mut row[0];
        // TODO(veluca): consider making `process_row_chunk` return a Result.
        let mut outbuf = self.buf.lock().unwrap();
        let mut outbuf = outbuf.as_rect_mut();
        let mut outbuf = outbuf
            .rect(position, (xsize, 1))
            .expect("mismatch in image size");
        outbuf.row(0).copy_from_slice(&input[..xsize]);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use rand::SeedableRng;
    use rand_xorshift::XorShiftRng;
    use test_log::test;

    #[test]
    fn save_stage() -> Result<()> {
        let save_stage = SaveStage::<u8>::new(0, (128, 128))?;
        let mut rng = XorShiftRng::seed_from_u64(0);
        let src = Image::<u8>::new_random((128, 128), &mut rng)?;

        for i in 0..128 {
            save_stage.process_row_chunk((0, i), 128, &mut [src.as_rect().row(i)]);
        }

        src.as_rect().check_equal(save_stage.buffer().as_rect());

        Ok(())
    }
}
