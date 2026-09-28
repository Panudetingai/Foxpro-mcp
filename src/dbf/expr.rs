//! A small FoxPro expression evaluator for record filters.
//!
//! Supported:
//! * literals: numbers, 'str' "str" [str], .T. .F. .NULL., {^2024-01-31}, {^2024-01-31 10:30:00}, {}
//! * operators: + - * / %, = == <> != # < > <= >= $, AND OR NOT (.AND. .OR. .NOT. !)
//! * functions: UPPER LOWER ALLTRIM TRIM RTRIM LTRIM LEFT RIGHT SUBSTR LEN AT ATC
//!   EMPTY ISNULL NVL IIF BETWEEN INLIST LIKE VAL STR INT ABS ROUND
//!   DTOS YEAR MONTH DAY DATE DELETED RECNO
//!
//! String `=` follows VFP's SET EXACT OFF rule (the left side must start with
//! the right side); `==` compares ignoring trailing blanks.

use super::value::Val;
use crate::error::{FoxProError, Result};
use chrono::{Datelike, Local, NaiveDate, NaiveDateTime};
use std::cmp::Ordering;

const MAX_DEPTH: usize = 64;
const MAX_EXPR_LEN: usize = 4096;
const MAX_TOKENS: usize = 512;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    Date(Option<NaiveDate>),
    DateTime(NaiveDateTime),
    Ident(String),
    Op(&'static str),
    LParen,
    RParen,
    Comma,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Lit(Val),
    Field(String),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
}

/// Data a filter is evaluated against.
pub trait Context {
    fn field(&mut self, name: &str) -> Result<Val>;
    fn recno(&self) -> u32;
    fn deleted(&self) -> bool;
}

fn err(msg: impl Into<String>) -> FoxProError {
    FoxProError::InvalidArgument(format!("expression: {}", msg.into()))
}

