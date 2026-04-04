use miette::{SourceOffset, SourceSpan};
use std::cmp::max;
use std::fmt::Debug;
use std::mem;
use strsim::damerau_levenshtein;

pub fn json5_error_to_offset(
    error: &json5::Error,
    source_string: &str,
    base_offset: usize,
) -> usize {
    match error.position() {
        Some(pos) => {
            base_offset
                + SourceOffset::from_location(source_string, pos.line + 1, pos.column + 1).offset()
        }
        None => match error.code() {
            Some(json5::ErrorCode::EofParsingArray)
            | Some(json5::ErrorCode::EofParsingBool)
            | Some(json5::ErrorCode::EofParsingComment)
            | Some(json5::ErrorCode::EofParsingEscapeSequence)
            | Some(json5::ErrorCode::EofParsingIdentifier)
            | Some(json5::ErrorCode::EofParsingNull)
            | Some(json5::ErrorCode::EofParsingNumber)
            | Some(json5::ErrorCode::EofParsingObject)
            | Some(json5::ErrorCode::EofParsingString)
            | Some(json5::ErrorCode::EofParsingValue) => base_offset + source_string.len(),
            _ => base_offset,
        },
    }
}

pub fn all_but_last_assert<I>(mut iter: I, last_should_be: I::Item) -> I
where
    I: DoubleEndedIterator,
    I::Item: PartialEq + Debug,
{
    assert_eq!(iter.next_back(), Some(last_should_be));
    iter
}

#[inline]
pub fn ensure_ordered<T: Ord>(a: &mut T, b: &mut T) {
    if a > b {
        mem::swap(a, b)
    }
}

#[inline]
pub fn order_tuple<T: Ord>((mut a, mut b): (T, T)) -> (T, T) {
    ensure_ordered(&mut a, &mut b);
    (a, b)
}

pub trait Combine<Rhs = Self> {
    type Output;

    fn combine(self, rhs: Rhs) -> Self::Output;
}

impl<Lhs, Rhs> Combine<Rhs> for Lhs
where
    Lhs: Into<SourceSpan>,
    Rhs: Into<SourceSpan>,
{
    type Output = SourceSpan;

    fn combine(self, rhs: Rhs) -> Self::Output {
        let (a, b) = order_tuple((self.into(), rhs.into()));
        (a.offset(), max(b.offset() - a.offset() + b.len(), a.len())).into()
    }
}

pub trait AsNum<Converted> {
    #[allow(clippy::wrong_self_convention)]
    fn as_num(self) -> Converted;
}

macro_rules! impl_num_as_num {
    ($source:ty => $($num_type:ty)+) => {
        $(
            impl AsNum<$num_type> for $source {
                fn as_num(self) -> $num_type {
                    self as $num_type
                }
            }
        )+
    };
}

impl_num_as_num!(u8 => u8 i8 u16 i16 u32 i32 u64 i64 u128 i128 f32 f64);
impl_num_as_num!(i8 => u8 i8 u16 i16 u32 i32 u64 i64 u128 i128 f32 f64);
impl_num_as_num!(u16 => u8 i8 u16 i16 u32 i32 u64 i64 u128 i128 f32 f64);
impl_num_as_num!(i16 => u8 i8 u16 i16 u32 i32 u64 i64 u128 i128 f32 f64);
impl_num_as_num!(u32 => u8 i8 u16 i16 u32 i32 u64 i64 u128 i128 f32 f64);
impl_num_as_num!(i32 => u8 i8 u16 i16 u32 i32 u64 i64 u128 i128 f32 f64);
impl_num_as_num!(u64 => u8 i8 u16 i16 u32 i32 u64 i64 u128 i128 f32 f64);
impl_num_as_num!(i64 => u8 i8 u16 i16 u32 i32 u64 i64 u128 i128 f32 f64);
impl_num_as_num!(u128 => u8 i8 u16 i16 u32 i32 u64 i64 u128 i128 f32 f64);
impl_num_as_num!(i128 => u8 i8 u16 i16 u32 i32 u64 i64 u128 i128 f32 f64);
impl_num_as_num!(f32 => u8 i8 u16 i16 u32 i32 u64 i64 u128 i128 f32 f64);
impl_num_as_num!(f64 => u8 i8 u16 i16 u32 i32 u64 i64 u128 i128 f32 f64);

pub fn closest_key(value: &str, keys: impl IntoIterator<Item = &'static str>) -> &'static str {
    keys.into_iter()
        .min_by_key(|x| damerau_levenshtein(x, value))
        .unwrap()
}

#[cfg(test)]
mod test {
    use crate::utils::Combine;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_combine() {
        assert_eq!((0, 5).combine((10, 5)), (0, 15).into());
        assert_eq!((20, 0).combine((0, 0)), (0, 20).into());
        assert_eq!((5, 20).combine((10, 5)), (5, 20).into());
        assert_eq!((10, 5).combine((5, 20)), (5, 20).into());
        assert_eq!((10..20).combine(15..30), (10..30).into());
    }
}
