//! when 受限表达式：解析为 AST + 求值（ADR-0006）。
//!
//! 语言受限、无图灵完备：比较、`&&`/`||`、字符串相等、存在性判断。
//! 越界语法在解析期报错拒绝。求值器在 model 内、Server 独享（ADR-0009）。

use std::fmt;

/// when 表达式 AST。
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// 字符串字面量。
    Literal(String),
    /// 变量引用（内置变量或 Pipeline 参数）。
    Var(String),
    /// 字符串相等 `==`。
    Eq(Box<Expr>, Box<Expr>),
    /// 字符串不等 `!=`。
    Ne(Box<Expr>, Box<Expr>),
    /// 数值比较。
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    /// 逻辑与 `&&`。
    And(Box<Expr>, Box<Expr>),
    /// 逻辑或 `||`。
    Or(Box<Expr>, Box<Expr>),
    /// 存在性判断（变量已定义）。
    Exists(String),
}

/// 数值比较操作符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `<`。
    Lt,
    /// `<=`。
    Le,
    /// `>`。
    Gt,
    /// `>=`。
    Ge,
}

impl fmt::Display for CmpOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        };
        write!(f, "{s}")
    }
}

/// when 表达式解析错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WhenParseError {
    /// 意外字符（位置为字节偏移）。
    #[error("when 表达式语法错误：位置 {pos} 附近出现意外字符")]
    Unexpected {
        /// 出错的字节偏移。
        pos: usize,
    },
    /// 括号不匹配。
    #[error("when 表达式括号不匹配")]
    UnbalancedParens,
    /// 缺少操作数。
    #[error("when 表达式缺少操作数")]
    MissingOperand,
    /// 未预期的表达式结尾。
    #[error("when 表达式意外结束")]
    UnexpectedEnd,
}

/// 把 when 表达式源码解析为 AST。失败即语法不合法（保存校验拒绝）。
pub fn parse(source: &str) -> Result<Expr, WhenParseError> {
    let tokens = tokenize(source)?;
    let mut parser = Parser { tokens, pos: 0 };
    let expr = parser.parse_or()?;
    if parser.pos < parser.tokens.len() {
        return Err(WhenParseError::Unexpected {
            pos: parser.tokens[parser.pos].1,
        });
    }
    Ok(expr)
}

// ---------------------------------------------------------------------------
// 词法
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Num(f64),
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LParen,
    RParen,
    Exists, // `exists`
}