fn tokenize(src: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()))
        {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            toks.push(Tok::Num(
                s.parse().map_err(|_| err(format!("bad number {s}")))?,
            ));
            continue;
        }
        if c == '.' {
            // .T. .F. .AND. .OR. .NOT. .NULL.
            let end = chars[i + 1..]
                .iter()
                .position(|&ch| ch == '.')
                .map(|p| i + 1 + p)
                .ok_or_else(|| err("unterminated dotted keyword"))?;
            let word: String = chars[i + 1..end].iter().collect::<String>().to_uppercase();
            toks.push(match word.as_str() {
                "T" | "Y" => Tok::Bool(true),
                "F" | "N" => Tok::Bool(false),
                "NULL" => Tok::Null,
                "AND" => Tok::Op("AND"),
                "OR" => Tok::Op("OR"),
                "NOT" => Tok::Op("NOT"),
                _ => return Err(err(format!("unknown keyword .{word}."))),
            });
            i = end + 1;
            continue;
        }
        if matches!(c, '\'' | '"' | '[') {
            let close = if c == '[' { ']' } else { c };
            let end = chars[i + 1..]
                .iter()
                .position(|&ch| ch == close)
                .map(|p| i + 1 + p)
                .ok_or_else(|| err("unterminated string"))?;
            toks.push(Tok::Str(chars[i + 1..end].iter().collect()));
            i = end + 1;
            continue;
        }
        if c == '{' {
            let end = chars[i + 1..]
                .iter()
                .position(|&ch| ch == '}')
                .map(|p| i + 1 + p)
                .ok_or_else(|| err("unterminated date literal"))?;
            let body: String = chars[i + 1..end].iter().collect();
            let body = body.trim().trim_start_matches('^').trim();
            if body.is_empty() || body == "/" || body == "/:" || body == ":" {
                toks.push(Tok::Date(None));
            } else if let Ok(d) = NaiveDate::parse_from_str(body, "%Y-%m-%d")
                .or_else(|_| NaiveDate::parse_from_str(body, "%Y/%m/%d"))
            {
                toks.push(Tok::Date(Some(d)));
            } else {
                let dt = ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y/%m/%d %H:%M:%S"]
                    .iter()
                    .find_map(|f| NaiveDateTime::parse_from_str(body, f).ok())
                    .ok_or_else(|| {
                        err(format!("bad date literal {{{body}}}; use {{^YYYY-MM-DD}}"))
                    })?;
                toks.push(Tok::DateTime(dt));
            }
            i = end + 1;
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let mut word: String = chars[start..i].iter().collect();
            // alias.field / alias->field: keep only the field name.
            if chars.get(i) == Some(&'.') && chars.get(i + 1).is_some_and(|c| c.is_alphabetic()) {
                i += 1;
                let s = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                word = chars[s..i].iter().collect();
            } else if chars.get(i) == Some(&'-') && chars.get(i + 1) == Some(&'>') {
                i += 2;
                let s = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                word = chars[s..i].iter().collect();
            }
            toks.push(match word.to_uppercase().as_str() {
                "AND" => Tok::Op("AND"),
                "OR" => Tok::Op("OR"),
                "NOT" => Tok::Op("NOT"),
                "NULL" => Tok::Null,
                _ => Tok::Ident(word),
            });
            continue;
        }
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        let op2 = match two.as_str() {
            "==" => Some("=="),
            "<>" => Some("<>"),
            "!=" => Some("<>"),
            "<=" => Some("<="),
            ">=" => Some(">="),
            "**" => Some("^"),
            _ => None,
        };
        if let Some(op) = op2 {
            toks.push(Tok::Op(op));
            i += 2;
            continue;
        }
        toks.push(match c {
            '=' => Tok::Op("="),
            '#' => Tok::Op("<>"),
            '<' => Tok::Op("<"),
            '>' => Tok::Op(">"),
            '$' => Tok::Op("$"),
            '+' => Tok::Op("+"),
            '-' => Tok::Op("-"),
            '*' => Tok::Op("*"),
            '/' => Tok::Op("/"),
            '%' => Tok::Op("%"),
            '^' => Tok::Op("^"),
            '!' => Tok::Op("NOT"),
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            ',' => Tok::Comma,
            other => return Err(err(format!("unexpected character '{other}'"))),
        });
        i += 1;
    }
    Ok(toks)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn eat_op(&mut self, ops: &[&'static str]) -> Option<&'static str> {
        if let Some(Tok::Op(op)) = self.peek()
            && ops.contains(op)
        {
            let op = *op;
            self.pos += 1;
            return Some(op);
        }
        None
    }

    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(err("expression is nested too deeply"));
        }
        Ok(())
    }

    fn or(&mut self) -> Result<Expr> {
        self.enter()?;
        let mut left = self.and()?;
        while self.eat_op(&["OR"]).is_some() {
            left = Expr::Binary("OR", Box::new(left), Box::new(self.and()?));
        }
        self.depth -= 1;
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr> {
        let mut left = self.not()?;
        while self.eat_op(&["AND"]).is_some() {
            left = Expr::Binary("AND", Box::new(left), Box::new(self.not()?));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr> {
        if self.eat_op(&["NOT"]).is_some() {
            self.enter()?;
            let inner = self.not()?;
            self.depth -= 1;
            return Ok(Expr::Unary("NOT", Box::new(inner)));
        }
        self.cmp()
    }

    fn cmp(&mut self) -> Result<Expr> {
        let left = self.add()?;
        if let Some(op) = self.eat_op(&["=", "==", "<>", "<", ">", "<=", ">=", "$"]) {
            let right = self.add()?;
            return Ok(Expr::Binary(op, Box::new(left), Box::new(right)));
        }
        Ok(left)
    }

    fn add(&mut self) -> Result<Expr> {
        let mut left = self.mul()?;
        while let Some(op) = self.eat_op(&["+", "-"]) {
            left = Expr::Binary(op, Box::new(left), Box::new(self.mul()?));
        }
        Ok(left)
    }

    fn mul(&mut self) -> Result<Expr> {
        let mut left = self.unary()?;
        while let Some(op) = self.eat_op(&["*", "/", "%", "^"]) {
            left = Expr::Binary(op, Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat_op(&["-"]).is_some() {
            self.enter()?;
            let inner = self.unary()?;
            self.depth -= 1;
            return Ok(Expr::Unary("-", Box::new(inner)));
        }
        if self.eat_op(&["+"]).is_some() {
            return self.unary();
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr> {
        match self.next() {
            Some(Tok::Num(n)) => Ok(Expr::Lit(Val::Num(n))),
            Some(Tok::Str(s)) => Ok(Expr::Lit(Val::Str(s))),
            Some(Tok::Bool(b)) => Ok(Expr::Lit(Val::Bool(b))),
            Some(Tok::Null) => Ok(Expr::Lit(Val::Null)),
            Some(Tok::Date(d)) => Ok(Expr::Lit(Val::Date(d))),
            Some(Tok::DateTime(dt)) => Ok(Expr::Lit(Val::DateTime(Some(dt)))),
            Some(Tok::LParen) => {
                let e = self.or()?;
                match self.next() {
                    Some(Tok::RParen) => Ok(e),
                    _ => Err(err("missing ')'")),
                }
            }
            Some(Tok::Ident(name)) => {
                if self.peek() == Some(&Tok::LParen) {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if self.peek() == Some(&Tok::RParen) {
                        self.pos += 1;
                    } else {
                        loop {
                            args.push(self.or()?);
                            match self.next() {
                                Some(Tok::Comma) => continue,
                                Some(Tok::RParen) => break,
                                _ => return Err(err(format!("missing ')' after {name}("))),
                            }
                        }
                    }
                    Ok(Expr::Call(name.to_uppercase(), args))
                } else {
                    Ok(Expr::Field(name))
                }
            }
            Some(t) => Err(err(format!("unexpected token {t:?}"))),
            None => Err(err("unexpected end of expression")),
        }
    }
}

pub fn parse(src: &str) -> Result<Expr> {
    if src.len() > MAX_EXPR_LEN {
        return Err(err("expression is too long"));
    }
    let toks = tokenize(src)?;
    if toks.is_empty() {
        return Err(err("expression is empty"));
    }
    // Bounds the depth of left-leaning operator chains during evaluation.
    if toks.len() > MAX_TOKENS {
        return Err(err("expression has too many terms"));
    }
    let mut p = Parser {
        toks,
        pos: 0,
        depth: 0,
    };
    let e = p.or()?;
    if p.pos < p.toks.len() {
        return Err(err(format!("unexpected {:?}", p.toks[p.pos])));
    }
    Ok(e)
}

impl Expr {
    /// Field names referenced by the expression (upper-case).
    pub fn fields(&self, out: &mut Vec<String>) {
        match self {
            Expr::Field(f) => {
                let f = f.to_uppercase();
                if !out.contains(&f) {
                    out.push(f);
                }
            }
            Expr::Unary(_, e) => e.fields(out),
            Expr::Binary(_, a, b) => {
                a.fields(out);
                b.fields(out);
            }
            Expr::Call(_, args) => args.iter().for_each(|a| a.fields(out)),
            Expr::Lit(_) => {}
        }
    }

    /// Evaluate as a filter: `.NULL.` counts as false.
    pub fn matches(&self, ctx: &mut dyn Context) -> Result<bool> {
        match self.eval(ctx)? {
            Val::Bool(b) => Ok(b),
            Val::Null => Ok(false),
            other => Err(err(format!(
                "filter must be a logical expression, got {}",
                other.type_name()
            ))),
        }
    }

    pub fn eval(&self, ctx: &mut dyn Context) -> Result<Val> {
        match self {
            Expr::Lit(v) => Ok(v.clone()),
            Expr::Field(name) => ctx.field(name),
            Expr::Unary("NOT", e) => Ok(match e.eval(ctx)? {
                Val::Bool(b) => Val::Bool(!b),
                Val::Null => Val::Null,
                v => return Err(mismatch("NOT", &v, &v)),
            }),
            Expr::Unary(_, e) => Ok(match e.eval(ctx)? {
                Val::Num(n) => Val::Num(-n),
                Val::Null => Val::Null,
                v => return Err(mismatch("-", &v, &v)),
            }),
            Expr::Binary("AND", a, b) => {
                let l = a.eval(ctx)?;
                if l == Val::Bool(false) {
                    return Ok(l);
                }
                logic("AND", l, b.eval(ctx)?)
            }
            Expr::Binary("OR", a, b) => {
                let l = a.eval(ctx)?;
                if l == Val::Bool(true) {
                    return Ok(l);
                }
                logic("OR", l, b.eval(ctx)?)
            }
            Expr::Binary(op, a, b) => binary(op, a.eval(ctx)?, b.eval(ctx)?),
            Expr::Call(name, args) => call(name, args, ctx),
        }
    }
}

fn mismatch(op: &str, a: &Val, b: &Val) -> FoxProError {
    err(format!(
        "operator/operand type mismatch: {} {op} {}",
        a.type_name(),
        b.type_name()
    ))
}

fn logic(op: &str, l: Val, r: Val) -> Result<Val> {
    let as_opt = |v: &Val| match v {
        Val::Bool(b) => Ok(Some(*b)),
        Val::Null => Ok(None),
        other => Err(mismatch(op, other, other)),
    };
    let (l, r) = (as_opt(&l)?, as_opt(&r)?);
    Ok(match (op, l, r) {
        ("AND", Some(false), _) | ("AND", _, Some(false)) => Val::Bool(false),
        ("AND", Some(true), Some(true)) => Val::Bool(true),
        ("OR", Some(true), _) | ("OR", _, Some(true)) => Val::Bool(true),
        ("OR", Some(false), Some(false)) => Val::Bool(false),
        _ => Val::Null,
    })
}

fn compare(a: &Val, b: &Val, op: &str) -> Result<Option<Ordering>> {
    Ok(match (a, b) {
        (Val::Null, _) | (_, Val::Null) => None,
        (Val::Num(x), Val::Num(y)) => x.partial_cmp(y),
        (Val::Str(x), Val::Str(y)) => Some(x.trim_end().cmp(y.trim_end())),
        (Val::Bool(x), Val::Bool(y)) => Some(x.cmp(y)),
        (Val::Date(x), Val::Date(y)) => Some(x.cmp(y)),
        (Val::DateTime(x), Val::DateTime(y)) => Some(x.cmp(y)),
        (Val::Date(x), Val::DateTime(y)) => Some(x.and_then(|d| d.and_hms_opt(0, 0, 0)).cmp(y)),
        (Val::DateTime(x), Val::Date(y)) => Some(x.cmp(&y.and_then(|d| d.and_hms_opt(0, 0, 0)))),
        _ => return Err(mismatch(op, a, b)),
    })
}

fn binary(op: &str, a: Val, b: Val) -> Result<Val> {
    match op {
        "=" | "<>" => {
            let eq = match (&a, &b) {
                (Val::Null, _) | (_, Val::Null) => return Ok(Val::Null),
                // SET EXACT OFF: compare up to the length of the right side.
                (Val::Str(x), Val::Str(y)) => x.starts_with(y.as_str()),
                _ => compare(&a, &b, op)? == Some(Ordering::Equal),
            };
            Ok(Val::Bool(if op == "=" { eq } else { !eq }))
        }
        "==" => Ok(match compare(&a, &b, op)? {
            None => Val::Null,
            Some(o) => Val::Bool(o == Ordering::Equal),
        }),
        "<" | ">" | "<=" | ">=" => Ok(match compare(&a, &b, op)? {
            None => Val::Null,
            Some(o) => Val::Bool(match op {
                "<" => o == Ordering::Less,
                ">" => o == Ordering::Greater,
                "<=" => o != Ordering::Greater,
                _ => o != Ordering::Less,
            }),
        }),
        "$" => match (&a, &b) {
            (Val::Null, _) | (_, Val::Null) => Ok(Val::Null),
            (Val::Str(x), Val::Str(y)) => Ok(Val::Bool(y.contains(x.as_str()))),
            _ => Err(mismatch(op, &a, &b)),
        },
        _ => match (&a, &b) {
            (Val::Null, _) | (_, Val::Null) => Ok(Val::Null),
            (Val::Num(x), Val::Num(y)) => {
                let v = match op {
                    "+" => x + y,
                    "-" => x - y,
                    "*" => x * y,
                    "/" if *y == 0.0 => return Err(err("division by zero")),
                    "/" => x / y,
                    "%" if *y == 0.0 => return Err(err("division by zero")),
                    "%" => x.rem_euclid(*y),
                    "^" => x.powf(*y),
                    _ => return Err(mismatch(op, &a, &b)),
                };
                Ok(Val::Num(v))
            }
            (Val::Str(x), Val::Str(y)) if op == "+" => Ok(Val::Str(format!("{x}{y}"))),
            (Val::Str(x), Val::Str(y)) if op == "-" => {
                // VFP: trailing blanks of the left operand move to the end.
                let trimmed = x.trim_end();
                let pad = x.len() - trimmed.len();
                Ok(Val::Str(format!("{trimmed}{y}{}", " ".repeat(pad))))
            }
            (Val::Date(Some(d)), Val::Num(n)) if op == "+" || op == "-" => {
                let days = chrono::Duration::days(if op == "+" { *n as i64 } else { -(*n as i64) });
                Ok(Val::Date(d.checked_add_signed(days)))
            }
            (Val::Date(Some(x)), Val::Date(Some(y))) if op == "-" => {
                Ok(Val::Num((*x - *y).num_days() as f64))
            }
            _ => Err(mismatch(op, &a, &b)),
        },
    }
}

fn like(pattern: &[char], text: &[char]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (Some('*'), _) => {
            like(&pattern[1..], text) || (!text.is_empty() && like(pattern, &text[1..]))
        }
        (Some('?'), Some(_)) => like(&pattern[1..], &text[1..]),
        (Some(p), Some(t)) if p == t => like(&pattern[1..], &text[1..]),
        _ => false,
    }
}

fn call(name: &str, args: &[Expr], ctx: &mut dyn Context) -> Result<Val> {
    let arity = |min: usize, max: usize| -> Result<()> {
        if args.len() < min || args.len() > max {
            Err(err(format!("{name}() expects {min}..{max} arguments")))
        } else {
            Ok(())
        }
    };
    // Lazily evaluated functions first.
    match name {
        "IIF" => {
            arity(3, 3)?;
            return match args[0].eval(ctx)? {
                Val::Bool(true) => args[1].eval(ctx),
                _ => args[2].eval(ctx),
            };
        }
        "DELETED" => return Ok(Val::Bool(ctx.deleted())),
        "RECNO" => return Ok(Val::Num(ctx.recno() as f64)),
        "DATE" => return Ok(Val::Date(Some(Local::now().date_naive()))),
        _ => {}
    }

    let vals: Vec<Val> = args.iter().map(|a| a.eval(ctx)).collect::<Result<_>>()?;
    let s = |i: usize| -> Result<&str> {
        match vals.get(i) {
            Some(Val::Str(s)) => Ok(s),
            Some(v) => Err(err(format!(
                "{name}() argument {} must be character, got {}",
                i + 1,
                v.type_name()
            ))),
            None => Err(err(format!("{name}() is missing argument {}", i + 1))),
        }
    };
    let n = |i: usize| -> Result<f64> {
        match vals.get(i) {
            Some(Val::Num(n)) => Ok(*n),
            Some(v) => Err(err(format!(
                "{name}() argument {} must be numeric, got {}",
                i + 1,
                v.type_name()
            ))),
            None => Err(err(format!("{name}() is missing argument {}", i + 1))),
        }
    };
    if vals.contains(&Val::Null) && !matches!(name, "ISNULL" | "NVL" | "EMPTY" | "INLIST") {
        return Ok(Val::Null);
    }
    let date_of = |v: &Val| -> Option<NaiveDate> {
        match v {
            Val::Date(d) => *d,
            Val::DateTime(dt) => dt.map(|d| d.date()),
            _ => None,
        }
    };

    Ok(match name {
        "UPPER" => {
            arity(1, 1)?;
            Val::Str(s(0)?.to_uppercase())
        }
        "LOWER" => {
            arity(1, 1)?;
            Val::Str(s(0)?.to_lowercase())
        }
        "ALLTRIM" => {
            arity(1, 1)?;
            Val::Str(s(0)?.trim().to_string())
        }
        "TRIM" | "RTRIM" => {
            arity(1, 1)?;
            Val::Str(s(0)?.trim_end().to_string())
        }
        "LTRIM" => {
            arity(1, 1)?;
            Val::Str(s(0)?.trim_start().to_string())
        }
        "LEFT" => {
            arity(2, 2)?;
            Val::Str(s(0)?.chars().take(n(1)?.max(0.0) as usize).collect())
        }
        "RIGHT" => {
            arity(2, 2)?;
            let chars: Vec<char> = s(0)?.chars().collect();
            let k = (n(1)?.max(0.0) as usize).min(chars.len());
            Val::Str(chars[chars.len() - k..].iter().collect())
        }
        "SUBSTR" => {
            arity(2, 3)?;
            let start = (n(1)?.max(1.0) as usize) - 1;
            let it = s(0)?.chars().skip(start);
            Val::Str(if args.len() == 3 {
                it.take(n(2)?.max(0.0) as usize).collect()
            } else {
                it.collect()
            })
        }
        "LEN" => {
            arity(1, 1)?;
            Val::Num(s(0)?.chars().count() as f64)
        }
        "AT" | "ATC" => {
            arity(2, 2)?;
            let (needle, hay) = if name == "ATC" {
                (s(0)?.to_lowercase(), s(1)?.to_lowercase())
            } else {
                (s(0)?.to_string(), s(1)?.to_string())
            };
            Val::Num(
                hay.find(&needle)
                    .map(|b| hay[..b].chars().count() + 1)
                    .unwrap_or(0) as f64,
            )
        }
        "LIKE" => {
            arity(2, 2)?;
            let p: Vec<char> = s(0)?.trim_end().chars().collect();
            let t: Vec<char> = s(1)?.trim_end().chars().collect();
            Val::Bool(like(&p, &t))
        }
        "EMPTY" => {
            arity(1, 1)?;
            Val::Bool(match &vals[0] {
                Val::Null => false,
                Val::Str(x) => x.trim().is_empty(),
                Val::Num(x) => *x == 0.0,
                Val::Bool(b) => !b,
                Val::Date(d) => d.is_none(),
                Val::DateTime(d) => d.is_none(),
                Val::Binary(b) => b.is_empty(),
            })
        }
        "ISNULL" => {
            arity(1, 1)?;
            Val::Bool(vals[0] == Val::Null)
        }
        "NVL" => {
            arity(2, 2)?;
            if vals[0] == Val::Null {
                vals[1].clone()
            } else {
                vals[0].clone()
            }
        }
        "BETWEEN" => {
            arity(3, 3)?;
            let lo = compare(&vals[0], &vals[1], "BETWEEN")?;
            let hi = compare(&vals[0], &vals[2], "BETWEEN")?;
            Val::Bool(lo != Some(Ordering::Less) && hi != Some(Ordering::Greater))
        }
        "INLIST" => {
            if args.len() < 2 {
                return Err(err("INLIST() expects at least 2 arguments"));
            }
            if vals[0] == Val::Null {
                return Ok(Val::Null);
            }
            let mut found = false;
            for v in &vals[1..] {
                if compare(&vals[0], v, "INLIST")? == Some(Ordering::Equal) {
                    found = true;
                    break;
                }
            }
            Val::Bool(found)
        }
        "VAL" => {
            arity(1, 1)?;
            let t = s(0)?.trim();
            let end = t
                .char_indices()
                .take_while(|(i, c)| {
                    c.is_ascii_digit() || *c == '.' || (*i == 0 && (*c == '-' || *c == '+'))
                })
                .count();
            Val::Num(t[..end].parse().unwrap_or(0.0))
        }
        "STR" => {
            arity(1, 3)?;
            let width = if args.len() > 1 { n(1)? as usize } else { 10 };
            let dec = if args.len() > 2 { n(2)? as usize } else { 0 };
            let text = format!("{:>width$.dec$}", n(0)?);
            Val::Str(if text.len() > width {
                "*".repeat(width)
            } else {
                text
            })
        }
        "INT" => {
            arity(1, 1)?;
            Val::Num(n(0)?.trunc())
        }
        "ABS" => {
            arity(1, 1)?;
            Val::Num(n(0)?.abs())
        }
        "ROUND" => {
            arity(2, 2)?;
            let f = 10f64.powi(n(1)? as i32);
            Val::Num((n(0)? * f).round() / f)
        }
        "DTOS" => {
            arity(1, 1)?;
            Val::Str(
                date_of(&vals[0])
                    .map(|d| d.format("%Y%m%d").to_string())
                    .unwrap_or_else(|| " ".repeat(8)),
            )
        }
        "YEAR" | "MONTH" | "DAY" => {
            arity(1, 1)?;
            let d = date_of(&vals[0]);
            Val::Num(match (name, d) {
                (_, None) => 0.0,
                ("YEAR", Some(d)) => d.year() as f64,
                ("MONTH", Some(d)) => d.month() as f64,
                (_, Some(d)) => d.day() as f64,
            })
        }
        _ => return Err(err(format!("unsupported function {name}()"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Rec(HashMap<String, Val>);

    impl Context for Rec {
        fn field(&mut self, name: &str) -> Result<Val> {
            self.0
                .get(&name.to_uppercase())
                .cloned()
                .ok_or_else(|| err(format!("unknown field {name}")))
        }
        fn recno(&self) -> u32 {
            7
        }
        fn deleted(&self) -> bool {
            false
        }
    }

    fn rec() -> Rec {
        let mut m = HashMap::new();
        m.insert("NAME".into(), Val::Str("John Smith    ".into()));
        m.insert("QTY".into(), Val::Num(5.0));
        m.insert("PAID".into(), Val::Bool(true));
        m.insert(
            "BORN".into(),
            Val::Date(NaiveDate::from_ymd_opt(1990, 5, 1)),
        );
        m.insert("NOTE".into(), Val::Null);
        Rec(m)
    }

    fn check(src: &str) -> bool {
        parse(src).unwrap().matches(&mut rec()).unwrap()
    }

    #[test]
    fn evaluates_common_filters() {
        assert!(check("name = 'John'"));
        assert!(!check("name == 'John'"));
        assert!(check("ALLTRIM(name) == 'John Smith'"));
        assert!(check("qty > 3 AND paid"));
        assert!(check("qty > 10 .OR. 'Smith' $ name"));
        assert!(check("NOT qty = 4"));
        assert!(check("born >= {^1990-01-01} and year(born) = 1990"));
        assert!(check("INLIST(qty, 1, 5, 9)"));
        assert!(check("LIKE('J*S*', name)"));
        assert!(check("ISNULL(note)"));
        assert!(!check("note = 'x'"));
        assert!(check("RECNO() = 7 AND !DELETED()"));
        assert!(check("customer.qty * 2 = 10"));
    }

    #[test]
    fn reports_errors_instead_of_panicking() {
        assert!(parse("qty >").is_err());
        assert!(parse("'unterminated").is_err());
        assert!(parse("(((qty)").is_err());
        assert!(parse(&"(".repeat(200)).is_err());
        assert!(parse("qty + 'a'").unwrap().eval(&mut rec()).is_err());
        assert!(parse("qty").unwrap().matches(&mut rec()).is_err());
        assert!(parse("qty / 0 = 1").unwrap().eval(&mut rec()).is_err());
    }
}
