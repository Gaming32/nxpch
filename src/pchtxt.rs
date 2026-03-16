use crate::output::PatchVec;
use capstone::arch::arm64::ArchMode;
use capstone::prelude::{BuildsCapstone, Capstone};
use miette::Diagnostic;
use std::fmt::Write;
use std::iter::Enumerate;
use std::num::ParseIntError;
use std::str::Lines;
use subslice_offset::SubsliceOffset;
use thiserror::Error;

pub fn pchtxt_to_patches(pchtxt: &str) -> ((PatchVec, u128), Vec<PchtxtDianostic>) {
    let mut vec = PatchVec::new();
    let mut diags = vec![];
    let mut offset_shift = 0;
    let mut last_comment = "";
    let mut build_id = None;
    let mut enabled = false;
    for line in PchtxtParseIter::new(pchtxt) {
        if let Some(diag) = line.diag {
            diags.push(diag);
        }
        match line.data {
            PchtxtLineData::Ignored => {}
            PchtxtLineData::BuildId(bid) => build_id = Some(bid),
            PchtxtLineData::Information => diags.push(PchtxtDianostic::Information {
                message: line.line.to_string(),
                at: (line.line_start, line.line.len()),
            }),
            PchtxtLineData::Comment => last_comment = line.line,
            PchtxtLineData::BigEndian => {}
            PchtxtLineData::LittleEndian => {}
            PchtxtLineData::Enable => {
                enabled = true;
                diags.push(PchtxtDianostic::PatchEnabled {
                    message: last_comment.to_string(),
                    at: (line.line_start, line.line.len()),
                });
            }
            PchtxtLineData::Flag(PchtxtFlag::PrintValues) => {}
            PchtxtLineData::Flag(PchtxtFlag::OffsetShift(shift)) => offset_shift = shift,
            PchtxtLineData::Stop => break,
            PchtxtLineData::Disable => enabled = false,
            PchtxtLineData::Patch { offset, patch, .. } => {
                if enabled {
                    let offset = offset.checked_add_signed(offset_shift).unwrap_or_else(|| {
                        diags.push(PchtxtDianostic::OverUnderFlow {
                            offset_shift,
                            at: (line.line_start, 8),
                        });
                        offset.wrapping_add_signed(offset_shift)
                    });
                    vec.put(offset, patch);
                }
            }
        }
    }
    if build_id.is_none() {
        diags.insert(0, PchtxtDianostic::MissingBuildId);
    }
    ((vec, build_id.unwrap_or_default()), diags)
}

