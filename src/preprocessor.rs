use crate::macros::{MacroDefine, MacroDiagnostic};
use crate::utils::json5_error_to_offset;
use miette::Diagnostic;
use subslice_offset::SubsliceOffset;
use thiserror::Error;

#[derive(Clone, Debug)]
pub enum PreprocessorDirective {
    If(String),
    ElseIf(String),
    Else,
    EndIf,
    IfDefined(String),
    IfNotDefined(String),
    ElseIfDefined(String),
    ElseIfNotDefined(String),
    Define(MacroDefine),
    Undefine(String),
    Error(String),
    Warning(String),
}

impl PreprocessorDirective {
    pub fn parse(
        line: &str,
        offset: usize,
        mut record_diagnostic: impl FnMut(PreprocessorDiagnostic),
    ) -> Option<Self> {
        let (keyword, body) = line.split_once(' ').unwrap_or((line, ""));
        let body = body.trim();
        let body_base_offset = line.subslice_offset(body).unwrap_or(keyword.len()) + offset;

        macro_rules! require_body {
            ($variant:ident, $what:literal) => {{
                if !body.is_empty() {
                    Some(Self::$variant(body.to_string()))
                } else {
                    record_diagnostic(PreprocessorDiagnostic::MissingBody {
                        keyword: $what,
                        at: body_base_offset,
                    });
                    None
                }
            }};
        }
        macro_rules! no_body {
            ($variant:ident, $what:literal) => {{
                if body.is_empty() {
                    Some(Self::$variant)
                } else {
                    record_diagnostic(PreprocessorDiagnostic::UnexpectedBody {
                        keyword: $what,
                        at: (body_base_offset, body.len()),
                    });
                    None
                }
            }};
        }
        macro_rules! require_var_name {
            ($variant:ident, $what:literal) => {{
                if MacroDefine::NAME_REGEX.test(body) {
                    Some(Self::$variant(body.to_string()))
                } else {
                    record_diagnostic(PreprocessorDiagnostic::InvalidVarName {
                        keyword: $what,
                        at: (body_base_offset, body.len()),
                    });
                    None
                }
            }};
        }
        macro_rules! require_string {
            ($variant:ident, $what:literal) => {
                match json5::from_str::<String>(body) {
                    Ok(s) => Some(Self::$variant(s)),
                    Err(err) => {
                        record_diagnostic(PreprocessorDiagnostic::InvalidString {
                            keyword: $what,
                            at: json5_error_to_offset(&err, body, body_base_offset),
                            cause: err,
                        });
                        None
                    }
                }
            };
        }

        match keyword {
            "#if" => require_body!(If, "if"),
            "#elif" => require_body!(ElseIf, "elif"),
            "#else" => no_body!(Else, "else"),
            "#endif" => no_body!(EndIf, "endif"),
            "#ifdef" => require_var_name!(IfDefined, "ifdef"),
            "#ifndef" => require_var_name!(IfNotDefined, "ifndef"),
            "#elifdef" => require_var_name!(ElseIfDefined, "elifdef"),
            "#elifndef" => require_var_name!(ElseIfNotDefined, "elifndef"),
            "#define" => Some(Self::Define(MacroDefine::parse(
                body,
                body_base_offset,
                |diag| record_diagnostic(diag.into()),
            )?)),
            "#undef" => require_var_name!(Undefine, "undef"),
            "#error" => require_string!(Error, "error"),
            "#warning" => require_string!(Warning, "warning"),
            _ => {
                record_diagnostic(PreprocessorDiagnostic::InvalidDirective {
                    directive: keyword.to_string(),
                    at: (offset, keyword.len()),
                });
                None
            }
        }
    }
}

#[derive(Debug, Clone, Diagnostic, Error)]
pub enum PreprocessorDiagnostic {
    #[error("#{keyword} missing argument")]
    MissingBody {
        keyword: &'static str,

        #[label("Expected argument")]
        at: usize,
    },

    #[error("#{keyword} takes no body")]
    UnexpectedBody {
        keyword: &'static str,

        #[label("Should not be present")]
        at: (usize, usize),
    },

    #[error("#{keyword} expects a single var name")]
    InvalidVarName {
        keyword: &'static str,

        #[label("Invalid variable name")]
        at: (usize, usize),
    },

    #[error("#{keyword} expects a single string")]
    InvalidString {
        keyword: &'static str,

        #[source]
        cause: json5::Error,
        #[label("{cause}")]
        at: usize,
    },

    #[error("Unknown directive {directive}")]
    InvalidDirective {
        directive: String,

        #[label("Unknown directive")]
        at: (usize, usize),
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    MacroDefine(#[from] MacroDiagnostic),
}
