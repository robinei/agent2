//! Minimal JS syntax highlighting for the source pane: a hand-rolled
//! tokenizer over the raw source (byte ranges → token kinds). Deliberately
//! not AST-based — comments and keywords aren't AST nodes (oxc's lexer is
//! internal), and a ~30-keyword dialect doesn't justify a grammar engine.
//! The scanner covers exactly what the renderer needs: comments, strings,
//! numbers, keywords, identifiers, punctuation.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    Ident,
    Number,
    Str,
    Comment,
    Punct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
}

const KEYWORDS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "of",
    "raise",
    "return",
    "switch",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "while",
];

/// Tokenize the whole source; gaps between tokens are whitespace.
pub fn tokenize(src: &str) -> Vec<Token> {
    let b = src.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                toks.push(Token {
                    start,
                    end: i,
                    kind: Kind::Comment,
                });
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                toks.push(Token {
                    start,
                    end: i,
                    kind: Kind::Comment,
                });
            }
            b'"' | b'\'' | b'`' => {
                let quote = c;
                i += 1;
                while i < b.len() && b[i] != quote {
                    // Skip escapes; a template's `${…}` is left inside Str.
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(b.len());
                toks.push(Token {
                    start,
                    end: i,
                    kind: Kind::Str,
                });
            }
            b'0'..=b'9' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_')
                {
                    i += 1;
                }
                toks.push(Token {
                    start,
                    end: i,
                    kind: Kind::Number,
                });
            }
            b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'$' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$')
                {
                    i += 1;
                }
                let kind = if KEYWORDS.contains(&&src[start..i]) {
                    Kind::Keyword
                } else {
                    Kind::Ident
                };
                toks.push(Token {
                    start,
                    end: i,
                    kind,
                });
            }
            _ => {
                // Multi-byte UTF-8 or punctuation: one char.
                let ch_len = src[i..].chars().next().map_or(1, |ch| ch.len_utf8());
                i += ch_len;
                toks.push(Token {
                    start,
                    end: i,
                    kind: Kind::Punct,
                });
            }
        }
    }
    toks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(Kind, &str)> {
        tokenize(src)
            .into_iter()
            .map(|t| (t.kind, &src[t.start..t.end]))
            .collect()
    }

    #[test]
    fn classifies_basic_tokens() {
        let ks = kinds("const x = 'hi'; // note");
        assert_eq!(ks[0], (Kind::Keyword, "const"));
        assert_eq!(ks[1], (Kind::Ident, "x"));
        assert_eq!(ks[2], (Kind::Punct, "="));
        assert_eq!(ks[3], (Kind::Str, "'hi'"));
        assert_eq!(ks[4], (Kind::Punct, ";"));
        assert_eq!(ks[5], (Kind::Comment, "// note"));
    }

    #[test]
    fn block_comments_and_numbers_span_correctly() {
        let ks = kinds("1 /* a\nb */ 2.5");
        assert_eq!(ks[0], (Kind::Number, "1"));
        assert_eq!(ks[1], (Kind::Comment, "/* a\nb */"));
        assert_eq!(ks[2], (Kind::Number, "2.5"));
    }

    #[test]
    fn unterminated_string_does_not_overrun() {
        let ks = kinds("'open");
        assert_eq!(ks[0], (Kind::Str, "'open"));
    }
}
