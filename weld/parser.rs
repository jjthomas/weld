//! Top-down recursive descent parser for Weld.
//!
//! Weld is designed to be parseable in one left-to-right pass through the input, without
//! backtracking, so we simply track a position as we go and keep incrementing it.

use std::vec::Vec;

use super::ast::Symbol;
use super::ast::BinOpKind::*;
use super::ast::ExprKind::*;
use super::ast::ScalarKind::*;
use super::error::*;
use super::partial_types::*;
use super::partial_types::PartialBuilderKind::*;
use super::partial_types::PartialType::*;
use super::program::*;
use super::tokenizer::*;
use super::tokenizer::Token::*;

#[cfg(test)] use super::pretty_print::*;

/// Parse the complete input string as a Weld program (optional macros plus one expression).
pub fn parse_program(input: &str) -> WeldResult<Program> {
    let tokens = try!(tokenize(input));
    let mut parser = Parser::new(&tokens);
    let res = parser.program();
    if res.is_ok() && !parser.is_done() {
        return weld_err!("Unexpected token: {}", parser.peek())
    }
    res
}

/// Parse the complete input string as a list of macros.
pub fn parse_macros(input: &str) -> WeldResult<Vec<Macro>> {
    let tokens = try!(tokenize(input));
    let mut parser = Parser::new(&tokens);
    let res = parser.macros();
    if res.is_ok() && !parser.is_done() {
        return weld_err!("Unexpected token: {}", parser.peek())
    }
    res
}

/// Parse the complete input string as an expression.
pub fn parse_expr(input: &str) -> WeldResult<PartialExpr> {
    let tokens = try!(tokenize(input));
    let mut parser = Parser::new(&tokens);
    let res = parser.expr().map(|b| *b);
    if res.is_ok() && !parser.is_done() {
        return weld_err!("Unexpected token: {}", parser.peek())
    }
    res
}

/// Parse the complete input string as a PartialType.
pub fn parse_type(input: &str) -> WeldResult<PartialType> {
    let tokens = try!(tokenize(input));
    let mut parser = Parser::new(&tokens);
    let res = parser.type_();
    if res.is_ok() && !parser.is_done() {
        return weld_err!("Unexpected token: {}", parser.peek())
    }
    res
}

/// A stateful object that parses a sequence of tokens, tracking its position at each point.
/// Assumes that the tokens end with a TEndOfInput.
struct Parser<'t> {
    tokens: &'t [Token],
    position: usize
}

impl<'t> Parser<'t> {
    fn new(tokens: &[Token]) -> Parser {
        Parser { tokens: tokens, position: 0 }
    }

