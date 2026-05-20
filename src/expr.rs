//! GitHub Actions `${{ }}` expression evaluation.
//!
//! Workflows reach the runner with `${{ ... }}` expressions still embedded in
//! `env:`, `with:`, `if:` and `run:` fields. Bash chokes on a literal `${{`,
//! so every expression must be evaluated against the job's contexts before a
//! step runs. This module is a self-contained tokenizer + recursive-descent
//! parser + evaluator covering the subset of the expression language that real
//! workflows use.
//!
//! Values are `serde_json::Value` so `toJSON`/`fromJSON` and context navigation
//! all work uniformly. An expression that references a missing context path
//! evaluates to an empty string — the literal `${{ }}` is never left in place.

#![allow(dead_code)]

use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

/// Job status, drives the `success()/failure()/always()/cancelled()` functions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobStatus {
    Success,
    Failure,
    Cancelled,
}

/// The set of contexts an expression can reference, plus job state needed by
/// the status functions and `hashFiles`.
pub struct Context {
    roots: HashMap<String, Value>,
    pub status: JobStatus,
    pub workspace: PathBuf,
}

impl Context {
    pub fn new() -> Self {
        Self {
            roots: HashMap::new(),
            status: JobStatus::Success,
            workspace: PathBuf::from("."),
        }
    }

    /// Install (or replace) a context root, e.g. `github`, `env`, `steps`.
    pub fn set(&mut self, name: &str, value: Value) {
        self.roots.insert(name.to_string(), value);
    }

    fn root(&self, name: &str) -> Value {
        self.roots.get(name).cloned().unwrap_or(Value::Null)
    }

