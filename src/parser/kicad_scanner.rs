use std::{iter::{Peekable}, str::CharIndices};

use anyhow::anyhow;


#[derive(Debug, PartialEq, Clone)]
pub enum Token {
    LParen,
    RParen,
    Symbol(String),
    Value(String)
}

impl Token {
    pub fn sym(str: &str) -> Token {
        return Token::Symbol(str.to_string());
    }
}

pub struct Scanner<'a> {
    data: &'a str,
    iter: Peekable<CharIndices<'a>>
}

impl Scanner<'_> {
    pub fn new(data: &str) -> Scanner {
        return Scanner {
            data: data,
            iter: data.char_indices().peekable()
        };
    }

    fn scan_string(&mut self) -> Option<anyhow::Result<Token>>  {
        let mut buf = String::new();

        // First char was already peeked in next()
        let Some((_, chr)) = self.iter.next() else {
            return Some(Err(anyhow!("failed to yield an already peeked char indice")));
        };
        // Dont push it to skip quotes

        loop {
            let Some((_, chr)) = self.iter.next() else {
                return Some(Err(anyhow!("reached EOF in the middle of string!")));
            };
            match chr {
                '\\' => {
                    let Some((_, esc)) = self.iter.next() else {
                        return Some(Err(anyhow!("reached EOF in the middle of string!")));
                    };
                    buf.push(esc);
                },
                '"' => break,
                c => buf.push(c)
            }
        }
        return Some(Ok(Token::Value(buf)))
    }

    fn scan_symbol(&mut self) -> Option<anyhow::Result<Token>>  {
        let mut buf = String::new();

        // First char was already peeked in next()
        let Some((_, chr)) = self.iter.next() else {
            return Some(Err(anyhow!("failed to yield an already peeked char indice")));
        };
        buf.push(chr);

        loop {
            let Some((_, next_char)) = self.iter.peek() else {
                break;
            };

            if *next_char == '(' ||
                *next_char == ')' ||
                *next_char == '"' ||
                next_char.is_whitespace() {
                    break;
            }
            let Some((_, chr)) = self.iter.next() else {
                return Some(Err(anyhow!("failed to yield an already peeked char indice")));
            };
            buf.push(chr);
        }
        return Some(Ok(Token::Symbol(buf)));
    }
}

impl Iterator for Scanner<'_> {
    type Item = anyhow::Result<Token>;

    fn next(&mut self) -> Option<anyhow::Result<Token>> {
        // Skip leading whitespace
        while self.iter.next_if(|(_, c)| c.is_whitespace()).is_some() {};

        let (_, next_char) = self.iter.peek()?;

        match next_char {
            '(' => {
                self.iter.next();
                return Some(Ok(Token::LParen))
            }
            ')' => {
                self.iter.next();
                return Some(Ok(Token::RParen))
            }
            '"' => {
                return self.scan_string();
            }
            _ => {
                return self.scan_symbol();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_scanned_list(a: &str, b: &[Token]) {
        let tokens: anyhow::Result<Vec<Token>> = Scanner::new(a).collect();
        let tokens = tokens.expect("scanner returned an error");

        assert_eq!(tokens.as_slice(), b);
    }

    fn assert_error(a: &str) {
        let result: anyhow::Result<Vec<_>, _> = Scanner::new(a).collect();

        assert!(result.is_err());
    }

    #[test]
    fn scans_simple_list() {
        assert_scanned_list("(export)", &[
            Token::LParen,
            Token::Symbol("export".to_string()),
            Token::RParen,
        ]);
    }

    #[test]
    fn scans_atom_at_eof() {
        assert_scanned_list("export", &[
            Token::Symbol("export".to_string()),
        ]);
    }

    #[test]
    fn scans_empty() {
        assert_scanned_list("", &[
        ]);
    }

    #[test]
    fn scans_whitespace() {
        assert_scanned_list("\t\n ", &[
        ]);
    }

    #[test]
    fn scans_string() {
        assert_scanned_list(r#""str""#, &[
            Token::Value("str".to_string()),
        ]);
    }

    #[test]
    fn scans_space_in_string() {
        assert_scanned_list(r#""s tr""#, &[
            Token::Value("s tr".to_string()),
        ]);
    }

    #[test]
    fn scans_paren_in_string() {
        assert_scanned_list(r#""s()tr""#, &[
            Token::Value("s()tr".to_string()),
        ]);
    }

    #[test]
    fn scans_escaped_quote_in_string() {
        assert_scanned_list(r#""s\"tr""#, &[
            Token::Value(r#"s"tr"#.to_string()),
        ]);
    }

    #[test]
    fn scans_escaped_slash_in_string() {
        assert_scanned_list(r#""s\\""#, &[
            Token::Value(r#"s\"#.to_string()),
        ]);
    }

    #[test]
    fn scans_escaped_slash_quote_in_string() {
        assert_scanned_list(r#""s\\\"a""#, &[
            Token::Value(r#"s\"a"#.to_string()),
        ]);
    }

    #[test]
    fn scans_empty_string() {
        assert_scanned_list(r#"("")"#, &[
            Token::LParen,
            Token::Value("".to_string()),
            Token::RParen,
        ]);
    }

    #[test]
    fn scans_no_space_parens() {
        assert_scanned_list("(a(b c)d)", &[
            Token::LParen,
            Token::Symbol("a".to_string()),
            Token::LParen,
            Token::Symbol("b".to_string()),
            Token::Symbol("c".to_string()),
            Token::RParen,
            Token::Symbol("d".to_string()),
            Token::RParen,
        ]);
    }

    #[test]
    fn scans_mixed_whitespace() {
        assert_scanned_list("(a\n\t b)", &[
            Token::LParen,
            Token::Symbol("a".to_string()),
            Token::Symbol("b".to_string()),
            Token::RParen,
        ]);

    }

    #[test]
    fn scans_mixed_whitespace_in_string() {
        assert_scanned_list("\"(a\n\t b)\"", &[
            Token::Value("(a\n\t b)".to_string()),
        ]);

    }

    #[test]
    fn scans_utf8() {
        assert_scanned_list(r#"(title "Krets Öåä µΩ")"#, &[
            Token::LParen,
            Token::Symbol("title".to_string()),
            Token::Value("Krets Öåä µΩ".to_string()),
            Token::RParen,
        ]);

    }

    #[test]
    fn scans_unterminated_string() {
        assert_error(r#"(title "board"#);
    }

    #[test]
    fn scans_trailing_backslash_in_string() {
        assert_error(r#"(title "\"#);
    }
}