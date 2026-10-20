//! Colors for the line as it's typed: commands green when they exist and
//! red when they don't, keywords magenta, quoted text yellow, variables
//! cyan, operators bold, comments gray. Only escape codes are added, so the
//! text keeps its width.

const GOOD: &str = "\x1b[32m";
const BAD: &str = "\x1b[31m";
const KEYWORD: &str = "\x1b[35m";
const STRING: &str = "\x1b[33m";
const VAR: &str = "\x1b[36m";
const OP: &str = "\x1b[1;36m";
const COMMENT: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

/// Words after which a command comes next.
const LEADING: &[&str] = &["if", "while", "else", "not", "!", "and", "or", "time"];

fn is_op(c: char) -> bool {
    matches!(c, '|' | '&' | ';' | '<' | '>')
}

pub fn highlight(chars: &[char], is_command: &dyn Fn(&str) -> bool) -> String {
    let mut out = String::new();
    let mut i = 0;
    let mut command_pos = true;
    let mut redirect = false;
    // `for x in ...`: x is a name, `in` a keyword, the rest plain words.
    let mut in_for = 0;
    let n = chars.len();
    while i < n {
        let c = chars[i];
        if c.is_whitespace() {
            if c == '\n' {
                command_pos = true;
                in_for = 0;
            }
            out.push(c);
            i += 1;
            continue;
        }
        if c == '#' {
            out.push_str(COMMENT);
            while i < n && chars[i] != '\n' {
                out.push(chars[i]);
                i += 1;
            }
            out.push_str(RESET);
            continue;
        }
        let redirect_2 = c == '2' && chars.get(i + 1) == Some(&'>');
        if is_op(c) || redirect_2 {
            out.push_str(OP);
            let start = i;
            if redirect_2 {
                out.push('2');
                i += 1;
            }
            while i < n && is_op(chars[i]) {
                out.push(chars[i]);
                i += 1;
            }
            // `2>&1`
            if chars[start..i].ends_with(&['>', '&']) && chars.get(i) == Some(&'1') {
                out.push('1');
                i += 1;
            }
            out.push_str(RESET);
            let op: String = chars[start..i].iter().collect();
            if op.contains(['|', ';']) || op == "&" || op == "&&" {
                command_pos = true;
                redirect = false;
                in_for = 0;
            } else if !op.ends_with("&1") {
                redirect = true;
            }
            continue;
        }
        // A word, up to whitespace or an operator. `plain` is its text
        // without quotes, to look up.
        let mut plain = String::new();
        let mut colored = String::new();
        while i < n && !chars[i].is_whitespace() && !is_op(chars[i]) {
            match chars[i] {
                '`' => {
                    colored.push('`');
                    if let Some(&next) = chars.get(i + 1) {
                        colored.push(next);
                        plain.push(next);
                    }
                    i += 2;
                }
                q @ ('\'' | '"') => {
                    colored.push_str(STRING);
                    colored.push(q);
                    i += 1;
                    while i < n && chars[i] != q {
                        if q == '"' && chars[i] == '$' {
                            i = variable(chars, i, &mut colored, STRING);
                            continue;
                        }
                        colored.push(chars[i]);
                        plain.push(chars[i]);
                        i += 1;
                    }
                    if i < n {
                        colored.push(q);
                        i += 1;
                    }
                    colored.push_str(RESET);
                }
                '$' => {
                    i = variable(chars, i, &mut colored, "");
                    plain.push('$');
                }
                ch => {
                    colored.push(ch);
                    plain.push(ch);
                    i += 1;
                }
            }
        }
        if redirect {
            redirect = false;
            out.push_str(&colored);
            continue;
        }
        if in_for > 0 {
            in_for += 1;
            if in_for == 3 && plain == "in" {
                out.push_str(&format!("{KEYWORD}{colored}{RESET}"));
            } else {
                out.push_str(&colored);
            }
            continue;
        }
        if command_pos {
            let word = plain.as_str();
            if LEADING.contains(&word) {
                out.push_str(&format!("{KEYWORD}{colored}{RESET}"));
                continue;
            }
            match word {
                "for" => {
                    out.push_str(&format!("{KEYWORD}{colored}{RESET}"));
                    in_for = 1;
                    command_pos = false;
                    continue;
                }
                "end" | "function" | "break" | "continue" | "return" => {
                    out.push_str(&format!("{KEYWORD}{colored}{RESET}"));
                    command_pos = false;
                    continue;
                }
                _ => {}
            }
            // `NAME=value` before a command leaves room for the command.
            if crate::shell::is_assignment_word(word) {
                out.push_str(&format!("{VAR}{colored}{RESET}"));
                continue;
            }
            command_pos = false;
            let color = if word.contains('$') || is_command(word) {
                GOOD
            } else {
                BAD
            };
            out.push_str(&format!("{color}{colored}{RESET}"));
            continue;
        }
        out.push_str(&colored);
    }
    out
}

/// Color `$NAME`, `${NAME}`, `$?`, `$(...)` starting at `i`, then switch
/// back to `after` (the color around it). Returns the index after it.
fn variable(chars: &[char], i: usize, out: &mut String, after: &str) -> usize {
    let n = chars.len();
    let mut j = i + 1;
    match chars.get(j) {
        Some('(') => {
            let mut depth = 0;
            while j < n {
                match chars[j] {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        Some('{') => {
            while j < n && chars[j] != '}' {
                j += 1;
            }
            j = (j + 1).min(n);
        }
        Some('?' | '@' | '*' | '#') => j += 1,
        _ => {
            while j < n && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
        }
    }
    out.push_str(VAR);
    out.extend(&chars[i..j]);
    out.push_str(RESET);
    out.push_str(after);
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for c in s.chars() {
            if c == '\x1b' {
                esc = true;
            } else if esc {
                if c.is_ascii_alphabetic() {
                    esc = false;
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn h(s: &str) -> String {
        highlight(&s.chars().collect::<Vec<_>>(), &|w| {
            ["echo", "ls", "grep"].contains(&w)
        })
    }

    #[test]
    fn keeps_the_text() {
        for line in [
            "echo \"hi $USER\" 'x' | grep -i a > out.txt 2>&1 # note",
            "for f in *.txt; echo $f; end",
            "X=1 nope $(ls | grep a) `\" ${HOME}",
            "echo \"unclosed",
        ] {
            assert_eq!(strip(&h(line)), line);
        }
    }

    #[test]
    fn colors_commands() {
        assert!(h("ls").starts_with(GOOD));
        assert!(h("nope").starts_with(BAD));
        let line = h("if ls; nope; end");
        assert!(line.starts_with(KEYWORD));
        assert!(line.contains(&format!("{GOOD}ls")));
        assert!(line.contains(&format!("{BAD}nope")));
        // A redirection's target isn't a command.
        assert!(!h("echo > nope").contains(BAD));
        assert!(h("X=1 ls").contains(&format!("{GOOD}ls")));
        assert!(h("for x in a b").contains(&format!("{KEYWORD}in")));
    }
}