fn tokenize(src: &str) -> Result<Vec<(Token, usize)>, WhenParseError> {
    let bytes = src.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                i += 1;
            }
            '(' => {
                tokens.push((Token::LParen, i));
                i += 1;
            }
            ')' => {
                tokens.push((Token::RParen, i));
                i += 1;
            }
            '&' => {
                if src[i..].starts_with("&&") {
                    tokens.push((Token::And, i));
                    i += 2;
                } else {
                    return Err(WhenParseError::Unexpected { pos: i });
                }
            }
            '|' => {
                if src[i..].starts_with("||") {
                    tokens.push((Token::Or, i));
                    i += 2;
                } else {
                    return Err(WhenParseError::Unexpected { pos: i });
                }
            }
            '=' => {
                if src[i..].starts_with("==") {
                    tokens.push((Token::Eq, i));
                    i += 2;
                } else {
                    return Err(WhenParseError::Unexpected { pos: i });
                }
            }
            '!' => {
                if src[i..].starts_with("!=") {
                    tokens.push((Token::Ne, i));
                    i += 2;
                } else {
                    return Err(WhenParseError::Unexpected { pos: i });
                }
            }
            '<' => {
                if src[i..].starts_with("<=") {
                    tokens.push((Token::Le, i));
                    i += 2;
                } else {
                    tokens.push((Token::Lt, i));
                    i += 1;
                }
            }
            '>' => {
                if src[i..].starts_with(">=") {
                    tokens.push((Token::Ge, i));
                    i += 2;
                } else {
                    tokens.push((Token::Gt, i));
                    i += 1;
                }
            }
            '"' => {
                let start = i;
                i += 1;
                let mut s = String::new();
                let mut closed = false;
                while i < bytes.len() {
                    let ch = bytes[i] as char;
                    if ch == '"' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    s.push(ch);
                    i += 1;
                }
                if !closed {
                    return Err(WhenParseError::Unexpected { pos: start });
                }
                tokens.push((Token::Str(s), start));
            }
            // `${name}` 变量引用（when 语言统一用 `${}` 语法，ADR-0006）。
            '$' => {
                let start = i;
                if let Some(end_rel) = src[i + 2..].find('}')
                    && src[i..].starts_with("${")
                {
                    let end = i + 2 + end_rel;
                    let name = &src[i + 2..end];
                    if name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
                        && !name.is_empty()
                    {
                        tokens.push((Token::Ident(name.to_string()), start));
                        i = end + 1;
                        continue;
                    }
                }
                return Err(WhenParseError::Unexpected { pos: start });
            }
            c if c.is_ascii_digit() || c == '-' || c == '.' => {
                let start = i;
                while i < bytes.len()
                    && ((bytes[i] as char).is_ascii_digit() || (bytes[i] as char) == '.')
                {
                    i += 1;
                }
                let text = &src[start..i];
                if let Ok(n) = text.parse::<f64>() {
                    tokens.push((Token::Num(n), start));
                } else {
                    return Err(WhenParseError::Unexpected { pos: start });
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < bytes.len()
                    && ((bytes[i] as char).is_ascii_alphanumeric() || (bytes[i] as char) == '_')
                {
                    i += 1;
                }
                let word = &src[start..i];
                match word {
                    "exists" => tokens.push((Token::Exists, start)),
                    "true" => tokens.push((Token::Str("true".into()), start)),
                    "false" => tokens.push((Token::Str("false".into()), start)),
                    _ => tokens.push((Token::Ident(word.to_string()), start)),
                }
            }
            _ => return Err(WhenParseError::Unexpected { pos: i }),
        }
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// 语法（递归下降，优先级：or < and < cmp < primary）
// ---------------------------------------------------------------------------

