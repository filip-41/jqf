//! Lexer for jqf query source.
//!
//! This stage produces tokens with byte spans back into the original source. Whitespace and comments are trivia: they
//! are skipped, never emitted as tokens. Source forms that cannot be tokenized are represented as [`TokenKind::Error`]
//! until diagnostics are layered on top.

use alloc::vec::Vec;

use jqf_source::Span;

use crate::{
    SyntaxInputError,
    input::validate_source_len,
    token::{Token, TokenKind},
};

/// Iterator over tokens in a query source string.
///
/// The lexer borrows its input and reports spans as byte offsets into that same string. It skips trivia, emits one
/// explicit [`TokenKind::Eof`] token, then returns `None` on subsequent iteration.
pub struct Lexer<'src> {
    source: &'src str,
    start: usize,
    end: usize,
    pos: usize,
    done: bool,
}

impl<'src> Lexer<'src> {
    /// Starts scanning `source` from byte offset zero.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxInputError::SourceTooLarge`] when compact token spans cannot represent the supplied source.
    pub fn new(source: &'src str) -> Result<Self, SyntaxInputError> {
        validate_source_len(source.len())?;
        Ok(Self::new_unchecked(source))
    }

    pub(crate) const fn new_unchecked(source: &'src str) -> Self {
        Self {
            source,
            start: 0,
            end: source.len(),
            pos: 0,
            done: false,
        }
    }

    pub(crate) fn within(source: &'src str, span: Span) -> Self {
        let range = span.range();
        debug_assert!(source.is_char_boundary(range.start));
        debug_assert!(source.is_char_boundary(range.end));
        Self {
            source,
            start: range.start,
            end: range.end,
            pos: range.start,
            done: false,
        }
    }

    fn token(&mut self) -> Token {
        self.skip_trivia();
        let start = self.pos;
        let Some(byte) = self.current_byte() else {
            self.done = true;
            return Token::new(TokenKind::Eof, Span::from_usize(start, start));
        };

        if self.starts_number(byte) {
            return self.consume_number(start);
        }

        let kind = if byte == b'"' {
            self.pos += 1;
            self.consume_string()
        } else if byte == b'@' && self.next_byte().is_some_and(is_ident_start) {
            self.pos += 1;
            self.consume_ident_continue();
            TokenKind::Format
        } else if byte == b'$' {
            self.pos += 1;
            self.consume_variable()
        } else if is_ident_start(byte) {
            self.pos += 1;
            self.consume_ident(start)
        } else if let Some(kind) = self.fixed_token_kind() {
            kind
        } else {
            self.advance_char();
            TokenKind::Error
        };
        Token::new(kind, Span::from_usize(start, self.pos))
    }

    fn skip_trivia(&mut self) {
        while let Some(byte) = self.current_byte() {
            if is_inter_token_whitespace(byte) {
                self.consume_whitespace();
            } else if byte == b'#' {
                self.consume_line_comment();
            } else {
                break;
            }
        }
    }

    fn consume_whitespace(&mut self) {
        while self.current_byte().is_some_and(is_inter_token_whitespace) {
            self.pos += 1;
        }
    }

    /// Consumes a `#` comment through the end of its logical line.
    ///
    /// An odd backslash run at the line ending escapes it and the comment continues onto the next line. See
    /// [`Self::at_line_ending`] for what counts as the end of a line — a bare carriage return does not, so
    /// `#x\r|length` comments the pipe out too.
    fn consume_line_comment(&mut self) {
        loop {
            let line_start = self.pos;
            while self.current_byte().is_some() && !self.at_line_ending() {
                self.pos += 1;
            }
            if self.backslash_run_len(line_start, self.pos).is_multiple_of(2) || !self.at_line_ending() {
                break;
            }
            self.consume_line_ending();
        }
    }

    /// Length of the unbroken backslash run ending at `pos`, searching back no further than `from`. An odd run escapes
    /// whatever follows it.
    fn backslash_run_len(&self, from: usize, pos: usize) -> usize {
        self.bytes()[from..pos]
            .iter()
            .rev()
            .take_while(|&&byte| byte == b'\\')
            .count()
    }

    /// Whether the cursor sits on a line ending that closes a comment.
    ///
    /// A line feed ends a line, alone or as the second byte of a CRLF pair; a carriage return only ends one when that
    /// line feed follows it. A lone carriage return is therefore comment text, not a terminator, and the CR of a CRLF
    /// pair is not a terminator either — the pair is one line ending.
    fn at_line_ending(&self) -> bool {
        match self.current_byte() {
            Some(b'\n') => true,
            Some(b'\r') => self.next_byte() == Some(b'\n'),
            _ => false,
        }
    }

