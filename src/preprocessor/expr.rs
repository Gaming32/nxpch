use logos::{Logos, Span, SpannedIter};
use miette::Diagnostic;
use std::iter::Peekable;
use std::num::ParseIntError;
use std::ops::Range;
use strum::IntoStaticStr;
use thiserror::Error;

pub type ExprValue = i64;

#[derive(Logos, Copy, Clone, Debug, PartialEq, Eq, IntoStaticStr)]
#[logos(skip r"\s+", error = TokenError)]
enum Token<'source> {
    #[token("*")]
    Multiply,
    #[token("/")]
    Divide,
    #[token("%")]
    Remainder,
    #[token("+")]
    Add,
    #[token("-")]
    Subtract,
    #[token("<<")]
    ShiftLeft,
    #[token(">>")]
    ShiftRight,
    #[token("<")]
    LessThan,
    #[token(">")]
    GreaterThan,
    #[token("<=")]
    LessThanOrEqual,
    #[token(">=")]
    GreaterThanOrEqual,
    #[token("==")]
    Equal,
    #[token("!=")]
    NotEqual,
    #[token("&")]
    BitAnd,
    #[token("|")]
    BitOr,
    #[token("^")]
    BitXor,
    #[token("~")]
    BitNot,
    #[token("!")]
    LogicalNot,
    #[token("&&")]
    LogicalAnd,
    #[token("||")]
    LogicalOr,
    #[token("(")]
    LeftParenthesis,
    #[token(")")]
    RightParenthesis,
    #[regex("0|[1-9][0-9]*", |lex| lex.slice().parse())]
    #[regex("0[xX][0-9a-fA-F]+", |lex| ExprValue::from_str_radix(&lex.slice()[2..], 16))]
    #[regex("0[bB][01]+", |lex| ExprValue::from_str_radix(&lex.slice()[2..], 2))]
    #[regex("0[0-7]+", |lex| ExprValue::from_str_radix(&lex.slice()[1..], 8))]
    #[regex("-9223372036854775808|-0[xX]80{15}|-0[bB]10{63}|-010{21}", |_| ExprValue::MIN)]
    Number(ExprValue),
    #[regex("[a-zA-Z_][a-zA-Z0-9_]*")]
    Identifier(&'source str),
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Error)]
enum TokenError {
    #[error("Invalid number: {0}")]
    InvalidNumber(#[from] ParseIntError),
    #[default]
    #[error("Unknown token")]
    Unknown,
}

pub type EvalResult = Result<ExprValue, ExprDiagnostic>;

pub fn evaluate<M: Fn(&str) -> bool>(
    expr: &str,
    offsets: &[usize],
    macro_defined: M,
) -> EvalResult {
    type Lex<'a, 'source> = &'a mut Peekable<SpannedIter<'source, Token<'source>>>;
    type TokenSpan<'source> = (Token<'source>, Span);
    type TokenPeekResult<'source> = Result<TokenSpan<'source>, Option<TokenSpan<'source>>>;