pub fn pchtxt_to_nxpch(pchtxt: &str) -> (String, Vec<PchtxtDianostic>) {
    fn push_comment(output: &mut String, x: &str) {
        let trimmed = x.trim_start();
        if trimmed.is_empty() {
            return;
        }
        if trimmed.starts_with("//") {
            output.push_str(x);
        } else if trimmed.starts_with('/') {
            output.push_str(&x[..x.len() - trimmed.len()]);
            output.push('/');
            output.push_str(trimmed);
        } else {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push(' ');
            }
            output.push_str("//");
            output.push_str(x);
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum Status {
        PreEnabled,
        Enabled,
        Disabled,
    }

    let mut output = String::new();
    let mut diags = vec![];
    let mut status = Status::PreEnabled;
    let mut capstone = None;
    for line in PchtxtParseIter::new(pchtxt) {
        if let Some(diag) = line.diag {
            diags.push(diag);
        }
        match line.data {
            PchtxtLineData::Ignored => {}
            PchtxtLineData::BuildId(bid) => {
                let _ = write!(output, "target_build = 0x{bid:X}");
            }
            PchtxtLineData::Information => {
                // #warning is the closest thing we can get unfortunately
                let _ = writeln!(output, "#warning \"{}\"", line.line[1..].escape_debug());
                continue;
            }
            PchtxtLineData::BigEndian
            | PchtxtLineData::LittleEndian
            | PchtxtLineData::Flag(PchtxtFlag::PrintValues)
            | PchtxtLineData::Stop => {
                let _ = writeln!(output, "// {}", line.line);
                continue;
            }
            PchtxtLineData::Comment => {
                // Guaranteed to start with at least 1 slash
                if !line.line.starts_with("//") {
                    output.push('/');
                }
                output.push_str(line.line);
            }
            PchtxtLineData::Enable => {
                if status == Status::Disabled {
                    output.push_str("#endif");
                    status = Status::Enabled;
                } else {
                    let _ = writeln!(output, "// {}", line.line);
                    status = Status::Enabled;
                    continue;
                }
            }
            PchtxtLineData::Flag(PchtxtFlag::OffsetShift(shift)) => {
                if status == Status::Disabled {
                    diags.push(PchtxtDianostic::DisabledPointerOffset {
                        at: (line.line_start, line.line.len()),
                    });
                }
                let _ = write!(output, "pointer_offset = 0x{shift:X}");
            }
            PchtxtLineData::Disable => {
                if status != Status::Disabled {
                    output.push_str("#if 0");
                    status = Status::Disabled;
                } else {
                    let _ = writeln!(output, "// {}", line.line);
                    continue;
                }
            }
            PchtxtLineData::Patch {
                offset,
                patch,
                is_string,
            } => {
                if status == Status::PreEnabled {
                    output.push_str("#if 0\n");
                    status = Status::Disabled;
                }
                if patch.is_empty() {
                    let _ = write!(output, "// 0x{offset:08X} = ");
                } else if is_string {
                    let _ = write!(output, "0x{offset:08X} = ");
                    let string_part = &patch[..patch.len() - 1];
                    if let Ok(str) = str::from_utf8(string_part) {
                        let _ = write!(output, "\"{}\"", str.escape_debug());
                    } else {
                        output.push('"');
                        for byte in string_part {
                            if !byte.is_ascii() || byte.is_ascii_control() {
                                let _ = write!(output, "\\x{byte:02X}");
                            } else {
                                if matches!(byte, b'\\' | b'"') {
                                    output.push('\\');
                                }
                                output.push(*byte as char);
                            }
                        }
                        output.push('"');
                    }
                } else if offset % 4 == 0 && patch.len() % 4 == 0 {
                    let mut offset = offset;
                    let mut patch = patch.as_slice();
                    let capstone = capstone.get_or_insert_with(|| {
                        Capstone::new().arm64().mode(ArchMode::Arm).build().unwrap()
                    });
                    loop {
                        let _ = write!(output, "0x{offset:08X} = ");
                        let insn = capstone.disasm_count(patch, offset as u64, 1).unwrap();
                        if let Some(insn) = insn.first() {
                            let mnemonic = insn.mnemonic().unwrap();
                            let operand = insn.op_str().unwrap();
                            output.push_str(mnemonic);
                            if !operand.is_empty() {
                                let _ = write!(output, " {operand}");
                            }
                        } else {
                            let _ = write!(
                                output,
                                ".int {}",
                                u32::from_le_bytes(*patch.first_chunk().unwrap()),
                            );
                        }
                        offset += 4;
                        patch = &patch[4..];
                        if !patch.is_empty() {
                            output.push('\n');
                        } else {
                            break;
                        }
                    }
                } else {
                    let mut offset = offset;
                    let mut patch = patch.as_slice();
                    loop {
                        let _ = write!(output, "0x{offset:08X} = ");
                        if let Some(long) = patch.first_chunk().copied().map(u64::from_le_bytes) {
                            let _ = write!(output, ".long {long}");
                            offset += 8;
                            patch = &patch[8..];
                        } else if let Some(int) =
                            patch.first_chunk().copied().map(u32::from_le_bytes)
                        {
                            let _ = write!(output, ".int {int}");
                            offset += 4;
                            patch = &patch[4..];
                        } else if let Some(short) =
                            patch.first_chunk().copied().map(u16::from_le_bytes)
                        {
                            let _ = write!(output, ".short {short}");
                            offset += 2;
                            patch = &patch[2..];
                        } else {
                            let _ = write!(output, ".byte {}", patch[0]);
                            offset += 1;
                            patch = &patch[1..];
                        }
                        if !patch.is_empty() {
                            output.push('\n');
                        } else {
                            break;
                        }
                    }
                }
            }
        }
        push_comment(&mut output, line.remainder);
        output.push('\n');
    }
    if status == Status::Disabled {
        output.push_str("#endif\n");
    }
    (output, diags)
}

#[derive(Clone, Debug, Diagnostic, Error)]
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

    #[error(
        "offset_shift used after explicit @disabled or after a patch without an explicit @enabled"
    )]
    #[diagnostic(
        code(pchtxt::disabled_offset_shift),
        severity(warn),
        help(
            "This may cause issues with nxpch, as this will put the pointer_offset in \
            preprocessor-disabled code, which will make the offset not apply in nxpch, while it \
            still does in pchtxt."
        )
    )]
    DisabledPointerOffset {
        #[label]
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

struct PchtxtParseIter<'a> {
    pchtxt: &'a str,
    lines: Enumerate<Lines<'a>>,
    finished: bool,
}