    /// Look at the next token to be parsed.
    fn peek(&self) -> &'t Token {
        &self.tokens[self.position]
    }

    /// Consume and return the next token.
    fn next(&mut self) -> &'t Token {
        let token = &self.tokens[self.position];
        self.position += 1;
        token
    }

    /// Consume the next token and check that it equals `expected`. If not, return an Err.
    fn consume(&mut self, expected: Token) -> WeldResult<()> {
        if *self.next() != expected {
            weld_err!("Expected '{}'", expected)
        } else {
            Ok(())
        }
    }

    /// Are we done parsing all the input?
    fn is_done(&self) -> bool {
        self.position == self.tokens.len() || *self.peek() == TEndOfInput
    }

    /// Parse a program (optional macros + one body expression) starting at the current position.
    fn program(&mut self) -> WeldResult<Program> {
        let macros = try!(self.macros());
        let body = try!(self.expr());
        Ok(Program { macros: macros, body: *body })
    }

    /// Parse a list of macros starting at the current position.
    fn macros(&mut self) -> WeldResult<Vec<Macro>> {
        let mut res: Vec<Macro> = Vec::new();
        while *self.peek() == TMacro {
            res.push(try!(self.macro_()));
        }
        Ok(res)
    }

    /// Parse a single macro starting at the current position.
    fn macro_(&mut self) -> WeldResult<Macro> {
        try!(self.consume(TMacro));
        let name = try!(self.symbol());
        let mut params: Vec<Symbol> = Vec::new();
        try!(self.consume(TOpenParen));
        while *self.peek() != TCloseParen {
            params.push(try!(self.symbol()));
            if *self.peek() == TComma {
                self.next();
            } else if *self.peek() != TCloseParen {
                return weld_err!("Expected ',' or ')'");
            }
        }
        try!(self.consume(TCloseParen));
        try!(self.consume(TEqual));
        let body = try!(self.expr());
        try!(self.consume(TSemicolon));
        Ok(Macro { name: name, parameters: params, body: *body })
    }

    /// Parse an expression starting at the current position.
    fn expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        if *self.peek() == TLet {
            self.let_expr()
        } else if *self.peek() == TBar || *self.peek() == TLogicalOr {
            self.lambda_expr()
        } else {
            self.operator_expr()
        }
    }

    /// Parse 'let name = value; body' starting at the current position.
    fn let_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        try!(self.consume(TLet));
        let name = try!(self.symbol());
        let ty = try!(self.optional_type());
        try!(self.consume(TEqual));
        let value = try!(self.operator_expr());
        try!(self.consume(TSemicolon));
        let body = try!(self.expr());
        let mut expr = expr_box(Let(name, value, body));
        expr.ty = ty;
        Ok(expr)
    }

    /// Parse '|params| body' starting at the current position.
    fn lambda_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        let mut params: Vec<PartialParameter> = Vec::new();
        // The next token could be either '||' if there are no params, or '|' if there are some.
        let token = self.next();
        if *token == TBar {
            while *self.peek() != TBar {
                let name = try!(self.symbol());
                let ty = try!(self.optional_type());
                params.push(PartialParameter { name: name, ty: ty });
                if *self.peek() == TComma {
                    self.next();
                } else if *self.peek() != TBar {
                    return weld_err!("Expected ',' or '|'")
                }
            }
            try!(self.consume(TBar));
        } else if *token != TLogicalOr {
            return weld_err!("Expected '|' or '||'")
        }
        let body = try!(self.expr());
        Ok(expr_box(Lambda(params, body)))
    }

    /// Parse an expression involving operators (||, &&, +, -, etc down the precedence chain)
    fn operator_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        self.logical_or_expr()
    }

    /// Parse a logical or expression with terms separated by || (for operator precedence).
    fn logical_or_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        let mut res = try!(self.logical_and_expr());
        while *self.peek() == TLogicalOr {
            self.consume(TLogicalOr)?;
            let right = try!(self.logical_and_expr());
            res = expr_box(BinOp(LogicalOr, res, right))
        }
        Ok(res)
    }

    /// Parse a logical and expression with terms separated by && (for operator precedence).
    fn logical_and_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        let mut res = try!(self.bitwise_or_expr());
        while *self.peek() == TLogicalAnd {
            self.consume(TLogicalAnd)?;
            let right = try!(self.bitwise_or_expr());
            res = expr_box(BinOp(LogicalAnd, res, right))
        }
        Ok(res)
    }

    /// Parse a bitwise or expression with terms separated by | (for operator precedence).
    fn bitwise_or_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        let mut res = try!(self.xor_expr());
        while *self.peek() == TBar {
            self.consume(TBar)?;
            let right = try!(self.xor_expr());
            res = expr_box(BinOp(BitwiseOr, res, right))
        }
        Ok(res)
    }

    /// Parse a bitwise or expression with terms separated by ^ (for operator precedence).
    fn xor_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        let mut res = try!(self.bitwise_and_expr());
        while *self.peek() == TXor {
            self.consume(TXor)?;
            let right = try!(self.bitwise_and_expr());
            res = expr_box(BinOp(Xor, res, right))
        }
        Ok(res)
    }

    /// Parse a bitwise and expression with terms separated by & (for operator precedence).
    fn bitwise_and_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        let mut res = try!(self.equality_expr());
        while *self.peek() == TBitwiseAnd {
            self.consume(TBitwiseAnd)?;
            let right = try!(self.equality_expr());
            res = expr_box(BinOp(BitwiseAnd, res, right))
        }
        Ok(res)
    }

    /// Parse an == or != expression (for operator precedence).
    fn equality_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        let mut res = try!(self.comparison_expr());
        // Unlike other expressions, we only allow one operator here; prevents stuff like a==b==c
        if *self.peek() == TEqualEqual || *self.peek() == TNotEqual {
            let token = self.next();
            let right = try!(self.comparison_expr());
            if *token == TEqualEqual {
                res = expr_box(BinOp(Equal, res, right))
            } else {
                res = expr_box(BinOp(NotEqual, res, right))
            }
        }
        Ok(res)
    }

    /// Parse a <, >, <= or >= expression (for operator precedence).
    fn comparison_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        let mut res = try!(self.sum_expr());
        // Unlike other expressions, we only allow one operator here; prevents stuff like a>b>c
        if *self.peek() == TLessThan || *self.peek() == TLessThanOrEqual ||
                *self.peek() == TGreaterThan || *self.peek() == TGreaterThanOrEqual {
            let op = match *self.next() {
                TLessThan => LessThan,
                TGreaterThan => GreaterThan,
                TLessThanOrEqual => LessThanOrEqual,
                _ => GreaterThanOrEqual
            };
            let right = try!(self.sum_expr());
            res = expr_box(BinOp(op, res, right))
        }
        Ok(res)
    }

    /// Parse a sum expression with terms separated by + and - (for operator precedence).
    fn sum_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        let mut res = try!(self.product_expr());
        while *self.peek() == TPlus || *self.peek() == TMinus {
            let token = self.next();
            let right = try!(self.product_expr());
            if *token == TPlus {
                res = expr_box(BinOp(Add, res, right))
            } else {
                res = expr_box(BinOp(Subtract, res, right))
            }
        }
        Ok(res)
    }

    /// Parse a product expression with terms separated by *, / and % (for precedence).
    fn product_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        let mut res = try!(self.ascribe_expr());
        while *self.peek() == TTimes || *self.peek() == TDivide || *self.peek() == TModulo {
            let op = match *self.next() {
                TTimes => Multiply,
                TDivide => Divide,
                _ => Modulo,
            };
            let right = try!(self.ascribe_expr());
            res = expr_box(BinOp(op, res, right))
        }
        Ok(res)
    }

    /// Parse a type abscription expression such as 'e: T', or lower-level ones in precedence.
    fn ascribe_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        let mut expr = try!(self.apply_expr());
        if *self.peek() == TColon {
            expr.ty = try!(self.optional_type());
        }
        Ok(expr)
    }

    /// Parse application chain expression such as a.0().3().
    fn apply_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        let mut expr = try!(self.leaf_expr());
        while *self.peek() == TDot || *self.peek() == TOpenParen {
            if *self.next() == TDot {
                match *self.next() {
                    TIdent(ref value) => {
                        if value.starts_with("$") {
                            match u32::from_str_radix(&value[1..], 10) {
                                Ok(index) => expr = expr_box(GetField(expr, index)),
                                _ => return weld_err!("Expected field index but got '{}'", value)
                            }
                        }
                    }

                    ref other => return weld_err!("Expected field index but got '{}'", other)
                }
            } else {  // TOpenParen
                let mut params: Vec<PartialExpr> = Vec::new();
                while *self.peek() != TCloseParen {
                    let param = try!(self.expr());
                    params.push(*param);
                    if *self.peek() == TComma {
                        self.next();
                    } else if *self.peek() != TCloseParen {
                        return weld_err!("Expected ',' or ')'")
                    }
                }
                try!(self.consume(TCloseParen));
                expr = expr_box(Apply(expr, params))
            }
        }
        Ok(expr)
    }

    /// Parse a terminal expression at the bottom of the precedence chain.
    fn leaf_expr(&mut self) -> WeldResult<Box<PartialExpr>> {
        match *self.next() {
            TI32Literal(value) => Ok(expr_box(I32Literal(value))),
            TI64Literal(value) => Ok(expr_box(I64Literal(value))),
            TF32Literal(value) => Ok(expr_box(F32Literal(value))),
            TF64Literal(value) => Ok(expr_box(F64Literal(value))),
            TBoolLiteral(value) => Ok(expr_box(BoolLiteral(value))),
            TIdent(ref name) => Ok(expr_box(Ident(Symbol{name: name.clone(), id: 0}))),

            TOpenParen => {
                let expr = try!(self.expr());
                if *self.next() != TCloseParen {
                    return weld_err!("Expected ')'")
                }
                Ok(expr)
            },

            TOpenBracket => {
                let mut exprs: Vec<PartialExpr> = Vec::new();
                while *self.peek() != TCloseBracket {
                    let expr = try!(self.expr());
                    exprs.push(*expr);
                    if *self.peek() == TComma {
                        self.next();
                    } else if *self.peek() != TCloseBracket {
                        return weld_err!("Expected ',' or ']'")
                    }
                }
                try!(self.consume(TCloseBracket));
                Ok(expr_box(MakeVector(exprs)))
            }

            TOpenBrace => {
                let mut exprs: Vec<PartialExpr> = Vec::new();
                while *self.peek() != TCloseBrace {
                    let expr = try!(self.expr());
                    exprs.push(*expr);
                    if *self.peek() == TComma {
                        self.next();
                    } else if *self.peek() != TCloseBrace {
                        return weld_err!("Expected ',' or '}}'")
                    }
                }
                try!(self.consume(TCloseBrace));
                Ok(expr_box(MakeStruct(exprs)))
            }

            TIf => {
                try!(self.consume(TOpenParen));
                let cond = try!(self.expr());
                try!(self.consume(TComma));
                let on_true = try!(self.expr());
                try!(self.consume(TComma));
                let on_false = try!(self.expr());
                try!(self.consume(TCloseParen));
                Ok(expr_box(If(cond, on_true, on_false)))
            }

            TFor => {
                try!(self.consume(TOpenParen));
                let data = try!(self.expr());
                try!(self.consume(TComma));
                let builders = try!(self.expr());
                try!(self.consume(TComma));
                let body = try!(self.expr());
                try!(self.consume(TCloseParen));
                Ok(expr_box(For(data, builders, body)))
            }

            TMerge => {
                try!(self.consume(TOpenParen));
                let builder = try!(self.expr());
                try!(self.consume(TComma));
                let value = try!(self.expr());
                try!(self.consume(TCloseParen));
                Ok(expr_box(Merge(builder, value)))
            }

            TResult => {
                try!(self.consume(TOpenParen));
                let builder = try!(self.expr());
                try!(self.consume(TCloseParen));
                Ok(expr_box(Res(builder)))
            }

            TAppender => {
                let mut elem_type = Unknown;
                if *self.peek() == TOpenBracket {
                    try!(self.consume(TOpenBracket));
                    elem_type = try!(self.type_());
                    try!(self.consume(TCloseBracket));
                }
                let mut expr = expr_box(NewBuilder);
                expr.ty = Builder(Appender(Box::new(elem_type)));
                Ok(expr)
            }

            ref other => weld_err!("Expected expression but got '{}'", other)
        }
    }

    /// Parse a symbol starting at the current input position.
    fn symbol(&mut self) -> WeldResult<Symbol> {
        match *self.next() {
            TIdent(ref name) => Ok(Symbol { name: name.clone(), id: 0 }),
            ref other => weld_err!("Expected identifier but got '{}'", other)
        }
    }

    /// Optionally parse a type annotation such as ": i32" and return the result as a PartialType;
    /// gives Unknown if there is no type annotation at the current position.
    fn optional_type(&mut self) -> WeldResult<PartialType> {
        if *self.peek() == TColon {
            try!(self.consume(TColon));
            self.type_()
        } else {
            Ok(Unknown)
        }
    }

    /// Parse a PartialType starting at the current input position.
    fn type_(&mut self) -> WeldResult<PartialType> {
        match *self.next() {
            TI32 => Ok(Scalar(I32)),
            TI64 => Ok(Scalar(I64)),
            TF32 => Ok(Scalar(F32)),
            TF64 => Ok(Scalar(F64)),
            TBool => Ok(Scalar(Bool)),

            TVec => {
                try!(self.consume(TOpenBracket));
                let elem_type = try!(self.type_());
                try!(self.consume(TCloseBracket));
                Ok(Vector(Box::new(elem_type)))
            }

            TAppender => {
                try!(self.consume(TOpenBracket));
                let elem_type = try!(self.type_());
                try!(self.consume(TCloseBracket));
                Ok(Builder(Appender(Box::new(elem_type))))
            }

            TOpenBrace => {
                let mut types: Vec<PartialType> = Vec::new();
                while *self.peek() != TCloseBrace {
                    let ty = try!(self.type_());
                    types.push(ty);
                    if *self.peek() == TComma {
                        self.next();
                    } else if *self.peek() != TCloseBrace {
                        return weld_err!("Expected ',' or '}}'")
                    }
                }
                try!(self.consume(TCloseBrace));
                Ok(Struct(types))
            },

            TQuestion => Ok(Unknown),

            ref other => weld_err!("Expected type but got '{}'", other)
        }
    }
}