    fn lex_peek<'source>(
        lexer: Lex<'_, 'source>,
        check: fn(Token) -> bool,
    ) -> Result<TokenPeekResult<'source>, ExprDiagnostic> {
        match lexer.peek().cloned() {
            // discriminant is used because of Number
            Some((Ok(tok), span)) if check(tok) => {
                lexer.next();
                Ok(Ok((tok, span)))
            }
            Some((Ok(tok), span)) => Ok(Err(Some((tok, span)))),
            Some((Err(err), span)) => Err(match err {
                TokenError::InvalidNumber(cause) => {
                    ExprDiagnostic::InvalidNumber { cause, at: span }
                }
                TokenError::Unknown => ExprDiagnostic::UnknownToken { at: span },
            }),
            None => Ok(Err(None)),
        }
    }
    macro_rules! lex_peek {
        ($lexer:ident, $pattern:pat $(,)?) => {
            lex_peek($lexer, |tok| matches!(tok, $pattern)).map(|x| x.ok().map(|(tok, _)| tok))
        };
    }
    macro_rules! lex_expect {
        ($lexer:ident, $message:literal, $pattern:pat $(,)?) => {
            match lex_peek($lexer, |tok| matches!(tok, $pattern)) {
                Ok(Ok(tok)) => Ok(tok),
                Ok(Err(Some((tok, span)))) => Err(ExprDiagnostic::UnexpectedToken {
                    expected: $message,
                    found: tok.into(),
                    at: span,
                }),
                Ok(Err(None)) => Err(ExprDiagnostic::Eol {
                    expected: $message,
                    at: 0, // Real value is filled in later
                }),
                Err(err) => Err(err),
            }
        };
    }

    fn logical_or(lexer: Lex, macro_defined: &impl Fn(&str) -> bool) -> EvalResult {
        let mut result = logical_and(lexer, macro_defined)?;
        while lex_peek!(lexer, Token::LogicalOr)?.is_some() {
            let right = logical_and(lexer, macro_defined)?;
            result = ((result != 0) || (right != 0)) as ExprValue;
        }
        Ok(result)
    }

    fn logical_and(lexer: Lex, macro_defined: &impl Fn(&str) -> bool) -> EvalResult {
        let mut result = bitwise_or(lexer, macro_defined)?;
        while lex_peek!(lexer, Token::LogicalAnd)?.is_some() {
            let right = bitwise_or(lexer, macro_defined)?;
            result = ((result != 0) && (right != 0)) as ExprValue;
        }
        Ok(result)
    }

    fn bitwise_or(lexer: Lex, macro_defined: &impl Fn(&str) -> bool) -> EvalResult {
        let mut result = bitwise_xor(lexer, macro_defined)?;
        while lex_peek!(lexer, Token::BitOr)?.is_some() {
            result |= bitwise_xor(lexer, macro_defined)?;
        }
        Ok(result)
    }

    fn bitwise_xor(lexer: Lex, macro_defined: &impl Fn(&str) -> bool) -> EvalResult {
        let mut result = bitwise_and(lexer, macro_defined)?;
        while lex_peek!(lexer, Token::BitXor)?.is_some() {
            result ^= bitwise_and(lexer, macro_defined)?;
        }
        Ok(result)
    }

    fn bitwise_and(lexer: Lex, macro_defined: &impl Fn(&str) -> bool) -> EvalResult {
        let mut result = equality(lexer, macro_defined)?;
        while lex_peek!(lexer, Token::BitAnd)?.is_some() {
            result &= equality(lexer, macro_defined)?;
        }
        Ok(result)
    }

    fn equality(lexer: Lex, macro_defined: &impl Fn(&str) -> bool) -> EvalResult {
        let mut result = relational(lexer, macro_defined)?;
        while let Some(token) = lex_peek!(lexer, Token::Equal | Token::NotEqual)? {
            let right = relational(lexer, macro_defined)?;
            result = match token {
                Token::Equal => result == right,
                Token::NotEqual => result == right,
                _ => unreachable!(),
            } as ExprValue;
        }
        Ok(result)
    }

    fn relational(lexer: Lex, macro_defined: &impl Fn(&str) -> bool) -> EvalResult {
        let mut result = shift(lexer, macro_defined)?;
        while let Some(token) = lex_peek!(
            lexer,
            Token::LessThan
                | Token::GreaterThan
                | Token::LessThanOrEqual
                | Token::GreaterThanOrEqual,
        )? {
            let right = shift(lexer, macro_defined)?;
            result = match token {
                Token::LessThan => result < right,
                Token::GreaterThan => result > right,
                Token::LessThanOrEqual => result <= right,
                Token::GreaterThanOrEqual => result >= right,
                _ => unreachable!(),
            } as ExprValue;
        }
        Ok(result)
    }

    fn shift(lexer: Lex, macro_defined: &impl Fn(&str) -> bool) -> EvalResult {
        let mut result = additive(lexer, macro_defined)?;
        while let Some(token) = lex_peek!(lexer, Token::ShiftLeft | Token::ShiftRight)? {
            let right = additive(lexer, macro_defined)?;
            result = match token {
                Token::ShiftLeft => result << right,
                Token::ShiftRight => result >> right,
                _ => unreachable!(),
            };
        }
        Ok(result)
    }

    fn additive(lexer: Lex, macro_defined: &impl Fn(&str) -> bool) -> EvalResult {
        let mut result = multiplicative(lexer, macro_defined)?;
        while let Some(token) = lex_peek!(lexer, Token::Add | Token::Subtract)? {
            let right = multiplicative(lexer, macro_defined)?;
            result = match token {
                Token::Add => result + right,
                Token::Subtract => result - right,
                _ => unreachable!(),
            };
        }
        Ok(result)
    }

    fn multiplicative(lexer: Lex, macro_defined: &impl Fn(&str) -> bool) -> EvalResult {
        let mut result = unary(lexer, macro_defined)?;
        while let Some(token) =
            lex_peek!(lexer, Token::Multiply | Token::Divide | Token::Remainder)?
        {
            let right = unary(lexer, macro_defined)?;
            result = match token {
                Token::Multiply => result * right,
                Token::Divide => result / right,
                Token::Remainder => result % right,
                _ => unreachable!(),
            };
        }
        Ok(result)
    }

    fn unary(lexer: Lex, macro_defined: &impl Fn(&str) -> bool) -> EvalResult {
        if let Some(token) = lex_peek!(
            lexer,
            Token::Add | Token::Subtract | Token::BitNot | Token::LogicalNot,
        )? {
            let right = unary(lexer, macro_defined)?;
            Ok(match token {
                Token::Add => right,
                Token::Subtract => -right,
                Token::BitNot => !right,
                Token::LogicalNot => (right == 0) as ExprValue,
                _ => unreachable!(),
            })
        } else {
            primary(lexer, macro_defined)
        }
    }

    fn primary(lexer: Lex, macro_defined: &impl Fn(&str) -> bool) -> EvalResult {
        let (tok, span) = lex_expect!(
            lexer,
            "an identifier, number, or parentheses",
            Token::Identifier(_) | Token::Number(_) | Token::LeftParenthesis,
        )?;
        match tok {
            Token::Identifier(ident) if lex_peek!(lexer, Token::LeftParenthesis)?.is_some() => {
                if ident != "defined" {
                    return Err(ExprDiagnostic::UnknownFunction {
                        function: ident.to_string(),
                        at: span,
                    });
                }
                let macro_name = lex_expect!(lexer, "a macro name", Token::Identifier(_))?.0;
                lex_expect!(
                    lexer,
                    "end parenthesis after macro name",
                    Token::RightParenthesis,
                )?;
                Ok(macro_defined(match macro_name {
                    Token::Identifier(x) => x,
                    _ => unreachable!(),
                }) as ExprValue)
            }
            Token::Identifier(_) => Ok(0),
            Token::Number(x) => Ok(x),
            Token::LeftParenthesis => {
                let inner_value = logical_or(lexer, macro_defined)?;
                lex_expect!(
                    lexer,
                    "end parenthesis after sub-expression",
                    Token::RightParenthesis,
                )?;
                Ok(inner_value)
            }
            _ => unreachable!(),
        }
    }

    fn parse_inner(expr: &str, macro_defined: impl Fn(&str) -> bool) -> EvalResult {
        let mut lexer = Token::lexer(expr).spanned().peekable();
        if lexer.peek().is_none() {
            return Ok(0);
        }
        let result = logical_or(&mut lexer, &macro_defined)?;
        match lexer.next() {
            Some((_, span)) => Err(ExprDiagnostic::UnexpectedTrailer { at: span }),
            None => Ok(result),
        }
    }

    let mut result = parse_inner(expr, macro_defined);
    match &mut result {
        Err(
            ExprDiagnostic::UnknownToken { at }
            | ExprDiagnostic::UnexpectedToken { at, .. }
            | ExprDiagnostic::InvalidNumber { at, .. }
            | ExprDiagnostic::UnknownFunction { at, .. }
            | ExprDiagnostic::UnexpectedTrailer { at },
        ) => {
            at.start = offsets[at.start];
            at.end = offsets
                .get(at.end)
                .copied()
                .unwrap_or_else(|| *offsets.last().unwrap() + 1);
            if at.end < at.start || at.end > at.start + expr.len() {
                at.end = at.start;
            }
        }
        Err(ExprDiagnostic::Eol { at, .. }) => {
            *at = offsets.last().copied().unwrap_or_default();
        }
        Ok(_) => {}
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq, Error, Diagnostic)]
pub enum ExprDiagnostic {
    #[error("Unknown token in #if expression")]
    #[diagnostic(code(preprocessor::expr::unknown_token))]
    UnknownToken {
        #[label]
        at: Range<usize>,
    },

