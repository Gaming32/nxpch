use crate::preprocessor::expr::{ExprDiagnostic, evaluate};
use crate::preprocessor::{MacroDefine, MacroDiagnostic};
use crate::utils::{Combine, json5_error_to_offset};
use miette::{Diagnostic, SourceSpan};
use std::borrow::Cow;
use std::collections::HashMap;
use subslice_offset::SubsliceOffset;
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct PreprocessorDirective {
    pub keyword_span: SourceSpan,
    pub body_span: SourceSpan,
    pub instruction: PreprocessorDirectiveInstruction,
}

#[derive(Clone, Debug)]
pub enum PreprocessorDirectiveInstruction {
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
        use PreprocessorDirectiveInstruction as PDI;

        let (keyword, body) = line.split_once(' ').unwrap_or((line, ""));
        let body = body.trim();
        let body_base_offset = line.subslice_offset(body).unwrap_or(keyword.len()) + offset;

        macro_rules! construct_result {
            ($instruction:expr) => {
                Some(Self {
                    keyword_span: (offset, keyword.len()).into(),
                    body_span: (body_base_offset, body.len()).into(),
                    instruction: $instruction,
                })
            };
        }
        macro_rules! require_body {
            ($variant:ident, $what:literal) => {{
                if !body.is_empty() {
                    construct_result!(PDI::$variant(body.to_string()))
                } else {
                    record_diagnostic(PreprocessorDiagnostic::MissingArgument {
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
                    construct_result!(PDI::$variant)
                } else {
                    record_diagnostic(PreprocessorDiagnostic::UnexpectedArgument {
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
                    construct_result!(PDI::$variant(body.to_string()))
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
                    Ok(s) => construct_result!(PDI::$variant(s)),
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
            "#define" => construct_result!(PDI::Define(MacroDefine::parse(
                body,
                body_base_offset,
                |diag| record_diagnostic(diag.into()),
            )?)),
            "#undef" => require_var_name!(Undefine, "undef"),
            "#error" => require_string!(Error, "error"),
            "#warning" => require_string!(Warning, "warning"),
            _ => {
                record_diagnostic(PreprocessorDiagnostic::UnknownDirective {
                    directive: keyword.to_string(),
                    at: (offset, keyword.len()),
                });
                None
            }
        }
    }
}

impl PreprocessorDirectiveInstruction {
    pub fn keyword(&self) -> &'static str {
        match self {
            Self::If(_) => "if",
            Self::ElseIf(_) => "elif",
            Self::Else => "else",
            Self::EndIf => "endif",
            Self::IfDefined(_) => "ifdef",
            Self::IfNotDefined(_) => "ifndef",
            Self::ElseIfDefined(_) => "elifdef",
            Self::ElseIfNotDefined(_) => "elifndef",
            Self::Define(_) => "define",
            Self::Undefine(_) => "undef",
            Self::Error(_) => "error",
            Self::Warning(_) => "warning",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreprocessorState {
    defines: HashMap<Cow<'static, str>, MacroDefine>,
    activity: Vec<ActivityState>,
}

impl PreprocessorState {
    pub fn new() -> Self {
        Self {
            defines: HashMap::new(),
            activity: vec![ActivityState::Root],
        }
    }

    pub fn active(&self) -> bool {
        self.activity.last().unwrap().active()
    }

    pub fn preprocess_line<'a>(
        &self,
        s: Cow<'a, str>,
        offset: usize,
        mut record_diagnostic: impl FnMut(PreprocessorDiagnostic),
    ) -> (Cow<'a, str>, Vec<usize>) {
        MacroDefine::expand_all_in(
            s,
            offset,
            |name| self.defines.get(name),
            |diag| record_diagnostic(diag.into()),
        )
    }

    pub fn exec(
        &mut self,
        directive: PreprocessorDirective,
        mut record_diagnostic: impl FnMut(PreprocessorDiagnostic),
    ) {
        use PreprocessorDirectiveInstruction as PDI;
        let is_if_elif = matches!(&directive.instruction, PDI::If(_) | PDI::ElseIf(_));
        let is_ifdef_elifdef = matches!(
            &directive.instruction,
            PDI::IfDefined(_) | PDI::ElseIfDefined(_),
        );
        let is_if_ifdef_ifndef = matches!(
            &directive.instruction,
            PDI::If(_) | PDI::IfDefined(_) | PDI::IfNotDefined(_),
        );
        let new_block = OpenBlock {
            keyword: directive.instruction.keyword(),
            keyword_span: directive.keyword_span,
        };
        match directive.instruction {
            PDI::If(condition)
            | PDI::ElseIf(condition)
            | PDI::IfDefined(condition)
            | PDI::IfNotDefined(condition)
            | PDI::ElseIfDefined(condition)
            | PDI::ElseIfNotDefined(condition) => {
                let macro_defined = |name: &str| self.defines.contains_key(&Cow::Borrowed(name));
                let resolve = || {
                    if is_if_elif {
                        let (expanded, expanded_offsets) = self.preprocess_line(
                            Cow::Owned(condition),
                            directive.body_span.offset(),
                            &mut record_diagnostic,
                        );
                        match evaluate(&expanded, &expanded_offsets, macro_defined) {
                            Ok(value) => value != 0,
                            Err(err) => {
                                record_diagnostic(PreprocessorDiagnostic::Expr(err));
                                false
                            }
                        }
                    } else if is_ifdef_elifdef {
                        macro_defined(&condition)
                    } else {
                        !macro_defined(&condition)
                    }
                };
                if is_if_ifdef_ifndef {
                    if !self.active() {
                        self.activity
                            .push(ActivityState::InactivePendingEndif(new_block));
                    } else if resolve() {
                        self.activity.push(ActivityState::ResolvedTrue(new_block));
                    } else {
                        self.activity.push(ActivityState::ResolvedFalse(new_block));
                    }
                } else {
                    let new_activity = match self.activity.last().unwrap() {
                        ActivityState::Root => {
                            record_diagnostic(PreprocessorDiagnostic::MissingIf {
                                keyword: new_block.keyword,
                                at: directive.keyword_span,
                            });
                            ActivityState::Root
                        }
                        ActivityState::ResolvedTrue(_) => {
                            ActivityState::InactivePendingEndif(new_block)
                        }
                        ActivityState::ResolvedFalse(_) => {
                            if resolve() {
                                ActivityState::ResolvedTrue(new_block)
                            } else {
                                ActivityState::ResolvedFalse(new_block)
                            }
                        }
                        ActivityState::InactivePendingEndif(_) => {
                            ActivityState::InactivePendingEndif(new_block)
                        }
                    };
                    *self.activity.last_mut().unwrap() = new_activity;
                }
            }
            PDI::Else => match self.activity.last_mut().unwrap() {
                ActivityState::Root => record_diagnostic(PreprocessorDiagnostic::MissingIf {
                    keyword: "else",
                    at: directive.keyword_span,
                }),
                x @ ActivityState::ResolvedTrue(_) => {
                    *x = ActivityState::InactivePendingEndif(new_block)
                }
                x @ ActivityState::ResolvedFalse(_) => *x = ActivityState::ResolvedTrue(new_block),
                x @ ActivityState::InactivePendingEndif(_) => {
                    *x = ActivityState::InactivePendingEndif(new_block)
                }
            },
            PDI::EndIf => {
                if self.try_pop().is_none() {
                    record_diagnostic(PreprocessorDiagnostic::MissingIf {
                        keyword: "endif",
                        at: directive.keyword_span,
                    });
                }
            }
            PDI::Define(define) => {
                let new_define = define.declaration_range;
                if let Some(old) = self.defines.insert(Cow::Owned(define.name.clone()), define) {
                    record_diagnostic(PreprocessorDiagnostic::DuplicateDefine {
                        name: old.name,
                        at: directive.keyword_span.combine(new_define),
                        original: old.declaration_range.into(),
                    });
                }
            }
            PDI::Undefine(name) => {
                self.defines.remove(&Cow::Owned(name));
            }
            PDI::Warning(warning) => record_diagnostic(PreprocessorDiagnostic::UserWarning {
                message: warning,
                at: directive.body_span,
            }),
            PDI::Error(error) => record_diagnostic(PreprocessorDiagnostic::UserError {
                message: error,
                at: directive.body_span,
            }),
        }
    }

    /// Resets the preprocessor to a fresh state, emitting any preprocessor::unterminated errors on
    /// the way.
    pub fn end(&mut self, mut record_diagnostic: impl FnMut(PreprocessorDiagnostic)) {
        while let Some(unclosed) = self.try_pop() {
            let block = match unclosed {
                ActivityState::Root => unreachable!(),
                ActivityState::ResolvedTrue(block) => block,
                ActivityState::ResolvedFalse(block) => block,
                ActivityState::InactivePendingEndif(block) => block,
            };
            record_diagnostic(PreprocessorDiagnostic::Unterminated {
                keyword: block.keyword,
                at: block.keyword_span,
            })
        }
        self.defines.clear();
    }

    fn try_pop(&mut self) -> Option<ActivityState> {
        self.activity.pop_if(|x| *x != ActivityState::Root)
    }
}

#[derive(Debug, Clone, PartialEq, Diagnostic, Error)]
pub enum PreprocessorDiagnostic {
    #[error("{message}")]
    #[diagnostic(code(preprocessor::user_warning), severity(warn))]
    UserWarning { message: String, at: SourceSpan },

    #[error("#{keyword} missing argument")]
    #[diagnostic(code(preprocessor::missing_argument))]
    MissingArgument {
        keyword: &'static str,

        #[label("Expected argument")]
        at: usize,
    },

    #[error("#{keyword} takes no body")]
    #[diagnostic(code(preprocessor::unexpected_argument))]
    UnexpectedArgument {
        keyword: &'static str,

        #[label("Should not be present")]
        at: (usize, usize),
    },

    #[error("#{keyword} expects a single var name")]
    #[diagnostic(code(preprocessor::invalid_var_name))]
    InvalidVarName {
        keyword: &'static str,

        #[label("Invalid variable name")]
        at: (usize, usize),
    },

    #[error("#{keyword} expects a single string")]
    #[diagnostic(code(preprocessor::invalid_string))]
    InvalidString {
        keyword: &'static str,

        #[source]
        cause: json5::Error,
        #[label("{cause}")]
        at: usize,
    },

    #[error("Unknown directive {directive}")]
    #[diagnostic(code(preprocessor::unknown_directive))]
    UnknownDirective {
        directive: String,

        #[label("Unknown directive")]
        at: (usize, usize),
    },

    #[error("#{keyword} without an #if")]
    #[diagnostic(code(preprocessor::missing_if))]
    MissingIf {
        keyword: &'static str,

        #[label("Unexpected #{keyword}")]
        at: SourceSpan,
    },

    #[error("Duplicate #define {name}")]
    #[diagnostic(code(preprocessor::duplicate_define))]
    DuplicateDefine {
        name: String,

        #[label("Duplicate define")]
        at: SourceSpan,
        #[label("Original defined here")]
        original: SourceSpan,
    },

    #[error("{message}")]
    #[diagnostic(code(preprocessor::user_error))]
    UserError { message: String, at: SourceSpan },

    #[error("Missing #endif")]
    #[diagnostic(code(preprocessor::unterminated))]
    Unterminated {
        keyword: &'static str,

        #[label("Unterminated #{keyword}")]
        at: SourceSpan,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Macro(#[from] MacroDiagnostic),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Expr(#[from] ExprDiagnostic),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ActivityState {
    Root,
    ResolvedTrue(OpenBlock),
    ResolvedFalse(OpenBlock),
    InactivePendingEndif(OpenBlock),
}

impl ActivityState {
    fn active(self) -> bool {
        matches!(self, Self::Root | Self::ResolvedTrue(_))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct OpenBlock {
    keyword: &'static str,
    keyword_span: SourceSpan,
}

#[cfg(test)]
mod test {
    use crate::preprocessor::{PreprocessorDiagnostic, PreprocessorDirective, PreprocessorState};
    use pretty_assertions::{assert_eq, assert_str_eq};
    use std::borrow::Cow;
    use str_block::str_block;
    use subslice_offset::SubsliceOffset;

    fn test_code(code: &str, expected_code: &str, expected_diags: &[PreprocessorDiagnostic]) {
        let mut diags = vec![];
        let mut record_diagnostic = |diag| diags.push(diag);
        let mut result = String::new();
        let mut preprocessor = PreprocessorState::new();
        for line in code.lines() {
            let offset = code.subslice_offset(line).unwrap();
            if line.starts_with('#') {
                let Some(directive) =
                    PreprocessorDirective::parse(line, offset, &mut record_diagnostic)
                else {
                    continue;
                };
                preprocessor.exec(directive, &mut record_diagnostic);
            } else if preprocessor.active() {
                result.push_str(
                    &preprocessor
                        .preprocess_line(Cow::Borrowed(line), offset, &mut record_diagnostic)
                        .0,
                );
                result.push('\n');
            }
        }
        preprocessor.end(record_diagnostic);

        assert_eq!(diags, expected_diags);
        assert_str_eq!(result, expected_code);
    }

    #[test]
    fn test_basic() {
        test_code("hello\n", "hello\n", &[]);

        test_code(
            str_block! {"
                #define X 5
                X
            "},
            "5\n",
            &[],
        );

        test_code(
            str_block! {"
                #ifdef NONEXISTENT
                hello
                #endif
            "},
            "",
            &[],
        );

        test_code(
            str_block! {"
                #if 1
                hello
            "},
            "hello\n",
            &[PreprocessorDiagnostic::Unterminated {
                keyword: "if",
                at: (0, 3).into(),
            }],
        );

        test_code(
            str_block! {"
                #if 0
                hello
            "},
            "",
            &[PreprocessorDiagnostic::Unterminated {
                keyword: "if",
                at: (0, 3).into(),
            }],
        );

        test_code(
            str_block! {"
                #define DO_MATH(a, b, c) a + b > c
                #if DO_MATH(5, 10, 14)
                math_success
                #endif
            "},
            "math_success\n",
            &[],
        );

        test_code(
            "#endif\n",
            "",
            &[PreprocessorDiagnostic::MissingIf {
                keyword: "endif",
                at: (0, 6).into(),
            }],
        );
    }

    #[test]
    fn test_if_elif_else() {
        test_code(
            str_block! {"
                #define HELLO
                #ifdef HELLO
                one
                #elif WORLD
                two
                #else
                three
                #endif
            "},
            "one\n",
            &[],
        );
        test_code(
            str_block! {"
                #define WORLD
                #ifdef HELLO
                one
                #elif WORLD
                two
                #else
                three
                #endif
            "},
            "three\n",
            &[],
        );
        test_code(
            str_block! {"
                #define WORLD 5
                #ifdef HELLO
                one
                #elif WORLD
                two
                #else
                three
                #endif
            "},
            "two\n",
            &[],
        );
        test_code(
            str_block! {"
                #ifdef HELLO
                one
                #elif WORLD
                two
                #else
                three
                #endif
            "},
            "three\n",
            &[],
        );
        test_code(
            str_block! {"
                #define WORLD 5
                #ifdef HELLO
                one
                #elif WORLD
                two
                #else
                three
            "},
            "two\n",
            &[PreprocessorDiagnostic::Unterminated {
                keyword: "else",
                at: (49, 5).into(),
            }],
        );
    }
}
