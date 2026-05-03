use crate::option::BuildId;
use crate::output::PatchVec;
use crate::parse::{ParsedCode, ParsedCodeInstruction};
use crate::utils::{EscapePchtxtStringChar, SpanExt};
use arcstr::ArcStr;
use keystone::{Arch, Keystone, Mode, error_msg};
use miette::{Diagnostic, SourceSpan};
use std::io;
use std::io::Write;
use thiserror::Error;

pub struct Assembler {
    keystone: Keystone,
}

impl Assembler {
    pub fn new() -> Self {
        Self {
            keystone: Keystone::new(Arch::ARM64, Mode::empty()).unwrap(),
        }
    }

    pub fn assemble(
        &self,
        code: impl IntoIterator<Item = ParsedCode>,
        labels: impl IntoIterator<Item = (ArcStr, u32)>,
        mut record_diagnostic: impl FnMut(AssembleDiagnostic),
    ) -> PatchVec {
        let mut generated_asm: String = labels
            .into_iter()
            .map(|(name, address)| format!("{name}={address}\n"))
            .collect();
        let asm_prefix_len = generated_asm.len();
        let mut asm_lines = vec![];
        let mut result = PatchVec::new();
        let mut code_iter = code.into_iter().peekable();
        while let Some(ParsedCode {
            address,
            instruction,
            instruction_span,
            ..
        }) = code_iter.next()
        {
            use ParsedCodeInstruction as PCI;
            match instruction {
                PCI::Byte(byte) => result.put_byte(address, byte),
                PCI::Short(short) => result.put(address, short.to_le_bytes()),
                PCI::Int(int) => result.put(address, int.to_le_bytes()),
                PCI::Long(long) => result.put(address, long.to_le_bytes()),
                PCI::Float(float) => result.put(address, float.to_le_bytes()),
                PCI::Double(double) => result.put(address, double.to_le_bytes()),
                PCI::String(string) => {
                    result.put(address, string.bytes());
                    result.put_byte(address + string.len() as u32, 0);
                }
                PCI::Asm(asm, code_address) => {
                    generated_asm.push_str(&asm);
                    asm_lines.push((asm, instruction_span));
                    while let Some((asm, instruction_span)) = code_iter.next_if_map(|x| match x {
                        ParsedCode {
                            address: next_address,
                            instruction: PCI::Asm(next_asm, next_code_address),
                            instruction_span: next_instruction_span,
                            ..
                        } if next_address == address + asm_lines.len() as u32 * 4
                            && next_code_address == code_address + asm_lines.len() as u64 * 4 =>
                        {
                            Ok((next_asm, next_instruction_span))
                        }
                        other => Err(other),
                    }) {
                        generated_asm.push('\n');
                        generated_asm.push_str(&asm);
                        asm_lines.push((asm, instruction_span));
                    }
                    let asm = match self.keystone.asm(generated_asm.clone(), code_address) {
                        Ok(result) => result,
                        Err(err) if asm_lines.len() == 1 => {
                            record_diagnostic(AssembleDiagnostic::InvalidAsm {
                                message: error_msg(err),
                                at: instruction_span,
                            });
                            generated_asm.truncate(asm_prefix_len);
                            asm_lines.clear();
                            continue;
                        }
                        Err(_) => {
                            generated_asm.truncate(asm_prefix_len);
                            for (line_offset, (line, line_span)) in asm_lines.drain(..).enumerate()
                            {
                                generated_asm.push_str(&line);
                                match self.keystone.asm(
                                    generated_asm.clone(),
                                    code_address + line_offset as u64 * 4,
                                ) {
                                    Ok(asm) => {
                                        assert_eq!(asm.size, 4);
                                        result.put(address + line_offset as u32 * 4, asm.bytes)
                                    }
                                    Err(err) => {
                                        record_diagnostic(AssembleDiagnostic::InvalidAsm {
                                            message: error_msg(err),
                                            at: line_span,
                                        });
                                    }
                                }
                                generated_asm.truncate(asm_prefix_len);
                            }
                            continue;
                        }
                    };
                    assert_eq!(asm.size as usize, asm_lines.len() * 4);
                    result.put(address, asm.bytes);
                    generated_asm.truncate(asm_prefix_len);
                    asm_lines.clear();
                }
            }
        }
        result
    }