struct Parser {
    tokens: Vec<(Token, usize)>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|(t, _)| t)
    }

    fn next(&mut self) -> Option<(Token, usize)> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn parse_or(&mut self) -> Result<Expr, WhenParseError> {
        let mut lhs = self.parse_and()?;
        while self.peek() == Some(&Token::Or) {
            self.next();
            let rhs = self.parse_and()?;
            lhs = Expr::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, WhenParseError> {
        let mut lhs = self.parse_cmp()?;
        while self.peek() == Some(&Token::And) {
            self.next();
            let rhs = self.parse_cmp()?;
            lhs = Expr::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_cmp(&mut self) -> Result<Expr, WhenParseError> {
        let lhs = self.parse_primary()?;
        match self.peek() {
            Some(&Token::Eq) => {
                self.next();
                let rhs = self.parse_primary()?;
                Ok(Expr::Eq(Box::new(lhs), Box::new(rhs)))
            }
            Some(&Token::Ne) => {
                self.next();
                let rhs = self.parse_primary()?;
                Ok(Expr::Ne(Box::new(lhs), Box::new(rhs)))
            }
            Some(&Token::Lt) => {
                self.next();
                let rhs = self.parse_primary()?;
                Ok(Expr::Cmp(
                    CmpOp::Lt,
                    Box::new(lhs),
                    Box::new(rhs),
                ))
            }
            Some(&Token::Le) => {
                self.next();
                let rhs = self.parse_primary()?;
                Ok(Expr::Cmp(
                    CmpOp::Le,
                    Box::new(lhs),
                    Box::new(rhs),
                ))
            }
            Some(&Token::Gt) => {
                self.next();
                let rhs = self.parse_primary()?;
                Ok(Expr::Cmp(
                    CmpOp::Gt,
                    Box::new(lhs),
                    Box::new(rhs),
                ))
            }
            Some(&Token::Ge) => {
                self.next();
                let rhs = self.parse_primary()?;
                Ok(Expr::Cmp(
                    CmpOp::Ge,
                    Box::new(lhs),
                    Box::new(rhs),
                ))
            }
            _ => Ok(lhs),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, WhenParseError> {
        let (tok, _pos) = self
            .next()
            .ok_or(WhenParseError::UnexpectedEnd)?;
        match tok {
            Token::LParen => {
                let inner = self.parse_or()?;
                match self.next() {
                    Some((Token::RParen, _)) => Ok(inner),
                    _ => Err(WhenParseError::UnbalancedParens),
                }
            }
            Token::Str(s) => Ok(Expr::Literal(s)),
            Token::Num(n) => Ok(Expr::Literal(n.to_string())),
            Token::Ident(name) => Ok(Expr::Var(name)),
            Token::Exists => {
                match self.next() {
                    Some((Token::Ident(name), _)) => Ok(Expr::Exists(name)),
                    _ => Err(WhenParseError::MissingOperand),
                }
            }
            _ => Err(WhenParseError::Unexpected {
                pos: 0,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// 求值
// ---------------------------------------------------------------------------

/// 求值错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvalError {
    /// 变量未定义（存在性判断之外）。
    #[error("when 求值：变量 `{0}` 未定义")]
    UndefinedVar(String),
    /// 数值比较遇非数值操作数。
    #[error("when 求值：`{0}` 不是数值，无法比较")]
    NotANumber(String),
}

/// 求值环境：变量名 → 值（内置变量与 Pipeline 参数的合并视图）。
pub trait Env {
    /// 取变量值；未定义返回 `None`。
    fn get(&self, name: &str) -> Option<&str>;
}

/// 对字面环境（map）求值。
pub fn eval(expr: &Expr, env: &impl Env) -> Result<bool, EvalError> {
    eval_inner(expr, env)
}

fn eval_inner(expr: &Expr, env: &impl Env) -> Result<bool, EvalError> {
    match expr {
        Expr::Literal(s) => Ok(!s.is_empty()),
        Expr::Var(name) => Ok(env.get(name).is_some_and(|v| !v.is_empty())),
        Expr::Exists(name) => Ok(env.get(name).is_some()),
        Expr::Eq(a, b) => {
            let (av, bv) = (resolve_str(a, env)?, resolve_str(b, env)?);
            Ok(av == bv)
        }
        Expr::Ne(a, b) => {
            let (av, bv) = (resolve_str(a, env)?, resolve_str(b, env)?);
            Ok(av != bv)
        }
        Expr::Cmp(op, a, b) => {
            let (av, bv) = (resolve_num(a, env)?, resolve_num(b, env)?);
            let r = match op {
                CmpOp::Lt => av < bv,
                CmpOp::Le => av <= bv,
                CmpOp::Gt => av > bv,
                CmpOp::Ge => av >= bv,
            };
            Ok(r)
        }
        Expr::And(a, b) => {
            if eval_inner(a, env)? {
                eval_inner(b, env)
            } else {
                Ok(false)
            }
        }
        Expr::Or(a, b) => {
            if eval_inner(a, env)? {
                Ok(true)
            } else {
                eval_inner(b, env)
            }
        }
    }
}

fn resolve_str(expr: &Expr, env: &impl Env) -> Result<String, EvalError> {
    match expr {
        Expr::Literal(s) => Ok(s.clone()),
        Expr::Var(name) => env
            .get(name)
            .map(str::to_owned)
            .ok_or_else(|| EvalError::UndefinedVar(name.clone())),
        _ => Err(EvalError::UndefinedVar(
            "when 比较只支持字面量或变量".into(),
        )),
    }
}

fn resolve_num(expr: &Expr, env: &impl Env) -> Result<f64, EvalError> {
    let s = resolve_str(expr, env)?;
    s.parse::<f64>()
        .map_err(|_| EvalError::NotANumber(s))
}

/// 便捷实现：把 map 当求值环境。
pub struct MapEnv<'a>(pub &'a std::collections::HashMap<String, String>);

impl Env for MapEnv<'_> {
    fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }
}

/// 便捷实现：把 `&[(&str, &str)]` 当求值环境。
pub struct SliceEnv<'a>(pub &'a [(&'a str, &'a str)]);

impl Env for SliceEnv<'_> {
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| *v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_equality_and_logic() {
        let expr = parse("${SISY_BRANCH} == \"main\" && exists SISY_COMMIT_ID").unwrap();
        match expr {
            Expr::And(a, b) => {
                assert!(matches!(*a, Expr::Eq(_, _)));
                assert!(matches!(*b, Expr::Exists(_)));
            }
            _ => panic!("expected And"),
        }
    }

    #[test]
    fn parses_numeric_compare() {
        let expr = parse("${SISY_BUILD_NUMBER} >= 2").unwrap();
        assert!(matches!(expr, Expr::Cmp(CmpOp::Ge, _, _)));
    }

    #[test]
    fn rejects_unbalanced_parens() {
        assert!(matches!(
            parse("(a == \"b\""),
            Err(WhenParseError::UnbalancedParens)
        ));
    }

    #[test]
    fn rejects_unknown_tokens() {
        assert!(parse("a => b").is_err());
        assert!(parse("x == \"a\" junk").is_err());
    }

    #[test]
    fn rejects_turing_complete_constructs() {
        // 赋值、函数调用等越界语法一律解析失败。
        assert!(parse("a = 1").is_err());
        assert!(parse("fn(x)").is_err());
        assert!(parse("loop { }").is_err());
    }

    #[test]
    fn eval_equality_with_env() {
        let env = SliceEnv(&[("SISY_BRANCH", "main")]);
        assert!(eval(&parse("${SISY_BRANCH} == \"main\"").unwrap(), &env).unwrap());
        assert!(!eval(&parse("${SISY_BRANCH} == \"dev\"").unwrap(), &env).unwrap());
    }

    #[test]
    fn eval_and_short_circuit() {
        let env = SliceEnv(&[("A", "1")]);
        // 左假即短路，右侧变量未定义也不报错。
        assert!(!eval(&parse("${A} == \"2\" && ${MISSING} == \"x\"").unwrap(), &env).unwrap());
        // 右侧变量未定义且左侧为真 → 报错。
        assert!(matches!(
            eval(&parse("${A} == \"1\" && ${MISSING} == \"x\"").unwrap(), &env),
            Err(EvalError::UndefinedVar(_))
        ));
    }

    #[test]
    fn eval_exists() {
        let env = SliceEnv(&[("SISY_COMMIT_ID", "abc123")]);
        assert!(eval(&parse("exists SISY_COMMIT_ID").unwrap(), &env).unwrap());
        assert!(!eval(&parse("exists SISY_NOPE").unwrap(), &env).unwrap());
    }

    #[test]
    fn eval_numeric_compare() {
        let env = SliceEnv(&[("SISY_BUILD_NUMBER", "3")]);
        assert!(eval(&parse("${SISY_BUILD_NUMBER} >= 2").unwrap(), &env).unwrap());
        assert!(!eval(&parse("${SISY_BUILD_NUMBER} >= 5").unwrap(), &env).unwrap());

        // 非数值操作数 → NotANumber
        let env_str = SliceEnv(&[("SISY_BRANCH", "main")]);
        assert!(matches!(
            eval(&parse("${SISY_BRANCH} >= 5").unwrap(), &env_str),
            Err(EvalError::NotANumber(_))
        ));
    }

    #[test]
    fn eval_or() {
        let env = SliceEnv(&[("SISY_BRANCH", "main")]);
        assert!(eval(
            &parse("${SISY_BRANCH} == \"main\" || ${MISSING} == \"x\"").unwrap(),
            &env
        )
        .unwrap());
    }
}
