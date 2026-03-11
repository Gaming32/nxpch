use miette::SourceOffset;
use std::fmt::Debug;

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
