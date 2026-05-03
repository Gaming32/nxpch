use crate::option::BuildId;
use crate::output::PatchVec;
use crate::parse::{CodeAddress, ParsedCode, ParsedCodeInstruction};
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
        labels: impl IntoIterator<Item = (ArcStr, CodeAddress)>,
        mut record_diagnostic: impl FnMut(AssembleDiagnostic),
    ) -> PatchVec {
        let mut generated_asm: String = labels
            .into_iter()
            .map(|(name, address)| format!("{name}={}\n", address.code_address()))
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
            let nso_address = address.nso_address();
            match instruction {
                PCI::Byte(byte) => result.put_byte(nso_address, byte),
                PCI::Short(short) => result.put(nso_address, short.to_le_bytes()),
                PCI::Int(int) => result.put(nso_address, int.to_le_bytes()),
                PCI::Long(long) => result.put(nso_address, long.to_le_bytes()),
                PCI::Float(float) => result.put(nso_address, float.to_le_bytes()),
                PCI::Double(double) => result.put(nso_address, double.to_le_bytes()),
                PCI::String(string) => {
                    result.put(nso_address, string.bytes());
                    result.put_byte(nso_address + string.len() as u32, 0);
                }
                PCI::Asm(asm) => {
                    let code_address = address.code_address();
                    generated_asm.push_str(&asm);
                    asm_lines.push((asm, instruction_span));
                    while let Some((asm, instruction_span)) = code_iter.next_if_map(|x| match x {
                        ParsedCode {
                            address: next_address,
                            instruction: PCI::Asm(next_asm),
                            instruction_span: next_instruction_span,
                            ..
                        } if next_address.code_address()
                            == code_address + asm_lines.len() as u32 * 4 =>
                        {
                            Ok((next_asm, next_instruction_span))
                        }
                        other => Err(other),
                    }) {
                        generated_asm.push('\n');
                        generated_asm.push_str(&asm);
                        asm_lines.push((asm, instruction_span));
                    }
                    let asm = match self
                        .keystone
                        .asm(generated_asm.clone(), code_address as u64)
                    {
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
                                    (code_address + line_offset as u32 * 4) as u64,
                                ) {
                                    Ok(asm) => {
                                        assert_eq!(asm.size, 4, "{:?}", asm.bytes);
                                        result.put(nso_address + line_offset as u32 * 4, asm.bytes)
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
                    assert_eq!(asm.size as usize, asm_lines.len() * 4, "{:?}", asm.bytes);
                    result.put(nso_address, asm.bytes);
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
        labels: impl IntoIterator<Item = (ArcStr, CodeAddress)>,
        mut output: impl Write,
        mut record_diagnostic: impl FnMut(AssembleDiagnostic),
    ) -> io::Result<()> {
        writeln!(output, "@nsobid-{build_id:032X}")?;
        writeln!(output, "@flag offset_shift 0x100")?;
        writeln!(output, "@enabled")?;
        writeln!(output)?;

        let mut generated_asm: String = labels
            .into_iter()
            .map(|(name, address)| format!("{name}={}\n", address.code_address()))
            .collect();
        let asm_prefix_len = generated_asm.len();
        let mut last_source_offset = 0;
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
            write!(output, "{:08X} ", instruction.address.code_address())?;
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
                PCI::Asm(asm) => {
                    generated_asm.push_str(&asm);
                    match self.keystone.asm(
                        generated_asm.clone(),
                        instruction.address.code_address() as u64,
                    ) {
                        Ok(asm) => {
                            assert_eq!(asm.size, 4, "{:?}", asm.bytes);
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
