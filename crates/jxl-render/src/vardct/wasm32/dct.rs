use jxl_grid::{MutableSubgrid, SimdVector};

use super::super::dct_common::{self, DctDirection};
use std::arch::wasm32::*;

const LANE_SIZE: usize = 4;
type Lane = v128;

#[inline(always)]
pub(crate) fn transpose_lane(lanes: &[Lane]) -> [Lane; 4] {
    assert_eq!(lanes.len(), 4);
    let mut out = [
        i32x4_shuffle::<0, 1, 4, 5>(lanes[0], lanes[1]),
        i32x4_shuffle::<0, 1, 4, 5>(lanes[2], lanes[3]),
        i32x4_shuffle::<2, 3, 6, 7>(lanes[0], lanes[1]),
        i32x4_shuffle::<2, 3, 6, 7>(lanes[2], lanes[3]),
    ];
    let a = i32x4_shuffle::<0, 2, 4, 6>(out[0], out[1]);
    let b = i32x4_shuffle::<1, 3, 5, 7>(out[0], out[1]);
    out[0] = a;
    out[1] = b;
    let a = i32x4_shuffle::<0, 2, 4, 6>(out[2], out[3]);
    let b = i32x4_shuffle::<1, 3, 5, 7>(out[2], out[3]);
    out[2] = a;
    out[3] = b;
    out
}

#[inline(always)]
pub(crate) unsafe fn dct_2d_wasm32_simd128(io: &mut MutableSubgrid<'_>, direction: DctDirection) {
    if io.width() % LANE_SIZE != 0 || io.height() % LANE_SIZE != 0 {
        return super::generic::dct_2d(io, direction);
    }

    let Some(mut io) = io.as_vectored() else {
        tracing::trace!("Input buffer is not aligned");
        return super::generic::dct_2d(io, direction);
    };

    if io.width() == 2 && io.height() == 8 {
        unsafe {
            return dct8x8(&mut io, direction);
        }
    }

    dct_2d_lane(&mut io, direction);
}

fn dct_2d_lane(io: &mut MutableSubgrid<'_, Lane>, direction: DctDirection) {
    let scratch_size = io.height().max(io.width() * LANE_SIZE) * 2;
    unsafe {
        let mut scratch_lanes = vec![Lane::zero(); scratch_size];
        column_dct_lane(io, &mut scratch_lanes, direction);
        row_dct_lane(io, &mut scratch_lanes, direction);
    }
}

#[inline]
unsafe fn dct4_vec_forward(v: Lane) -> Lane {
    const SEC0: f32 = 0.5411961;
    const SEC1: f32 = 1.306563;

    let vrev = i32x4_shuffle::<3, 2, 1, 0>(v, v);
    let vneg = f32x4_neg(v);
    let vadd = i32x4_shuffle::<0, 1, 6, 7>(v, vneg);
    let addsub = vrev.add(vadd);

    let a = i32x4_shuffle::<0, 3, 1, 2>(addsub, addsub);
    let mul_a = Lane::set([
        0.25,
        (std::f32::consts::FRAC_1_SQRT_2 / 2.0 + 0.25) * SEC0,
        -0.25,
        -0.25 * SEC1,
    ]);
    let b = i32x4_shuffle::<1, 2, 0, 3>(addsub, addsub);
    let mul_b = Lane::set([
        0.25,
        (std::f32::consts::FRAC_1_SQRT_2 / 2.0 - 0.25) * SEC1,
        0.25,
        0.25 * SEC0,
    ]);
    a.muladd(mul_a, b.mul(mul_b))
}

#[inline]
pub(crate) unsafe fn dct4_vec_inverse(v: Lane) -> Lane {
    const SEC0: f32 = 0.5411961;
    const SEC1: f32 = 1.306563;

    let v_flip = i32x4_shuffle::<2, 3, 0, 1>(v, v);
    let mul_a = Lane::set([1.0, (std::f32::consts::SQRT_2 + 1.0) * SEC0, -1.0, -SEC1]);
    let mul_b = Lane::set([1.0, SEC0, 1.0, (std::f32::consts::SQRT_2 - 1.0) * SEC1]);
    let tmp = v.muladd(mul_a, v_flip.mul(mul_b));

    let tmp_neg = f32x4_neg(tmp);
    let tmp_a = i32x4_shuffle::<0, 2, 2, 0>(tmp, tmp);
    let tmp_b = i32x4_shuffle::<1, 3, 7, 5>(tmp, tmp_neg);
    tmp_a.add(tmp_b)
}

