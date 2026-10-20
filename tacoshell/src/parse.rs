//! Command lines: a lexer that keeps Windows paths intact (backslash is just
//! a character; the escape is a backtick, as in PowerShell) and a parser for
//! `;` / `&` / newline sequences, `&&` / `||`, pipes, redirections, and
//! fish-style blocks: `if … else … end`, `for x in …; … end`,
//! `while …; … end` and `function name; … end`.

use std::collections::HashMap;

/// One piece of a word, expanded when the command runs (so `set X=1 && echo
/// $X` sees the new value).
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Lit(String),
    /// An unquoted `*` or `?`, for matching file names
    Wild(char),
    /// `$NAME`, `${NAME}` or `%NAME%`
    Var(String),
    /// `$@`, `$*` or `$argv`: a function's arguments, one word each unless
    /// quoted
    AllArgs {
        quoted: bool,
    },
    /// `$?`, the last exit code
    Status,
    /// A leading unquoted `~`
    Home,
    /// `{a,b,c}`: one word per choice
    Brace(Vec<Word>),
    /// `$(command)`: its output; one word per line unless quoted
    Subst {
        script: String,
        quoted: bool,
    },
    /// `$((1 + 2))`
    Arith(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Word(pub Vec<Part>);

impl Word {
    /// The word's text if it has no expansions in it.
    pub fn literal(&self) -> Option<String> {
        let mut s = String::new();
        for part in &self.0 {
            match part {
                Part::Lit(t) => s.push_str(t),
                _ => return None,
            }
        }
        Some(s)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Redirect {
    /// `>` / `>>` (fd 1) and `2>` / `2>>` (fd 2)
    Out { fd: u8, append: bool, target: Word },
    /// `<`
    In(Word),
    /// `2>&1`
    ErrToOut,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Command {
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pipeline {
    /// `! cmd` or `not cmd`: success and failure swap
    pub negate: bool,
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Pipeline(Pipeline),
    If {
        cond: Script,
        then: Script,
        otherwise: Option<Script>,
    },
    While {
        cond: Script,
        body: Script,
    },
    For {
        var: String,
        items: Vec<Word>,
        body: Script,
    },
    Function {
        name: String,
        body: Script,
    },
}

/// How a statement connects to the one after it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Connector {
    /// `;`, `&`, a new line, or the end: always run the next one
    Then,
    /// `&&`: run the next one if this one succeeded
    And,
    /// `||`: run the next one if this one failed
    Or,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Script {
    pub items: Vec<(Stmt, Connector)>,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(Word),
    Pipe,
    And,
    Or,
    Then,
    Redirect(RedirOp),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum RedirOp {
    Out { fd: u8, append: bool },
    In,
    ErrToOut,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    /// The line just isn't finished (an open quote or block, a trailing
    /// `|`): more lines can complete it.
    pub incomplete: bool,
}

impl ParseError {
    fn new(message: impl Into<String>) -> ParseError {
        ParseError {
            message: message.into(),
            incomplete: false,
        }
    }

    fn incomplete(message: impl Into<String>) -> ParseError {
        ParseError {
            message: message.into(),
            incomplete: true,
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

const KEYWORDS: &[&str] = &["if", "else", "end", "for", "while", "function"];

fn is_var_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_special(c: char) -> bool {
    matches!(c, '|' | '&' | ';' | '<' | '>')
}

/// Push text onto a word, merging it with a literal before it.
fn push_lit(parts: &mut Vec<Part>, text: &str) {
    if let Some(Part::Lit(s)) = parts.last_mut() {
        s.push_str(text);
    } else {
        parts.push(Part::Lit(text.to_string()));
    }
}

fn lex(line: &str) -> Result<Vec<Token>, ParseError> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            tokens.push(Token::Then);
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '#' {
            // A comment runs to the end of the line.
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        let next = chars.get(i + 1).copied();
        let out = |fd, append| Token::Redirect(RedirOp::Out { fd, append });
        let op = match (c, next) {
            ('|', Some('|')) => Some((Token::Or, 2)),
            ('|', _) => Some((Token::Pipe, 1)),
            ('&', Some('&')) => Some((Token::And, 2)),
            ('&', Some('>')) => {
                return Err(ParseError::new("`&>` isn't supported; use `> file 2>&1`"))
            }
            ('&', _) | (';', _) => Some((Token::Then, 1)),
            ('>', Some('>')) => Some((out(1, true), 2)),
            ('>', _) => Some((out(1, false), 1)),
            ('<', _) => Some((Token::Redirect(RedirOp::In), 1)),
            ('2', Some('>')) => {
                let rest: String = chars[i + 2..].iter().take(2).collect();
                if rest == "&1" {
                    Some((Token::Redirect(RedirOp::ErrToOut), 4))
                } else if rest.starts_with('>') {
                    Some((out(2, true), 3))
                } else {
                    Some((out(2, false), 2))
                }
            }
            _ => None,
        };
        if let Some((token, len)) = op {
            tokens.push(token);
            i += len;
            continue;
        }
        let (word, end) = lex_word(&chars, i, true)?;
        tokens.push(Token::Word(word));
        i = end;
    }
    Ok(tokens)
}

/// Lex one word starting at `i`: it runs until unquoted whitespace or an
/// operator (or, when `stop_at_ops` is false, to the end of `chars`).
fn lex_word(chars: &[char], mut i: usize, stop_at_ops: bool) -> Result<(Word, usize), ParseError> {
    let mut parts = Vec::new();
    if chars[i] == '~'
        && chars
            .get(i + 1)
            .is_none_or(|&n| n == '/' || n == '\\' || n.is_whitespace() || is_special(n))
    {
        parts.push(Part::Home);
        i += 1;
    }
    while i < chars.len() {
        let c = chars[i];
        if stop_at_ops && (c.is_whitespace() || is_special(c)) {
            break;
        }
        match c {
            '`' => {
                if let Some(&n) = chars.get(i + 1) {
                    push_lit(&mut parts, &n.to_string());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            '\'' => {
                let end = chars[i + 1..]
                    .iter()
                    .position(|&ch| ch == '\'')
                    .ok_or_else(|| ParseError::incomplete("missing closing '"))?;
                let text: String = chars[i + 1..i + 1 + end].iter().collect();
                // An empty '' still makes a (empty) word.
                push_lit(&mut parts, &text);
                i += end + 2;
            }
            '"' => {
                i += 1;
                let mut closed = false;
                push_lit(&mut parts, "");
                while i < chars.len() {
                    match chars[i] {
                        '"' => {
                            closed = true;
                            i += 1;
                            break;
                        }
                        '`' if i + 1 < chars.len() => {
                            push_lit(&mut parts, &chars[i + 1].to_string());
                            i += 2;
                        }
                        '$' | '%' => i = lex_var(chars, i, true, &mut parts)?,
                        ch => {
                            push_lit(&mut parts, &ch.to_string());
                            i += 1;
                        }
                    }
                }
                if !closed {
                    return Err(ParseError::incomplete("missing closing \""));
                }
            }
            '$' | '%' => i = lex_var(chars, i, false, &mut parts)?,
            '*' | '?' => {
                parts.push(Part::Wild(c));
                i += 1;
            }
            '{' => match brace_choices(chars, i)? {
                Some((choices, end)) => {
                    parts.push(Part::Brace(choices));
                    i = end;
                }
                None => {
                    push_lit(&mut parts, "{");
                    i += 1;
                }
            },
            ch => {
                push_lit(&mut parts, &ch.to_string());
                i += 1;
            }
        }
    }
    Ok((Word(parts), i))
}

/// Where the quote, paren or brace opened at `open` closes, skipping
/// quoted text and nested pairs.
fn matching(chars: &[char], open: usize, (o, c): (char, char)) -> Option<usize> {
    let mut depth = 0;
    let mut quote: Option<char> = None;
    let mut i = open;
    while i < chars.len() {
        let ch = chars[i];
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch == '`' => i += 1,
            None if ch == o => depth += 1,
            None if ch == c => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            None => {}
        }
        i += 1;
    }
    None
}

/// `{a,b,c}` at `open`: its choices and the index after it, or `None` when
/// it's just a `{` (no comma, no closing brace, or spaces inside).
fn brace_choices(chars: &[char], open: usize) -> Result<Option<(Vec<Word>, usize)>, ParseError> {
    let Some(close) = matching(chars, open, ('{', '}')) else {
        return Ok(None);
    };
    let inner = &chars[open + 1..close];
    if inner.iter().any(|c| c.is_whitespace()) {
        return Ok(None);
    }
    // Split at commas outside nested braces and quotes.
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < inner.len() {
        match inner[i] {
            '{' => i = matching(inner, i, ('{', '}')).unwrap_or(i),
            '\'' | '"' => {
                let q = inner[i];
                i += inner[i + 1..]
                    .iter()
                    .position(|&c| c == q)
                    .map_or(0, |p| p + 1);
            }
            ',' => {
                pieces.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if pieces.is_empty() {
        return Ok(None);
    }
    pieces.push(&inner[start..]);
    let mut choices = Vec::new();
    for piece in pieces {
        choices.push(if piece.is_empty() {
            Word::default()
        } else {
            lex_word(piece, 0, false)?.0
        });
    }
    Ok(Some((choices, close + 1)))
}

/// Lex `$NAME`, `${NAME}`, `$?`, `$@`, `$(…)`, `$((…))` or `%NAME%`
/// starting at `i`; anything else is a literal `$` or `%`. Returns the
/// index after it.
fn lex_var(
    chars: &[char],
    i: usize,
    quoted: bool,
    parts: &mut Vec<Part>,
) -> Result<usize, ParseError> {
    let c = chars[i];
    if c == '$' {
        match chars.get(i + 1) {
            Some('?') => {
                parts.push(Part::Status);
                return Ok(i + 2);
            }
            Some('@') | Some('*') => {
                parts.push(Part::AllArgs { quoted });
                return Ok(i + 2);
            }
            Some('#') => {
                parts.push(Part::Var("#".into()));
                return Ok(i + 2);
            }
            Some('(') => {
                let close = matching(chars, i + 1, ('(', ')'))
                    .ok_or_else(|| ParseError::incomplete("missing closing )"))?;
                let inner: String = chars[i + 2..close].iter().collect();
                if inner.starts_with('(') && inner.ends_with(')') && inner.len() >= 2 {
                    parts.push(Part::Arith(inner[1..inner.len() - 1].to_string()));
                } else {
                    parts.push(Part::Subst {
                        script: inner,
                        quoted,
                    });
                }
                return Ok(close + 1);
            }
            Some('{') => {
                if let Some(end) = chars[i + 2..].iter().position(|&ch| ch == '}') {
                    let name: String = chars[i + 2..i + 2 + end].iter().collect();
                    if !name.is_empty() {
                        parts.push(Part::Var(name));
                        return Ok(i + 3 + end);
                    }
                }
            }
            Some(&n) if is_var_char(n) => {
                let len = chars[i + 1..]
                    .iter()
                    .take_while(|&&ch| is_var_char(ch))
                    .count();
                let name: String = chars[i + 1..i + 1 + len].iter().collect();
                parts.push(if name == "argv" {
                    Part::AllArgs { quoted }
                } else {
                    Part::Var(name)
                });
                return Ok(i + 1 + len);
            }
            _ => {}
        }
    } else {
        // %NAME%, as in cmd. Names may hold anything but %, spaces and quotes
        // (cmd allows `%ProgramFiles(x86)%`).
        let rest = &chars[i + 1..];
        if let Some(end) = rest
            .iter()
            .position(|&ch| ch == '%' || ch.is_whitespace() || ch == '"')
        {
            if rest[end] == '%' && end > 0 {
                let name: String = rest[..end].iter().collect();
                parts.push(Part::Var(name));
                return Ok(i + 2 + end);
            }
        }
    }
    push_lit(parts, &c.to_string());
    Ok(i + 1)
}

/// Expand aliases: a word at the start of a command that is a plain name in
/// `aliases` is replaced by the alias's tokens (which may hold pipes and
/// such). An alias isn't expanded inside its own expansion.
fn expand_aliases(
    tokens: Vec<Token>,
    aliases: &HashMap<String, String>,
    active: &mut Vec<String>,
) -> Result<Vec<Token>, ParseError> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut at_start = true;
    for token in tokens {
        let starts_next = matches!(token, Token::Pipe | Token::And | Token::Or | Token::Then);
        if let (true, Token::Word(word)) = (at_start, &token) {
            if let Some(name) = word.literal() {
                let key = alias_key(&name);
                if let Some(value) = aliases.get(&key).filter(|_| !active.contains(&key)) {
                    active.push(key);
                    let inner = expand_aliases(lex(value)?, aliases, active)?;
                    active.pop();
                    out.extend(inner);
                    at_start = false;
                    continue;
                }
            }
        }
        // A word after a redirection operator is its target, not a command.
        at_start = starts_next || (at_start && matches!(token, Token::Redirect(_)));
        out.push(token);
    }
    Ok(out)
}

/// Alias names, like command names on Windows, ignore case.
pub fn alias_key(name: &str) -> String {
    if cfg!(windows) {
        name.to_lowercase()
    } else {
        name.to_string()
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn keyword(&self) -> Option<String> {
        match self.peek() {
            Some(Token::Word(w)) => w.literal().filter(|k| KEYWORDS.contains(&k.as_str())),
            _ => None,
        }
    }

    fn skip_newlines(&mut self) {
        while self.peek() == Some(&Token::Then) {
            self.pos += 1;
        }
    }

    /// Statements up to one of `ends` (which is consumed and returned), or
    /// to the end of input when `ends` is empty.
    fn sequence(&mut self, ends: &[&str]) -> Result<(Script, Option<String>), ParseError> {
        let mut script = Script::default();
        loop {
            self.skip_newlines();
            if self.peek().is_none() {
                return match ends.last() {
                    Some(_) => Err(ParseError::incomplete("missing `end`")),
                    None => Ok((script, None)),
                };
            }
            if let Some(k) = self.keyword() {
                if ends.contains(&k.as_str()) {
                    self.pos += 1;
                    return Ok((script, Some(k)));
                }
                if k == "end" || k == "else" {
                    return Err(ParseError::new(format!("`{k}` without a matching block")));
                }
            }
            let stmt = self.statement()?;
            let connector = self.connector()?;
            script.items.push((stmt, connector));
        }
    }

    /// What follows a statement: `&&`, `||`, or a separator.
    fn connector(&mut self) -> Result<Connector, ParseError> {
        let (connector, op) = match self.peek() {
            Some(Token::And) => (Connector::And, "&&"),
            Some(Token::Or) => (Connector::Or, "||"),
            Some(Token::Then) => {
                self.pos += 1;
                return Ok(Connector::Then);
            }
            None => return Ok(Connector::Then),
            Some(_) => return Err(ParseError::new("unexpected text after a block")),
        };
        self.pos += 1;
        self.skip_newlines();
        if self.peek().is_none() {
            return Err(ParseError::incomplete(format!("nothing after `{op}`")));
        }
        Ok(connector)
    }

    /// Pipelines joined by `&&` / `||` up to a separator: an `if` or
    /// `while` condition.
    fn condition(&mut self) -> Result<Script, ParseError> {
        let mut script = Script::default();
        loop {
            if self.peek().is_none() {
                return Err(ParseError::incomplete("missing `end`"));
            }
            let pipeline = self.pipeline()?;
            let connector = match self.peek() {
                Some(Token::And) => Connector::And,
                Some(Token::Or) => Connector::Or,
                _ => {
                    script
                        .items
                        .push((Stmt::Pipeline(pipeline), Connector::Then));
                    return Ok(script);
                }
            };
            self.pos += 1;
            self.skip_newlines();
            script.items.push((Stmt::Pipeline(pipeline), connector));
        }
    }

    fn statement(&mut self) -> Result<Stmt, ParseError> {
        match self.keyword().as_deref() {
            Some("if") => {
                self.pos += 1;
                let cond = self.condition()?;
                let (then, end) = self.sequence(&["else", "end"])?;
                let otherwise = if end.as_deref() == Some("else") {
                    if self.keyword().as_deref() == Some("if") {
                        // `else if …` shares the outer block's `end`.
                        let nested = self.statement()?;
                        Some(Script {
                            items: vec![(nested, Connector::Then)],
                        })
                    } else {
                        Some(self.sequence(&["end"])?.0)
                    }
                } else {
                    None
                };
                Ok(Stmt::If {
                    cond,
                    then,
                    otherwise,
                })
            }
            Some("while") => {
                self.pos += 1;
                let cond = self.condition()?;
                let body = self.sequence(&["end"])?.0;
                Ok(Stmt::While { cond, body })
            }
            Some("for") => {
                self.pos += 1;
                let var = self.name("for needs a variable name: for x in a b c")?;
                match self.peek() {
                    Some(Token::Word(w)) if w.literal().as_deref() == Some("in") => self.pos += 1,
                    None => return Err(ParseError::incomplete("missing `in`")),
                    _ => return Err(ParseError::new("for x in a b c; …; end")),
                }
                let mut items = Vec::new();
                while let Some(Token::Word(w)) = self.peek() {
                    items.push(w.clone());
                    self.pos += 1;
                }
                let body = self.sequence(&["end"])?.0;
                Ok(Stmt::For { var, items, body })
            }
            Some("function") => {
                self.pos += 1;
                let name = self.name("function needs a name: function greet; …; end")?;
                let body = self.sequence(&["end"])?.0;
                Ok(Stmt::Function { name, body })
            }
            _ => Ok(Stmt::Pipeline(self.pipeline()?)),
        }
    }

    fn name(&mut self, usage: &str) -> Result<String, ParseError> {
        match self.peek() {
            Some(Token::Word(w)) => match w.literal() {
                Some(name) if !name.is_empty() => {
                    self.pos += 1;
                    Ok(name)
                }
                _ => Err(ParseError::new(usage)),
            },
            None => Err(ParseError::incomplete(usage)),
            _ => Err(ParseError::new(usage)),
        }
    }

    fn pipeline(&mut self) -> Result<Pipeline, ParseError> {
        let mut pipeline = Pipeline::default();
        if let Some(Token::Word(w)) = self.peek() {
            if matches!(w.literal().as_deref(), Some("!") | Some("not")) {
                pipeline.negate = true;
                self.pos += 1;
            }
        }
        loop {
            let command = self.command()?;
            if command.words.is_empty() {
                let what = match self.peek() {
                    Some(Token::Pipe) => "nothing before `|`",
                    Some(Token::And) => "nothing before `&&`",
                    Some(Token::Or) => "nothing before `||`",
                    _ if !command.redirects.is_empty() => "a redirection with no command",
                    _ => "missing command",
                };
                return Err(ParseError::new(what));
            }
            pipeline.commands.push(command);
            if self.peek() != Some(&Token::Pipe) {
                return Ok(pipeline);
            }
            self.pos += 1;
            self.skip_newlines();
            if self.peek().is_none() {
                return Err(ParseError::incomplete("nothing after `|`"));
            }
        }
    }

    fn command(&mut self) -> Result<Command, ParseError> {
        let mut command = Command::default();
        loop {
            match self.peek().cloned() {
                Some(Token::Word(w)) => {
                    command.words.push(w);
                    self.pos += 1;
                }
                Some(Token::Redirect(op)) => {
                    self.pos += 1;
                    let redirect = match op {
                        RedirOp::ErrToOut => Redirect::ErrToOut,
                        _ => {
                            let Some(Token::Word(target)) = self.peek().cloned() else {
                                return Err(ParseError::new("a redirection needs a file name"));
                            };
                            self.pos += 1;
                            match op {
                                RedirOp::Out { fd, append } => Redirect::Out { fd, append, target },
                                _ => Redirect::In(target),
                            }
                        }
                    };
                    command.redirects.push(redirect);
                }
                _ => return Ok(command),
            }
        }
    }
}

pub fn parse(line: &str, aliases: &HashMap<String, String>) -> Result<Script, ParseError> {
    let tokens = expand_aliases(lex(line)?, aliases, &mut Vec::new())?;
    let mut parser = Parser { tokens, pos: 0 };
    Ok(parser.sequence(&[])?.0)
}

/// Whether `text` is an unfinished command that more lines could complete.
pub fn is_incomplete(text: &str, aliases: &HashMap<String, String>) -> bool {
    matches!(parse(text, aliases), Err(e) if e.incomplete)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: &str) -> Script {
        parse(line, &HashMap::new()).unwrap()
    }

    fn lit(s: &str) -> Word {
        Word(vec![Part::Lit(s.into())])
    }

    fn pipeline(s: &Script, i: usize) -> &Pipeline {
        match &s.items[i].0 {
            Stmt::Pipeline(p) => p,
            other => panic!("not a pipeline: {other:?}"),
        }
    }

    fn words(s: &Script) -> &Vec<Word> {
        &pipeline(s, 0).commands[0].words
    }

    #[test]
    fn windows_paths_keep_backslashes() {
        let s = p(r"cd C:\Users\lx\My` Docs");
        assert_eq!(words(&s), &vec![lit("cd"), lit(r"C:\Users\lx\My Docs")]);
    }

    #[test]
    fn quotes_and_variables() {
        let s = p(r#"echo "hi $USER" '$x' %TEMP%\a ${A_B}c $? 100%"#);
        let w = words(&s);
        assert_eq!(
            w[1],
            Word(vec![Part::Lit("hi ".into()), Part::Var("USER".into())])
        );
        assert_eq!(w[2], lit("$x"));
        assert_eq!(
            w[3],
            Word(vec![Part::Var("TEMP".into()), Part::Lit(r"\a".into())])
        );
        assert_eq!(
            w[4],
            Word(vec![Part::Var("A_B".into()), Part::Lit("c".into())])
        );
        assert_eq!(w[5], Word(vec![Part::Status]));
        assert_eq!(w[6], lit("100%"));
    }

    #[test]
    fn wildcards_braces_and_substitutions() {
        let s =
            p(r#"ls *.r? "*" a{b,c{d,e},}f $(pwd) "$(ls | sort)" $((1 + 2)) $@ "$argv" {x} {a b}"#);
        let w = words(&s);
        assert_eq!(
            w[1],
            Word(vec![
                Part::Wild('*'),
                Part::Lit(".r".into()),
                Part::Wild('?')
            ])
        );
        assert_eq!(w[2], lit("*"));
        let Part::Brace(choices) = &w[3].0[1] else {
            panic!("{:?}", w[3])
        };
        assert_eq!(choices.len(), 3);
        assert_eq!(choices[2], Word::default());
        assert_eq!(
            w[4],
            Word(vec![Part::Subst {
                script: "pwd".into(),
                quoted: false
            }])
        );
        assert_eq!(
            w[5],
            Word(vec![
                Part::Lit("".into()),
                Part::Subst {
                    script: "ls | sort".into(),
                    quoted: true
                }
            ])
        );
        assert_eq!(w[6], Word(vec![Part::Arith("1 + 2".into())]));
        assert_eq!(w[7], Word(vec![Part::AllArgs { quoted: false }]));
        assert_eq!(
            w[8],
            Word(vec![Part::Lit("".into()), Part::AllArgs { quoted: true }])
        );
        assert_eq!(w[9], lit("{x}"));
        assert_eq!(w[10], lit("{a"));
    }

    #[test]
    fn empty_quotes_are_a_word() {
        let s = p(r#"echo "" ''"#);
        assert_eq!(words(&s).len(), 3);
    }

    #[test]
    fn tilde_only_at_word_start() {
        let s = p("cd ~/src a~b ~");
        let w = words(&s);
        assert_eq!(w[1], Word(vec![Part::Home, Part::Lit("/src".into())]));
        assert_eq!(w[2], lit("a~b"));
        assert_eq!(w[3], Word(vec![Part::Home]));
    }

    #[test]
    fn pipes_and_connectors() {
        let s = p("a | b && c || d; e & f # comment\ng");
        let shape: Vec<(usize, Connector)> = (0..s.items.len())
            .map(|i| (pipeline(&s, i).commands.len(), s.items[i].1))
            .collect();
        assert_eq!(
            shape,
            vec![
                (2, Connector::And),
                (1, Connector::Or),
                (1, Connector::Then),
                (1, Connector::Then),
                (1, Connector::Then),
                (1, Connector::Then)
            ]
        );
    }

    #[test]
    fn redirections() {
        let s = p("prog <in.txt >out.txt 2>>err.txt 2>&1");
        let c = &pipeline(&s, 0).commands[0];
        assert_eq!(c.words, vec![lit("prog")]);
        assert_eq!(
            c.redirects,
            vec![
                Redirect::In(lit("in.txt")),
                Redirect::Out {
                    fd: 1,
                    append: false,
                    target: lit("out.txt")
                },
                Redirect::Out {
                    fd: 2,
                    append: true,
                    target: lit("err.txt")
                },
                Redirect::ErrToOut,
            ]
        );
    }

    #[test]
    fn a_2_inside_a_word_is_not_a_redirect() {
        let s = p("echo a2>x");
        let c = &pipeline(&s, 0).commands[0];
        assert_eq!(c.words, vec![lit("echo"), lit("a2")]);
        assert_eq!(
            c.redirects,
            vec![Redirect::Out {
                fd: 1,
                append: false,
                target: lit("x")
            }]
        );
    }

    #[test]
    fn blocks() {
        let s = p(
            "if test -f x; echo yes; else if true; echo maybe; else; echo no; end && echo after",
        );
        let Stmt::If {
            cond,
            then,
            otherwise,
        } = &s.items[0].0
        else {
            panic!()
        };
        assert_eq!(cond.items.len(), 1);
        assert_eq!(then.items.len(), 1);
        let Stmt::If {
            otherwise: inner, ..
        } = &otherwise.as_ref().unwrap().items[0].0
        else {
            panic!()
        };
        assert!(inner.is_some());
        assert_eq!(s.items[0].1, Connector::And);

        let s = p("for f in *.txt a b\n  echo $f\nend");
        let Stmt::For { var, items, body } = &s.items[0].0 else {
            panic!()
        };
        assert_eq!((var.as_str(), items.len(), body.items.len()), ("f", 3, 1));

        let s = p("function greet\n echo hi $argv\nend; while not false && true; break; end");
        assert!(matches!(&s.items[0].0, Stmt::Function { name, .. } if name == "greet"));
        let Stmt::While { cond, .. } = &s.items[1].0 else {
            panic!()
        };
        assert_eq!(cond.items.len(), 2);
        assert!(matches!(&cond.items[0].0, Stmt::Pipeline(p) if p.negate));
    }

    #[test]
    fn errors() {
        let none = HashMap::new();
        for bad in ["| a", "a >", "> f", "end", "if true; else; else; end"] {
            let e = parse(bad, &none).unwrap_err();
            assert!(!e.incomplete, "{bad} should be an error, not unfinished");
        }
        for unfinished in [
            "a |",
            "a &&",
            "echo 'x",
            "echo \"x",
            "if true",
            "for x in a b; echo",
            "echo $(ls",
        ] {
            assert!(
                is_incomplete(unfinished, &none),
                "{unfinished} should want more"
            );
        }
        assert!(parse(";; a ;", &none).is_ok());
        assert_eq!(parse("", &none).unwrap(), Script::default());
    }

    #[test]
    fn aliases_expand_at_command_start_only() {
        let mut aliases = HashMap::new();
        aliases.insert("ll".to_string(), "ls -a | sort".to_string());
        aliases.insert("ls".to_string(), "ls --color".to_string());
        let s = parse("ll && echo ll", &aliases).unwrap();
        let first = &pipeline(&s, 0).commands;
        assert_eq!(first.len(), 2);
        // `ls` inside `ll` expands too, but not recursively into itself.
        assert_eq!(first[0].words, vec![lit("ls"), lit("--color"), lit("-a")]);
        assert_eq!(
            pipeline(&s, 1).commands[0].words,
            vec![lit("echo"), lit("ll")]
        );
    }
}
