//! S-expression parser and writer for KiCad-compatible board files.
//!
//! Clean-room implementation of the s-expression format used by KiCad
//! `.kicad_pcb` / `.kicad_sch` files: atoms, quoted strings, nested lists.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Sexp {
    /// A bare atom (symbol or number as written in the file)
    Atom(String),
    /// A double-quoted string (escapes: \\ and \")
    Str(String),
    /// A parenthesized list
    List(Vec<Sexp>),
}

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at offset {}", self.message, self.offset)
    }
}

impl std::error::Error for ParseError {}

pub fn parse(input: &str) -> Result<Sexp, ParseError> {
    let bytes = input.as_bytes();
    let mut pos = 0usize;
    let result = parse_expr(input, bytes, &mut pos, 0)?;
    skip_ws(bytes, &mut pos);
    if pos != bytes.len() {
        return Err(ParseError {
            message: format!("trailing data after expression"),
            offset: pos,
        });
    }
    Ok(result)
}

fn skip_ws(b: &[u8], pos: &mut usize) {
    while *pos < b.len() && (b[*pos] as char).is_whitespace() {
        *pos += 1;
    }
}

const MAX_DEPTH: usize = 1000;

fn parse_expr(input: &str, b: &[u8], pos: &mut usize, depth: usize) -> Result<Sexp, ParseError> {
    if depth > MAX_DEPTH {
        return Err(ParseError {
            message: format!("nesting too deep (limit {})", MAX_DEPTH),
            offset: *pos,
        });
    }
    skip_ws(b, pos);
    if *pos >= b.len() {
        return Err(ParseError {
            message: "unexpected end of input".into(),
            offset: *pos,
        });
    }
    match b[*pos] {
        b'(' => {
            *pos += 1;
            let mut items = Vec::new();
            loop {
                skip_ws(b, pos);
                if *pos >= b.len() {
                    return Err(ParseError {
                        message: "unterminated list".into(),
                        offset: *pos,
                    });
                }
                if b[*pos] == b')' {
                    *pos += 1;
                    return Ok(Sexp::List(items));
                }
                items.push(parse_expr(input, b, pos, depth + 1)?);
            }
        }
        b'"' => {
            *pos += 1;
            let mut s = String::new();
            loop {
                if *pos >= b.len() {
                    return Err(ParseError {
                        message: "unterminated string".into(),
                        offset: *pos,
                    });
                }
                match b[*pos] {
                    b'"' => {
                        *pos += 1;
                        return Ok(Sexp::Str(s));
                    }
                    b'\\' => {
                        *pos += 1;
                        if *pos >= b.len() {
                            return Err(ParseError {
                                message: "unterminated escape".into(),
                                offset: *pos,
                            });
                        }
                        s.push(b[*pos] as char);
                        *pos += 1;
                    }
                    _ => {
                        s.push(b[*pos] as char);
                        *pos += 1;
                    }
                }
            }
        }
        _ => {
            let start = *pos;
            while *pos < b.len()
                && !(b[*pos] as char).is_whitespace()
                && b[*pos] != b'('
                && b[*pos] != b')'
                && b[*pos] != b'"'
            {
                *pos += 1;
            }
            if *pos == start {
                return Err(ParseError {
                    message: format!("unexpected character '{}'", b[*pos] as char),
                    offset: *pos,
                });
            }
            // UTF-8 safety: slicing must land on a char boundary.
            // Walk back if we stopped mid-character.
            while *pos > start && !input.is_char_boundary(*pos) {
                *pos -= 1;
            }
            // If we walked all the way back (lone continuation byte), consume
            // at least one byte so the caller always makes progress.
            if *pos == start {
                *pos = start + 1;
                // skip any remaining continuation bytes
                while *pos < b.len() && !input.is_char_boundary(*pos) {
                    *pos += 1;
                }
            }
            Ok(Sexp::Atom(input[start..*pos].to_string()))
        }
    }
}

impl Sexp {
    /// If this is a list whose first element is the given symbol, return its children.
    pub fn as_tagged(&self, tag: &str) -> Option<&[Sexp]> {
        match self {
            Sexp::List(items) if !items.is_empty() => {
                match &items[0] {
                    Sexp::Atom(a) if a == tag => Some(&items[1..]),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub fn atom(&self) -> Option<&str> {
        match self {
            Sexp::Atom(a) => Some(a),
            _ => None,
        }
    }

    pub fn str_value(&self) -> Option<&str> {
        match self {
            Sexp::Str(s) | Sexp::Atom(s) => Some(s),
            Sexp::List(_) => None,
        }
    }

    pub fn f64_value(&self) -> Option<f64> {
        self.str_value().and_then(|s| s.parse().ok())
    }
}

impl fmt::Display for Sexp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Sexp::Atom(a) => write!(f, "{}", a),
            Sexp::Str(s) => {
                write!(f, "\"")?;
                for c in s.chars() {
                    match c {
                        '"' => write!(f, "\\\"")?,
                        '\\' => write!(f, "\\\\")?,
                        _ => write!(f, "{}", c)?,
                    }
                }
                write!(f, "\"")
            }
            Sexp::List(items) => {
                write!(f, "(")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, ")")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_list() {
        let s = parse("(version 20211014)").unwrap();
        assert_eq!(s, Sexp::List(vec![
            Sexp::Atom("version".into()),
            Sexp::Atom("20211014".into()),
        ]));
    }

    #[test]
    fn parses_nested() {
        let s = parse("(a (b (c 1)) \"str\")").unwrap();
        let items = match &s {
            Sexp::List(v) => v.clone(),
            _ => unreachable!(),
        };
        assert_eq!(items[0], Sexp::Atom("a".into()));
        assert_eq!(items[2], Sexp::Str("str".into()));
        assert!(items[1].as_tagged("b").is_some());
    }

    #[test]
    fn parses_escapes() {
        let s = parse(r#"("a \"quoted\" thing")"#).unwrap();
        let items = match &s { Sexp::List(v) => v, _ => unreachable!() };
        assert_eq!(items[0], Sexp::Str("a \"quoted\" thing".into()));
    }

    #[test]
    fn rejects_unterminated() {
        assert!(parse("(a (b").is_err());
        assert!(parse("(a \"unterminated").is_err());
    }

    #[test]
    fn round_trips() {
        let src = r#"(kicad_pcb (version 20211014) (footprint "x" (at 1.5 2.5 0)))"#;
        let parsed = parse(src).unwrap();
        let printed = parsed.to_string();
        let reparsed = parse(&printed).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn tagged_access() {
        let s = parse("(at 66.766 20.889 0)").unwrap();
        let args = s.as_tagged("at").unwrap();
        assert_eq!(args[0].f64_value(), Some(66.766));
        assert_eq!(args[1].f64_value(), Some(20.889));
    }
}