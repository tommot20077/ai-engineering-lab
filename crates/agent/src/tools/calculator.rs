//! `calculator`:模型算大數字常錯,交給程式算(design §7)。
//!
//! 自己寫 lexer + Pratt parser,不用任何 eval。三段:
//! 字串 →(lexer)→ token 清單 →(parser)→ 語法樹 `Expr` →(eval)→ `f64`。

use std::iter::{Enumerate, Peekable};
use std::str::Chars;
use std::vec;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::{Tool, ToolContext, ToolError};

/// 算式的長度上限(bytes)。
pub(crate) const MAX_EXPRESSION_BYTES: usize = 1024;

/// 巢狀的深度上限(括號、負號、`^` 的右邊)。
pub(crate) const MAX_DEPTH: usize = 64;

/// `calculator` 工具。
pub struct Calculator;

// 欄位上的 `///` 會送給模型看。
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CalculatorArgs {
    /// 算式,例如 "(1 + 2) * 3 ^ 2"。
    pub expression: String,
}

impl Tool for Calculator {
    type Args = CalculatorArgs;

    fn name(&self) -> &str {
        "calculator"
    }

    fn description(&self) -> &str {
        "計算數學算式。需要精確的數字時一定要用(大數字的乘除、次方、多步驟計算),不要心算。\
         支援 + - * /、%(餘數,不是百分比;百分比請寫成 * 0.15)、^(次方)、括號、負號,\
         函式 sqrt abs ln log10 floor ceil round,常數 pi e。\
         以 64 位元浮點數計算,約 15~16 位有效數字。"
    }

    async fn call(&self, args: CalculatorArgs, _ctx: &ToolContext) -> Result<String, ToolError> {
        evaluate(&args.expression).map_err(|e| match e {
            // 算式本身沒錯、只是算不出來:工具執行失敗。其餘是算式寫錯:參數的問題。
            CalcError::DivisionByZero | CalcError::NotFinite => ToolError::Failed(e.to_string()),
            _ => ToolError::InvalidArgs(e.to_string()),
        })
    }
}

/// 算式有問題,或算不出有限的數。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub(crate) enum CalcError {
    #[error("the expression is longer than {MAX_EXPRESSION_BYTES} bytes")]
    TooLong,
    #[error("the expression is empty")]
    Empty,
    #[error("unexpected character {ch:?} at position {at}")]
    UnexpectedChar { ch: char, at: usize },
    #[error("invalid number {text:?} at position {at}")]
    BadNumber { text: String, at: usize },
    #[error("unexpected {text:?} at position {at}")]
    UnexpectedToken { text: String, at: usize },
    #[error("the expression ends too early")]
    UnexpectedEnd,
    #[error("unknown name {name:?} at position {at}; known names: {KNOWN_NAMES}")]
    UnknownName { name: String, at: usize },
    #[error("{name:?} needs parentheses, like {name}(2)")]
    MissingParen { name: String },
    #[error("the expression nests deeper than {MAX_DEPTH} levels")]
    TooDeep,
    #[error("division by zero")]
    DivisionByZero,
    #[error("the result is not a finite number")]
    NotFinite,
}

const KNOWN_NAMES: &str = "pi, e, sqrt, abs, ln, log10, floor, ceil, round";

// ---- lexer:字串 → token ----

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Num(f64),
    Name,
    Op(Op),
    LParen,
    RParen,
}

#[derive(Debug, Clone)]
struct Token {
    kind: Kind,
    /// 原本的寫法,錯誤訊息用。
    text: String,
    /// 第幾個字元(從 1 開始)。
    at: usize,
}

// `Peekable<Enumerate<Chars>>`:一個字元一個字元往前走,`peek()` 偷看下一個但不拿走。
// Java 對照:像包了一層、可以 peek 的 `Iterator<Character>`。`Enumerate` 順便給出第幾個字元。
type CharStream<'a> = Peekable<Enumerate<Chars<'a>>>;

