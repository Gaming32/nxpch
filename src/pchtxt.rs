use crate::output::PatchVec;
use miette::Diagnostic;
use std::num::ParseIntError;
use subslice_offset::SubsliceOffset;
use thiserror::Error;

pub fn pchtxt_to_patches(pchtxt: &str) -> ((PatchVec, u128), Vec<PchtxtDianostic>) {
    let mut vec = PatchVec::new();
    let mut diags = vec![];
    let mut enabled = false;
    let mut offset_shift = 0;
    let mut last_comment = "";
    let mut build_id = None;
    for line in pchtxt.lines() {
        if line.len() < 2 {
            continue;
        }
        let line_start = pchtxt.subslice_offset(line).unwrap();
        if build_id.is_none()
            && let Some(build_id_text) = line.strip_prefix("@nsobid-")
        {
            let hex_len = build_id_text
                .bytes()
                .position(|x| !x.is_ascii_hexdigit())
                .unwrap_or(build_id_text.len());
            match u128::from_str_radix(&build_id_text[..hex_len], 16) {
                Ok(bid) => build_id = Some(bid),
                Err(err) => {
                    diags.push(PchtxtDianostic::InvalidBuildId {
                        cause: err,
                        at: (line_start + 8, hex_len),
                    });
                    build_id = Some(0); // Suppress "Missing @nsobid" error
                }
            }
            continue;
        }
        match line.as_bytes()[0] {
            b'#' => diags.push(PchtxtDianostic::Information {
                message: line.to_string(),
                at: (line_start, line.len()),
            }),
            b'/' => last_comment = line,
            b'@' => match line.as_bytes()[1] {
                b'b' | b'B' => diags.push(PchtxtDianostic::BigEndian {
                    at: (line_start, line.len()),
                }),
                b'l' | b'L' => {}
                b'e' | b'E' => {
                    enabled = true;
                    diags.push(PchtxtDianostic::PatchEnabled {
                        message: last_comment.to_string(),
                        at: (line_start, line.len()),
                    });
                }
                b'f' | b'F' => {
                    if line.len() < 7 {
                        continue;
                    }
                    let Some(flag_name) = cut_out_value(pchtxt, line, 6, &mut diags) else {
                        continue;
                    };

                    if flag_name.eq_ignore_ascii_case("print_values") {
                        // Do nothing
                    } else {
                        if line.len() < flag_name.len() + 8 {
                            diags.push(PchtxtDianostic::UnknownFlag {
                                at: (pchtxt.subslice_offset(flag_name).unwrap(), flag_name.len()),
                            });
                            continue;
                        }
                        let Some(flag_value) =
                            cut_out_value(pchtxt, line, 6 + flag_name.len() + 1, &mut diags)
                        else {
                            continue;
                        };
                        if flag_name.eq_ignore_ascii_case("offset_shift") {
                            offset_shift =
                                parse_int::parse::<i32>(flag_value).ok().unwrap_or_default();
                        } else {
                            diags.push(PchtxtDianostic::UnknownFlag {
                                at: (pchtxt.subslice_offset(flag_name).unwrap(), flag_name.len()),
                            });
                        }
                    }
                }
                b's' | b'S' => break,
                _ => enabled = false,
            },
            _ => {
                if !enabled || line.len() < 11 {
                    continue;
                }
                let mut offset = match u32::from_str_radix(&line[..8], 16) {
                    Ok(off) => off.checked_add_signed(offset_shift).unwrap_or_else(|| {
                        diags.push(PchtxtDianostic::OverUnderFlow {
                            offset_shift,
                            at: (line_start, 8),
                        });
                        off.wrapping_add_signed(offset_shift)
                    }),
                    Err(err) => {
                        diags.push(PchtxtDianostic::InvalidOffset {
                            cause: err,
                            at: (line_start, 8),
                        });
                        continue;
                    }
                };

                if !line.is_char_boundary(9) {
                    let offset = line.floor_char_boundary(9);
                    diags.push(PchtxtDianostic::UnexpectedUnicode {
                        at: (line_start + offset, line.ceil_char_boundary(9) - offset),
                    });
                    continue;
                }

                let value_str = &line[9..];
                if value_str.as_bytes()[0] == b'"' {
                    let value = value_str
                        .as_bytes()
                        .array_windows()
                        .skip(1)
                        .position(|&[a, b]| b == b'"' && a != b'\\')
                        .map(|x| &value_str.as_bytes()[1..x + 2]);
                    let Some(value) = value else {
                        diags.push(PchtxtDianostic::UnterminatedStringLiteral {
                            at: line_start + line.len(),
                        });
                        continue;
                    };
                    let mut value_index = 0;
                    while value_index < value.len() {
                        let byte = if value[value_index] == b'\\' {
                            value_index += 1;
                            match value[value_index] {
                                b'a' => b'\x07',
                                b'b' => b'\x08',
                                b'f' => b'\x0C',
                                b'n' => b'\n',
                                b'r' => b'\r',
                                b't' => b'\t',
                                o => o,
                            }
                        } else {
                            value[value_index]
                        };
                        vec.put_byte(offset, byte);
                        offset += 1;
                        value_index += 1;
                    }
                    vec.put_byte(offset, 0);
                } else {
                    let Some(value) = cut_out_value(pchtxt, value_str, 0, &mut diags) else {
                        continue;
                    };
                    if value.len() % 2 != 0 {
                        diags.push(PchtxtDianostic::OddHexValueLength {
                            at: (line_start + 9, value.len()),
                        });
                        continue;
                    }
                    if let Some(non_hex) = value
                        .as_bytes()
                        .iter()
                        .position(|&x| !x.is_ascii_hexdigit())
                    {
                        diags.push(PchtxtDianostic::InvalidHexValue {
                            at: (line_start + 9 + non_hex, 1),
                        });
                        continue;
                    }
                    let mut index = 0;
                    while index < value.len() {
                        vec.put_byte(
                            offset,
                            u8::from_str_radix(&value[index..index + 2], 16).unwrap(),
                        );
                        offset += 1;
                        index += 2;
                    }
                }
            }
        }
    }
    if build_id.is_none() {
        diags.insert(0, PchtxtDianostic::MissingBuildId);
    }
    ((vec, build_id.unwrap_or_default()), diags)
}