    /// Replace every `${{ ... }}` in `input` with the rendered evaluation.
    /// Text outside the markers is passed through verbatim.
    pub fn render(&self, input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let bytes = input.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'$' && input[i..].starts_with("${{") {
                if let Some(end) = find_expr_end(&input[i + 3..]) {
                    let expr = &input[i + 3..i + 3 + end];
                    out.push_str(&render_value(&self.eval(expr)));
                    i = i + 3 + end + 2; // skip past closing }}
                    continue;
                }
            }
            // copy one UTF-8 char
            let ch_len = utf8_len(bytes[i]);
            out.push_str(&input[i..i + ch_len]);
            i += ch_len;
        }
        out
    }

    /// Evaluate an `if:` condition to a boolean. Accepts the value with or
    /// without a surrounding `${{ }}`. An unparseable condition is treated as
    /// false (the step is skipped) — never a panic.
    pub fn eval_condition(&self, input: &str) -> bool {
        let trimmed = input.trim();
        let expr = if trimmed.starts_with("${{") && trimmed.ends_with("}}") {
            trimmed[3..trimmed.len() - 2].trim()
        } else {
            trimmed
        };
        truthy(&self.eval(expr))
    }

    /// True if the expression text mentions a status function — used to decide
    /// whether GitHub's implicit `success() &&` prefix applies to an `if:`.
    pub fn mentions_status_fn(input: &str) -> bool {
        let lower = input.to_lowercase();
        ["success(", "failure(", "always(", "cancelled("]
            .iter()
            .any(|f| lower.contains(f))
    }

    /// Evaluate a single expression (no `${{ }}` markers) to a value.
    pub fn eval(&self, expr: &str) -> Value {
        match tokenize(expr).and_then(|toks| Parser::new(toks).parse()) {
            Ok(ast) => self.eval_node(&ast),
            Err(_) => Value::Null,
        }
    }

    fn eval_node(&self, node: &Node) -> Value {
        match node {
            Node::Lit(v) => v.clone(),
            Node::Ident(name) => self.root(name),
            Node::Index(obj, key) => {
                let o = self.eval_node(obj);
                let k = self.eval_node(key);
                index_value(&o, &k)
            }
            Node::Not(inner) => Value::Bool(!truthy(&self.eval_node(inner))),
            Node::Binary(op, l, r) => self.eval_binary(*op, l, r),
            Node::Call(name, args) => self.eval_call(name, args),
        }
    }

    fn eval_binary(&self, op: Op, l: &Node, r: &Node) -> Value {
        match op {
            // `||` / `&&` are value-returning short-circuits, matching GitHub:
            // `${{ inputs.x || 'default' }}` yields the first truthy operand.
            Op::Or => {
                let lv = self.eval_node(l);
                if truthy(&lv) {
                    lv
                } else {
                    self.eval_node(r)
                }
            }
            Op::And => {
                let lv = self.eval_node(l);
                if !truthy(&lv) {
                    lv
                } else {
                    self.eval_node(r)
                }
            }
            _ => {
                let lv = self.eval_node(l);
                let rv = self.eval_node(r);
                let b = match op {
                    Op::Eq => loose_eq(&lv, &rv),
                    Op::Ne => !loose_eq(&lv, &rv),
                    Op::Lt => num_cmp(&lv, &rv).map(|o| o.is_lt()).unwrap_or(false),
                    Op::Le => num_cmp(&lv, &rv).map(|o| o.is_le()).unwrap_or(false),
                    Op::Gt => num_cmp(&lv, &rv).map(|o| o.is_gt()).unwrap_or(false),
                    Op::Ge => num_cmp(&lv, &rv).map(|o| o.is_ge()).unwrap_or(false),
                    Op::Or | Op::And => unreachable!(),
                };
                Value::Bool(b)
            }
        }
    }

    fn eval_call(&self, name: &str, args: &[Node]) -> Value {
        let a: Vec<Value> = args.iter().map(|n| self.eval_node(n)).collect();
        match name.to_lowercase().as_str() {
            "contains" if a.len() == 2 => Value::Bool(fn_contains(&a[0], &a[1])),
            "startswith" if a.len() == 2 => Value::Bool(
                coerce_str(&a[0])
                    .to_lowercase()
                    .starts_with(&coerce_str(&a[1]).to_lowercase()),
            ),
            "endswith" if a.len() == 2 => Value::Bool(
                coerce_str(&a[0])
                    .to_lowercase()
                    .ends_with(&coerce_str(&a[1]).to_lowercase()),
            ),
            "format" if !a.is_empty() => Value::String(fn_format(&a)),
            "join" if !a.is_empty() => {
                let sep = a.get(1).map(coerce_str).unwrap_or_else(|| ",".to_string());
                Value::String(fn_join(&a[0], &sep))
            }
            "tojson" if a.len() == 1 => {
                Value::String(serde_json::to_string_pretty(&a[0]).unwrap_or_default())
            }
            "fromjson" if a.len() == 1 => {
                serde_json::from_str(&coerce_str(&a[0])).unwrap_or(Value::Null)
            }
            "hashfiles" if !a.is_empty() => {
                let pats: Vec<String> = a.iter().map(coerce_str).collect();
                Value::String(fn_hash_files(&self.workspace, &pats))
            }
            "success" => Value::Bool(self.status == JobStatus::Success),
            "failure" => Value::Bool(self.status == JobStatus::Failure),
            "cancelled" => Value::Bool(self.status == JobStatus::Cancelled),
            "always" => Value::Bool(true),
            _ => Value::Null,
        }
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

// --- value helpers -------------------------------------------------------

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// Find the byte offset of the `}}` that closes an expression, respecting
/// single-quoted strings (which may contain a literal `}`).
fn find_expr_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        if in_str {
            if bytes[i] == b'\'' {
                in_str = false;
            }
        } else if bytes[i] == b'\'' {
            in_str = true;
        } else if bytes[i] == b'}' && i + 1 < bytes.len() && bytes[i + 1] == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Render a value as it appears when interpolated into text.
pub fn render_value(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// String coercion used by `format`, `join`, `contains`, etc.
fn coerce_str(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// GitHub truthiness: empty string / 0 / null / false are falsy; everything
/// else (including non-empty arrays and objects) is truthy.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Numeric coercion for comparisons. Returns NaN when a value can't be a number.
fn coerce_num(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(true) => 1.0,
        Value::Bool(false) => 0.0,
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else {
                t.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Value::Array(_) | Value::Object(_) => f64::NAN,
    }
}

fn num_cmp(l: &Value, r: &Value) -> Option<std::cmp::Ordering> {
    let (a, b) = (coerce_num(l), coerce_num(r));
    a.partial_cmp(&b)
}

/// GitHub's loose `==`. Same JSON types compare directly (strings are
/// case-insensitive); mixed types are coerced to number.
fn loose_eq(l: &Value, r: &Value) -> bool {
    match (l, r) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::String(a), Value::String(b)) => a.eq_ignore_ascii_case(b),
        (Value::Number(_), Value::Number(_)) => {
            let (a, b) = (coerce_num(l), coerce_num(r));
            a == b
        }
        (Value::Array(a), Value::Array(b)) => a == b,
        (Value::Object(a), Value::Object(b)) => a == b,
        _ => {
            let (a, b) = (coerce_num(l), coerce_num(r));
            !a.is_nan() && !b.is_nan() && a == b
        }
    }
}

fn index_value(obj: &Value, key: &Value) -> Value {
    match obj {
        Value::Object(m) => m.get(&coerce_str(key)).cloned().unwrap_or(Value::Null),
        Value::Array(arr) => {
            let idx = coerce_num(key);
            if idx.is_finite() && idx >= 0.0 {
                arr.get(idx as usize).cloned().unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        }
        _ => Value::Null,
    }
}

fn fn_contains(haystack: &Value, needle: &Value) -> bool {
    match haystack {
        Value::Array(arr) => arr.iter().any(|e| loose_eq(e, needle)),
        _ => coerce_str(haystack)
            .to_lowercase()
            .contains(&coerce_str(needle).to_lowercase()),
    }
}

fn fn_format(args: &[Value]) -> String {
    let fmt = coerce_str(&args[0]);
    let mut out = String::with_capacity(fmt.len());
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    out.push('{');
                } else {
                    let mut num = String::new();
                    while let Some(&d) = chars.peek() {
                        if d == '}' {
                            break;
                        }
                        num.push(d);
                        chars.next();
                    }
                    chars.next(); // consume '}'
                    if let Ok(idx) = num.trim().parse::<usize>() {
                        if let Some(v) = args.get(idx + 1) {
                            out.push_str(&coerce_str(v));
                        }
                    }
                }
            }
            '}' => {
                if chars.peek() == Some(&'}') {
                    chars.next();
                }
                out.push('}');
            }
            _ => out.push(c),
        }
    }
    out
}