fn lex(expression: &str) -> Result<Vec<Token>, CalcError> {
    let mut chars: CharStream = expression.chars().enumerate().peekable();
    let mut tokens = Vec::new();
    // 每一輪先拿走一個字元,再決定怎麼處理:不管後面的分支怎麼寫,迴圈一定往前走、一定會結束。
    // 舊寫法是「先 peek、各分支自己拿走」:只要有一個分支沒拿(cargo-mutants 把 `take_while` 換成回傳空字串),
    // 就會一直讀同一個字元、一直塞 token,幾秒內吃掉好幾 GB 記憶體,CI 機器直接被關掉(第 2 步的 Mutants (diff))。
    while let Some((i, c)) = chars.next() {
        let at = i + 1;
        let token = match c {
            c if c.is_whitespace() => continue,
            '0'..='9' | '.' => number(c, &mut chars, at)?,
            c if c.is_ascii_alphabetic() => {
                let mut text = String::from(c);
                text.push_str(&take_while(&mut chars, |c| c.is_ascii_alphanumeric()));
                Token {
                    kind: Kind::Name,
                    text,
                    at,
                }
            }
            c => Token {
                // `ok_or(..)?`:不認得的字元,把 `None` 換成錯誤丟出去。
                kind: symbol(c).ok_or(CalcError::UnexpectedChar { ch: c, at })?,
                text: c.to_string(),
                at,
            },
        };
        tokens.push(token);
    }
    Ok(tokens)
}

/// 單一字元的 token。
fn symbol(c: char) -> Option<Kind> {
    Some(match c {
        '(' => Kind::LParen,
        ')' => Kind::RParen,
        '+' => Kind::Op(Op::Add),
        '-' => Kind::Op(Op::Sub),
        '*' => Kind::Op(Op::Mul),
        '/' => Kind::Op(Op::Div),
        '%' => Kind::Op(Op::Rem),
        '^' => Kind::Op(Op::Pow),
        _ => return None,
    })
}

/// 讀一個數字:`12`、`1.5`、`.5`、`1.`、`1e3`、`2E-1`。
/// `first` 是已經拿走的第一個字元。
fn number(first: char, chars: &mut CharStream, at: usize) -> Result<Token, CalcError> {
    let mut text = String::from(first);
    text.push_str(&take_while(chars, |c| c.is_ascii_digit() || c == '.'));
    // 指數:`e` 後面要接數字(或正負號再接數字),才算數字的一部分;`2e` 的 `e` 是另一個 token。
    // 判斷要往後看兩個字元,`peek()` 只能看一個:複製一份 iterator 往前走,不影響原本的位置。
    // (`Peekable<Chars>` 實作了 `Clone`,複製只是幾個指標,不會複製字串。)
    if let Some(&(_, e @ ('e' | 'E'))) = chars.peek() {
        let mut ahead = chars.clone();
        ahead.next();
        let sign = ahead
            .next_if(|&(_, c)| c == '+' || c == '-')
            .map(|(_, c)| c);
        if ahead.peek().is_some_and(|&(_, c)| c.is_ascii_digit()) {
            *chars = ahead;
            text.push(e);
            text.extend(sign);
            text.push_str(&take_while(chars, |c| c.is_ascii_digit()));
        }
    }
    // `str::parse::<f64>` 決定合不合法:`1.2.3`、`.` 會失敗。
    match text.parse::<f64>() {
        Ok(value) => Ok(Token {
            kind: Kind::Num(value),
            text,
            at,
        }),
        Err(_) => Err(CalcError::BadNumber { text, at }),
    }
}

fn take_while(chars: &mut CharStream, keep: impl Fn(char) -> bool) -> String {
    let mut text = String::new();
    // `next_if`:下一個符合條件才拿走,不符合就留著。
    while let Some((_, c)) = chars.next_if(|&(_, c)| keep(c)) {
        text.push(c);
    }
    text
}

// ---- parser:token → 語法樹 ----