#[inline]
unsafe fn dct8_vec_forward(vl: Lane, vr: Lane) -> (Lane, Lane) {
    #[allow(clippy::excessive_precision)]
    let sec_vec = Lane::set([
        0.2548977895520796,
        0.30067244346752264,
        0.4499881115682078,
        1.2814577238707527,
    ]);
    let vr_rev = i32x4_shuffle::<3, 2, 1, 0>(vr, vr);
    let input0 = vl.add(vr_rev).mul(Lane::splat_f32(0.5));
    let input1 = vl.sub(vr_rev).mul(sec_vec);
    let output0 = dct4_vec_forward(input0);
    let output1 = dct4_vec_forward(input1);
    let output1_shifted = i32x4_shuffle::<1, 2, 3, 4>(output1, Lane::zero());
    let output1_mul = Lane::set([std::f32::consts::SQRT_2, 1.0, 1.0, 1.0]);
    let output1 = output1.muladd(output1_mul, output1_shifted);
    (
        i32x4_shuffle::<0, 4, 1, 5>(output0, output1),
        i32x4_shuffle::<2, 6, 3, 7>(output0, output1),
    )
}

#[inline]
pub(crate) unsafe fn dct8_vec_inverse(vl: Lane, vr: Lane) -> (Lane, Lane) {
    #[allow(clippy::excessive_precision)]
    let sec_vec = Lane::set([
        0.5097955791041592,
        0.6013448869350453,
        0.8999762231364156,
        2.5629154477415055,
    ]);
    let input0 = i32x4_shuffle::<0, 2, 4, 6>(vl, vr);
    let input1 = i32x4_shuffle::<1, 3, 5, 7>(vl, vr);
    let input1_shifted = i32x4_shuffle::<3, 4, 5, 6>(Lane::zero(), input1);
    let input1_mul = Lane::set([std::f32::consts::SQRT_2, 1.0, 1.0, 1.0]);
    let input1 = input1.muladd(input1_mul, input1_shifted);
    let output0 = dct4_vec_inverse(input0);
    let output1 = dct4_vec_inverse(input1);
    let output1 = output1.mul(sec_vec);
    let sub = output0.sub(output1);
    (output0.add(output1), i32x4_shuffle::<3, 2, 1, 0>(sub, sub))
}

unsafe fn dct8x8(io: &mut MutableSubgrid<'_, Lane>, direction: DctDirection) {
    let (mut col0, mut col1) = io.split_horizontal(1);

    if direction == DctDirection::Forward {
        dct8_forward(&mut col0);
        dct8_forward(&mut col1);
        for y in 0..8 {
            let row = io.get_row_mut(y);
            let (vl, vr) = dct8_vec_forward(row[0], row[1]);
            row[0] = vl;
            row[1] = vr;
        }
    } else {
        dct8_inverse(&mut col0);
        dct8_inverse(&mut col1);
        for y in 0..8 {
            let row = io.get_row_mut(y);
            let (vl, vr) = dct8_vec_inverse(row[0], row[1]);
            row[0] = vl;
            row[1] = vr;
        }
    }
}

unsafe fn column_dct_lane(
    io: &mut MutableSubgrid<'_, Lane>,
    scratch: &mut [Lane],
    direction: DctDirection,
) {
    let width = io.width();
    let height = io.height();
    let (io_lanes, scratch_lanes) = scratch[..height * 2].split_at_mut(height);
    for x in 0..width {
        for (y, input) in io_lanes.iter_mut().enumerate() {
            *input = io.get(x, y);
        }
        dct(io_lanes, scratch_lanes, direction);
        for (y, output) in io_lanes.chunks_exact(LANE_SIZE).enumerate() {
            let [o0, o1, o2, o3] = transpose_lane(output);
            *io.get_mut(x, y * LANE_SIZE) = o0;
            *io.get_mut(x, y * LANE_SIZE + 1) = o1;
            *io.get_mut(x, y * LANE_SIZE + 2) = o2;
            *io.get_mut(x, y * LANE_SIZE + 3) = o3;
        }
    }
}