fn cut_out_value<'a>(
    base: &str,
    s: &'a str,
    start: usize,
    diags: &mut Vec<PchtxtDianostic>,
) -> Option<&'a str> {
    let len = s.as_bytes()[start..]
        .iter()
        .position(|&c| matches!(c, b' ' | b'/' | b'\r' | b'\n'))
        .unwrap_or(s.len() - start);
    let result = s.get(start..start + len);
    if result.is_none() {
        let offset = base.subslice_offset(s).unwrap();
        let char_floor = s.floor_char_boundary(start);
        diags.push(PchtxtDianostic::UnexpectedUnicode {
            at: (
                offset + char_floor,
                s.ceil_char_boundary(start + len) - char_floor,
            ),
        });
    }
    result
}

#[derive(Debug, Diagnostic, Error)]
pub enum PchtxtDianostic {
    #[error("{message}")]
    #[diagnostic(code(pchtxt::info), severity(advice))]
    Information {
        message: String,
        #[label]
        at: (usize, usize),
    },

    #[error("Patch read: {message}")]
    #[diagnostic(code(pchtxt::enabled), severity(advice))]
    PatchEnabled {
        message: String,
        #[label]
        at: (usize, usize),
    },

    #[error("Unexpected unicode")]
    #[diagnostic(code(pchtxt::unexpected_unicode))]
    UnexpectedUnicode {
        #[label]
        at: (usize, usize),
    },

    #[error("Big-Endian is no longer supported. Proceeding as little-endian.")]
    #[diagnostic(code(pchtxt::big_endian), severity(warn))]
    BigEndian {
        #[label]
        at: (usize, usize),
    },

    #[error("Over/underflow from adding offset_shift")]
    #[diagnostic(code(pchtxt::overflow), severity(warn))]
    OverUnderFlow {
        offset_shift: i32,

        #[label("Current offset shift is 0x{offset_shift:X}")]
        at: (usize, usize),
    },

    #[error("Missing @nsobid")]
    #[diagnostic(code(pchtxt::missing_bid))]
    MissingBuildId,

    #[error("Invalid @nsobid")]
    #[diagnostic(code(pchtxt::invalid_bid))]
    InvalidBuildId {
        #[source]
        cause: ParseIntError,

        #[label("{cause}")]
        at: (usize, usize),
    },

    #[error("Unknown flag")]
    #[diagnostic(code(pchtxt::unknown_flag))]
    UnknownFlag {
        #[label]
        at: (usize, usize),
    },

    #[error("Invalid offset")]
    #[diagnostic(code(pchtxt::invalid_offset))]
    InvalidOffset {
        #[source]
        cause: ParseIntError,

        #[label("{cause}")]
        at: (usize, usize),
    },

    #[error("Unterminated string literal")]
    #[diagnostic(code(pchtxt::unterminated_string))]
    UnterminatedStringLiteral {
        #[label("Expected \"")]
        at: usize,
    },

    #[error("Odd number of characters in hex value")]
    #[diagnostic(code(pchtxt::odd_hex_value_len))]
    OddHexValueLength {
        #[label("{} characters long", at.1)]
        at: (usize, usize),
    },

    #[error("Hex value is not hex")]
    #[diagnostic(code(pchtxt::invalid_hex_value))]
    InvalidHexValue {
        #[label("Should be hex")]
        at: (usize, usize),
    },
}
