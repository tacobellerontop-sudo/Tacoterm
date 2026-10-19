//! Command lines: a lexer that keeps Windows paths intact (backslash is just
//! a character; the escape is a backtick, as in PowerShell) and a parser for
//! `;` / `&` sequences, `&&` / `||`, pipes and redirections.

use std::collections::HashMap;

/// One piece of a word, expanded when the command runs (so `set X=1 && echo
/// $X` sees the new value).
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Lit(String),
    /// `$NAME`, `${NAME}` or `%NAME%`
    Var(String),
    /// `$?`, the last exit code
    Status,
    /// A leading unquoted `~`
    Home,
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

#[derive(Debug, Clone, PartialEq)]
pub struct Pipeline {
    pub commands: Vec<Command>,
}

/// How a pipeline connects to the one after it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Connector {
    /// `;`, `&` or end of line: always run the next one
    Then,
    /// `&&`: run the next one if this one succeeded
    And,
    /// `||`: run the next one if this one failed
    Or,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Script {
    pub items: Vec<(Pipeline, Connector)>,
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
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

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
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '#' {
            break;
        }
        let next = chars.get(i + 1).copied();
        let op = match (c, next) {
            ('|', Some('|')) => Some((Token::Or, 2)),
            ('|', _) => Some((Token::Pipe, 1)),
            ('&', Some('&')) => Some((Token::And, 2)),
            ('&', Some('>')) => {
                return Err(ParseError("`&>` isn't supported; use `> file 2>&1`".into()))
            }
            ('&', _) | (';', _) => Some((Token::Then, 1)),
            ('>', Some('>')) => Some((
                Token::Redirect(RedirOp::Out {
                    fd: 1,
                    append: true,
                }),
                2,
            )),
            ('>', _) => Some((
                Token::Redirect(RedirOp::Out {
                    fd: 1,
                    append: false,
                }),
                1,
            )),
            ('<', _) => Some((Token::Redirect(RedirOp::In), 1)),
            ('2', Some('>')) => {
                let rest: String = chars[i + 2..].iter().take(2).collect();
                if rest == "&1" {
                    Some((Token::Redirect(RedirOp::ErrToOut), 4))
                } else if rest.starts_with('>') {
                    Some((
                        Token::Redirect(RedirOp::Out {
                            fd: 2,
                            append: true,
                        }),
                        3,
                    ))
                } else {
                    Some((
                        Token::Redirect(RedirOp::Out {
                            fd: 2,
                            append: false,
                        }),
                        2,
                    ))
                }
            }
            _ => None,
        };
        if let Some((token, len)) = op {
            tokens.push(token);
            i += len;
            continue;
        }

        // A word: runs until unquoted whitespace or an operator.
        let mut parts = Vec::new();
        if c == '~'
            && next.is_none_or(|n| n == '/' || n == '\\' || n.is_whitespace() || is_special(n))
        {
            parts.push(Part::Home);
            i += 1;
        }
        while i < chars.len() {
            let c = chars[i];
            if c.is_whitespace() || is_special(c) {
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
                        .ok_or_else(|| ParseError("missing closing '".into()))?;
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
                            '$' | '%' => i = lex_var(&chars, i, &mut parts),
                            ch => {
                                push_lit(&mut parts, &ch.to_string());
                                i += 1;
                            }
                        }
                    }
                    if !closed {
                        return Err(ParseError("missing closing \"".into()));
                    }
                }
                '$' | '%' => i = lex_var(&chars, i, &mut parts),
                ch => {
                    push_lit(&mut parts, &ch.to_string());
                    i += 1;
                }
            }
        }
        tokens.push(Token::Word(Word(parts)));
    }
    Ok(tokens)
}

