//! `$((…))` and `math`: whole-number arithmetic with `+ - * / % **`,
//! comparisons (1 or 0), `&& || !` and parentheses. Names are variables
//! (`x` or `$x`); an unset one is 0.

pub fn eval(expr: &str, var: &dyn Fn(&str) -> Option<String>) -> Result<i64, String> {
    let tokens = tokenize(expr)?;
    let mut p = Parser {
        tokens,
        pos: 0,
        var,
    };
    let value = p.or()?;
    match p.tokens.get(p.pos) {
        None => Ok(value),
        Some(t) => Err(format!("unexpected `{t}`")),
    }
}

fn tokenize(expr: &str) -> Result<Vec<String>, String> {
    let chars: Vec<char> = expr.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if c.is_ascii_digit() {
            while i < chars.len() && chars[i].is_ascii_alphanumeric() {
                i += 1;
            }
        } else if c.is_ascii_alphabetic() || c == '_' || c == '$' {
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
        } else {
            let two: String = chars[i..].iter().take(2).collect();
            i += if ["**", "<=", ">=", "==", "!=", "&&", "||"].contains(&two.as_str()) {
                2
            } else if "+-*/%()<>!".contains(c) {
                1
            } else {
                return Err(format!("unexpected `{c}`"));
            };
        }
        tokens.push(chars[start..i].iter().collect());
    }
    Ok(tokens)
}

struct Parser<'a> {
    tokens: Vec<String>,
    pos: usize,
    var: &'a dyn Fn(&str) -> Option<String>,
}

impl Parser<'_> {
    fn eat(&mut self, ops: &[&str]) -> Option<String> {
        let t = self.tokens.get(self.pos)?;
        if ops.contains(&t.as_str()) {
            self.pos += 1;
            Some(t.clone())
        } else {
            None
        }
    }

    fn or(&mut self) -> Result<i64, String> {
        let mut v = self.and()?;
        while self.eat(&["||"]).is_some() {
            let r = self.and()?;
            v = (v != 0 || r != 0) as i64;
        }
        Ok(v)
    }

    fn and(&mut self) -> Result<i64, String> {
        let mut v = self.compare()?;
        while self.eat(&["&&"]).is_some() {
            let r = self.compare()?;
            v = (v != 0 && r != 0) as i64;
        }
        Ok(v)
    }

    fn compare(&mut self) -> Result<i64, String> {
        let mut v = self.sum()?;
        while let Some(op) = self.eat(&["<", ">", "<=", ">=", "==", "!="]) {
            let r = self.sum()?;
            v = match op.as_str() {
                "<" => v < r,
                ">" => v > r,
                "<=" => v <= r,
                ">=" => v >= r,
                "==" => v == r,
                _ => v != r,
            } as i64;
        }
        Ok(v)
    }

    fn sum(&mut self) -> Result<i64, String> {
        let mut v = self.product()?;
        while let Some(op) = self.eat(&["+", "-"]) {
            let r = self.product()?;
            v = if op == "+" {
                v.wrapping_add(r)
            } else {
                v.wrapping_sub(r)
            };
        }
        Ok(v)
    }

    fn product(&mut self) -> Result<i64, String> {
        let mut v = self.power()?;
        while let Some(op) = self.eat(&["*", "/", "%"]) {
            let r = self.power()?;
            v = match op.as_str() {
                "*" => v.wrapping_mul(r),
                "/" => v.checked_div(r).ok_or("division by zero")?,
                _ => v.checked_rem(r).ok_or("division by zero")?,
            };
        }
        Ok(v)
    }

    fn power(&mut self) -> Result<i64, String> {
        let base = self.unary()?;
        if self.eat(&["**"]).is_some() {
            let exp = self.power()?;
            return Ok(base.wrapping_pow(exp.clamp(0, u32::MAX as i64) as u32));
        }
        Ok(base)
    }

    fn unary(&mut self) -> Result<i64, String> {
        match self.eat(&["-", "+", "!"]).as_deref() {
            Some("-") => Ok(self.unary()?.wrapping_neg()),
            Some("+") => self.unary(),
            Some(_) => Ok((self.unary()? == 0) as i64),
            None => self.atom(),
        }
    }

    fn atom(&mut self) -> Result<i64, String> {
        let Some(t) = self.tokens.get(self.pos).cloned() else {
            return Err("unfinished expression".into());
        };
        self.pos += 1;
        if t == "(" {
            let v = self.or()?;
            return match self.eat(&[")"]) {
                Some(_) => Ok(v),
                None => Err("missing )".into()),
            };
        }
        if t.starts_with(|c: char| c.is_ascii_digit()) {
            return number(&t);
        }
        if t.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '$') {
            let name = t.trim_start_matches('$');
            let value = (self.var)(name).unwrap_or_default();
            let value = value.trim();
            return if value.is_empty() {
                Ok(0)
            } else {
                number(value).map_err(|_| format!("{name} isn't a number: {value}"))
            };
        }
        Err(format!("unexpected `{t}`"))
    }
}

fn number(t: &str) -> Result<i64, String> {
    let (digits, radix) = match t.get(..2) {
        Some("0x") | Some("0X") => (&t[2..], 16),
        Some("0b") | Some("0B") => (&t[2..], 2),
        _ => (t, 10),
    };
    i64::from_str_radix(digits, radix).map_err(|_| format!("not a number: {t}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(s: &str) -> Result<i64, String> {
        eval(s, &|name| match name {
            "x" => Some("7".into()),
            "word" => Some("abc".into()),
            _ => None,
        })
    }

    #[test]
    fn arithmetic() {
        assert_eq!(e("1 + 2 * 3"), Ok(7));
        assert_eq!(e("(1 + 2) * 3"), Ok(9));
        assert_eq!(e("2 ** 3 ** 2"), Ok(512));
        assert_eq!(e("-x + $x % 4"), Ok(-4));
        assert_eq!(e("10 / 3"), Ok(3));
        assert_eq!(e("x > 5 && !(x == 8)"), Ok(1));
        assert_eq!(e("0x10 + 0b11 + unset"), Ok(19));
        assert!(e("1 / 0").is_err());
        assert!(e("word + 1").is_err());
        assert!(e("1 +").is_err());
        assert!(e("(1").is_err());
        assert!(e("1 2").is_err());
    }
}
