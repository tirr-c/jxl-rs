// Copyright (c) the JPEG XL Project Authors. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use crate::error::Error;

pub trait FloorLog2 {
    fn floor_log2(&self) -> Self;
}

pub trait CeilLog2 {
    fn ceil_log2(&self) -> Self;
}

impl FloorLog2 for u32 {
    fn floor_log2(&self) -> Self {
        debug_assert_ne!(*self, 0);
        0u32.leading_zeros() - self.leading_zeros() - 1
    }
}

impl FloorLog2 for usize {
    fn floor_log2(&self) -> Self {
        debug_assert_ne!(*self, 0);
        (0usize.leading_zeros() - self.leading_zeros() - 1) as usize
    }
}

impl<T> CeilLog2 for T
where
    T: FloorLog2,
    T: std::ops::Add<Output = Self>,
    T: std::ops::Sub<Output = Self>,
    T: std::ops::BitAnd<Output = Self>,
    T: std::cmp::PartialEq,
    T: From<u8>,
    T: Copy,
{
    fn ceil_log2(&self) -> Self {
        if (*self & (*self - 1.into())) != 0.into() {
            self.floor_log2() + 1.into()
        } else {
            self.floor_log2()
        }
    }
}

pub struct ConcatSlice<'first, 'second> {
    slices: (&'first [u8], &'second [u8]),
    ptr: usize,
}

impl<'first, 'second> ConcatSlice<'first, 'second> {
    pub fn new(slice0: &'first [u8], slice1: &'second [u8]) -> Self {
        Self {
            slices: (slice0, slice1),
            ptr: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.slices.0.len() + self.slices.1.len()
    }

    pub fn remaining_slices(&self) -> (&'first [u8], &'second [u8]) {
        let (slice0, slice1) = self.slices;
        let total_len = self.len();
        let ptr = self.ptr;
        if ptr >= total_len {
            (&[], &[])
        } else if let Some(second_slice_ptr) = ptr.checked_sub(slice0.len()) {
            (&[], &slice1[second_slice_ptr..])
        } else {
            (&slice0[ptr..], slice1)
        }
    }

    pub fn advance(&mut self, bytes: usize) {
        self.ptr += bytes;
    }

    pub fn peek<'out>(&self, out_buf: &'out mut [u8]) -> &'out mut [u8] {
        let (slice0, slice1) = self.remaining_slices();
        let total_len = slice0.len() + slice1.len();

        let out_bytes = out_buf.len().min(total_len);
        let out_buf = &mut out_buf[..out_bytes];

        if out_bytes <= slice0.len() {
            out_buf.copy_from_slice(&slice0[..out_bytes]);
        } else {
            let (out_first, out_second) = out_buf.split_at_mut(slice0.len());
            out_first.copy_from_slice(slice0);
            out_second.copy_from_slice(&slice1[..out_second.len()]);
        }

        out_buf
    }

    pub fn fill_vec(&mut self, max_bytes: Option<usize>, v: &mut Vec<u8>) -> Result<usize, Error> {
        let (slice0, slice1) = self.remaining_slices();
        let total_len = slice0.len() + slice1.len();

        let out_bytes = max_bytes.unwrap_or(usize::MAX).min(total_len);
        v.try_reserve(out_bytes)?;

        if out_bytes <= slice0.len() {
            v.extend_from_slice(&slice0[..out_bytes]);
        } else {
            let second_slice_len = out_bytes - slice0.len();
            v.extend_from_slice(slice0);
            v.extend_from_slice(&slice1[..second_slice_len]);
        }

        self.advance(out_bytes);
        Ok(out_bytes)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn test_floor() {
        assert_eq!(0, 1u32.floor_log2());
        assert_eq!(1, 2u32.floor_log2());
        assert_eq!(1, 3u32.floor_log2());
        assert_eq!(2, 4u32.floor_log2());
    }
    #[test]
    fn test_ceil() {
        assert_eq!(0, 1u32.ceil_log2());
        assert_eq!(1, 2u32.ceil_log2());
        assert_eq!(2, 3u32.ceil_log2());
        assert_eq!(2, 4u32.ceil_log2());
    }
    #[test]
    fn test_floor_us() {
        assert_eq!(0, 1usize.floor_log2());
        assert_eq!(1, 2usize.floor_log2());
        assert_eq!(1, 3usize.floor_log2());
        assert_eq!(2, 4usize.floor_log2());
    }
    #[test]
    fn test_ceil_us() {
        assert_eq!(0, 1usize.ceil_log2());
        assert_eq!(1, 2usize.ceil_log2());
        assert_eq!(2, 3usize.ceil_log2());
        assert_eq!(2, 4usize.ceil_log2());
    }

    #[test]
    fn test_concat_slice_peek_advance() {
        let mut reader = ConcatSlice::new(&[0, 1, 2, 3], &[4, 5, 6, 7]);
        let mut buf = [0u8; 8];

        let actual = reader.peek(&mut buf[..1]);
        assert_eq!(actual, &[0]);
        reader.advance(actual.len());

        let actual = reader.peek(&mut buf[..2]);
        assert_eq!(actual, &[1, 2]);
        reader.advance(actual.len());

        let actual = reader.peek(&mut buf[..3]);
        assert_eq!(actual, &[3, 4, 5]);
        reader.advance(actual.len());

        let actual = reader.peek(&mut buf);
        assert_eq!(actual, &[6, 7]);
        reader.advance(actual.len());

        let actual = reader.peek(&mut buf);
        assert!(actual.is_empty());
    }

    #[test]
    fn test_concat_slice_fill_vec() {
        let mut reader = ConcatSlice::new(&[0, 1, 2, 3], &[4, 5, 6, 7]);
        let mut v = Vec::new();

        let count = reader.fill_vec(Some(3), &mut v).unwrap();
        assert_eq!(count, 3);
        assert_eq!(&v, &[0, 1, 2]);

        let count = reader.fill_vec(None, &mut v).unwrap();
        assert_eq!(count, 5);
        assert_eq!(&v, &[0, 1, 2, 3, 4, 5, 6, 7]);
    }
}