#[test]
fn basic_parsing() {
    let e = parse_expr("10 - 2 - 3 + 1").unwrap();
    assert_eq!(print_expr(&e), "(((10-2)-3)+1)");

    let e = parse_expr("10 * 2 - 4 - 3 / 1").unwrap();
    assert_eq!(print_expr(&e), "(((10*2)-4)-(3/1))");

    let e = parse_expr("[1, 2+3, 2]").unwrap();
    assert_eq!(print_expr(&e), "[1,(2+3),2]");

    let e = parse_expr("let a = 3+2; let b = (let c=a; c); b").unwrap();
    assert_eq!(print_expr(&e), "(let a=((3+2));(let b=((let c=(a);c));b))");

    let e = parse_expr("let a: vec[i32] = [2, 3]; a").unwrap();
    assert_eq!(print_expr(&e), "(let a=([2,3]);a)");

    let e = parse_expr("|a, b:i32| a+b").unwrap();
    assert_eq!(print_typed_expr(&e), "|a:?,b:i32|(a:?+b:?)");

    let e = parse_expr("|| a||b").unwrap();
    assert_eq!(print_expr(&e), "||(a||b)");

    let e = parse_expr("a.$0.$1").unwrap();
    assert_eq!(print_expr(&e), "a.$0.$1");

    let e = parse_expr("a(0,1).$0").unwrap();
    assert_eq!(print_expr(&e), "(a)(0,1).$0");

    let e = parse_expr("a.$0(0,1).$1()").unwrap();
    assert_eq!(print_expr(&e), "((a.$0)(0,1).$1)()");

    let e = parse_expr("a>b==c").unwrap();
    assert_eq!(print_expr(&e), "((a>b)==c)");

    assert!(parse_expr("a>b>c").is_err());
    assert!(parse_expr("a==b==c").is_err());

    let e = parse_expr("appender[?]").unwrap();
    assert_eq!(print_expr(&e), "appender[?]");

    let e = parse_expr("appender[i32]").unwrap();
    assert_eq!(print_expr(&e), "appender[i32]");

    let e = parse_expr("a: i32 + b").unwrap();
    assert_eq!(print_typed_expr(&e), "(a:i32+b:?)");

    assert!(parse_expr("10 * * 2").is_err());

    let p = parse_program("macro a(x) = x+x; macro b() = 5; a(b)").unwrap();
    assert_eq!(p.macros.len(), 2);
    assert_eq!(print_expr(&p.body), "(a)(b)");
    assert_eq!(print_expr(&p.macros[0].body), "(x+x)");
    assert_eq!(print_expr(&p.macros[1].body), "5");

    let t = parse_type("{i32, vec[vec[?]], ?}").unwrap();
    assert_eq!(print_type(&t), "{i32,vec[vec[?]],?}");

    let t = parse_type("{}").unwrap();
    assert_eq!(print_type(&t), "{}");
}

