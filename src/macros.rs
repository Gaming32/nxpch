use miette::Diagnostic;
use serde_with::{DeserializeFromStr, SerializeDisplay};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::{Display, Formatter, Write};
use std::ops::Range;
use std::str::FromStr;
use subslice_offset::SubsliceOffset;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, SerializeDisplay, DeserializeFromStr)]
pub struct MacroDefine {
    pub name: String,
    pub args: Option<Vec<String>>,
    pub expansion: String,
    pub declaration_range: (usize, usize),
    pub expansion_offset: usize,
}

impl MacroDefine {
    pub const NAME_REGEX: ere::Regex = ere::compile_regex!("[a-zA-Z_][a-zA-Z0-9_]*");

    pub fn parse(
        s: &str,
        offset: usize,
        mut record_diagnostic: impl FnMut(MacroDiagnostic),
    ) -> Option<Self> {
        let Some([_, Some(name), args, _, expansion]) = ere::compile_regex!(
            r"^([a-zA-Z_][a-zA-Z0-9_]*)(\([a-zA-Z0-9_,[:blank:]]*\))?([[:blank:]]+(.*))?$"
        )
        .exec(s) else {
            record_diagnostic(MacroDiagnostic::InvalidMacro {
                at: (offset, s.len()),
            });
            return None;
        };
        let declaration_end = {
            let declaration_last = args.unwrap_or(name);
            s.subslice_offset(declaration_last).unwrap() + declaration_last.len()
        };
        let expansion = expansion.unwrap_or(&s[declaration_end..declaration_end]);
        Some(MacroDefine {
            name: name.to_string(),
            args: args.map(|text| {
                if text.len() < 3 {
                    return vec![];
                }
                text[1..text.len() - 1]
                    .split(',')
                    .map(|arg| {
                        let arg = arg.trim();
                        if !ere::compile_regex!("^[a-zA-Z_][a-zA-Z0-9_]*$").test(arg) {
                            record_diagnostic(MacroDiagnostic::InvalidArg {
                                at: (offset + s.subslice_offset(arg).unwrap(), arg.len()),
                            });
                        }
                        arg.to_string()
                    })
                    .collect()
            }),
            expansion: expansion.to_string(),
            declaration_range: (offset, declaration_end),
            expansion_offset: offset + s.subslice_offset(expansion).unwrap(),
        })
    }

    pub fn expand_parsed(
        &self,
        arg_values: Option<&[(&str, usize)]>,
    ) -> Option<(Cow<'_, str>, Vec<usize>)> {
        if arg_values.map(<[_]>::len) != self.args.as_ref().map(Vec::len) {
            return None;
        }
        let mut result = Cow::Borrowed(self.expansion.as_str());
        let mut result_range = Self::len_vec(self.expansion_offset, self.expansion.len());
        if let Some(arg_values) = arg_values {
            let arg_map = self
                .args
                .as_ref()
                .unwrap()
                .iter()
                .map(String::as_str)
                .zip(arg_values)
                .collect::<HashMap<_, _>>();
            let mut index = 0;
            while let Some([Some(found)]) = Self::NAME_REGEX.exec(&result[index..]) {
                let found_index = result.subslice_offset(found).unwrap();
                let found_len = found.len();
                let found_end = found_index + found_len;
                if let Some((new_value, new_value_offset)) = arg_map.get(found) {
                    result
                        .to_mut()
                        .replace_range(found_index..found_end, new_value);
                    result_range.splice(
                        found_index..found_end,
                        Self::len_range(*new_value_offset, new_value.len()),
                    );
                    index = found_index + new_value.len();
                } else {
                    index = found_end;
                }
            }
        }
        Some((result, result_range))
    }