unsafe fn row_dct_lane(
    io: &mut MutableSubgrid<'_, Lane>,
    scratch: &mut [Lane],
    direction: DctDirection,
) {
    let width = io.width() * LANE_SIZE;
    let height = io.height();
    let (io_lanes, scratch_lanes) = scratch[..width * 2].split_at_mut(width);
    for y in (0..height).step_by(LANE_SIZE) {
        for (x, input) in io_lanes.chunks_exact_mut(LANE_SIZE).enumerate() {
            for (dy, input) in input.iter_mut().enumerate() {
                *input = io.get(x, y + dy);
            }
        }
        dct(io_lanes, scratch_lanes, direction);
        for (x, output) in io_lanes.chunks_exact(LANE_SIZE).enumerate() {
            let [o0, o1, o2, o3] = transpose_lane(output);
            *io.get_mut(x, y) = o0;
            *io.get_mut(x, y + 1) = o1;
            *io.get_mut(x, y + 2) = o2;
            *io.get_mut(x, y + 3) = o3;
        }
    }
}

#[inline]
unsafe fn dct4_forward(input: [Lane; 4]) -> [Lane; 4] {
    let sec0 = Lane::splat_f32(0.5411961 / 4.0);
    let sec1 = Lane::splat_f32(1.306563 / 4.0);

    let sum03 = input[0].add(input[3]);
    let sum12 = input[1].add(input[2]);
    let tmp0 = input[0].sub(input[3]).mul(sec0);
    let tmp1 = input[1].sub(input[2]).mul(sec1);
    let out0 = tmp0.add(tmp1);
    let out1 = tmp0.sub(tmp1);

    [
        sum03.add(sum12).mul(Lane::splat_f32(0.25)),
        out0.mul(Lane::splat_f32(std::f32::consts::SQRT_2))
            .add(out1),
        sum03.sub(sum12).mul(Lane::splat_f32(0.25)),
        out1,
    ]
}

#[inline]
pub(crate) unsafe fn dct4_inverse(input: [Lane; 4]) -> [Lane; 4] {
    let sec0 = Lane::splat_f32(0.5411961);
    let sec1 = Lane::splat_f32(1.306563);

    let tmp0 = input[1].mul(Lane::splat_f32(std::f32::consts::SQRT_2));
    let tmp1 = input[1].add(input[3]);
    let out0 = tmp0.add(tmp1).mul(sec0);
    let out1 = tmp0.sub(tmp1).mul(sec1);
    let sum02 = input[0].add(input[2]);
    let sub02 = input[0].sub(input[2]);

    [
        sum02.add(out0),
        sub02.add(out1),
        sub02.sub(out1),
        sum02.sub(out0),
    ]
}

#[inline]
unsafe fn dct8_forward(io: &mut MutableSubgrid<'_, Lane>) {
    assert!(io.height() == 8);
    let sec = dct_common::sec_half_small(8);

    let half = Lane::splat_f32(0.5);
    let input0 = [
        io.get(0, 0).add(io.get(0, 7)).mul(half),
        io.get(0, 1).add(io.get(0, 6)).mul(half),
        io.get(0, 2).add(io.get(0, 5)).mul(half),
        io.get(0, 3).add(io.get(0, 4)).mul(half),
    ];
    let input1 = [
        io.get(0, 0)
            .sub(io.get(0, 7))
            .mul(Lane::splat_f32(sec[0] / 2.0)),
        io.get(0, 1)
            .sub(io.get(0, 6))
            .mul(Lane::splat_f32(sec[1] / 2.0)),
        io.get(0, 2)
            .sub(io.get(0, 5))
            .mul(Lane::splat_f32(sec[2] / 2.0)),
        io.get(0, 3)
            .sub(io.get(0, 4))
            .mul(Lane::splat_f32(sec[3] / 2.0)),
    ];
    let output0 = dct4_forward(input0);
    for (idx, v) in output0.into_iter().enumerate() {
        *io.get_mut(0, idx * 2) = v;
    }
    let mut output1 = dct4_forward(input1);
    output1[0] = output1[0].mul(Lane::splat_f32(std::f32::consts::SQRT_2));
    for idx in 0..3 {
        *io.get_mut(0, idx * 2 + 1) = output1[idx].add(output1[idx + 1]);
    }
    *io.get_mut(0, 7) = output1[3];
}