fn fn_join(arr: &Value, sep: &str) -> String {
    match arr {
        Value::Array(items) => items.iter().map(coerce_str).collect::<Vec<_>>().join(sep),
        other => coerce_str(other),
    }
}

/// `hashFiles(patterns...)` — sha256 over the sha256 of each matching file,
/// in sorted path order, so the result is deterministic. Patterns are matched
/// relative to the workspace and support `*`, `?` and `**`.
fn fn_hash_files(workspace: &std::path::Path, patterns: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut matched: Vec<PathBuf> = Vec::new();
    let mut all_files: Vec<PathBuf> = Vec::new();
    collect_files(workspace, &mut all_files);
    for pat in patterns {
        for f in &all_files {
            if let Ok(rel) = f.strip_prefix(workspace) {
                if glob_match(pat, &rel.to_string_lossy()) && !matched.contains(f) {
                    matched.push(f.clone());
                }
            }
        }
    }
    if matched.is_empty() {
        return String::new();
    }
    matched.sort();
    let mut outer = Sha256::new();
    for f in &matched {
        if let Ok(bytes) = std::fs::read(f) {
            let mut inner = Sha256::new();
            inner.update(&bytes);
            outer.update(inner.finalize());
        }
    }
    format!("{:x}", outer.finalize())
}

fn collect_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    if out.len() > 100_000 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // skip .git for sanity
            if path.file_name().map(|n| n == ".git").unwrap_or(false) {
                continue;
            }
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Minimal glob: `**` matches any path segments, `*` matches within a segment,
/// `?` matches one char.
fn glob_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let txt: Vec<&str> = path.split('/').collect();
    seg_match(&pat, &txt)
}