    pub fn expand_all_in<'a>(
        s: &str,
        offset: usize,
        get_macro: impl Fn(&str) -> Option<&'a MacroDefine>,
        mut record_diagnostic: impl FnMut(MacroDiagnostic),
    ) -> Cow<'_, str> {
        let mut result = Cow::Borrowed(s);
        let mut offsets = Self::len_vec(offset, s.len());
        let mut skip_index = 0;
        while let Some((used_macro, call_range, args)) =
            Self::find_first_macro_use(&result, skip_index, offset, &get_macro)
        {
            if let Some((new_value, new_value_range)) = used_macro.expand_parsed(args.as_deref()) {
                result
                    .to_mut()
                    .replace_range(call_range.clone(), &new_value);
                offsets.splice(call_range.clone(), new_value_range);
                // Advance to the start so we expand macros inside a macro
                skip_index = call_range.start;
            } else {
                record_diagnostic(MacroDiagnostic::WrongNumberOfMacroArgs {
                    expected: used_macro.args.as_ref().unwrap().len(),
                    found: args.unwrap().len(),
                    call_site: (offsets[call_range.start], call_range.len()),
                    declaration_site: used_macro.declaration_range,
                });
                skip_index = call_range.end;
            }
        }
        result
    }

    fn find_first_macro_use<'a>(
        s: &str,
        skip_index: usize,
        base_offset: usize,
        get_macro: impl Fn(&str) -> Option<&'a MacroDefine>,
    ) -> Option<(&'a MacroDefine, Range<usize>, Option<Vec<(&str, usize)>>)> {
        let mut current = &s[skip_index..];
        loop {
            let Some([Some(found)]) = Self::NAME_REGEX.exec(current) else {
                return None;
            };
            let start_index = s.subslice_offset(found).unwrap();
            let mut end_index = start_index + found.len();
            let Some(found_macro) = get_macro(found) else {
                current = &s[end_index..];
                continue;
            };
            let mut has_args = false;
            if found_macro.args.is_some() {
                if end_index < s.len() && s.as_bytes()[end_index] == b'(' {
                    has_args = true;
                    end_index += 1;
                    let mut paren_depth = 1usize;
                    while end_index < s.len() && paren_depth > 0 {
                        match s.as_bytes()[end_index] {
                            b'(' => paren_depth += 1,
                            b')' => paren_depth -= 1,
                            _ => {}
                        }
                        end_index += 1;
                    }
                    if paren_depth > 0 {
                        return None;
                    }
                } else {
                    current = &s[end_index..];
                    continue;
                }
            }
            return Some((
                found_macro,
                start_index..end_index,
                has_args.then(|| {
                    let args_start = start_index + found.len() + 1;
                    if end_index - 1 == args_start {
                        return vec![];
                    }
                    s[args_start..end_index - 1]
                        .split(',')
                        .map(|arg| {
                            let arg = arg.trim();
                            (arg, s.subslice_offset(arg).unwrap() + base_offset)
                        })
                        .collect()
                }),
            ));
        }
    }

    fn len_vec(start: usize, len: usize) -> Vec<usize> {
        Self::len_range(start, len).collect()
    }

    #[inline]
    fn len_range(start: usize, len: usize) -> Range<usize> {
        start..start + len
    }
}

impl Display for MacroDefine {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)?;
        if let Some(args) = &self.args {
            f.write_char(')')?;
            if !args.is_empty() {
                f.write_str(&args[0])?;
                for arg in &args[1..] {
                    f.write_fmt(format_args!(", {arg}"))?;
                }
            }
            f.write_char(')')?;
        }
        if !self.expansion.is_empty() {
            f.write_fmt(format_args!(" {}", self.expansion))?;
        }
        Ok(())
    }
}