    pub fn assemble_debug_pchtxt(
        &self,
        original_source: &str,
        build_id: BuildId,
        code: impl IntoIterator<Item = ParsedCode>,
        labels: impl IntoIterator<Item = (ArcStr, u32)>,
        mut output: impl Write,
        mut record_diagnostic: impl FnMut(AssembleDiagnostic),
    ) -> io::Result<()> {
        writeln!(output, "@nsobid-{build_id:032X}")?;
        writeln!(output, "@enabled")?;
        writeln!(output)?;

        let mut generated_asm: String = labels
            .into_iter()
            .map(|(name, address)| format!("{name}={address}\n"))
            .collect();
        let asm_prefix_len = generated_asm.len();
        let mut last_source_offset = 0;
        let mut last_offset_shift = 0;
        for instruction in code {
            use ParsedCodeInstruction as PCI;
            for line in original_source[last_source_offset..instruction.line_span.offset()].lines()
            {
                if line.is_empty() {
                    writeln!(output)?;
                    continue;
                }
                writeln!(output, "// {line}")?;
            }
            let offset_shift = instruction.address - instruction.unshifted_address;
            if offset_shift != last_offset_shift {
                last_offset_shift = offset_shift;
                writeln!(output, "@flag offset_shift 0x{offset_shift:X}")?;
            }
            write!(output, "{:08X} ", instruction.unshifted_address)?;
            match instruction.instruction {
                PCI::Byte(byte) => write!(output, "{byte:02X}")?,
                // fmt formats as if BE, but the Switch uses LE, so we need to swap the bytes for correct formatting
                PCI::Short(short) => write!(output, "{:04X}", short.swap_bytes())?,
                PCI::Int(int) => write!(output, "{:08X}", int.swap_bytes())?,
                PCI::Long(long) => write!(output, "{:016X}", long.swap_bytes())?,
                PCI::Float(float) => write!(output, "{:08X}", float.to_bits().swap_bytes())?,
                PCI::Double(double) => write!(output, "{:016X}", double.to_bits().swap_bytes())?,
                PCI::String(str) => {
                    output.write_all(b"\"")?;
                    for char in str.chars() {
                        write!(output, "{}", EscapePchtxtStringChar(char))?;
                    }
                    output.write_all(b"\"")?;
                }
                PCI::Asm(asm, code_address) => {
                    generated_asm.push_str(&asm);
                    match self.keystone.asm(generated_asm.clone(), code_address) {
                        Ok(asm) => {
                            assert_eq!(asm.size, 4);
                            for byte in asm.bytes {
                                write!(output, "{byte:02X}")?;
                            }
                        }
                        Err(err) => record_diagnostic(AssembleDiagnostic::InvalidAsm {
                            message: error_msg(err),
                            at: instruction.instruction_span,
                        }),
                    }
                    generated_asm.truncate(asm_prefix_len);
                }
            }
            write!(
                output,
                " // {}",
                &original_source[instruction.line_span.to_range()],
            )?;
            last_source_offset = instruction.line_span.end();
        }

        if last_source_offset < original_source.len() {
            for line in original_source[last_source_offset..].lines() {
                if line.is_empty() {
                    writeln!(output)?;
                    continue;
                }
                writeln!(output, "// {line}")?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum AssembleDiagnostic {
    #[error("Failed to assemble assembly")]
    #[diagnostic(code(assemble::invalid_asm))]
    InvalidAsm {
        message: String,

        #[label("{message}")]
        at: SourceSpan,
    },
}