fn seg_match(pat: &[&str], txt: &[&str]) -> bool {
    match pat.first() {
        None => txt.is_empty(),
        Some(&"**") => {
            // `**` consumes zero or more segments
            for skip in 0..=txt.len() {
                if seg_match(&pat[1..], &txt[skip..]) {
                    return true;
                }
            }
            false
        }
        Some(p) => {
            if txt.is_empty() {
                return false;
            }
            wildcard_match(p, txt[0]) && seg_match(&pat[1..], &txt[1..])
        }
    }
}

fn wildcard_match(pat: &str, txt: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = txt.chars().collect();
    wm(&p, &t)
}

fn wm(p: &[char], t: &[char]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some('*') => (0..=t.len()).any(|i| wm(&p[1..], &t[i..])),
        Some('?') => !t.is_empty() && wm(&p[1..], &t[1..]),
        Some(&c) => !t.is_empty() && t[0] == c && wm(&p[1..], &t[1..]),
    }
}

// --- tokenizer -----------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Num(f64),
    Str(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    Comma,
    Op(Op),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut toks = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '(' => {
                toks.push(Token::LParen);
                i += 1;
            }
            ')' => {
                toks.push(Token::RParen);
                i += 1;
            }
            '[' => {
                toks.push(Token::LBracket);
                i += 1;
            }
            ']' => {
                toks.push(Token::RBracket);
                i += 1;
            }
            '.' if i + 1 >= chars.len() || !chars[i + 1].is_ascii_digit() => {
                toks.push(Token::Dot);
                i += 1;
            }
            ',' => {
                toks.push(Token::Comma);
                i += 1;
            }
            '\'' => {
                let mut s = String::new();
                i += 1;
                loop {
                    if i >= chars.len() {
                        return Err("unterminated string".into());
                    }
                    if chars[i] == '\'' {
                        if i + 1 < chars.len() && chars[i + 1] == '\'' {
                            s.push('\'');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        s.push(chars[i]);
                        i += 1;
                    }
                }
                toks.push(Token::Str(s));
            }
            '|' => {
                if i + 1 < chars.len() && chars[i + 1] == '|' {
                    toks.push(Token::Op(Op::Or));
                    i += 2;
                } else {
                    return Err("expected ||".into());
                }
            }
            '&' => {
                if i + 1 < chars.len() && chars[i + 1] == '&' {
                    toks.push(Token::Op(Op::And));
                    i += 2;
                } else {
                    return Err("expected &&".into());
                }
            }
            '=' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    toks.push(Token::Op(Op::Eq));
                    i += 2;
                } else {
                    return Err("expected ==".into());
                }
            }
            '!' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    toks.push(Token::Op(Op::Ne));
                    i += 2;
                } else {
                    toks.push(Token::Ident("!".into())); // unary not marker
                    i += 1;
                }
            }
            '<' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    toks.push(Token::Op(Op::Le));
                    i += 2;
                } else {
                    toks.push(Token::Op(Op::Lt));
                    i += 1;
                }
            }
            '>' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    toks.push(Token::Op(Op::Ge));
                    i += 2;
                } else {
                    toks.push(Token::Op(Op::Gt));
                    i += 1;
                }
            }
            '0'..='9' => {
                let mut s = String::new();
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    s.push(chars[i]);
                    i += 1;
                }
                let n: f64 = s.parse().map_err(|_| format!("bad number: {}", s))?;
                toks.push(Token::Num(n));
            }
            c if c.is_alphabetic() || c == '_' || c == '*' => {
                let mut s = String::new();
                while i < chars.len()
                    && (chars[i].is_alphanumeric()
                        || chars[i] == '_'
                        || chars[i] == '-'
                        || chars[i] == '*')
                {
                    s.push(chars[i]);
                    i += 1;
                }
                toks.push(Token::Ident(s));
            }
            _ => return Err(format!("unexpected char: {}", c)),
        }
    }
    Ok(toks)
}

// --- parser --------------------------------------------------------------

