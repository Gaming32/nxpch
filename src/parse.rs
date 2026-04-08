use crate::option::{BuildId, NxpchOption, OutputFormat};
use crate::pre_parse::PreParsedStatement;
use crate::preprocessor::{MacroDefine, PreprocessorDiagnostic, PreprocessorState};
use crate::utils::{AsNum, closest_key};
use clap::ValueEnum;
use miette::{Diagnostic, SourceOffset, SourceSpan};
use num_traits::{Num, Signed, Unsigned};
use ordered_float::OrderedFloat;
use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::num::{IntErrorKind, ParseIntError};
use std::sync::Arc;
use std::vec;
use subslice_offset::SubsliceOffset;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ParsingResult {
    pub build_target: BuildTarget,
    pub target_build: BuildId,
    pub forced_output_format: Option<OutputFormat>,
    pub user_settings: Vec<Arc<str>>,
    pub code: Vec<(u32, ParsedCode)>,
    pub labels: Vec<(String, u32)>,
}

#[derive(Clone, Debug, Default)]
pub struct ForcedBuildOption {
    pub build_id: Option<BuildId>,
    pub options: Vec<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum BuildTarget {
    Emulator,
    Hardware,
}

pub fn parse_statements<I, Defines, Targets>(
    statements: I,
    initial_defines: Defines,
    build_targets: Targets,
    forced: ForcedBuildOption,
    mut record_diagnostic: impl FnMut(ParseDiagnostic),
) -> BTreeSet<ParsingResult>
where
    I: IntoIterator<Item = PreParsedStatement>,
    I::IntoIter: Clone,
    Defines: IntoIterator<Item = MacroDefine>,
    Targets: IntoIterator<Item = BuildTarget>,
{
    let mut state = ParseState::new(statements.into_iter(), &forced);

    {
        let start_state = &mut state.active_states[0];
        for define in initial_defines {
            start_state
                .preprocessor
                .define(define, diag(&mut record_diagnostic));
        }

        let mut new_start_states = vec![];
        start_state.make_forks(&mut new_start_states, build_targets, |target, fork| {
            fork.build_target = target;
            fork.preprocessor.define(
                MacroDefine::create_const(
                    match target {
                        BuildTarget::Emulator => "EMULATOR".into(),
                        BuildTarget::Hardware => "HARDWARE".into(),
                    },
                    "1".into(),
                ),
                diag(&mut record_diagnostic),
            );
        });
        state.active_states.extend(new_start_states);
    }

    while state.step(&mut record_diagnostic) {}
    state.finished_results
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParsedCode {
    Byte(u8),
    Short(u16),
    Int(u32),
    Long(u64),
    Float(OrderedFloat<f32>),
    Double(OrderedFloat<f64>),
    String(String),
    Asm(String, u64, SourceSpan),
}

#[derive(Clone)]
struct ParseState<'forced, I> {
    active_states: Vec<ParseSubState<'forced, I>>,
    new_states: Vec<ParseSubState<'forced, I>>,
    finished_results: BTreeSet<ParsingResult>,
}

impl<'forced, I> ParseState<'forced, I>
where
    I: Iterator<Item = PreParsedStatement> + Clone,
{
    fn new(statements: I, filters: &'forced ForcedBuildOption) -> Self {
        Self {
            active_states: vec![ParseSubState::new(statements, filters)],
            new_states: vec![],
            finished_results: BTreeSet::new(),
        }
    }

    fn step(&mut self, mut record_diagnostic: impl FnMut(ParseDiagnostic)) -> bool {
        self.finished_results.extend(
            self.active_states
                .extract_if(.., |state| {
                    !state.step(&mut self.new_states, &mut record_diagnostic)
                })
                .filter_map(|state| {
                    Some(ParsingResult {
                        build_target: state.build_target,
                        target_build: state.target_build.map(|(bid, _)| bid)?,
                        user_settings: state.user_settings,
                        forced_output_format: state.forced_output_format.map(|(fmt, _)| fmt),
                        code: state.code_output,
                        labels: state
                            .code_labels
                            .into_iter()
                            .map(|(name, (_, address))| (name, address))
                            .collect(),
                    })
                }),
        );
        self.active_states.append(&mut self.new_states);
        !self.active_states.is_empty()
    }
}

#[derive(Clone)]
struct ParseSubState<'forced, I> {
    statements: I,
    preprocessor: PreprocessorState,
    forced: &'forced ForcedBuildOption,