/// 語法樹。
// enum 直接包含自己(`Neg(Expr)`)的話,大小會無限大,compiler 不准;中間隔一層 `Box`(放在 heap 上的指標)
// 大小就固定了。Java 不用想這件事:物件欄位本來就是參考。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Num(f64),
    Neg(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
    Call(Func, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
}

impl Op {
    /// Pratt parser 的 binding power:(左邊, 右邊)。數字越大綁得越緊。
    /// 左邊 < 右邊 → 同一層由左往右算;左邊 > 右邊(`^`)→ 由右往左算。
    fn binding_power(self) -> (u8, u8) {
        match self {
            Op::Add | Op::Sub => (1, 2),
            Op::Mul | Op::Div | Op::Rem => (3, 4),
            Op::Pow => (8, 7),
        }
    }
}

/// 負號的 binding power:比 `*` 緊、比 `^` 鬆,所以 `-2 ^ 2` 是 `-(2 ^ 2)`。
const NEG_POWER: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Func {
    Sqrt,
    Abs,
    Ln,
    Log10,
    Floor,
    Ceil,
    Round,
}

impl Func {
    fn from_name(name: &str) -> Option<Func> {
        Some(match name {
            "sqrt" => Func::Sqrt,
            "abs" => Func::Abs,
            "ln" => Func::Ln,
            "log10" => Func::Log10,
            "floor" => Func::Floor,
            "ceil" => Func::Ceil,
            "round" => Func::Round,
            _ => return None,
        })
    }

    fn apply(self, x: f64) -> f64 {
        match self {
            Func::Sqrt => x.sqrt(),
            Func::Abs => x.abs(),
            Func::Ln => x.ln(),
            Func::Log10 => x.log10(),
            Func::Floor => x.floor(),
            Func::Ceil => x.ceil(),
            Func::Round => x.round(),
        }
    }
}

fn constant(name: &str) -> Option<f64> {
    match name {
        "pi" => Some(std::f64::consts::PI),
        "e" => Some(std::f64::consts::E),
        _ => None,
    }
}

struct Parser {
    tokens: Peekable<vec::IntoIter<Token>>,
    depth: usize,
}

impl Parser {
    /// Pratt parser 的核心:先讀一個「開頭」(數字、括號、負號、函式),
    /// 再看後面的運算子:綁得比 `min_power` 緊就吃下來、遞迴讀右邊;不夠緊就交還給外層。
    fn expr(&mut self, min_power: u8) -> Result<Expr, CalcError> {
        // 每一層遞迴都會用掉一點 stack。遞迴太深是 stack overflow,整個程式直接中止,
        // 不是可以處理的錯誤(Java 的 StackOverflowError 還能 catch,Rust 不行),所以自己數。
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(CalcError::TooDeep);
        }
        let mut left = self.prefix()?;
        // `while let Some(Kind::Op(op))`:下一個是運算子才繼續;是 `)` 或結尾就停。
        while let Some(Kind::Op(op)) = self.tokens.peek().map(|t| t.kind) {
            let (left_power, right_power) = op.binding_power();
            if left_power < min_power {
                break;
            }
            self.tokens.next();
            let right = self.expr(right_power)?;
            left = Expr::Bin(op, Box::new(left), Box::new(right));
        }
        self.depth -= 1;
        Ok(left)
    }

    fn prefix(&mut self) -> Result<Expr, CalcError> {
        let token = self.tokens.next().ok_or(CalcError::UnexpectedEnd)?;
        match token.kind {
            Kind::Num(value) => Ok(Expr::Num(value)),
            Kind::Op(Op::Sub) => Ok(Expr::Neg(Box::new(self.expr(NEG_POWER)?))),
            Kind::LParen => {
                let inner = self.expr(0)?;
                self.close_paren()?;
                Ok(inner)
            }
            Kind::Name => self.name(token),
            Kind::Op(_) | Kind::RParen => Err(unexpected(token)),
        }
    }

    fn name(&mut self, token: Token) -> Result<Expr, CalcError> {
        if let Some(value) = constant(&token.text) {
            return Ok(Expr::Num(value));
        }
        let Some(func) = Func::from_name(&token.text) else {
            return Err(CalcError::UnknownName {
                name: token.text,
                at: token.at,
            });
        };
        // `next_if`:下一個是 `(` 才拿走。
        if self.tokens.next_if(|t| t.kind == Kind::LParen).is_none() {
            return Err(CalcError::MissingParen { name: token.text });
        }
        let argument = self.expr(0)?;
        self.close_paren()?;
        Ok(Expr::Call(func, Box::new(argument)))
    }

    fn close_paren(&mut self) -> Result<(), CalcError> {
        match self.tokens.next() {
            Some(t) if t.kind == Kind::RParen => Ok(()),
            Some(t) => Err(unexpected(t)),
            None => Err(CalcError::UnexpectedEnd),
        }
    }
}