#[test]
fn operator_precedence() {
    let e = parse_expr("a - b - c - d").unwrap();
    assert_eq!(print_expr(&e), "(((a-b)-c)-d)");

    let e = parse_expr("a || b && c | d ^ e & f == g > h + i * j").unwrap();
    assert_eq!(print_expr(&e), "(a||(b&&(c|(d^(e&(f==(g>(h+(i*j)))))))))");

    let e = parse_expr("a * b + c > d == e & f ^ g | h && i || j").unwrap();
    assert_eq!(print_expr(&e), "(((((((((a*b)+c)>d)==e)&f)^g)|h)&&i)||j)");

    let e = parse_expr("a / b - c <= d != e & f ^ g | h && i || j").unwrap();
    assert_eq!(print_expr(&e), "(((((((((a/b)-c)<=d)!=e)&f)^g)|h)&&i)||j)");

    let e = parse_expr("a % b - c >= d != e & f ^ g | h && i || j").unwrap();
    assert_eq!(print_expr(&e), "(((((((((a%b)-c)>=d)!=e)&f)^g)|h)&&i)||j)");
}

#[test]
fn read_to_end_of_input() {
    assert!(parse_expr("a + b").is_ok());
    assert!(parse_expr("a + b macro").is_err());
    assert!(parse_type("vec[i32]").is_ok());
    assert!(parse_expr("vec[i32] 1").is_err());
    assert!(parse_program("macro a() = b; a() + b").is_ok());
    assert!(parse_program("macro a() = b; a() + b;").is_err());
}