impl FromStr for MacroDefine {
    type Err = MacroDiagnostic;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut first_error = None;
        let result = Self::parse(s, 0, |e| {
            first_error.get_or_insert(e);
        });
        if let Some(err) = first_error {
            Err(err)
        } else {
            Ok(result.expect("Should return None with no diagnostics"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Diagnostic, Error)]
pub enum MacroDiagnostic {
    #[error("Invalid macro syntax")]
    #[diagnostic(code(macros::invalid))]
    InvalidMacro {
        #[label(r#"Macros should follow the format "MACRO_NAME", "MACRO_NAME expansion", or "MACRO_NAME(arg1, arg2) expansion""#)]
        at: (usize, usize),
    },

    #[error("Invalid macro argument")]
    #[diagnostic(code(macros::invalid_arg))]
    InvalidArg {
        #[label(r#"Should only contain ASCII letters, numbers, and _"#)]
        at: (usize, usize),
    },

    #[error("Wrong number of macro arguments. Expected {expected}, but received {found}.")]
    #[diagnostic(code(macros::invalid_arg))]
    WrongNumberOfMacroArgs {
        expected: usize,
        found: usize,

        #[label("Expected {expected} arguments")]
        call_site: (usize, usize),
        #[label("Macro declared here")]
        declaration_site: (usize, usize),
    },
}

#[cfg(test)]
mod test {
    use crate::macros::{MacroDefine, MacroDiagnostic};
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;

    fn parse_in_place(s: &str) -> Result<MacroDefine, Vec<MacroDiagnostic>> {
        let mut diags = vec![];
        let result = MacroDefine::parse(s, 0, |diag| diags.push(diag));
        if diags.is_empty() {
            Ok(result.unwrap())
        } else {
            Err(diags)
        }
    }

    #[test]
    fn test_parse() {
        assert_eq!(
            parse_in_place("MACRO_NAME"),
            Ok(MacroDefine {
                name: "MACRO_NAME".to_string(),
                args: None,
                expansion: "".to_string(),
                declaration_range: (0, 10),
                expansion_offset: 10,
            }),
        );
        assert_eq!(
            parse_in_place("MACRO_NAME expansion"),
            Ok(MacroDefine {
                name: "MACRO_NAME".to_string(),
                args: None,
                expansion: "expansion".to_string(),
                declaration_range: (0, 10),
                expansion_offset: 11,
            }),
        );
        assert_eq!(
            parse_in_place("MACRO_NAME() another"),
            Ok(MacroDefine {
                name: "MACRO_NAME".to_string(),
                args: Some(vec![]),
                expansion: "another".to_string(),
                declaration_range: (0, 12),
                expansion_offset: 13,
            }),
        );
        assert_eq!(
            parse_in_place("MACRO_NAME(arg1, arg2) expansion"),
            Ok(MacroDefine {
                name: "MACRO_NAME".to_string(),
                args: Some(vec!["arg1".to_string(), "arg2".to_string()]),
                expansion: "expansion".to_string(),
                declaration_range: (0, 22),
                expansion_offset: 23,
            }),
        );
    }

    #[test]
    fn test_parse_failure() {
        assert_eq!(
            parse_in_place("general: invalidity"),
            Err(vec![MacroDiagnostic::InvalidMacro { at: (0, 19) }]),
        );
        assert_eq!(
            parse_in_place("INVALID_ARGUMENT(arg with space,)"),
            Err(vec![
                MacroDiagnostic::InvalidArg { at: (17, 14) },
                MacroDiagnostic::InvalidArg { at: (32, 0) },
            ]),
        );
    }

    #[test]
    fn test_expand_parsed() {
        assert_eq!(
            parse_in_place("MACRO_NAME").unwrap().expand_parsed(None),
            Some(("".into(), vec![])),
        );
        assert_eq!(
            parse_in_place("MACRO_NAME expansion")
                .unwrap()
                .expand_parsed(None),
            Some(("expansion".into(), MacroDefine::len_vec(11, 9))),
        );
        assert_eq!(
            parse_in_place("MACRO_NAME() another")
                .unwrap()
                .expand_parsed(Some(&[])),
            Some(("another".into(), MacroDefine::len_vec(13, 7))),
        );
        assert_eq!(
            parse_in_place("MACRO_NAME(arg1, arg2) (arg1) * (arg2)")
                .unwrap()
                .expand_parsed(Some(&[("5", 100), ("8", 200)])),
            Some((
                "(5) * (8)".into(),
                vec![23, 100, 28, 29, 30, 31, 32, 200, 37]
            )),
        );
        assert_eq!(
            parse_in_place("MACRO_NAME(arg1, arg2) (arg1) * (arg2)")
                .unwrap()
                .expand_parsed(None),
            None,
        );
        assert_eq!(
            parse_in_place("MACRO_NAME(arg1, arg2) (arg2) * (arg1)")
                .unwrap()
                .expand_parsed(Some(&[("arg1", 100), ("arg2", 200)])),
            Some((
                "(arg2) * (arg1)".into(),
                vec![
                    23, 200, 201, 202, 203, 28, 29, 30, 31, 32, 100, 101, 102, 103, 37
                ]
            )),
        );
        assert_eq!(
            parse_in_place("MACRO_NAME")
                .unwrap()
                .expand_parsed(Some(&[])),
            None,
        );
        assert_eq!(
            parse_in_place("MACRO_NAME")
                .unwrap()
                .expand_parsed(Some(&[("arg1", 100)])),
            None,
        );
    }

    #[test]
    fn test_expand_all_in() {
        let macros = HashMap::from([
            ("BLANK_MACRO", parse_in_place("BLANK_MACRO").unwrap()),
            (
                "SIMPLE_MACRO",
                parse_in_place("SIMPLE_MACRO something").unwrap(),
            ),
            (
                "SIMPLE_MACRO_REQUIRED_ARGS",
                parse_in_place("SIMPLE_MACRO_REQUIRED_ARGS() other").unwrap(),
            ),
            (
                "ARG_MACRO",
                parse_in_place("ARG_MACRO(arg) hello & (arg)").unwrap(),
            ),
            (
                "MULTI_ARG_MACRO",
                parse_in_place("MULTI_ARG_MACRO(arg1, arg2) ((arg1) | (arg2))").unwrap(),
            ),
            (
                "RECURSIVE_MACRO",
                parse_in_place(
                    "RECURSIVE_MACRO(arg1, arg2) ARG_MACRO(arg2) * arg1 + arg2 * ARG_MACRO(arg1)",
                )
                .unwrap(),
            ),
            (
                "FANCY_RECURSIVE_MACRO",
                parse_in_place(
                    "RECURSIVE_MACRO(arg1, arg2, target_macro) target_macro(arg1, arg2)",
                )
                .unwrap(),
            ),
            (
                "REFERENCE_MACRO",
                parse_in_place("REFERENCE_MACRO ARG_MACRO").unwrap(),
            ),
        ]);
        let test_expand = move |code| {
            let mut diags = vec![];
            let result = MacroDefine::expand_all_in(
                code,
                100,
                |name| macros.get(name),
                |diag| diags.push(diag),
            );
            (result, diags)
        };

        assert_eq!(test_expand("a + b"), ("a + b".into(), vec![]));
        assert_eq!(test_expand("a + BLANK_MACRO"), ("a + ".into(), vec![]));
        assert_eq!(
            test_expand("a + SIMPLE_MACRO"),
            ("a + something".into(), vec![]),
        );
        assert_eq!(
            test_expand("a + SIMPLE_MACRO + BLANK_MACRO + c + SIMPLE_MACRO"),
            ("a + something +  + c + something".into(), vec![]),
        );
        assert_eq!(
            test_expand("SIMPLE_MACRO()"),
            ("something()".into(), vec![]),
        );
        assert_eq!(
            test_expand("SIMPLE_MACRO_REQUIRED_ARGS()"),
            ("other".into(), vec![]),
        );
        assert_eq!(
            test_expand("SIMPLE_MACRO_REQUIRED_ARGS"),
            ("SIMPLE_MACRO_REQUIRED_ARGS".into(), vec![]),
        );
        assert_eq!(
            test_expand("ARG_MACRO(55)"),
            ("hello & (55)".into(), vec![])
        );
        assert_eq!(
            test_expand("ARG_MACRO(with space)"),
            ("hello & (with space)".into(), vec![]),
        );
        assert_eq!(
            test_expand("1 + ARG_MACRO(55, too many) + 2"),
            (
                "1 + ARG_MACRO(55, too many) + 2".into(),
                vec![MacroDiagnostic::WrongNumberOfMacroArgs {
                    expected: 1,
                    found: 2,
                    call_site: (104, 23),
                    declaration_site: (0, 14),
                }]
            ),
        );
        assert_eq!(
            test_expand("SIMPLE_MACRO(no arg)"),
            ("something(no arg)".into(), vec![]),
        );
        assert_eq!(test_expand("something()"), ("something()".into(), vec![]));
        assert_eq!(test_expand("ARG_MACRO"), ("ARG_MACRO".into(), vec![]));
        assert_eq!(
            test_expand(
                "SIMPLE_MACRO(^) * ARG_MACRO(55) + MULTI_ARG_MACRO(58, %) / MULTI_ARG_MACRO(%)"
            ),
            (
                "something(^) * hello & (55) + ((58) | (%)) / MULTI_ARG_MACRO(%)".into(),
                vec![MacroDiagnostic::WrongNumberOfMacroArgs {
                    expected: 2,
                    found: 1,
                    call_site: (159, 18),
                    declaration_site: (0, 27),
                }]
            ),
        );
        assert_eq!(
            test_expand("RECURSIVE_MACRO(89, 90)"),
            ("hello & (90) * 89 + 90 * hello & (89)".into(), vec![]),
        );
        assert_eq!(
            test_expand("FANCY_RECURSIVE_MACRO(89, 90, MULTI_ARG_MACRO)"),
            ("((89) | (90))".into(), vec![]),
        );
        assert_eq!(
            test_expand("REFERENCE_MACRO(55)"),
            ("hello & (55)".into(), vec![]),
        );
    }
}
