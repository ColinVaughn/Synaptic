//! SYNQL lexer: source string -> tokens (with char-offset positions for errors).

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Match,
    Where,
    Return,
    Limit,
    And,
    Or,
    Not,
    Has,
    Ident(String),
    Str(String),
    Num(f64),
    LParen,
    RParen,
    Colon,
    Comma,
    Dot,
    DotDot,         // .. (variable-length path bound separator)
    Star,           // * (variable-length path / count(*))
    DashBracket,    // -[
    ArrowR,         // ]->
    DashBracketEnd, // ]-
    ArrowLDash,     // <-[
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Regex, // =~
}

/// A token with its char offset in the source (for error messages).
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned {
    pub tok: Tok,
    pub at: usize,
}

/// Tokenize `input`. Keywords are case-insensitive. Strings are single- or
/// double-quoted with `\` escapes. Errors carry the offending char offset.
pub fn lex(input: &str) -> Result<Vec<Spanned>, String> {
    let chars: Vec<char> = input.chars().collect();
    let get = |j: usize| chars.get(j).copied();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let at = i;
        match c {
            '(' => {
                out.push(Spanned {
                    tok: Tok::LParen,
                    at,
                });
                i += 1;
            }
            ')' => {
                out.push(Spanned {
                    tok: Tok::RParen,
                    at,
                });
                i += 1;
            }
            ':' => {
                out.push(Spanned {
                    tok: Tok::Colon,
                    at,
                });
                i += 1;
            }
            ',' => {
                out.push(Spanned {
                    tok: Tok::Comma,
                    at,
                });
                i += 1;
            }
            '.' => {
                if get(i + 1) == Some('.') {
                    out.push(Spanned {
                        tok: Tok::DotDot,
                        at,
                    });
                    i += 2;
                } else {
                    out.push(Spanned { tok: Tok::Dot, at });
                    i += 1;
                }
            }
            '*' => {
                out.push(Spanned { tok: Tok::Star, at });
                i += 1;
            }
            '\'' | '"' => {
                let q = c;
                i += 1;
                let mut s = String::new();
                loop {
                    match get(i) {
                        None => return Err(format!("unterminated string at {at}")),
                        Some(ch) if ch == q => {
                            i += 1;
                            break;
                        }
                        Some('\\') => {
                            if let Some(n) = get(i + 1) {
                                s.push(n);
                                i += 2;
                            } else {
                                i += 1;
                            }
                        }
                        Some(ch) => {
                            s.push(ch);
                            i += 1;
                        }
                    }
                }
                out.push(Spanned {
                    tok: Tok::Str(s),
                    at,
                });
            }
            c if c.is_ascii_digit() => {
                let mut j = i;
                // Consume digits and a single decimal point, but only when the dot
                // is followed by a digit so `1..3` lexes as Num `..` Num, not "1..3".
                while j < chars.len()
                    && (chars[j].is_ascii_digit()
                        || (chars[j] == '.' && get(j + 1).is_some_and(|d| d.is_ascii_digit())))
                {
                    j += 1;
                }
                let lit: String = chars[i..j].iter().collect();
                let num: f64 = lit
                    .parse()
                    .map_err(|_| format!("bad number '{lit}' at {at}"))?;
                out.push(Spanned {
                    tok: Tok::Num(num),
                    at,
                });
                i = j;
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut j = i;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let word: String = chars[i..j].iter().collect();
                let tok = match word.to_ascii_lowercase().as_str() {
                    "match" => Tok::Match,
                    "where" => Tok::Where,
                    "return" => Tok::Return,
                    "limit" => Tok::Limit,
                    "and" => Tok::And,
                    "or" => Tok::Or,
                    "not" => Tok::Not,
                    "has" => Tok::Has,
                    _ => Tok::Ident(word),
                };
                out.push(Spanned { tok, at });
                i = j;
            }
            '<' => {
                if get(i + 1) == Some('-') && get(i + 2) == Some('[') {
                    out.push(Spanned {
                        tok: Tok::ArrowLDash,
                        at,
                    });
                    i += 3;
                } else if get(i + 1) == Some('=') {
                    out.push(Spanned { tok: Tok::Le, at });
                    i += 2;
                } else {
                    out.push(Spanned { tok: Tok::Lt, at });
                    i += 1;
                }
            }
            '>' => {
                if get(i + 1) == Some('=') {
                    out.push(Spanned { tok: Tok::Ge, at });
                    i += 2;
                } else {
                    out.push(Spanned { tok: Tok::Gt, at });
                    i += 1;
                }
            }
            '=' => {
                if get(i + 1) == Some('~') {
                    out.push(Spanned {
                        tok: Tok::Regex,
                        at,
                    });
                    i += 2;
                } else {
                    out.push(Spanned { tok: Tok::Eq, at });
                    i += 1;
                }
            }
            '!' => {
                if get(i + 1) == Some('=') {
                    out.push(Spanned { tok: Tok::Ne, at });
                    i += 2;
                } else {
                    return Err(format!("unexpected '!' at {at}"));
                }
            }
            '-' => {
                if get(i + 1) == Some('[') {
                    out.push(Spanned {
                        tok: Tok::DashBracket,
                        at,
                    });
                    i += 2;
                } else {
                    return Err(format!("unexpected '-' at {at}"));
                }
            }
            ']' => {
                if get(i + 1) == Some('-') && get(i + 2) == Some('>') {
                    out.push(Spanned {
                        tok: Tok::ArrowR,
                        at,
                    });
                    i += 3;
                } else if get(i + 1) == Some('-') {
                    out.push(Spanned {
                        tok: Tok::DashBracketEnd,
                        at,
                    });
                    i += 2;
                } else {
                    return Err(format!("unexpected ']' at {at}"));
                }
            }
            other => return Err(format!("unexpected '{other}' at {at}")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Tok> {
        lex(s).unwrap().into_iter().map(|s| s.tok).collect()
    }

    #[test]
    fn lexes_a_property_query() {
        assert_eq!(
            toks("MATCH (c:class) WHERE c.loc > 500 RETURN c"),
            vec![
                Tok::Match,
                Tok::LParen,
                Tok::Ident("c".into()),
                Tok::Colon,
                Tok::Ident("class".into()),
                Tok::RParen,
                Tok::Where,
                Tok::Ident("c".into()),
                Tok::Dot,
                Tok::Ident("loc".into()),
                Tok::Gt,
                Tok::Num(500.0),
                Tok::Return,
                Tok::Ident("c".into()),
            ]
        );
    }

    #[test]
    fn lexes_relationship_arrows() {
        assert_eq!(
            toks("(a)-[:calls]->(b)"),
            vec![
                Tok::LParen,
                Tok::Ident("a".into()),
                Tok::RParen,
                Tok::DashBracket,
                Tok::Colon,
                Tok::Ident("calls".into()),
                Tok::ArrowR,
                Tok::LParen,
                Tok::Ident("b".into()),
                Tok::RParen,
            ]
        );
        assert_eq!(toks("<-[]-").first(), Some(&Tok::ArrowLDash));
    }

    #[test]
    fn lexes_strings_and_regex_op() {
        assert_eq!(
            toks(r#"name =~ "^Foo""#),
            vec![
                Tok::Ident("name".into()),
                Tok::Regex,
                Tok::Str("^Foo".into())
            ]
        );
    }

    #[test]
    fn bad_char_errors_with_offset() {
        let e = lex("MATCH ?").unwrap_err();
        assert!(e.contains("at 6"), "{e}");
    }
}
