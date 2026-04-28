use crate::output::PatchVec;
use crate::parse::ParsedCode;
use keystone::{Arch, Keystone, Mode, error_msg};
use miette::{Diagnostic, SourceSpan};
use std::sync::Arc;
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
        code: impl IntoIterator<Item = (u32, ParsedCode)>,
        labels: impl IntoIterator<Item = (Arc<str>, u32)>,
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
        while let Some((address, instruction)) = code_iter.next() {
            match instruction {
                ParsedCode::Byte(byte) => result.put_byte(address, byte),
                ParsedCode::Short(short) => result.put(address, short.to_le_bytes()),
                ParsedCode::Int(int) => result.put(address, int.to_le_bytes()),
                ParsedCode::Long(long) => result.put(address, long.to_le_bytes()),
                ParsedCode::Float(float) => result.put(address, float.to_le_bytes()),
                ParsedCode::Double(double) => result.put(address, double.to_le_bytes()),
                ParsedCode::String(string) => result.put(address, string.bytes()),
                ParsedCode::Asm(asm, code_address, source_span) => {
                    generated_asm.push_str(&asm);
                    asm_lines.push((asm, source_span));
                    while let Some((asm, source_span)) = code_iter.next_if_map(|x| match x {
                        (
                            next_address,
                            ParsedCode::Asm(next_asm, next_code_address, next_source_span),
                        ) if next_address == address + asm_lines.len() as u32 * 4
                            && next_code_address == code_address + asm_lines.len() as u64 * 4 =>
                        {
                            Ok((next_asm, next_source_span))
                        }
                        other => Err(other),
                    }) {
                        generated_asm.push('\n');
                        generated_asm.push_str(&asm);
                        asm_lines.push((asm, source_span));
                    }
                    let asm = match self.keystone.asm(generated_asm.clone(), code_address) {
                        Ok(result) => result,
                        Err(err) if asm_lines.len() == 1 => {
                            record_diagnostic(AssembleDiagnostic::InvalidAsm {
                                message: error_msg(err),
                                at: source_span,
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