    #[error("Unexpected token {found:?} in #if expression")]
    #[diagnostic(code(preprocessor::expr::unexpected_token))]
    UnexpectedToken {
        expected: &'static str,
        found: &'static str,

        #[label("Expected {expected}")]
        at: Range<usize>,
    },

    #[error("Invalid number in #if expression")]
    #[diagnostic(code(preprocessor::expr::invalid_number))]
    InvalidNumber {
        #[source]
        cause: ParseIntError,

        #[label("{cause}")]
        at: Range<usize>,
    },

    #[error("Unexpected trailing tokens in #if expression")]
    #[diagnostic(code(preprocessor::expr::unexpected_trailer))]
    UnexpectedTrailer {
        #[label]
        at: Range<usize>,
    },

    #[error("Unexpected end of line in #if expression")]
    #[diagnostic(code(preprocessor::expr::eol))]
    Eol {
        expected: &'static str,

        #[label("Expected {expected}")]
        at: usize,
    },

    #[error("Unknown builtin function in #if expression")]
    #[diagnostic(code(preprocessor::expr::unexpected_token))]
    UnknownFunction {
        function: String,

        #[label("Unknown function \"{function}\"")]
        at: Range<usize>,
    },
}

#[cfg(test)]
mod test {
    use crate::preprocessor::MacroDefine;
    use crate::preprocessor::expr::{EvalResult, ExprDiagnostic, ExprValue, evaluate};
    use pretty_assertions::assert_eq;