    fn consume_line_ending(&mut self) {
        if self.peek(b'\r') {
            self.pos += 2;
        } else {
            self.pos += 1;
        }
    }

    fn fixed_token_kind(&mut self) -> Option<TokenKind> {
        // Hand-written match, longest first. The table is the spelling inventory, not the scan;
        // `lexer_dispatch_covers_the_complete_fixed_token_inventory` pins the table-to-lexer direction.
        let rest = &self.bytes()[self.pos..self.end];
        let (length, kind) = match rest {
            [b'?', b'/', b'/', ..] => (3, TokenKind::DestructureAlt),
            [b'?', ..] => (1, TokenKind::Question),
            [b'/', b'/', b'=', ..] => (3, TokenKind::AltAssign),
            [b'/', b'/', ..] => (2, TokenKind::Alt),
            [b'/', b'=', ..] => (2, TokenKind::DivAssign),
            [b'/', ..] => (1, TokenKind::Slash),
            [b'=', b'=', ..] => (2, TokenKind::Eq),
            [b'=', b'>', ..] => (2, TokenKind::FatArrow),
            [b'=', ..] => (1, TokenKind::Assign),
            [b'!', b'=', ..] => (2, TokenKind::Ne),
            [b'|', b'=', ..] => (2, TokenKind::PipeAssign),
            [b'|', ..] => (1, TokenKind::Pipe),
            [b':', b':', ..] => (2, TokenKind::DoubleColon),
            [b':', ..] => (1, TokenKind::Colon),
            [b'.', b'@', ..] => (2, TokenKind::DotAt),
            [b'.', b'&', ..] => (2, TokenKind::DotAmp),
            [b'.', b'.', ..] => (2, TokenKind::DotDot),
            [b'.', ..] => (1, TokenKind::Dot),
            [b'~', ..] => (1, TokenKind::Tilde),
            [b'+', b'=', ..] => (2, TokenKind::AddAssign),
            [b'+', ..] => (1, TokenKind::Plus),
            [b'-', b'=', ..] => (2, TokenKind::SubAssign),
            [b'-', ..] => (1, TokenKind::Minus),
            [b'*', b'=', ..] => (2, TokenKind::MulAssign),
            [b'*', ..] => (1, TokenKind::Star),
            [b'%', b'=', ..] => (2, TokenKind::ModAssign),
            [b'%', ..] => (1, TokenKind::Percent),
            [b'<', b'=', ..] => (2, TokenKind::Le),
            [b'<', ..] => (1, TokenKind::Lt),
            [b'>', b'=', ..] => (2, TokenKind::Ge),
            [b'>', ..] => (1, TokenKind::Gt),
            [b',', ..] => (1, TokenKind::Comma),
            [b';', ..] => (1, TokenKind::Semi),
            [b'(', ..] => (1, TokenKind::LParen),
            [b')', ..] => (1, TokenKind::RParen),
            [b'[', ..] => (1, TokenKind::LBracket),
            [b']', ..] => (1, TokenKind::RBracket),
            [b'{', ..] => (1, TokenKind::LBrace),
            [b'}', ..] => (1, TokenKind::RBrace),
            _ => return None,
        };
        self.pos += length;
        Some(kind)
    }

    fn starts_number(&self, byte: u8) -> bool {
        byte.is_ascii_digit() || (byte == b'.' && self.next_byte().is_some_and(|next| next.is_ascii_digit()))
    }

    fn consume_number(&mut self, start: usize) -> Token {
        if self.peek(b'.') {
            self.pos += 1;
            self.consume_digits();
        } else {
            self.consume_digits();
            if self.peek(b'.') && self.next_byte().is_none_or(|byte| !matches!(byte, b'@' | b'&')) {
                self.pos += 1;
                self.consume_digits();
            }
        }

        // Longest match, not commitment: an `e`/`E` that is not followed by an exponent belongs to whatever comes next,
        // so `1end` is the number `1` and the `end` keyword rather than one malformed number token.
        let mantissa_end = self.pos;
        if self.current_byte().is_some_and(|byte| byte == b'e' || byte == b'E') {
            self.pos += 1;
            if self.current_byte().is_some_and(|byte| byte == b'+' || byte == b'-') {
                self.pos += 1;
            }
            if self.current_byte().is_some_and(|byte| byte.is_ascii_digit()) {
                self.consume_digits();
            } else {
                self.pos = mantissa_end;
            }
        }

        Token::new(TokenKind::Number, Span::from_usize(start, self.pos))
    }

