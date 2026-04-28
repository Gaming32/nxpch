use crate::option::{NxpchOption, OptionParseError};
use crate::preprocessor::{PreprocessorDiagnostic, PreprocessorDirective};
use crate::utils::json5_error_to_offset;
use miette::{Diagnostic, SourceOffset, SourceSpan};
use std::ops::Range;
use std::sync::Arc;
use subslice_offset::SubsliceOffset;
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct PreParsedCode {
    pub statements: Vec<(Range<usize>, PreParsedStatement)>,
    pub diagnostics: Vec<PreParseDiagnostic>,
}

#[derive(Clone, Debug)]
pub enum PreParsedStatement {
    Option(Arc<NxpchOption>, SourceSpan),
    Preprocessor(PreprocessorDirective),
    Code(Arc<str>, SourceSpan),
}

impl PreParsedCode {
    pub fn parse(input: &str) -> Self {
        let mut statements = vec![];
        let mut diags = vec![];
        let mut current = input;
        while !current.is_empty() {
            current = current.trim_start();
            if current.starts_with("//") {
                let eol = current.find('\n').unwrap_or(current.len() - 1);
                current = &current[eol + 1..]
            } else if let Some([Some(full_match), Some(option_name)]) =
                ere::compile_regex!("^([a-z_]+)[[:blank:]]*=[[:blank:]]*").exec(current)
            {
                let json_string = json5_trim(&current[full_match.len()..]);
                let start_offset = input.subslice_offset(json_string).unwrap();
                let name_span = (
                    input.subslice_offset(option_name).unwrap(),
                    option_name.len(),
                )
                    .into();
                match NxpchOption::parse(option_name, json_string) {
                    Ok(mut option) => {
                        option.update_offsets(json_string, start_offset);
                        let statement_start = input.subslice_offset(current).unwrap();
                        let statement_end =
                            statement_start + full_match.len() + json_string.trim_end().len();
                        statements.push((
                            statement_start..statement_end,
                            PreParsedStatement::Option(Arc::new(option), name_span),
                        ));
                    }
                    Err(OptionParseError::InvalidOption(err)) => {
                        diags.push(PreParseDiagnostic::InvalidOptionValue {
                            option: option_name.to_string(),
                            at: json5_error_to_offset(&err, json_string, start_offset),
                            cause: err,
                        });
                    }
                    Err(OptionParseError::UnknownOption { closest }) => {
                        diags.push(PreParseDiagnostic::UnknownOption {
                            option: option_name.to_string(),
                            closest,
                            at: name_span,
                        });
                    }
                }
                current = &current[full_match.len() + json_string.len()..];
            } else if current.starts_with('#') {
                let (line, remaining) = find_up_to_comment(current);
                current = remaining;
                let start_offset = input.subslice_offset(line).unwrap();
                let directive = PreprocessorDirective::parse(line, start_offset, |diag| {
                    diags.push(diag.into())
                });
                if let Some(directive) = directive {
                    statements.push((
                        start_offset..start_offset + line.len(),
                        PreParsedStatement::Preprocessor(directive),
                    ));
                }
            } else if !current.is_empty() {
                let (line, remaining) = find_up_to_comment(current);
                current = remaining;
                let start_offset = input.subslice_offset(line).unwrap();
                statements.push((
                    start_offset..start_offset + line.len(),
                    PreParsedStatement::Code(line.into(), (start_offset, line.len()).into()),
                ));
            }
        }
        Self {
            statements,
            diagnostics: diags,
        }
    }
}

fn json5_trim(s: &str) -> &str {
    let mut depth = 0usize;
    let mut idx = 0;
    while idx < s.len() {
        match s.as_bytes()[idx] {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    idx += 1;
                    break;
                }
            }
            b'\'' | b'"' => idx = skip_string(s, idx),
            b if depth == 0 && b.is_ascii_whitespace() => {
                idx += 1;
                break;
            }
            _ => {}
        }
        idx += 1;
    }
    if let Some(extra) = s[idx..].find('\n') {
        &s[..idx + extra + 1]
    } else {
        s
    }
}

fn find_up_to_comment(text: &str) -> (&str, &str) {
    let (line, remaining) = text.split_at(text.find('\n').unwrap_or(text.len() - 1) + 1);
    let line = line.trim_start();
    if line.contains("//") {
        let bytes = text.as_bytes();
        let mut idx = 0;
        while idx < line.len() - 1 {
            match bytes[idx] {
                b'/' if bytes[idx + 1] == b'/' => {
                    return (line[..idx].trim_end(), remaining);
                }
                b'\'' | b'"' => idx = skip_string(line, idx),
                _ => {}
            }
            idx += 1;
        }
    }
    (line.trim_end(), remaining)
}

fn skip_string(s: &str, mut idx: usize) -> usize {
    let start = s.as_bytes()[idx];
    idx += 1;
    while idx < s.len() {
        match s.as_bytes()[idx] {
            b'\\' => idx += 1,
            x if x == start => break,
            _ => {}
        }
        idx += 1;
    }
    idx
}

#[derive(Debug, Clone, Diagnostic, Error)]
pub enum PreParseDiagnostic {
    #[error("Invalid value for {option}")]
    #[diagnostic(code(pre_parse::invalid_option_value))]
    InvalidOptionValue {
        option: String,

        #[source]
        cause: json5::Error,
        #[label("{cause}")]
        at: SourceOffset,
    },

    #[error("Unknown option {option}")]
    #[diagnostic(code(pre_parse::unknown_option))]
    UnknownOption {
        option: String,

        closest: &'static str,
        #[label("Did you mean {closest}?")]
        at: SourceSpan,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Preprocessor(#[from] PreprocessorDiagnostic),
}

#[cfg(test)]
mod test {
    use crate::pre_parse::json5_trim;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_json5_trim() {
        assert_eq!(json5_trim("'hello'"), "'hello'");
        assert_eq!(
            json5_trim("{hello: 1234, bellows: [5678]} testing testing"),
            "{hello: 1234, bellows: [5678]} testing testing",
        );
        assert_eq!(
            json5_trim("{hello: 1234, bellows: [5678]} \ntesting testing"),
            "{hello: 1234, bellows: [5678]} \n",
        );
    }
}