impl<'a> PchtxtParseIter<'a> {
    fn new(pchtxt: &'a str) -> Self {
        Self {
            pchtxt,
            lines: pchtxt.lines().enumerate(),
            finished: false,
        }
    }
}

impl<'a> Iterator for PchtxtParseIter<'a> {
    type Item = PchtxtLine<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let (line_num, line) = self.lines.next()?;
        let mut diag = None;
        let line_start = self.pchtxt.subslice_offset(line).unwrap();
        if line.len() < 2 || self.finished {
            return Some(PchtxtLine {
                line,
                line_start,
                data: PchtxtLineData::Ignored,
                remainder: line,
                diag,
            });
        }
        if line_num == 0
            && let Some(build_id_text) = line.strip_prefix("@nsobid-")
        {
            let hex_len = build_id_text
                .bytes()
                .position(|x| !x.is_ascii_hexdigit())
                .unwrap_or(build_id_text.len());
            let bid = match u128::from_str_radix(&build_id_text[..hex_len], 16) {
                Ok(bid) => bid,
                Err(err) => {
                    diag = Some(PchtxtDianostic::InvalidBuildId {
                        cause: err,
                        at: (line_start + 8, hex_len),
                    });
                    0
                }
            };
            return Some(PchtxtLine {
                line,
                line_start,
                data: PchtxtLineData::BuildId(bid),
                remainder: &build_id_text[hex_len..],
                diag,
            });
        }
        let (data, remainder) = match line.as_bytes()[0] {
            b'#' => (PchtxtLineData::Information, &line[line.len()..]),
            b'/' => (PchtxtLineData::Comment, &line[line.len()..]),
            b'@' => match line.as_bytes()[1] {
                b'b' | b'B' => {
                    diag = Some(PchtxtDianostic::BigEndian {
                        at: (line_start, line.len()),
                    });
                    (
                        PchtxtLineData::BigEndian,
                        strip_prefix_case_insensitive(&line[2..], "ig-endian"),
                    )
                }
                b'l' | b'L' => (
                    PchtxtLineData::LittleEndian,
                    strip_prefix_case_insensitive(&line[2..], "ittle-endian"),
                ),
                b'e' | b'E' => (
                    PchtxtLineData::Enable,
                    strip_prefix_case_insensitive(&line[2..], "nabled"),
                ),
                b'f' | b'F' => 'parse_flag: {
                    if line.len() < 7 {
                        break 'parse_flag (PchtxtLineData::Ignored, line);
                    }
                    let Some(flag_name) = cut_out_value(self.pchtxt, line, 6, &mut diag) else {
                        break 'parse_flag (PchtxtLineData::Ignored, line);
                    };

                    if flag_name.eq_ignore_ascii_case("print_values") {
                        (
                            PchtxtLineData::Flag(PchtxtFlag::PrintValues),
                            &line[6 + flag_name.len()..],
                        )
                    } else {
                        if line.len() < flag_name.len() + 8 {
                            diag = Some(PchtxtDianostic::UnknownFlag {
                                at: (line_start + 6, flag_name.len()),
                            });
                            break 'parse_flag (PchtxtLineData::Ignored, line);
                        }
                        let Some(flag_value) =
                            cut_out_value(self.pchtxt, line, 6 + flag_name.len() + 1, &mut diag)
                        else {
                            break 'parse_flag (PchtxtLineData::Ignored, line);
                        };
                        let remainder = &line[6 + flag_name.len() + 1 + flag_value.len()..];
                        if flag_name.eq_ignore_ascii_case("offset_shift") {
                            (
                                PchtxtLineData::Flag(PchtxtFlag::OffsetShift(
                                    parse_int::parse::<i32>(flag_value).ok().unwrap_or_default(),
                                )),
                                remainder,
                            )
                        } else {
                            diag = Some(PchtxtDianostic::UnknownFlag {
                                at: (line_start + 6, flag_name.len()),
                            });
                            (PchtxtLineData::Ignored, line)
                        }
                    }
                }
                b's' | b'S' => {
                    self.finished = true;
                    (
                        PchtxtLineData::Stop,
                        strip_prefix_case_insensitive(&line[2..], "top"),
                    )
                }
                _ => (
                    PchtxtLineData::Disable,
                    strip_prefix_case_insensitive(&line[1..], "disabled"),
                ),
            },
            _ => 'parse_patch: {
                if line.len() < 11 {
                    break 'parse_patch (PchtxtLineData::Ignored, line);
                }
                let offset = match u32::from_str_radix(&line[..8], 16) {
                    Ok(off) => off,
                    Err(err) => {
                        diag = Some(PchtxtDianostic::InvalidOffset {
                            cause: err,
                            at: (line_start, 8),
                        });
                        break 'parse_patch (PchtxtLineData::Ignored, line);
                    }
                };