#[derive(Debug)]
enum Node {
    Lit(Value),
    Ident(String),
    Index(Box<Node>, Box<Node>),
    Call(String, Vec<Node>),
    Not(Box<Node>),
    Binary(Op, Box<Node>, Box<Node>),
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(toks: Vec<Token>) -> Self {
        Self { toks, pos: 0 }
    }

    fn parse(mut self) -> Result<Node, String> {
        let node = self.parse_or()?;
        if self.pos != self.toks.len() {
            return Err("trailing tokens".into());
        }
        Ok(node)
    }

    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos)
    }

    fn parse_or(&mut self) -> Result<Node, String> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Some(Token::Op(Op::Or))) {
            self.pos += 1;
            let right = self.parse_and()?;
            left = Node::Binary(Op::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Node, String> {
        let mut left = self.parse_cmp()?;
        while matches!(self.peek(), Some(Token::Op(Op::And))) {
            self.pos += 1;
            let right = self.parse_cmp()?;
            left = Node::Binary(Op::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_cmp(&mut self) -> Result<Node, String> {
        let left = self.parse_unary()?;
        if let Some(Token::Op(op)) = self.peek() {
            let op = *op;
            if matches!(op, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge) {
                self.pos += 1;
                let right = self.parse_unary()?;
                return Ok(Node::Binary(op, Box::new(left), Box::new(right)));
            }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Node, String> {
        if matches!(self.peek(), Some(Token::Ident(s)) if s == "!") {
            self.pos += 1;
            let inner = self.parse_unary()?;
            return Ok(Node::Not(Box::new(inner)));
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Node, String> {
        let mut node = self.parse_primary()?;
        loop {
            match self.peek() {
                Some(Token::Dot) => {
                    self.pos += 1;
                    match self.peek() {
                        Some(Token::Ident(name)) => {
                            let name = name.clone();
                            self.pos += 1;
                            node = Node::Index(
                                Box::new(node),
                                Box::new(Node::Lit(Value::String(name))),
                            );
                        }
                        _ => return Err("expected identifier after '.'".into()),
                    }
                }
                Some(Token::LBracket) => {
                    self.pos += 1;
                    let key = self.parse_or()?;
                    if !matches!(self.peek(), Some(Token::RBracket)) {
                        return Err("expected ']'".into());
                    }
                    self.pos += 1;
                    node = Node::Index(Box::new(node), Box::new(key));
                }
                _ => break,
            }
        }
        Ok(node)
    }

    fn parse_primary(&mut self) -> Result<Node, String> {
        match self.peek().cloned() {
            Some(Token::LParen) => {
                self.pos += 1;
                let inner = self.parse_or()?;
                if !matches!(self.peek(), Some(Token::RParen)) {
                    return Err("expected ')'".into());
                }
                self.pos += 1;
                Ok(inner)
            }
            Some(Token::Num(n)) => {
                self.pos += 1;
                Ok(Node::Lit(Value::from(n)))
            }
            Some(Token::Str(s)) => {
                self.pos += 1;
                Ok(Node::Lit(Value::String(s)))
            }
            Some(Token::Ident(name)) => {
                self.pos += 1;
                if name == "!" {
                    return Err("unexpected '!'".into());
                }
                // function call?
                if matches!(self.peek(), Some(Token::LParen)) {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Some(Token::RParen)) {
                        loop {
                            args.push(self.parse_or()?);
                            match self.peek() {
                                Some(Token::Comma) => {
                                    self.pos += 1;
                                }
                                Some(Token::RParen) => break,
                                _ => return Err("expected ',' or ')'".into()),
                            }
                        }
                    }
                    self.pos += 1; // consume ')'
                    return Ok(Node::Call(name, args));
                }
                match name.as_str() {
                    "true" => Ok(Node::Lit(Value::Bool(true))),
                    "false" => Ok(Node::Lit(Value::Bool(false))),
                    "null" => Ok(Node::Lit(Value::Null)),
                    _ => Ok(Node::Ident(name)),
                }
            }
            other => Err(format!("unexpected token: {:?}", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Context {
        let mut c = Context::new();
        c.set(
            "github",
            json!({
                "ref": "refs/tags/v1.2.3",
                "sha": "abc123",
                "repository": "cali/scrytti",
                "event_name": "push",
                "event": {"inputs": {"tag": "v9.9.9", "dry": "true"}},
                "run_number": 42
            }),
        );
        c.set("env", json!({"FOO": "bar", "EMPTY": ""}));
        c.set("secrets", json!({"TOKEN": "s3cr3t"}));
        c.set("inputs", json!({"tag": "v9.9.9"}));
        c.set("matrix", json!({"os": "linux"}));
        c.set(
            "steps",
            json!({"build": {"outputs": {"artifact": "out.tar"}}}),
        );
        c.set("needs", json!({"compile": {"outputs": {"ver": "1.0"}}}));
        c
    }

    #[test]
    fn literal_passthrough() {
        let c = Context::new();
        assert_eq!(c.render("plain text"), "plain text");
    }

    #[test]
    fn simple_context_access() {
        let c = ctx();
        assert_eq!(
            c.render("tag is ${{ github.ref }}"),
            "tag is refs/tags/v1.2.3"
        );
    }

    #[test]
    fn nested_event_inputs() {
        let c = ctx();
        assert_eq!(c.render("${{ github.event.inputs.tag }}"), "v9.9.9");
    }

    #[test]
    fn missing_path_is_empty_string() {
        let c = ctx();
        assert_eq!(c.render("[${{ github.nonexistent.deep }}]"), "[]");
        assert_eq!(c.render("[${{ totally.absent }}]"), "[]");
    }

    #[test]
    fn number_context_renders() {
        let c = ctx();
        assert_eq!(c.render("${{ github.run_number }}"), "42");
    }

    #[test]
    fn string_literal_and_equality() {
        let c = ctx();
        assert_eq!(c.eval("github.event_name == 'push'"), json!(true));
        assert_eq!(c.eval("github.event_name == 'pull_request'"), json!(false));
    }

    #[test]
    fn equality_case_insensitive() {
        let c = ctx();
        assert_eq!(c.eval("'PUSH' == 'push'"), json!(true));
    }

    #[test]
    fn logical_operators() {
        let c = ctx();
        assert_eq!(c.eval("true && false"), json!(false));
        assert_eq!(c.eval("true || false"), json!(true));
        assert_eq!(c.eval("!false"), json!(true));
    }

    #[test]
    fn or_returns_value_not_bool() {
        let c = ctx();
        // missing → empty string (falsy) → falls through to default
        assert_eq!(c.render("${{ inputs.missing || 'default' }}"), "default");
        assert_eq!(c.render("${{ inputs.tag || 'default' }}"), "v9.9.9");
    }

    #[test]
    fn comparison_operators() {
        let c = Context::new();
        assert_eq!(c.eval("3 > 2"), json!(true));
        assert_eq!(c.eval("2 >= 2"), json!(true));
        assert_eq!(c.eval("1 < 0"), json!(false));
        assert_eq!(c.eval("'5' > '4'"), json!(true));
    }

    #[test]
    fn fn_contains_string_and_array() {
        let c = Context::new();
        assert_eq!(c.eval("contains('hello world', 'world')"), json!(true));
        assert_eq!(c.eval("contains('hello', 'XYZ')"), json!(false));
        assert_eq!(
            c.eval("contains(fromJSON('[\"a\",\"b\"]'), 'b')"),
            json!(true)
        );
    }

    #[test]
    fn fn_startswith_endswith() {
        let c = ctx();
        assert_eq!(c.eval("startsWith(github.ref, 'refs/tags/')"), json!(true));
        assert_eq!(c.eval("endsWith(github.ref, 'v1.2.3')"), json!(true));
        assert_eq!(
            c.eval("startsWith(github.ref, 'refs/heads/')"),
            json!(false)
        );
    }

    #[test]
    fn fn_format() {
        let c = Context::new();
        assert_eq!(c.eval("format('{0}-{1}-{0}', 'a', 'b')"), json!("a-b-a"));
        assert_eq!(c.eval("format('{{literal}}')"), json!("{literal}"));
    }

    #[test]
    fn fn_join() {
        let c = Context::new();
        assert_eq!(
            c.eval("join(fromJSON('[\"x\",\"y\",\"z\"]'), '/')"),
            json!("x/y/z")
        );
        assert_eq!(c.eval("join(fromJSON('[1,2,3]'))"), json!("1,2,3"));
    }

    #[test]
    fn fn_tojson_fromjson() {
        let c = Context::new();
        assert_eq!(c.eval("fromJSON('{\"a\":1}').a"), json!(1));
        let t = c.eval("toJSON(fromJSON('[1,2]'))");
        assert!(t.as_str().unwrap().contains("1"));
    }

    #[test]
    fn steps_outputs_access() {
        let c = ctx();
        assert_eq!(c.render("${{ steps.build.outputs.artifact }}"), "out.tar");
    }

    #[test]
    fn needs_outputs_access() {
        let c = ctx();
        assert_eq!(c.render("${{ needs.compile.outputs.ver }}"), "1.0");
    }

    #[test]
    fn status_functions() {
        let mut c = Context::new();
        c.status = JobStatus::Success;
        assert!(c.eval_condition("success()"));
        assert!(!c.eval_condition("failure()"));
        assert!(c.eval_condition("always()"));
        c.status = JobStatus::Failure;
        assert!(c.eval_condition("failure()"));
        assert!(!c.eval_condition("success()"));
        c.status = JobStatus::Cancelled;
        assert!(c.eval_condition("cancelled()"));
    }

    #[test]
    fn condition_with_expr_markers() {
        let c = ctx();
        assert!(c.eval_condition("${{ github.event_name == 'push' }}"));
        assert!(c.eval_condition("github.event_name == 'push'"));
    }

    #[test]
    fn condition_complex() {
        let c = ctx();
        assert!(
            c.eval_condition("startsWith(github.ref, 'refs/tags/') && github.event_name == 'push'")
        );
        assert!(!c.eval_condition(
            "startsWith(github.ref, 'refs/heads/') && github.event_name == 'push'"
        ));
    }

    #[test]
    fn run_body_interpolation() {
        let c = ctx();
        let run = "echo building ${{ github.repository }} at ${{ github.sha }}";
        assert_eq!(c.render(run), "echo building cali/scrytti at abc123");
    }

    #[test]
    fn multiple_expressions_one_line() {
        let c = ctx();
        assert_eq!(
            c.render("${{ env.FOO }}/${{ secrets.TOKEN }}"),
            "bar/s3cr3t"
        );
    }

    #[test]
    fn string_with_brace_inside() {
        let c = Context::new();
        // closing }} must not be detected inside the single-quoted string
        assert_eq!(c.render("${{ format('a}b', 1) }}"), "a}b");
    }

    #[test]
    fn unparseable_expr_is_empty() {
        let c = Context::new();
        // garbage → Null → empty, never leaves literal ${{ }}
        assert_eq!(c.render("x${{ @#$%^ }}y"), "xy");
    }

    #[test]
    fn no_literal_left_behind() {
        let c = ctx();
        let out = c.render("a ${{ missing.thing }} b ${{ github.sha }} c");
        assert!(!out.contains("${{"));
        assert_eq!(out, "a  b abc123 c");
    }

    #[test]
    fn index_with_bracket() {
        let c = ctx();
        assert_eq!(c.render("${{ github['sha'] }}"), "abc123");
    }

    #[test]
    fn mentions_status_fn_detection() {
        assert!(Context::mentions_status_fn("success() && true"));
        assert!(Context::mentions_status_fn("always()"));
        assert!(!Context::mentions_status_fn("github.ref == 'x'"));
    }

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("*.txt", "a.txt"));
        assert!(!glob_match("*.txt", "a.rs"));
        assert!(glob_match("**/Cargo.toml", "src/sub/Cargo.toml"));
        assert!(glob_match("**/Cargo.toml", "Cargo.toml"));
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/sub/main.rs"));
    }
}