    /// Consumes a string body after its opening quote.
    ///
    /// The stack holds contexts open above the outer string: `"` for a nested string, a bracket for interpolation
    /// nesting. String and interpolation alternate. `)` closes the current interpolation even if `[`/`{` are still open
    /// — those unclosed brackets belong to the interpolation parse, not to string extent. Nested interpolations stack
    /// as far as they are written.
    fn consume_string(&mut self) -> TokenKind {
        let mut stack = Vec::new();
        while let Some(byte) = self.current_byte() {
            let current = self.pos;
            self.pos += 1;
            if stack.last().is_none_or(|context| *context == b'"') {
                if byte == b'(' && !self.backslash_run_len(self.start, current).is_multiple_of(2) {
                    stack.push(b'(');
                } else if byte == b'"' && self.backslash_run_len(self.start, current).is_multiple_of(2) {
                    // The outermost string ends the token; a nested one only returns to the interpolation that opened
                    // it.
                    if stack.pop().is_none() {
                        return TokenKind::String;
                    }
                }
                continue;
            }
            match byte {
                b'#' => self.consume_line_comment(),
                b'"' => stack.push(b'"'),
                b'(' | b'[' | b'{' => stack.push(byte),
                b')' => {
                    // `)` closes the interpolation even if `[`/`{` are still open; those belong to the interpolation
                    // parse.
                    while matches!(stack.last(), Some(&b'[' | &b'{')) {
                        stack.pop();
                    }
                    if stack.last() == Some(&b'(') {
                        stack.pop();
                    }
                }
                b']' if stack.last() == Some(&b'[') => {
                    stack.pop();
                }
                b'}' if stack.last() == Some(&b'{') => {
                    stack.pop();
                }
                _ => {}
            }
        }
        TokenKind::Error
    }

    fn consume_variable(&mut self) -> TokenKind {
        if !self.current_byte().is_some_and(is_ident_start) {
            self.consume_invalid_variable_spelling();
            return TokenKind::Error;
        }
        self.consume_ident_continue();
        while self.source[self.pos..self.end].starts_with("::") {
            if self.pos + 2 >= self.end || !is_ident_start(self.bytes()[self.pos + 2]) {
                self.pos += 2;
                self.consume_invalid_variable_spelling();
                return TokenKind::Error;
            }
            self.pos += 2;
            self.consume_ident_continue();
        }
        TokenKind::Variable
    }

    fn consume_invalid_variable_spelling(&mut self) {
        while self
            .current_byte()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b':'))
        {
            self.pos += 1;
        }
    }

    fn consume_ident(&mut self, start: usize) -> TokenKind {
        self.consume_ident_continue();
        match &self.source[start..self.pos] {
            "and" => TokenKind::And,
            "or" => TokenKind::Or,
            "as" => TokenKind::As,
            "def" => TokenKind::Def,
            "module" => TokenKind::Module,
            "import" => TokenKind::Import,
            "include" => TokenKind::Include,
            "if" => TokenKind::If,
            "then" => TokenKind::Then,
            "elif" => TokenKind::Elif,
            "else" => TokenKind::Else,
            "end" => TokenKind::End,
            "try" => TokenKind::Try,
            "catch" => TokenKind::Catch,
            "reduce" => TokenKind::Reduce,
            "foreach" => TokenKind::Foreach,
            "label" => TokenKind::Label,
            "break" => TokenKind::Break,
            "let" => TokenKind::Let,
            "empty" => TokenKind::Empty,
            "null" => TokenKind::Null,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            _ => TokenKind::Ident,
        }
    }

    fn bytes(&self) -> &[u8] {
        self.source.as_bytes()
    }

    fn current_byte(&self) -> Option<u8> {
        (self.pos < self.end).then(|| self.bytes()[self.pos])
    }

    fn next_byte(&self) -> Option<u8> {
        (self.pos + 1 < self.end).then(|| self.bytes()[self.pos + 1])
    }

    fn peek(&self, byte: u8) -> bool {
        self.current_byte() == Some(byte)
    }

    fn advance_char(&mut self) {
        self.pos += self.source[self.pos..self.end].chars().next().map_or(1, char::len_utf8);
    }

    fn consume_digits(&mut self) {
        while self.current_byte().is_some_and(|byte| byte.is_ascii_digit()) {
            self.pos += 1;
        }
    }

    fn consume_ident_continue(&mut self) {
        while self
            .current_byte()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            self.pos += 1;
        }
    }
}

impl Iterator for Lexer<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done { None } else { Some(self.token()) }
    }
}

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

/// Whitespace that may separate two tokens.
///
/// Deliberately narrower than [`u8::is_ascii_whitespace`], which also admits the form feed: only space, tab, line feed
/// and carriage return separate tokens here, so a form feed or vertical tab in program text is an invalid character
/// rather than silent trivia.
fn is_inter_token_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}