                if !line.is_char_boundary(9) {
                    let offset = line.floor_char_boundary(9);
                    diag = Some(PchtxtDianostic::UnexpectedUnicode {
                        at: (line_start + offset, line.ceil_char_boundary(9) - offset),
                    });
                    break 'parse_patch (PchtxtLineData::Ignored, line);
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
                        diag = Some(PchtxtDianostic::UnterminatedStringLiteral {
                            at: line_start + line.len(),
                        });
                        break 'parse_patch (PchtxtLineData::Ignored, line);
                    };
                    let mut patch = Vec::with_capacity(value.len() + 1);
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
                        patch.push(byte);
                        value_index += 1;
                    }
                    patch.push(0);
                    (
                        PchtxtLineData::Patch {
                            offset,
                            patch,
                            is_string: true,
                        },
                        &value_str[1 + value.len() + 1..],
                    )
                } else {
                    let Some(value) = cut_out_value(self.pchtxt, value_str, 0, &mut diag) else {
                        break 'parse_patch (PchtxtLineData::Ignored, line);
                    };
                    if value.len() % 2 != 0 {
                        diag = Some(PchtxtDianostic::OddHexValueLength {
                            at: (line_start + 9, value.len()),
                        });
                        break 'parse_patch (PchtxtLineData::Ignored, line);
                    }
                    if let Some(non_hex) = value
                        .as_bytes()
                        .iter()
                        .position(|&x| !x.is_ascii_hexdigit())
                    {
                        diag = Some(PchtxtDianostic::InvalidHexValue {
                            at: (line_start + 9 + non_hex, 1),
                        });
                        break 'parse_patch (PchtxtLineData::Ignored, line);
                    }
                    let mut patch = Vec::with_capacity(value.len() / 2);
                    let mut index = 0;
                    while index < value.len() {
                        patch.push(u8::from_str_radix(&value[index..index + 2], 16).unwrap());
                        index += 2;
                    }
                    (
                        PchtxtLineData::Patch {
                            offset,
                            patch,
                            is_string: false,
                        },
                        &value_str[value.len()..],
                    )
                }
            }
        };
        Some(PchtxtLine {
            line,
            line_start,
            data,
            remainder,
            diag,
        })
    }
}

fn strip_prefix_case_insensitive<'a>(text: &'a str, prefix: &str) -> &'a str {
    if prefix.len() <= text.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix) {
        &text[prefix.len()..]
    } else {
        text
    }
}

fn cut_out_value<'a>(
    base: &str,
    s: &'a str,
    start: usize,
    diag: &mut Option<PchtxtDianostic>,
) -> Option<&'a str> {
    let len = s.as_bytes()[start..]
        .iter()
        .position(|&c| matches!(c, b' ' | b'/' | b'\r' | b'\n'))
        .unwrap_or(s.len() - start);
    let result = s.get(start..start + len);
    if result.is_none() {
        let offset = base.subslice_offset(s).unwrap();
        let char_floor = s.floor_char_boundary(start);
        *diag = Some(PchtxtDianostic::UnexpectedUnicode {
            at: (
                offset + char_floor,
                s.ceil_char_boundary(start + len) - char_floor,
            ),
        });
    }
    result
}

#[derive(Clone, Debug)]
struct PchtxtLine<'a> {
    line: &'a str,
    line_start: usize,
    data: PchtxtLineData,
    remainder: &'a str,
    /// Only Some for parse errors. Other lines can be generated later.
    diag: Option<PchtxtDianostic>,
}

#[derive(Clone, Debug)]
enum PchtxtLineData {
    Ignored,
    BuildId(u128),
    Information,
    Comment,
    BigEndian,
    LittleEndian,
    Enable,
    Flag(PchtxtFlag),
    Stop,
    Disable,
    Patch {
        offset: u32,
        patch: Vec<u8>,
        is_string: bool,
    },
}

#[derive(Copy, Clone, Debug)]
enum PchtxtFlag {
    PrintValues,
    OffsetShift(i32),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PchtxtParseState {
    Enabled,
    Disabled,
    Stopped,
}