    build_target: BuildTarget,
    target_build: Option<(BuildId, SourceSpan)>,
    pointer_offset: i32,
    user_settings: Vec<Arc<str>>,
    forced_output_format: Option<(OutputFormat, SourceSpan)>,

    code_output: Vec<(u32, ParsedCode)>,
    code_multi: Option<u32>,
    code_multi_ended: Option<SourceOffset>,
    code_labels: HashMap<String, (SourceSpan, u32)>,
}

impl<'forced, I> ParseSubState<'forced, I>
where
    I: Iterator<Item = PreParsedStatement> + Clone,
{
    fn new(statements: I, filters: &'forced ForcedBuildOption) -> Self {
        Self {
            statements,
            forced: filters,
            preprocessor: PreprocessorState::new(),
            build_target: BuildTarget::Emulator,
            target_build: None,
            pointer_offset: 0,
            user_settings: vec![],
            forced_output_format: None,
            code_output: vec![],
            code_multi: None,
            code_multi_ended: None,
            code_labels: HashMap::new(),
        }
    }

    fn step(
        &mut self,
        new_states: &mut Vec<ParseSubState<'forced, I>>,
        mut record_diagnostic: impl FnMut(ParseDiagnostic),
    ) -> bool {
        let Some(statement) = self.statements.next() else {
            self.preprocessor.end(diag(&mut record_diagnostic));
            // TODO: Make early exits also trigger this check
            if self.target_build.is_none()
                && (!self.code_output.is_empty() || !self.code_labels.is_empty())
            {
                record_diagnostic(ParseDiagnostic::MissingBuildId);
            }
            return false;
        };
        match statement {
            PreParsedStatement::Option(option, name_span) => {
                if !self.preprocessor.active() {
                    return true;
                }
                match option {
                    NxpchOption::TargetBuild(option) => match self.target_build {
                        Some((_, original_span)) => {
                            record_diagnostic(ParseDiagnostic::DuplicateBuildId {
                                at: name_span,
                                original: original_span,
                            });
                        }
                        None => {
                            if let Some(forced) = self.forced.build_id
                                && option.0 != forced
                            {
                                return false;
                            }
                            self.target_build = Some((option.0, name_span));
                        }
                    },
                    NxpchOption::TargetBuilds(mut option) => {
                        if let Some((_, original_span)) = self.target_build {
                            record_diagnostic(ParseDiagnostic::DuplicateBuildId {
                                at: name_span,
                                original: original_span,
                            });
                            return true;
                        }
                        if let Some(forced) = self.forced.build_id {
                            option.0.retain(|entry| entry.id == forced);
                        }
                        return self.make_forks(new_states, option.0, |entry, fork| {
                            fork.target_build = Some((entry.id, name_span));
                            for define in entry.defines {
                                fork.preprocessor
                                    .define(define, diag(&mut record_diagnostic));
                            }
                        });
                    }
                    NxpchOption::PointerOffset(option) => {
                        self.pointer_offset = option.0;
                        self.end_multi_code(name_span.offset());
                    }
                    NxpchOption::UserSettings(mut option) => {
                        if let Some(forced) = self.forced.options.get(self.user_settings.len()..) {
                            for (forced_value, settings) in forced.iter().zip(&mut option.0) {
                                settings.retain(|setting| &*setting.name == forced_value);
                            }
                        }
                        return self.make_deep_forks(new_states, option.0, |setting, fork| {
                            fork.user_settings.push(setting.name);
                            for define in setting.defines {
                                fork.preprocessor
                                    .define(define, diag(&mut record_diagnostic));
                            }
                        });
                    }
                    NxpchOption::OutputFormat(option) => match self.target_build {
                        Some((_, original_span)) => {
                            record_diagnostic(ParseDiagnostic::DuplicateOutputFormat {
                                at: name_span,
                                original: original_span,
                            });
                        }
                        None => self.forced_output_format = Some((option.0, name_span)),
                    },
                }
            }
            PreParsedStatement::Preprocessor(directive) => {
                self.preprocessor.exec(directive, diag(record_diagnostic))
            }
            PreParsedStatement::Code(code, code_span) => {
                if !self.preprocessor.active() {
                    return true;
                }
                let (code, code_offsets) = self.preprocessor.preprocess_line(
                    Cow::Borrowed(&code),
                    code_span.offset(),
                    diag(&mut record_diagnostic),
                );
                if let Some((target, value)) = code.split_once('=')
                    && !target.contains('"')
                {
                    self.end_multi_code(code_span.offset());
                    let target = target.trim_end();
                    let value = value.trim_start();
                    let is_origin = target == ".origin";
                    let offset = if is_origin { value } else { target };
                    let offset_span = (
                        code_offsets[code.subslice_offset(offset).unwrap()],
                        offset.len(),
                    )
                        .into();
                    let offset = match parse_int::parse::<u32>(offset) {
                        Ok(offset) => offset,
                        Err(err) => {
                            record_diagnostic(ParseDiagnostic::InvalidCodeOffset {
                                cause: err.to_string(),
                                at: offset_span,
                            });
                            return true;
                        }
                    };
                    let offset = match offset.checked_add_signed(self.pointer_offset) {
                        Some(offset) => offset,
                        None => {
                            record_diagnostic(ParseDiagnostic::OverUnderFlow {
                                pointer_offset: self.pointer_offset,
                                at: offset_span,
                            });
                            return true;
                        }
                    };
                    if is_origin {
                        self.code_multi = Some(offset);
                    } else {
                        self.parse_code(
                            code_offsets[code.subslice_offset(value).unwrap()],
                            offset,
                            value,
                            record_diagnostic,
                        );
                    }
                } else if let Some(offset) = self.code_multi {
                    self.code_multi =
                        Some(self.parse_code(code_span.offset(), offset, &code, record_diagnostic));
                } else {
                    record_diagnostic(ParseDiagnostic::NoOriginCode {
                        at: code_span,
                        reset_from: self.code_multi_ended,
                    });
                }
            }
        }
        true
    }

    fn make_forks<T>(
        &mut self,
        new_states: &mut Vec<ParseSubState<'forced, I>>,
        values: impl IntoIterator<Item = T>,
        mut define_fork: impl FnMut(T, &mut Self),
    ) -> bool {
        let mut values = values.into_iter();
        let Some(first) = values.next() else {
            return false;
        };
        for value in values {
            let mut new_this = self.clone();
            define_fork(value, &mut new_this);
            new_states.push(new_this);
        }
        define_fork(first, self);
        true
    }

    fn make_deep_forks<T, TS>(
        &mut self,
        new_states: &mut Vec<ParseSubState<'forced, I>>,
        values: impl IntoIterator<Item = TS>,
        mut define_fork: impl FnMut(T, &mut Self),
    ) -> bool
    where
        TS: IntoIterator<Item = T>,
        TS::IntoIter: Clone,
    {
        let first_state_idx = new_states.len();
        let mut additional_new_states = vec![];
        for value in values {
            let sub_values = value.into_iter();
            for other_state in &mut new_states[first_state_idx..] {
                if !other_state.make_forks(
                    &mut additional_new_states,
                    sub_values.clone(),
                    &mut define_fork,
                ) {
                    new_states.truncate(first_state_idx);
                    return false;
                }
            }
            if !self.make_forks(&mut additional_new_states, sub_values, &mut define_fork) {
                new_states.truncate(first_state_idx);
                return false;
            }
            new_states.append(&mut additional_new_states);
        }
        true
    }

    fn end_multi_code(&mut self, line_start: usize) {
        self.code_multi = None;
        self.code_multi_ended = Some(line_start.into());
    }

    fn parse_code(
        &mut self,
        source_offset: usize,
        offset: u32,
        code: &str,
        mut record_diagnostic: impl FnMut(ParseDiagnostic),
    ) -> u32 {
        if let Some(label_name) = code.strip_suffix(':') {
            let span = (source_offset, label_name.len()).into();
            if MacroDefine::NAME_REGEX.test(label_name) {
                match self.code_labels.entry(label_name.to_string()) {
                    Entry::Occupied(entry) => {
                        record_diagnostic(ParseDiagnostic::DuplicateLabels {
                            at: span,
                            original: entry.get().0,
                        });
                    }
                    Entry::Vacant(entry) => {
                        entry.insert((span, offset));
                    }
                }
            } else {
                record_diagnostic(ParseDiagnostic::InvalidLabel { at: span });
            }
            return offset;
        }
        if let Some(directive) = code.strip_prefix('.') {
            let Some((directive, value)) = directive.split_once(' ') else {
                record_diagnostic(ParseDiagnostic::MissingDataValue {
                    at: (source_offset + directive.len()).into(),
                });
                return offset;
            };
            let directive = directive.trim_end();
            let value = value.trim_start();
            let value_span = (
                source_offset + code.subslice_offset(value).unwrap(),
                value.len(),
            )
                .into();
            macro_rules! parse_int {
                ($variant:ident $unsigned:ident $signed:ident) => {
                    (
                        ParsedCode::$variant(
                            match parse_maybe_signed::<$unsigned, $signed>(value) {
                                Ok(value) => value,
                                Err(err) => {
                                    record_diagnostic(ParseDiagnostic::InvalidInteger {
                                        cause: err.to_string(),
                                        at: value_span,
                                    });
                                    0
                                }
                            },
                        ),
                        $unsigned::BITS / 8,
                    )
                };
            }
            macro_rules! parse_float {
                ($variant:ident $bits:literal) => {
                    (
                        ParsedCode::$variant(match parse_int::parse(value) {
                            Ok(value) => OrderedFloat(value),
                            Err(err) => {
                                record_diagnostic(ParseDiagnostic::InvalidFloat {
                                    cause: err.to_string(),
                                    at: value_span,
                                });
                                OrderedFloat(0.0)
                            }
                        }),
                        $bits / 8,
                    )
                };
            }
            let (parsed_value, value_width) = match directive {
                "byte" => parse_int!(Byte u8 i8),
                "short" => parse_int!(Short u16 i16),
                "int" => parse_int!(Int u32 i32),
                "long" => parse_int!(Long u64 i64),
                "float" => parse_float!(Float 32),
                "double" => parse_float!(Double 64),
                "string" => {
                    let parsed = match json5::from_str::<String>(value) {
                        Ok(value) => value,
                        Err(err) => {
                            record_diagnostic(ParseDiagnostic::InvalidString {
                                at: match err.position() {
                                    Some(pos) => (value_span.offset() + pos.column, 1),
                                    None => (value_span.offset() + value_span.len(), 0),
                                }
                                .into(),
                                cause: err.to_string(),
                            });
                            "".to_string()
                        }
                    };
                    let length = parsed.len();
                    (ParsedCode::String(parsed), length as u32)
                }
                _ => {
                    record_diagnostic(ParseDiagnostic::UnknownDataDirective {
                        closest: closest_key(
                            directive,
                            ["byte", "short", "int", "long", "float", "double", "string"],
                        ),
                        at: (source_offset, directive.len()).into(),
                    });
                    return offset;
                }
            };
            self.code_output.push((offset, parsed_value));
            return offset + value_width;
        }
        self.code_output.push((
            offset,
            ParsedCode::Asm(
                code.to_string(),
                (offset as u64)
                    .checked_sub_signed(self.pointer_offset as i64)
                    .unwrap(),
                (source_offset, code.len()).into(),
            ),
        ));
        offset + 4
    }
}