#[inline]
unsafe fn dct8_inverse(io: &mut MutableSubgrid<'_, Lane>) {
    assert!(io.height() == 8);
    let sec = dct_common::sec_half_small(8);

    let input0 = [io.get(0, 0), io.get(0, 2), io.get(0, 4), io.get(0, 6)];
    let input1 = [
        io.get(0, 1).mul(Lane::splat_f32(std::f32::consts::SQRT_2)),
        io.get(0, 3).add(io.get(0, 1)),
        io.get(0, 5).add(io.get(0, 3)),
        io.get(0, 7).add(io.get(0, 5)),
    ];
    let output0 = dct4_inverse(input0);
    let output1 = dct4_inverse(input1);
    for (idx, &sec) in sec.iter().enumerate() {
        let r = output1[idx].mul(Lane::splat_f32(sec));
        *io.get_mut(0, idx) = output0[idx].add(r);
        *io.get_mut(0, 7 - idx) = output0[idx].sub(r);
    }
}

unsafe fn dct(io: &mut [Lane], scratch: &mut [Lane], direction: DctDirection) {
    let n = io.len();
    assert!(scratch.len() == n);

    if n == 0 {
        return;
    }
    if n == 1 {
        return;
    }

    if n == 2 {
        let tmp0 = io[0].add(io[1]);
        let tmp1 = io[0].sub(io[1]);
        if direction == DctDirection::Forward {
            let half = Lane::splat_f32(0.5);
            io[0] = tmp0.mul(half);
            io[1] = tmp1.mul(half);
        } else {
            io[0] = tmp0;
            io[1] = tmp1;
        }
        return;
    }

    if n == 4 {
        if direction == DctDirection::Forward {
            io.copy_from_slice(&dct4_forward([io[0], io[1], io[2], io[3]]));
        } else {
            io.copy_from_slice(&dct4_inverse([io[0], io[1], io[2], io[3]]));
        }
        return;
    }

    if n == 8 {
        if direction == DctDirection::Forward {
            dct8_forward(&mut MutableSubgrid::from_buf(io, 1, 8, 1));
        } else {
            dct8_inverse(&mut MutableSubgrid::from_buf(io, 1, 8, 1));
        }
        return;
    }

    assert!(n.is_power_of_two());

    let sqrt2 = Lane::splat_f32(std::f32::consts::SQRT_2);
    if direction == DctDirection::Forward {
        let (input0, input1) = scratch.split_at_mut(n / 2);
        for (idx, &sec) in dct_common::sec_half(n).iter().enumerate() {
            input0[idx] = io[idx].add(io[n - idx - 1]).mul(Lane::splat_f32(0.5));
            input1[idx] = io[idx].sub(io[n - idx - 1]).mul(Lane::splat_f32(sec / 2.0));
        }
        let (output0, output1) = io.split_at_mut(n / 2);
        dct(input0, output0, DctDirection::Forward);
        dct(input1, output1, DctDirection::Forward);
        for (idx, v) in input0.iter().enumerate() {
            io[idx * 2] = *v;
        }
        input1[0] = input1[0].mul(sqrt2);
        for idx in 0..(n / 2 - 1) {
            io[idx * 2 + 1] = input1[idx].add(input1[idx + 1]);
        }
        io[n - 1] = input1[n / 2 - 1];
    } else {
        let (input0, input1) = scratch.split_at_mut(n / 2);
        for idx in 1..(n / 2) {
            let idx = n / 2 - idx;
            input0[idx] = io[idx * 2];
            input1[idx] = io[idx * 2 + 1].add(io[idx * 2 - 1]);
        }
        input0[0] = io[0];
        input1[0] = io[1].mul(sqrt2);
        let (output0, output1) = io.split_at_mut(n / 2);
        dct(input0, output0, DctDirection::Inverse);
        dct(input1, output1, DctDirection::Inverse);
        for (idx, &sec) in dct_common::sec_half(n).iter().enumerate() {
            let r = input1[idx].mul(Lane::splat_f32(sec));
            output0[idx] = input0[idx].add(r);
            output1[n / 2 - idx - 1] = input0[idx].sub(r);
        }
    }
}