    fn eval(expr: &str) -> EvalResult {
        evaluate(expr, &MacroDefine::len_vec(0, expr.len()), |name| {
            name == "REAL_MACRO"
        })
    }

    #[test]
    fn test_primary() {
        assert_eq!(eval(""), Ok(0));
        assert_eq!(eval("   "), Ok(0));

        assert_eq!(eval("an_identifier"), Ok(0));

        assert_eq!(eval("0"), Ok(0));
        assert_eq!(eval("5"), Ok(5));
        assert_eq!(eval("10"), Ok(10));
        assert_eq!(eval("0x0"), Ok(0x0));
        assert_eq!(eval("0xA"), Ok(0xA));
        assert_eq!(eval("0x10"), Ok(0x10));
        assert_eq!(eval("0b0"), Ok(0b0));
        assert_eq!(eval("0b1"), Ok(0b1));
        assert_eq!(eval("0b10"), Ok(0b10));
        assert_eq!(eval("00"), Ok(0o0));
        assert_eq!(eval("04"), Ok(0o4));
        assert_eq!(eval("010"), Ok(0o10));
        assert_eq!(eval("-0x8000000000000000"), Ok(-0x8000000000000000));
        assert_eq!(
            eval("0x8000000000000000"),
            Err(ExprDiagnostic::InvalidNumber {
                cause: ExprValue::from_str_radix("8000000000000000", 16)
                    .err()
                    .unwrap(),
                at: 0..18,
            })
        );

        assert_eq!(eval("defined(REAL_MACRO)"), Ok(1));
        assert_eq!(eval("defined(FAKE_MACRO)"), Ok(0));
        assert_eq!(
            eval("defined(bad syntax)"),
            Err(ExprDiagnostic::UnexpectedToken {
                expected: "end parenthesis after macro name",
                found: "Identifier",
                at: 12..18,
            }),
        );
        assert_eq!(
            eval("invalid_function(5)"),
            Err(ExprDiagnostic::UnknownFunction {
                function: "invalid_function".to_string(),
                at: 0..16,
            }),
        );

        assert_eq!(
            eval("(0"),
            Err(ExprDiagnostic::Eol {
                expected: "end parenthesis after sub-expression",
                at: 1,
            })
        );
    }

    #[test]
    fn test_other() {
        assert_eq!(eval("-0"), Ok(0));
        assert_eq!(eval("-10"), Ok(-10));
        assert_eq!(eval("-0x10"), Ok(-0x10));
        assert_eq!(eval("-0b10"), Ok(-0b10));
        assert_eq!(eval("-010"), Ok(-0o10));
        assert_eq!(eval("-0x7fffffffffffffff"), Ok(-0x7fffffffffffffff));
    }
}