/// Lex `$NAME`, `${NAME}`, `$?` or `%NAME%` starting at `i`; anything else
/// is a literal `$` or `%`. Returns the index after it.
fn lex_var(chars: &[char], i: usize, parts: &mut Vec<Part>) -> usize {
    let c = chars[i];
    if c == '$' {
        match chars.get(i + 1) {
            Some('?') => {
                parts.push(Part::Status);
                return i + 2;
            }
            Some('{') => {
                if let Some(end) = chars[i + 2..].iter().position(|&ch| ch == '}') {
                    let name: String = chars[i + 2..i + 2 + end].iter().collect();
                    if !name.is_empty() {
                        parts.push(Part::Var(name));
                        return i + 3 + end;
                    }
                }
            }
            Some(&n) if is_var_char(n) => {
                let len = chars[i + 1..]
                    .iter()
                    .take_while(|&&ch| is_var_char(ch))
                    .count();
                let name: String = chars[i + 1..i + 1 + len].iter().collect();
                parts.push(Part::Var(name));
                return i + 1 + len;
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
                return i + 2 + end;
            }
        }
    }
    push_lit(parts, &c.to_string());
    i + 1
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

pub fn parse(line: &str, aliases: &HashMap<String, String>) -> Result<Script, ParseError> {
    let tokens = expand_aliases(lex(line)?, aliases, &mut Vec::new())?;
    let mut script = Script::default();
    let mut pipeline = Pipeline {
        commands: Vec::new(),
    };
    let mut command = Command::default();
    let mut tokens = tokens.into_iter().peekable();

    fn finish_command(
        pipeline: &mut Pipeline,
        command: &mut Command,
        op: &str,
    ) -> Result<(), ParseError> {
        if command.words.is_empty() {
            return Err(ParseError(format!("nothing before `{op}`")));
        }
        pipeline.commands.push(std::mem::take(command));
        Ok(())
    }

    while let Some(token) = tokens.next() {
        match token {
            Token::Word(w) => command.words.push(w),
            Token::Redirect(op) => {
                let redirect = match op {
                    RedirOp::ErrToOut => Redirect::ErrToOut,
                    _ => {
                        let Some(Token::Word(target)) = tokens.next() else {
                            return Err(ParseError("a redirection needs a file name".into()));
                        };
                        match op {
                            RedirOp::Out { fd, append } => Redirect::Out { fd, append, target },
                            _ => Redirect::In(target),
                        }
                    }
                };
                command.redirects.push(redirect);
            }
            Token::Pipe => {
                finish_command(&mut pipeline, &mut command, "|")?;
                if tokens.peek().is_none() {
                    return Err(ParseError("nothing after `|`".into()));
                }
            }
            Token::And | Token::Or | Token::Then => {
                let (connector, op) = match token {
                    Token::And => (Connector::And, "&&"),
                    Token::Or => (Connector::Or, "||"),
                    _ => (Connector::Then, ";"),
                };
                if command.words.is_empty()
                    && pipeline.commands.is_empty()
                    && connector == Connector::Then
                {
                    // Stray `;` (an empty statement) is harmless.
                    continue;
                }
                finish_command(&mut pipeline, &mut command, op)?;
                if connector != Connector::Then && tokens.peek().is_none() {
                    return Err(ParseError(format!("nothing after `{op}`")));
                }
                script.items.push((
                    std::mem::replace(
                        &mut pipeline,
                        Pipeline {
                            commands: Vec::new(),
                        },
                    ),
                    connector,
                ));
            }
        }
    }
    if !command.words.is_empty() {
        pipeline.commands.push(command);
    } else if !command.redirects.is_empty() {
        return Err(ParseError("a redirection with no command".into()));
    }
    if !pipeline.commands.is_empty() {
        script.items.push((pipeline, Connector::Then));
    }
    Ok(script)
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

    #[test]
    fn windows_paths_keep_backslashes() {
        let s = p(r"cd C:\Users\lx\My` Docs");
        assert_eq!(
            s.items[0].0.commands[0].words,
            vec![lit("cd"), lit(r"C:\Users\lx\My Docs")]
        );
    }

    #[test]
    fn quotes_and_variables() {
        let s = p(r#"echo "hi $USER" '$x' %TEMP%\a ${A_B}c $? 100%"#);
        let w = &s.items[0].0.commands[0].words;
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
    fn empty_quotes_are_a_word() {
        let s = p(r#"echo "" ''"#);
        assert_eq!(s.items[0].0.commands[0].words.len(), 3);
    }

    #[test]
    fn tilde_only_at_word_start() {
        let s = p("cd ~/src a~b ~");
        let w = &s.items[0].0.commands[0].words;
        assert_eq!(w[1], Word(vec![Part::Home, Part::Lit("/src".into())]));
        assert_eq!(w[2], lit("a~b"));
        assert_eq!(w[3], Word(vec![Part::Home]));
    }

    #[test]
    fn pipes_and_connectors() {
        let s = p("a | b && c || d; e & f # comment");
        let shape: Vec<(usize, Connector)> = s
            .items
            .iter()
            .map(|(pl, c)| (pl.commands.len(), *c))
            .collect();
        assert_eq!(
            shape,
            vec![
                (2, Connector::And),
                (1, Connector::Or),
                (1, Connector::Then),
                (1, Connector::Then),
                (1, Connector::Then)
            ]
        );
    }

    #[test]
    fn redirections() {
        let s = p("prog <in.txt >out.txt 2>>err.txt 2>&1");
        let c = &s.items[0].0.commands[0];
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
        let c = &s.items[0].0.commands[0];
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
    fn errors() {
        let none = HashMap::new();
        for bad in ["| a", "a |", "a &&", "echo 'x", "echo \"x", "a >", "> f"] {
            assert!(parse(bad, &none).is_err(), "{bad} should not parse");
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
        let first = &s.items[0].0.commands;
        assert_eq!(first.len(), 2);
        // `ls` inside `ll` expands too, but not recursively into itself.
        assert_eq!(first[0].words, vec![lit("ls"), lit("--color"), lit("-a")]);
        assert_eq!(s.items[1].0.commands[0].words, vec![lit("echo"), lit("ll")]);
    }
}
