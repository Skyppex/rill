//! Conversion from the engine's internal `f32` to host sample formats.
//!
//! Internally a `sample` is nominally in [-1.0, 1.0] and may go past that
//! mid-graph. Conversion happens only at the I/O boundary, where integer
//! formats clip.

/// A sample format a host buffer can hold.
pub trait OutSample: Copy {
    fn from_f32(x: f32) -> Self;
}

impl OutSample for f32 {
    #[inline(always)]
    fn from_f32(x: f32) -> Self {
        x
    }
}

impl OutSample for f64 {
    #[inline(always)]
    fn from_f32(x: f32) -> Self {
        f64::from(x)
    }
}

impl OutSample for i16 {
    #[inline(always)]
    fn from_f32(x: f32) -> Self {
        (x.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
    }
}

impl OutSample for i32 {
    #[inline(always)]
    fn from_f32(x: f32) -> Self {
        (f64::from(x).clamp(-1.0, 1.0) * f64::from(i32::MAX)).round() as i32
    }
}

impl OutSample for u8 {
    #[inline(always)]
    fn from_f32(x: f32) -> Self {
        ((x.clamp(-1.0, 1.0) * 127.0).round() + 128.0) as u8
    }
}

impl OutSample for u16 {
    #[inline(always)]
    fn from_f32(x: f32) -> Self {
        ((x.clamp(-1.0, 1.0) * i16::MAX as f32).round() + 32768.0) as u16
    }
}

/// 24-bit value in the low bits of an `i32`, the way most APIs pass it.
pub fn to_i24(x: f32) -> i32 {
    const MAX: f32 = ((1 << 23) - 1) as f32;
    (x.clamp(-1.0, 1.0) * MAX).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_formats_clip_and_center() {
        assert_eq!(i16::from_f32(0.0), 0);
        assert_eq!(i16::from_f32(1.0), i16::MAX);
        assert_eq!(i16::from_f32(-1.0), -i16::MAX);
        assert_eq!(i16::from_f32(3.0), i16::MAX);
        assert_eq!(i16::from_f32(f32::NEG_INFINITY), -i16::MAX);

        assert_eq!(i32::from_f32(1.0), i32::MAX);
        assert_eq!(i32::from_f32(-2.0), -i32::MAX);

        assert_eq!(u8::from_f32(0.0), 128);
        assert_eq!(u8::from_f32(1.0), 255);
        assert_eq!(u8::from_f32(-1.0), 1);

        assert_eq!(u16::from_f32(0.0), 32768);
        assert_eq!(u16::from_f32(1.0), u16::MAX);

        assert_eq!(to_i24(1.0), (1 << 23) - 1);
        assert_eq!(to_i24(-5.0), -((1 << 23) - 1));
    }

    #[test]
    fn nan_becomes_silence_for_integers() {
        // `as` saturates NaN to 0; for unsigned formats that is the offset.
        assert_eq!(i16::from_f32(f32::NAN), 0);
        assert_eq!(i32::from_f32(f32::NAN), 0);
    }
}