fn unexpected(token: Token) -> CalcError {
    CalcError::UnexpectedToken {
        text: token.text,
        at: token.at,
    }
}

/// 整段:檢查長度 → 解析 → 計算 → 排版。
pub(crate) fn evaluate(expression: &str) -> Result<String, CalcError> {
    if expression.len() > MAX_EXPRESSION_BYTES {
        return Err(CalcError::TooLong);
    }
    let tree = parse(expression)?;
    Ok(format_number(eval(&tree)?))
}

/// 解析成語法樹(不檢查長度,測試直接用它餵超長的輸入)。
pub(crate) fn parse(expression: &str) -> Result<Expr, CalcError> {
    let tokens = lex(expression)?;
    if tokens.is_empty() {
        return Err(CalcError::Empty);
    }
    let mut parser = Parser {
        tokens: tokens.into_iter().peekable(),
        depth: 0,
    };
    let tree = parser.expr(0)?;
    // 讀完一個完整的算式之後還有東西,例如 `2(3)`、`1 2`:不猜使用者的意思。
    match parser.tokens.next() {
        Some(extra) => Err(unexpected(extra)),
        None => Ok(tree),
    }
}

// ---- eval:語法樹 → 數字 ----

/// 每一步都檢查結果是不是有限的數:只在最後檢查的話,`1 / (10 ^ 400)` 會變成 `1 / ∞ = 0`。
// 遞迴的深度跟語法樹一樣,已經被 `MAX_DEPTH` 限制住。
fn eval(expr: &Expr) -> Result<f64, CalcError> {
    let value = match expr {
        Expr::Num(value) => *value,
        // `eval(inner)?`:`inner` 是 `&Box<Expr>`,會自動解參考成 `&Expr`(deref coercion)。
        Expr::Neg(inner) => -eval(inner)?,
        Expr::Call(func, argument) => func.apply(eval(argument)?),
        Expr::Bin(op, left, right) => {
            let (a, b) = (eval(left)?, eval(right)?);
            match op {
                Op::Add => a + b,
                Op::Sub => a - b,
                Op::Mul => a * b,
                Op::Div | Op::Rem if b == 0.0 => return Err(CalcError::DivisionByZero),
                Op::Div => a / b,
                Op::Rem => a % b,
                Op::Pow => a.powf(b),
            }
        }
    };
    if value.is_finite() {
        Ok(value)
    } else {
        Err(CalcError::NotFinite)
    }
}

/// 排版:2^53 以內的整數印出每一位;很大或很小的數用科學記號;沒有 `-0`。
fn format_number(value: f64) -> String {
    let size = value.abs();
    if size == 0.0 {
        // `-0.0 == 0.0` 是 true,兩種 0 都走這裡。
        "0".to_string()
    } else if !(1e-6..1e16).contains(&size) {
        format!("{value:e}")
    } else {
        // Rust 的 `{}` 印出「最短、而且能精確還原」的寫法:12.0 印成 `12`,0.1 印成 `0.1`。
        format!("{value}")
    }
}

#[cfg(test)]
mod tests;