fn parse_maybe_signed<U, S>(input: &str) -> Result<U, ParseIntError>
where
    U: Num<FromStrRadixErr = ParseIntError> + Unsigned,
    S: Num<FromStrRadixErr = ParseIntError> + Signed + AsNum<U>,
{
    match parse_int::parse::<S>(input) {
        Ok(x) => Ok(x.as_num()),
        Err(err) if *err.kind() == IntErrorKind::PosOverflow => {
            match parse_int::parse::<U>(input) {
                Ok(x) => Ok(x),
                Err(err) => Err(err),
            }
        }
        Err(err) => Err(err),
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Diagnostic, Error)]
pub enum ParseDiagnostic {
    #[error("Build ID specified more than once")]
    #[diagnostic(code(parse::duplicate_build_id))]
    DuplicateBuildId {
        #[label("Duplicate declaration")]
        at: SourceSpan,

        #[label("Original declaration")]
        original: SourceSpan,
    },

    #[error("Forced output format specified more than once")]
    #[diagnostic(code(parse::duplicate_output_format))]
    DuplicateOutputFormat {
        #[label("Duplicate declaration")]
        at: SourceSpan,
        #[label("Original declaration")]
        original: SourceSpan,
    },

    #[error("Build ID was never specified")]
    #[diagnostic(code(parse::missing_build_id))]
    MissingBuildId,

