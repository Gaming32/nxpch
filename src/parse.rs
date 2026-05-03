use crate::option::{BuildId, NxpchOption, OutputFormat};
use crate::pre_parse::{PreParsedStatement, PreParsedStatementContent};
use crate::preprocessor::{MacroDefine, PreprocessorDiagnostic, PreprocessorState};
use crate::utils::{AsNum, closest_key, order_diags_by_labels};
use arcstr::ArcStr;
use clap::ValueEnum;
use itertools::Either;
use miette::{Diagnostic, SourceOffset, SourceSpan};
use num_traits::{Num, Signed, Unsigned};
use ordered_float::OrderedFloat;
use rayon::prelude::{IntoParallelIterator, ParallelIterator};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, LinkedList};
use std::num::{IntErrorKind, ParseIntError};
use std::sync::{Arc, Mutex};
use std::{mem, vec};
use subslice_offset::SubsliceOffset;
use thiserror::Error;

pub type SettingsVec = Arc<Vec<ArcStr>>;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ParsingResult {
    pub build_target: BuildTarget,
    pub mod_name: Option<ArcStr>,
    pub mod_version: ArcStr,
    pub target_build: BuildId,
    pub forced_output_format: Option<OutputFormat>,
    pub user_settings: SettingsVec,
    pub code: Arc<Vec<ParsedCode>>,
    pub labels: Vec<(ArcStr, CodeAddress)>,
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
    mut record_diagnostic: impl FnMut(ParseDiagnostic) + Send,
) -> Vec<ParsingResult>
where
    I: IntoIterator<Item = PreParsedStatement>,
    I::IntoIter: Clone + Send,
    Defines: IntoIterator<Item = MacroDefine>,
    Targets: IntoIterator<Item = BuildTarget>,
{
    let mut state = ParseState::new(statements.into_iter(), &forced);

    {
        let start_state = &mut state.active_states[0];
        for define in initial_defines {
            start_state
                .preprocessor
                .define(Arc::new(define), diag(&mut record_diagnostic));
        }

        let mut new_start_states = vec![];
        start_state.make_forks(&mut new_start_states, build_targets, |target, fork| {
            fork.build_target = target;
            fork.preprocessor.define(
                Arc::new(MacroDefine::create_const(
                    match target {
                        BuildTarget::Emulator => "EMULATOR".into(),
                        BuildTarget::Hardware => "HARDWARE".into(),
                    },
                    "1".into(),
                )),
                diag(&mut record_diagnostic),
            );
        });
        state.active_states.extend(new_start_states);
    }

    while state.step(&mut record_diagnostic) {}
    state.finished_results
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ParsedCode {
    pub line_span: SourceSpan,
    pub address: CodeAddress,
    pub instruction: ParsedCodeInstruction,
    pub instruction_span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParsedCodeInstruction {
    Byte(u8),
    Short(u16),
    Int(u32),
    Long(u64),
    Float(OrderedFloat<f32>),
    Double(OrderedFloat<f64>),
    String(ArcStr),
    Asm(ArcStr),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CodeAddress(u32);

impl CodeAddress {
    pub const NSO_HEADER_LEN: u32 = 0x100;
    pub const MAX_ALLOWED_ADDR: u32 = u32::MAX - Self::NSO_HEADER_LEN;

    pub fn new(addr: u32) -> Option<Self> {
        if addr <= Self::MAX_ALLOWED_ADDR {
            Some(Self(addr))
        } else {
            None
        }
    }

    pub fn code_address(self) -> u32 {
        self.0
    }

    pub fn nso_address(self) -> u32 {
        self.0 + Self::NSO_HEADER_LEN
    }

    pub fn increment(self, other: u32) -> Option<Self> {
        self.0.checked_add(other).and_then(Self::new)
    }

    pub fn remaining_bytes(self) -> u32 {
        Self::MAX_ALLOWED_ADDR - self.0 + 1
    }
}

#[derive(Clone)]
struct ParseState<'forced, I> {
    active_states: Vec<ParseSubState<'forced, I>>,
    finished_results: Vec<ParsingResult>,
}

impl<'forced, I> ParseState<'forced, I>
where
    I: Iterator<Item = PreParsedStatement> + Clone + Send,
{
    fn new(statements: I, filters: &'forced ForcedBuildOption) -> Self {
        Self {
            active_states: vec![ParseSubState::new(statements, filters)],
            finished_results: vec![],
        }
    }

    fn step(&mut self, record_diagnostic: impl FnMut(ParseDiagnostic) + Send) -> bool {
        let record_diagnostic = Mutex::new(record_diagnostic);
        let (new_active_states, new_finished_results): (LinkedList<_>, LinkedList<_>) =
            mem::take(&mut self.active_states)
                .into_par_iter()
                .partition_map(|state| state.step(&mut *record_diagnostic.lock().unwrap()));
        for new_states in new_active_states {
            self.active_states.extend(new_states);
        }
        self.finished_results
            .extend(new_finished_results.into_iter().flatten());
        !self.active_states.is_empty()
    }
}

#[derive(Clone)]
struct ParseSubState<'forced, I> {
    statements: I,
    preprocessor: PreprocessorState,
    forced: &'forced ForcedBuildOption,

    build_target: BuildTarget,
    mod_name: Option<ArcStr>,
    mod_version: Option<ArcStr>,
    target_build: Option<(BuildId, SourceSpan)>,
    user_settings: SettingsVec,
    forced_output_format: Option<(OutputFormat, SourceSpan)>,

    code_output: Arc<Vec<ParsedCode>>,
    code_multi: Option<CodeAddress>,
    code_multi_ended: Option<SourceOffset>,
    code_labels: Arc<HashMap<ArcStr, (SourceSpan, CodeAddress)>>,
}

impl<'forced, I> ParseSubState<'forced, I>
where
    I: Iterator<Item = PreParsedStatement> + Clone,
{
    fn new(statements: I, filters: &'forced ForcedBuildOption) -> Self {
        Self {
            statements,
            forced: filters,
            preprocessor: PreprocessorState::default(),
            build_target: BuildTarget::Emulator,
            mod_name: None,
            mod_version: None,
            target_build: None,
            user_settings: Arc::new(vec![]),
            forced_output_format: None,
            code_output: Arc::new(vec![]),
            code_multi: None,
            code_multi_ended: None,
            code_labels: Arc::new(HashMap::new()),
        }
    }

    fn step(
        mut self,
        mut record_diagnostic: impl FnMut(ParseDiagnostic),
    ) -> Either<Vec<Self>, Option<ParsingResult>> {
        let mut new_states = vec![];
        while self.step_once(&mut new_states, &mut record_diagnostic) {
            if !new_states.is_empty() {
                new_states.push(self);
                return Either::Left(new_states);
            }
        }
        assert!(new_states.is_empty());
        Either::Right(self.finish())
    }

    fn finish(self) -> Option<ParsingResult> {
        Some(ParsingResult {
            build_target: self.build_target,
            mod_name: self.mod_name,
            mod_version: self.mod_version.unwrap_or(FALLBACK_MOD_VERSION),
            target_build: self.target_build.map(|(bid, _)| bid)?,
            user_settings: self.user_settings,
            forced_output_format: self.forced_output_format.map(|(fmt, _)| fmt),
            code: self.code_output,
            labels: self
                .code_labels
                .iter()
                .map(|(name, &(_, address))| (name.clone(), address))
                .collect(),
        })
    }

    fn step_once(
        &mut self,
        new_states: &mut Vec<Self>,
        mut record_diagnostic: impl FnMut(ParseDiagnostic),
    ) -> bool {
        let Some(statement) = self.statements.next() else {
            mem::take(&mut self.preprocessor).end(diag(&mut record_diagnostic));
            if !self.code_output.is_empty() || !self.code_labels.is_empty() {
                if self.mod_version.is_none() {
                    record_diagnostic(ParseDiagnostic::MissingModVersion);
                }
                if self.target_build.is_none() {
                    record_diagnostic(ParseDiagnostic::MissingBuildId);
                }
            }
            return false;
        };
        match statement.content {
            PreParsedStatementContent::Option(option, name_span) => {
                if !self.preprocessor.active() {
                    return true;
                }
                match &*option {
                    NxpchOption::ModName(option) => self.mod_name = Some(option.0.clone()),
                    NxpchOption::ModVersion(option) => self.mod_version = Some(option.0.clone()),
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
                                return self.early_exit();
                            }
                            self.target_build = Some((option.0, name_span));
                        }
                    },
                    NxpchOption::TargetBuilds(option) => {
                        if let Some((_, original_span)) = self.target_build {
                            record_diagnostic(ParseDiagnostic::DuplicateBuildId {
                                at: name_span,
                                original: original_span,
                            });
                            return true;
                        }
                        let forced_bid = self.forced.build_id;
                        return self.make_forks(
                            new_states,
                            option
                                .0
                                .iter()
                                .filter(|opt| forced_bid.is_none_or(|forced| opt.id == forced)),
                            |entry, fork| {
                                fork.target_build = Some((entry.id, name_span));
                                for define in &entry.defines {
                                    fork.preprocessor
                                        .define(define.clone(), diag(&mut record_diagnostic));
                                }
                            },
                        );
                    }
                    NxpchOption::UserSettings(option) => {
                        let mut option = Cow::Borrowed(&option.0);
                        if let Some(forced) = self.forced.options.get(self.user_settings.len()..)
                            && !forced.is_empty()
                        {
                            for (forced_value, settings) in forced.iter().zip(option.to_mut()) {
                                settings.retain(|setting| &*setting.name == forced_value);
                            }
                        }
                        return self.make_deep_forks(new_states, &*option, |setting, fork| {
                            Arc::make_mut(&mut fork.user_settings).push(setting.name.clone());
                            for define in &setting.defines {
                                fork.preprocessor
                                    .define(define.clone(), diag(&mut record_diagnostic));
                            }
                        });
                    }
                    NxpchOption::OutputFormat(option) => match self.forced_output_format {
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
            PreParsedStatementContent::Preprocessor(directive) => {
                self.preprocessor.exec(directive, diag(record_diagnostic))
            }
            PreParsedStatementContent::Code(code, code_span) => {
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
                    let address = if is_origin { value } else { target };
                    let address_span = (
                        code_offsets[code.subslice_offset(address).unwrap()],
                        address.len(),
                    )
                        .into();
                    let offset = match parse_int::parse::<u32>(address) {
                        Ok(address) => match CodeAddress::new(address) {
                            Some(address) => address,
                            None => {
                                record_diagnostic(ParseDiagnostic::UnsupportedCodeAddress {
                                    at: address_span,
                                });
                                return true;
                            }
                        },
                        Err(err) => {
                            record_diagnostic(ParseDiagnostic::InvalidCodeAddress {
                                cause: err.to_string(),
                                at: address_span,
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
                            statement.line_span,
                            record_diagnostic,
                        );
                    }
                } else if let Some(offset) = self.code_multi {
                    self.code_multi = self.parse_code(
                        code_span.offset(),
                        offset,
                        &code,
                        statement.line_span,
                        record_diagnostic,
                    );
                    if self.code_multi.is_none() {
                        self.code_multi_ended = Some(statement.line_span.offset().into());
                    }
                } else {
                    record_diagnostic(ParseDiagnostic::NoOriginCode {
                        at: code_span,
                        cleared_from: self.code_multi_ended,
                    });
                }
            }
        }
        true
    }

    fn early_exit(&mut self) -> bool {
        self.target_build = None;
        false
    }

    fn make_forks<T>(
        &mut self,
        new_states: &mut Vec<Self>,
        values: impl IntoIterator<Item = T>,
        mut define_fork: impl FnMut(T, &mut Self),
    ) -> bool {
        let mut values = values.into_iter();
        let Some(first) = values.next() else {
            return self.early_exit();
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
        new_states: &mut Vec<Self>,
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
                    return self.early_exit();
                }
            }
            if !self.make_forks(&mut additional_new_states, sub_values, &mut define_fork) {
                new_states.truncate(first_state_idx);
                return self.early_exit();
            }
            new_states.append(&mut additional_new_states);
        }
        true
    }

    fn end_multi_code(&mut self, line_start: usize) {
        if self.code_multi.is_some() {
            self.code_multi_ended = Some(line_start.into());
        }
        self.code_multi = None;
    }

    fn parse_code(
        &mut self,
        source_offset: usize,
        address: CodeAddress,
        code: &str,
        line_span: SourceSpan,
        mut record_diagnostic: impl FnMut(ParseDiagnostic),
    ) -> Option<CodeAddress> {
        if let Some(label_name) = code.strip_suffix(':') {
            let span = (source_offset, label_name.len()).into();
            if MacroDefine::NAME_REGEX.test(label_name) {
                match Arc::make_mut(&mut self.code_labels).entry(label_name.into()) {
                    Entry::Occupied(entry) => {
                        record_diagnostic(ParseDiagnostic::DuplicateLabels {
                            at: span,
                            original: entry.get().0,
                        });
                    }
                    Entry::Vacant(entry) => {
                        entry.insert((span, address));
                    }
                }
            } else {
                record_diagnostic(ParseDiagnostic::InvalidLabel { at: span });
            }
            return Some(address);
        }
        let instruction_span = (source_offset, code.len()).into();
        let (parsed_value, value_width) = if let Some(directive) = code.strip_prefix('.') {
            let Some((directive, value)) = directive.split_once(' ') else {
                record_diagnostic(ParseDiagnostic::MissingDataValue {
                    at: (source_offset + directive.len()).into(),
                });
                return Some(address);
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
                        ParsedCodeInstruction::$variant(
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
                        ParsedCodeInstruction::$variant(match parse_int::parse(value) {
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
            match directive {
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
                    (ParsedCodeInstruction::String(parsed.into()), length as u32)
                }
                _ => {
                    record_diagnostic(ParseDiagnostic::UnknownDataDirective {
                        closest: closest_key(
                            directive,
                            ["byte", "short", "int", "long", "float", "double", "string"],
                        ),
                        at: (source_offset, directive.len()).into(),
                    });
                    return Some(address);
                }
            }
        } else {
            (ParsedCodeInstruction::Asm(code.into()), 4)
        };
        if value_width > address.remaining_bytes() {
            record_diagnostic(ParseDiagnostic::CodeOverflow {
                bytes: value_width,
                bytes_available: address.remaining_bytes(),
                at: instruction_span,
            });
            return None;
        }
        Arc::make_mut(&mut self.code_output).push(ParsedCode {
            address,
            instruction: parsed_value,
            line_span,
            instruction_span,
        });
        address.increment(value_width)
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

const FALLBACK_MOD_VERSION: ArcStr = arcstr::literal!("0.1.0");

#[derive(Debug, PartialEq, Eq, Hash, Diagnostic, Error)]
pub enum ParseDiagnostic {
    #[error("Mod version was never specified, falling back to \"{FALLBACK_MOD_VERSION}\".")]
    #[diagnostic(code(parse::missing_mod_version), severity(warn))]
    MissingModVersion,

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

    #[error(
        "Addresses exceeding 0x{:X} are unsupported due to Atmosphère limitations",
        CodeAddress::MAX_ALLOWED_ADDR
    )]
    #[diagnostic(code(parse::unsupported_code_address))]
    UnsupportedCodeAddress {
        #[label("Unsupported address")]
        at: SourceSpan,
    },

    #[error("Invalid code address")]
    #[diagnostic(code(parse::invalid_code_address))]
    InvalidCodeAddress {
        cause: String,

        #[label("{cause}")]
        at: SourceSpan,
    },

    #[error("Code is too big to fit in remaining patch space")]
    #[diagnostic(code(parse::code_overflow))]
    CodeOverflow {
        bytes: u32,
        bytes_available: u32,

        #[label("Code takes {bytes} bytes, but only {bytes_available} are available to fill")]
        at: SourceSpan,
    },

    #[error("Code without = or .origin")]
    #[diagnostic(
        code(parse::no_origin_code),
        help(
            "The .origin may have been cleared by = code or by reaching the end of the allowed address space."
        )
    )]
    NoOriginCode {
        #[label(
            "Code must either be specified like \"0xDEADBEEF = code\", or it must have a .origin preceding it"
        )]
        at: SourceSpan,
        #[label(".origin was cleared due to this line")]
        cleared_from: Option<SourceOffset>,
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

impl Ord for ParseDiagnostic {
    fn cmp(&self, other: &Self) -> Ordering {
        order_diags_by_labels(self, other)
    }
}

impl PartialOrd for ParseDiagnostic {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn diag<D>(mut recorder: impl FnMut(ParseDiagnostic)) -> impl FnMut(D)
where
    D: Into<ParseDiagnostic>,
{
    move |diag| recorder(diag.into())
}