    #[error("Invalid code offset")]
    #[diagnostic(code(parse::invalid_code_offset))]
    InvalidCodeOffset {
        cause: String,

        #[label("{cause}")]
        at: SourceSpan,
    },

    #[error("Over/underflow from adding pointer_offset")]
    #[diagnostic(code(parse::overflow))]
    OverUnderFlow {
        pointer_offset: i32,

        #[label("Current pointer offset is 0x{pointer_offset:X}")]
        at: SourceSpan,
    },

    #[error("Code without = or .origin")]
    #[diagnostic(code(parse::no_origin_code))]
    NoOriginCode {
        #[label(
            "Code must either be specified like \"0xDEADBEEF = code\", or it must have a .origin preceding it"
        )]
        at: SourceSpan,
        #[label(".origin was reset due to this line")]
        reset_from: Option<SourceOffset>,
    },

    #[error("Duplicate label declaration")]
    #[diagnostic(code(parse::duplicate_label))]
    DuplicateLabels {
        #[label("Duplicate label")]
        at: SourceSpan,
        #[label("Original defined here")]
        original: SourceSpan,
    },

    #[error("Label declarations must have a single name")]
    #[diagnostic(code(parse::invalid_label))]
    InvalidLabel {
        #[label("Invalid label name")]
        at: SourceSpan,
    },

    #[error("Data directive missing value")]
    #[diagnostic(code(parse::missing_data_value))]
    MissingDataValue {
        #[label("Expected value")]
        at: SourceOffset,
    },

    #[error("Invalid integer in data directive")]
    #[diagnostic(code(parse::invalid_integer))]
    InvalidInteger {
        cause: String,

        #[label("{cause}")]
        at: SourceSpan,
    },

    #[error("Invalid floating-point number in data directive")]
    #[diagnostic(code(parse::invalid_float))]
    InvalidFloat {
        cause: String,

        #[label("{cause}")]
        at: SourceSpan,
    },

    #[error("Invalid string in data directive")]
    #[diagnostic(code(parse::invalid_float))]
    InvalidString {
        cause: String,

        #[label("{cause}")]
        at: SourceSpan,
    },

    #[error("Unknown data directive")]
    #[diagnostic(code(parse::unknown_data_directive))]
    UnknownDataDirective {
        closest: &'static str,

        #[label("Did you mean {closest}?")]
        at: SourceSpan,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Preprocessor(#[from] PreprocessorDiagnostic),
}

fn diag<D>(mut recorder: impl FnMut(ParseDiagnostic)) -> impl FnMut(D)
where
    D: Into<ParseDiagnostic>,
{
    move |diag| recorder(diag.into())
}
