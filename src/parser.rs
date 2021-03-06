//! Main module defining the lexer and parser.

use crate::any::{Dynamic, Union};
use crate::calc_fn_hash;
use crate::engine::{make_getter, make_setter, Engine, FunctionsLib};
use crate::error::{LexError, ParseError, ParseErrorType};
use crate::optimize::{optimize_into_ast, OptimizationLevel};
use crate::scope::{EntryType as ScopeEntryType, Scope};
use crate::token::{Position, Token, TokenIterator};
use crate::utils::{StaticVec, EMPTY_TYPE_ID};

#[cfg(not(feature = "no_module"))]
use crate::module::ModuleRef;

#[cfg(feature = "no_module")]
#[derive(Debug, Eq, PartialEq, Clone, Hash, Copy, Default)]
pub struct ModuleRef;

use crate::stdlib::{
    borrow::Cow,
    boxed::Box,
    char,
    collections::HashMap,
    format,
    iter::{empty, repeat, Peekable},
    num::NonZeroUsize,
    ops::{Add, Deref, DerefMut},
    rc::Rc,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};

/// The system integer type.
///
/// If the `only_i32` feature is enabled, this will be `i32` instead.
#[cfg(not(feature = "only_i32"))]
pub type INT = i64;

/// The system integer type.
///
/// If the `only_i32` feature is not enabled, this will be `i64` instead.
#[cfg(feature = "only_i32")]
pub type INT = i32;

/// The system floating-point type.
///
/// Not available under the `no_float` feature.
#[cfg(not(feature = "no_float"))]
pub type FLOAT = f64;

type PERR = ParseErrorType;

/// Compiled AST (abstract syntax tree) of a Rhai script.
///
/// Currently, `AST` is neither `Send` nor `Sync`. Turn on the `sync` feature to make it `Send + Sync`.
#[derive(Debug, Clone, Default)]
pub struct AST(
    /// Global statements.
    Vec<Stmt>,
    /// Script-defined functions, wrapped in an `Arc` for shared access.
    #[cfg(feature = "sync")]
    Arc<FunctionsLib>,
    /// Script-defined functions, wrapped in an `Rc` for shared access.
    #[cfg(not(feature = "sync"))]
    Rc<FunctionsLib>,
);

impl AST {
    /// Create a new `AST`.
    pub fn new(statements: Vec<Stmt>, fn_lib: FunctionsLib) -> Self {
        #[cfg(feature = "sync")]
        return Self(statements, Arc::new(fn_lib));
        #[cfg(not(feature = "sync"))]
        return Self(statements, Rc::new(fn_lib));
    }

    /// Get the statements.
    pub(crate) fn statements(&self) -> &Vec<Stmt> {
        &self.0
    }

    /// Get a mutable reference to the statements.
    pub(crate) fn statements_mut(&mut self) -> &mut Vec<Stmt> {
        &mut self.0
    }

    /// Get the script-defined functions.
    pub(crate) fn fn_lib(&self) -> &FunctionsLib {
        self.1.as_ref()
    }

    /// Merge two `AST` into one.  Both `AST`'s are untouched and a new, merged, version
    /// is returned.
    ///
    /// The second `AST` is simply appended to the end of the first _without any processing_.
    /// Thus, the return value of the first `AST` (if using expression-statement syntax) is buried.
    /// Of course, if the first `AST` uses a `return` statement at the end, then
    /// the second `AST` will essentially be dead code.
    ///
    /// All script-defined functions in the second `AST` overwrite similarly-named functions
    /// in the first `AST` with the same number of parameters.
    ///
    /// # Example
    ///
    /// ```
    /// # fn main() -> Result<(), Box<rhai::EvalAltResult>> {
    /// # #[cfg(not(feature = "no_function"))]
    /// # {
    /// use rhai::Engine;
    ///
    /// let engine = Engine::new();
    ///
    /// let ast1 = engine.compile(r#"fn foo(x) { 42 + x } foo(1)"#)?;
    /// let ast2 = engine.compile(r#"fn foo(n) { "hello" + n } foo("!")"#)?;
    ///
    /// let ast = ast1.merge(&ast2);    // Merge 'ast2' into 'ast1'
    ///
    /// // Notice that using the '+' operator also works:
    /// // let ast = &ast1 + &ast2;
    ///
    /// // 'ast' is essentially:
    /// //
    /// //    fn foo(n) { "hello" + n } // <- definition of first 'foo' is overwritten
    /// //    foo(1)                    // <- notice this will be "hello1" instead of 43,
    /// //                              //    but it is no longer the return value
    /// //    foo("!")                  // returns "hello!"
    ///
    /// // Evaluate it
    /// assert_eq!(engine.eval_ast::<String>(&ast)?, "hello!");
    /// # }
    /// # Ok(())
    /// # }
    /// ```
    pub fn merge(&self, other: &Self) -> Self {
        let Self(statements, functions) = self;

        let ast = match (statements.is_empty(), other.0.is_empty()) {
            (false, false) => {
                let mut statements = statements.clone();
                statements.extend(other.0.iter().cloned());
                statements
            }
            (false, true) => statements.clone(),
            (true, false) => other.0.clone(),
            (true, true) => vec![],
        };

        Self::new(ast, functions.merge(other.1.as_ref()))
    }

    /// Clear all function definitions in the `AST`.
    #[cfg(not(feature = "no_function"))]
    pub fn clear_functions(&mut self) {
        #[cfg(feature = "sync")]
        {
            self.1 = Arc::new(Default::default());
        }
        #[cfg(not(feature = "sync"))]
        {
            self.1 = Rc::new(Default::default());
        }
    }

    /// Clear all statements in the `AST`, leaving only function definitions.
    #[cfg(not(feature = "no_function"))]
    pub fn retain_functions(&mut self) {
        self.0 = vec![];
    }
}

impl Add<Self> for &AST {
    type Output = AST;

    fn add(self, rhs: Self) -> Self::Output {
        self.merge(rhs)
    }
}

/// A type representing the access mode of a scripted function.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum FnAccess {
    /// Private function.
    Private,
    /// Public function.
    Public,
}

/// A scripted function definition.
#[derive(Debug, Clone)]
pub struct FnDef {
    /// Function name.
    pub name: String,
    /// Function access mode.
    pub access: FnAccess,
    /// Names of function parameters.
    pub params: StaticVec<String>,
    /// Function body.
    pub body: Stmt,
    /// Position of the function definition.
    pub pos: Position,
}

/// A sharable script-defined function.
#[cfg(feature = "sync")]
pub type SharedFnDef = Arc<FnDef>;
/// A sharable script-defined function.
#[cfg(not(feature = "sync"))]
pub type SharedFnDef = Rc<FnDef>;

/// `return`/`throw` statement.
#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy)]
pub enum ReturnType {
    /// `return` statement.
    Return,
    /// `throw` statement.
    Exception,
}

/// A type that encapsulates a local stack with variable names to simulate an actual runtime scope.
#[derive(Debug, Clone, Default)]
struct Stack(Vec<(String, ScopeEntryType)>);

impl Stack {
    /// Create a new `Stack`.
    pub fn new() -> Self {
        Default::default()
    }
    /// Find a variable by name in the `Stack`, searching in reverse.
    /// The return value is the offset to be deducted from `Stack::len`,
    /// i.e. the top element of the `Stack` is offset 1.
    /// Return zero when the variable name is not found in the `Stack`.
    pub fn find(&self, name: &str) -> Option<NonZeroUsize> {
        self.0
            .iter()
            .rev()
            .enumerate()
            .find(|(_, (n, typ))| match typ {
                ScopeEntryType::Normal | ScopeEntryType::Constant => *n == name,
                ScopeEntryType::Module => false,
            })
            .and_then(|(i, _)| NonZeroUsize::new(i + 1))
    }
    /// Find a module by name in the `Stack`, searching in reverse.
    /// The return value is the offset to be deducted from `Stack::len`,
    /// i.e. the top element of the `Stack` is offset 1.
    /// Return zero when the variable name is not found in the `Stack`.
    pub fn find_module(&self, name: &str) -> Option<NonZeroUsize> {
        self.0
            .iter()
            .rev()
            .enumerate()
            .find(|(_, (n, typ))| match typ {
                ScopeEntryType::Module => *n == name,
                ScopeEntryType::Normal | ScopeEntryType::Constant => false,
            })
            .and_then(|(i, _)| NonZeroUsize::new(i + 1))
    }
}

impl Deref for Stack {
    type Target = Vec<(String, ScopeEntryType)>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Stack {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// A statement.
///
/// Each variant is at most one pointer in size (for speed),
/// with everything being allocated together in one single tuple.
#[derive(Debug, Clone)]
pub enum Stmt {
    /// No-op.
    Noop(Position),
    /// if expr { stmt } else { stmt }
    IfThenElse(Box<(Expr, Stmt, Option<Stmt>)>),
    /// while expr { stmt }
    While(Box<(Expr, Stmt)>),
    /// loop { stmt }
    Loop(Box<Stmt>),
    /// for id in expr { stmt }
    For(Box<(String, Expr, Stmt)>),
    /// let id = expr
    Let(Box<((String, Position), Option<Expr>)>),
    /// const id = expr
    Const(Box<((String, Position), Expr)>),
    /// { stmt; ... }
    Block(Box<(StaticVec<Stmt>, Position)>),
    /// { stmt }
    Expr(Box<Expr>),
    /// continue
    Continue(Position),
    /// break
    Break(Position),
    /// return/throw
    ReturnWithVal(Box<((ReturnType, Position), Option<Expr>)>),
    /// import expr as module
    Import(Box<(Expr, (String, Position))>),
    /// expr id as name, ...
    Export(Box<StaticVec<((String, Position), Option<(String, Position)>)>>),
}

impl Default for Stmt {
    fn default() -> Self {
        Self::Noop(Default::default())
    }
}

impl Stmt {
    /// Get the `Position` of this statement.
    pub fn position(&self) -> Position {
        match self {
            Stmt::Noop(pos) | Stmt::Continue(pos) | Stmt::Break(pos) => *pos,
            Stmt::Let(x) => (x.0).1,
            Stmt::Const(x) => (x.0).1,
            Stmt::ReturnWithVal(x) => (x.0).1,
            Stmt::Block(x) => x.1,
            Stmt::IfThenElse(x) => x.0.position(),
            Stmt::Expr(x) => x.position(),
            Stmt::While(x) => x.1.position(),
            Stmt::Loop(x) => x.position(),
            Stmt::For(x) => x.2.position(),
            Stmt::Import(x) => (x.1).1,
            Stmt::Export(x) => (x.get(0).0).1,
        }
    }

    /// Is this statement self-terminated (i.e. no need for a semicolon terminator)?
    pub fn is_self_terminated(&self) -> bool {
        match self {
            Stmt::IfThenElse(_)
            | Stmt::While(_)
            | Stmt::Loop(_)
            | Stmt::For(_)
            | Stmt::Block(_) => true,

            // A No-op requires a semicolon in order to know it is an empty statement!
            Stmt::Noop(_) => false,

            Stmt::Let(_)
            | Stmt::Const(_)
            | Stmt::Import(_)
            | Stmt::Export(_)
            | Stmt::Expr(_)
            | Stmt::Continue(_)
            | Stmt::Break(_)
            | Stmt::ReturnWithVal(_) => false,
        }
    }

    /// Is this statement _pure_?
    pub fn is_pure(&self) -> bool {
        match self {
            Stmt::Noop(_) => true,
            Stmt::Expr(expr) => expr.is_pure(),
            Stmt::IfThenElse(x) if x.2.is_some() => {
                x.0.is_pure() && x.1.is_pure() && x.2.as_ref().unwrap().is_pure()
            }
            Stmt::IfThenElse(x) => x.1.is_pure(),
            Stmt::While(x) => x.0.is_pure() && x.1.is_pure(),
            Stmt::Loop(x) => x.is_pure(),
            Stmt::For(x) => x.1.is_pure() && x.2.is_pure(),
            Stmt::Let(_) | Stmt::Const(_) => false,
            Stmt::Block(x) => x.0.iter().all(Stmt::is_pure),
            Stmt::Continue(_) | Stmt::Break(_) | Stmt::ReturnWithVal(_) => false,
            Stmt::Import(_) => false,
            Stmt::Export(_) => false,
        }
    }
}

#[cfg(not(feature = "no_module"))]
type MRef = Option<Box<ModuleRef>>;
#[cfg(feature = "no_module")]
type MRef = Option<ModuleRef>;

/// An expression.
///
/// Each variant is at most one pointer in size (for speed),
/// with everything being allocated together in one single tuple.
#[derive(Debug, Clone)]
pub enum Expr {
    /// Integer constant.
    IntegerConstant(Box<(INT, Position)>),
    /// Floating-point constant.
    #[cfg(not(feature = "no_float"))]
    FloatConstant(Box<(FLOAT, Position)>),
    /// Character constant.
    CharConstant(Box<(char, Position)>),
    /// String constant.
    StringConstant(Box<(String, Position)>),
    /// Variable access - ((variable name, position), optional modules, hash, optional index)
    Variable(Box<((String, Position), MRef, u64, Option<NonZeroUsize>)>),
    /// Property access.
    Property(Box<((String, String, String), Position)>),
    /// { stmt }
    Stmt(Box<(Stmt, Position)>),
    /// func(expr, ... ) - ((function name, position), optional modules, hash, arguments, optional default value)
    /// Use `Cow<'static, str>` because a lot of operators (e.g. `==`, `>=`) are implemented as function calls
    /// and the function names are predictable, so no need to allocate a new `String`.
    FnCall(
        Box<(
            (Cow<'static, str>, Position),
            MRef,
            u64,
            StaticVec<Expr>,
            Option<Dynamic>,
        )>,
    ),
    /// expr = expr
    Assignment(Box<(Expr, Expr, Position)>),
    /// lhs.rhs
    Dot(Box<(Expr, Expr, Position)>),
    /// expr[expr]
    Index(Box<(Expr, Expr, Position)>),
    /// [ expr, ... ]
    Array(Box<(StaticVec<Expr>, Position)>),
    /// #{ name:expr, ... }
    Map(Box<(StaticVec<((String, Position), Expr)>, Position)>),
    /// lhs in rhs
    In(Box<(Expr, Expr, Position)>),
    /// lhs && rhs
    And(Box<(Expr, Expr, Position)>),
    /// lhs || rhs
    Or(Box<(Expr, Expr, Position)>),
    /// true
    True(Position),
    /// false
    False(Position),
    /// ()
    Unit(Position),
}

impl Default for Expr {
    fn default() -> Self {
        Self::Unit(Default::default())
    }
}

impl Expr {
    /// Get the `Dynamic` value of a constant expression.
    ///
    /// # Panics
    ///
    /// Panics when the expression is not constant.
    pub fn get_constant_value(&self) -> Dynamic {
        match self {
            Self::IntegerConstant(x) => x.0.into(),
            #[cfg(not(feature = "no_float"))]
            Self::FloatConstant(x) => x.0.into(),
            Self::CharConstant(x) => x.0.into(),
            Self::StringConstant(x) => x.0.clone().into(),
            Self::True(_) => true.into(),
            Self::False(_) => false.into(),
            Self::Unit(_) => ().into(),

            #[cfg(not(feature = "no_index"))]
            Self::Array(x) if x.0.iter().all(Self::is_constant) => Dynamic(Union::Array(Box::new(
                x.0.iter().map(Self::get_constant_value).collect::<Vec<_>>(),
            ))),

            #[cfg(not(feature = "no_object"))]
            Self::Map(x) if x.0.iter().all(|(_, v)| v.is_constant()) => {
                Dynamic(Union::Map(Box::new(
                    x.0.iter()
                        .map(|((k, _), v)| (k.clone(), v.get_constant_value()))
                        .collect::<HashMap<_, _>>(),
                )))
            }

            _ => panic!("cannot get value of non-constant expression"),
        }
    }

    /// Get the display value of a constant expression.
    ///
    /// # Panics
    ///
    /// Panics when the expression is not constant.
    pub fn get_constant_str(&self) -> String {
        match self {
            #[cfg(not(feature = "no_float"))]
            Self::FloatConstant(x) => x.0.to_string(),

            Self::IntegerConstant(x) => x.0.to_string(),
            Self::CharConstant(x) => x.0.to_string(),
            Self::StringConstant(_) => "string".to_string(),
            Self::True(_) => "true".to_string(),
            Self::False(_) => "false".to_string(),
            Self::Unit(_) => "()".to_string(),

            Self::Array(x) if x.0.iter().all(Self::is_constant) => "array".to_string(),

            _ => panic!("cannot get value of non-constant expression"),
        }
    }

    /// Get the `Position` of the expression.
    pub fn position(&self) -> Position {
        match self {
            #[cfg(not(feature = "no_float"))]
            Self::FloatConstant(x) => x.1,

            Self::IntegerConstant(x) => x.1,
            Self::CharConstant(x) => x.1,
            Self::StringConstant(x) => x.1,
            Self::Array(x) => x.1,
            Self::Map(x) => x.1,
            Self::Property(x) => x.1,
            Self::Stmt(x) => x.1,
            Self::Variable(x) => (x.0).1,
            Self::FnCall(x) => (x.0).1,

            Self::And(x) | Self::Or(x) | Self::In(x) => x.2,

            Self::True(pos) | Self::False(pos) | Self::Unit(pos) => *pos,

            Self::Assignment(x) | Self::Dot(x) | Self::Index(x) => x.0.position(),
        }
    }

    /// Override the `Position` of the expression.
    pub(crate) fn set_position(mut self, new_pos: Position) -> Self {
        match &mut self {
            #[cfg(not(feature = "no_float"))]
            Self::FloatConstant(x) => x.1 = new_pos,

            Self::IntegerConstant(x) => x.1 = new_pos,
            Self::CharConstant(x) => x.1 = new_pos,
            Self::StringConstant(x) => x.1 = new_pos,
            Self::Array(x) => x.1 = new_pos,
            Self::Map(x) => x.1 = new_pos,
            Self::Variable(x) => (x.0).1 = new_pos,
            Self::Property(x) => x.1 = new_pos,
            Self::Stmt(x) => x.1 = new_pos,
            Self::FnCall(x) => (x.0).1 = new_pos,
            Self::And(x) => x.2 = new_pos,
            Self::Or(x) => x.2 = new_pos,
            Self::In(x) => x.2 = new_pos,
            Self::True(pos) => *pos = new_pos,
            Self::False(pos) => *pos = new_pos,
            Self::Unit(pos) => *pos = new_pos,
            Self::Assignment(x) => x.2 = new_pos,
            Self::Dot(x) => x.2 = new_pos,
            Self::Index(x) => x.2 = new_pos,
        }

        self
    }

    /// Is the expression pure?
    ///
    /// A pure expression has no side effects.
    pub fn is_pure(&self) -> bool {
        match self {
            Self::Array(x) => x.0.iter().all(Self::is_pure),

            Self::Index(x) | Self::And(x) | Self::Or(x) | Self::In(x) => {
                let (lhs, rhs, _) = x.as_ref();
                lhs.is_pure() && rhs.is_pure()
            }

            Self::Stmt(x) => x.0.is_pure(),

            Self::Variable(_) => true,

            expr => expr.is_constant(),
        }
    }

    /// Is the expression a constant?
    pub fn is_constant(&self) -> bool {
        match self {
            #[cfg(not(feature = "no_float"))]
            Self::FloatConstant(_) => true,

            Self::IntegerConstant(_)
            | Self::CharConstant(_)
            | Self::StringConstant(_)
            | Self::True(_)
            | Self::False(_)
            | Self::Unit(_) => true,

            // An array literal is constant if all items are constant
            Self::Array(x) => x.0.iter().all(Self::is_constant),

            // An map literal is constant if all items are constant
            Self::Map(x) => x.0.iter().map(|(_, expr)| expr).all(Self::is_constant),

            // Check in expression
            Self::In(x) => match (&x.0, &x.1) {
                (Self::StringConstant(_), Self::StringConstant(_))
                | (Self::CharConstant(_), Self::StringConstant(_)) => true,
                _ => false,
            },

            _ => false,
        }
    }

    /// Is a particular token allowed as a postfix operator to this expression?
    pub fn is_valid_postfix(&self, token: &Token) -> bool {
        match self {
            #[cfg(not(feature = "no_float"))]
            Self::FloatConstant(_) => false,

            Self::IntegerConstant(_)
            | Self::CharConstant(_)
            | Self::In(_)
            | Self::And(_)
            | Self::Or(_)
            | Self::True(_)
            | Self::False(_)
            | Self::Unit(_) => false,

            Self::StringConstant(_)
            | Self::Stmt(_)
            | Self::FnCall(_)
            | Self::Assignment(_)
            | Self::Dot(_)
            | Self::Index(_)
            | Self::Array(_)
            | Self::Map(_) => match token {
                Token::LeftBracket => true,
                _ => false,
            },

            Self::Variable(_) => match token {
                Token::LeftBracket | Token::LeftParen => true,
                #[cfg(not(feature = "no_module"))]
                Token::DoubleColon => true,
                _ => false,
            },

            Self::Property(_) => match token {
                Token::LeftBracket | Token::LeftParen => true,
                _ => false,
            },
        }
    }

    /// Convert a `Variable` into a `Property`.  All other variants are untouched.
    pub(crate) fn into_property(self) -> Self {
        match self {
            Self::Variable(x) if x.1.is_none() => {
                let (name, pos) = x.0;
                let getter = make_getter(&name);
                let setter = make_setter(&name);
                Self::Property(Box::new(((name.clone(), getter, setter), pos)))
            }
            _ => self,
        }
    }
}

/// Consume a particular token, checking that it is the expected one.
fn eat_token(input: &mut Peekable<TokenIterator>, token: Token) -> Position {
    let (t, pos) = input.next().unwrap();

    if t != token {
        panic!(
            "expecting {} (found {}) at {}",
            token.syntax(),
            t.syntax(),
            pos
        );
    }
    pos
}

/// Match a particular token, consuming it if matched.
fn match_token(input: &mut Peekable<TokenIterator>, token: Token) -> Result<bool, Box<ParseError>> {
    let (t, _) = input.peek().unwrap();
    if *t == token {
        eat_token(input, token);
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Parse ( expr )
fn parse_paren_expr<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    pos: Position,
    allow_stmt_expr: bool,
) -> Result<Expr, Box<ParseError>> {
    if match_token(input, Token::RightParen)? {
        return Ok(Expr::Unit(pos));
    }

    let expr = parse_expr(input, stack, allow_stmt_expr)?;

    match input.next().unwrap() {
        // ( xxx )
        (Token::RightParen, _) => Ok(expr),
        // ( <error>
        (Token::LexError(err), pos) => return Err(PERR::BadInput(err.to_string()).into_err(pos)),
        // ( xxx ???
        (_, pos) => Err(PERR::MissingToken(
            Token::RightParen.into(),
            "for a matching ( in this expression".into(),
        )
        .into_err(pos)),
    }
}

/// Parse a function call.
fn parse_call_expr<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    id: String,
    #[cfg(not(feature = "no_module"))] mut modules: Option<Box<ModuleRef>>,
    #[cfg(feature = "no_module")] modules: Option<ModuleRef>,
    begin: Position,
    allow_stmt_expr: bool,
) -> Result<Expr, Box<ParseError>> {
    let mut args = StaticVec::new();

    match input.peek().unwrap() {
        // id <EOF>
        (Token::EOF, pos) => {
            return Err(PERR::MissingToken(
                Token::RightParen.into(),
                format!("to close the arguments list of this function call '{}'", id),
            )
            .into_err(*pos))
        }
        // id <error>
        (Token::LexError(err), pos) => return Err(PERR::BadInput(err.to_string()).into_err(*pos)),
        // id()
        (Token::RightParen, _) => {
            eat_token(input, Token::RightParen);

            #[cfg(not(feature = "no_module"))]
            let hash_fn_def = {
                if let Some(modules) = modules.as_mut() {
                    modules.set_index(stack.find_module(&modules.get(0).0));

                    // Rust functions are indexed in two steps:
                    // 1) Calculate a hash in a similar manner to script-defined functions,
                    //    i.e. qualifiers + function name + no parameters.
                    // 2) Calculate a second hash with no qualifiers, empty function name, and
                    //    the actual list of parameter `TypeId`'.s
                    // 3) The final hash is the XOR of the two hashes.
                    calc_fn_hash(modules.iter().map(|(m, _)| m.as_str()), &id, empty())
                } else {
                    calc_fn_hash(empty(), &id, empty())
                }
            };
            // Qualifiers (none) + function name + no parameters.
            #[cfg(feature = "no_module")]
            let hash_fn_def = calc_fn_hash(empty(), &id, empty());

            return Ok(Expr::FnCall(Box::new((
                (id.into(), begin),
                modules,
                hash_fn_def,
                args,
                None,
            ))));
        }
        // id...
        _ => (),
    }

    loop {
        args.push(parse_expr(input, stack, allow_stmt_expr)?);

        match input.peek().unwrap() {
            // id(...args)
            (Token::RightParen, _) => {
                eat_token(input, Token::RightParen);
                let args_iter = repeat(EMPTY_TYPE_ID()).take(args.len());

                #[cfg(not(feature = "no_module"))]
                let hash_fn_def = {
                    if let Some(modules) = modules.as_mut() {
                        modules.set_index(stack.find_module(&modules.get(0).0));

                        // Rust functions are indexed in two steps:
                        // 1) Calculate a hash in a similar manner to script-defined functions,
                        //    i.e. qualifiers + function name + dummy parameter types (one for each parameter).
                        // 2) Calculate a second hash with no qualifiers, empty function name, and
                        //    the actual list of parameter `TypeId`'.s
                        // 3) The final hash is the XOR of the two hashes.
                        calc_fn_hash(modules.iter().map(|(m, _)| m.as_str()), &id, args_iter)
                    } else {
                        calc_fn_hash(empty(), &id, args_iter)
                    }
                };
                // Qualifiers (none) + function name + dummy parameter types (one for each parameter).
                #[cfg(feature = "no_module")]
                let hash_fn_def = calc_fn_hash(empty(), &id, args_iter);

                return Ok(Expr::FnCall(Box::new((
                    (id.into(), begin),
                    modules,
                    hash_fn_def,
                    args,
                    None,
                ))));
            }
            // id(...args,
            (Token::Comma, _) => {
                eat_token(input, Token::Comma);
            }
            // id(...args <EOF>
            (Token::EOF, pos) => {
                return Err(PERR::MissingToken(
                    Token::RightParen.into(),
                    format!("to close the arguments list of this function call '{}'", id),
                )
                .into_err(*pos))
            }
            // id(...args <error>
            (Token::LexError(err), pos) => {
                return Err(PERR::BadInput(err.to_string()).into_err(*pos))
            }
            // id(...args ???
            (_, pos) => {
                return Err(PERR::MissingToken(
                    Token::Comma.into(),
                    format!("to separate the arguments to function call '{}'", id),
                )
                .into_err(*pos))
            }
        }
    }
}

/// Parse an indexing chain.
/// Indexing binds to the right, so this call parses all possible levels of indexing following in the input.
fn parse_index_chain<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    lhs: Expr,
    pos: Position,
    allow_stmt_expr: bool,
) -> Result<Expr, Box<ParseError>> {
    let idx_expr = parse_expr(input, stack, allow_stmt_expr)?;

    // Check type of indexing - must be integer or string
    match &idx_expr {
        // lhs[int]
        Expr::IntegerConstant(x) if x.0 < 0 => {
            return Err(PERR::MalformedIndexExpr(format!(
                "Array access expects non-negative index: {} < 0",
                x.0
            ))
            .into_err(x.1))
        }
        Expr::IntegerConstant(x) => match lhs {
            Expr::Array(_) | Expr::StringConstant(_) => (),

            Expr::Map(_) => {
                return Err(PERR::MalformedIndexExpr(
                    "Object map access expects string index, not a number".into(),
                )
                .into_err(x.1))
            }

            #[cfg(not(feature = "no_float"))]
            Expr::FloatConstant(x) => {
                return Err(PERR::MalformedIndexExpr(
                    "Only arrays, object maps and strings can be indexed".into(),
                )
                .into_err(x.1))
            }

            Expr::CharConstant(x) => {
                return Err(PERR::MalformedIndexExpr(
                    "Only arrays, object maps and strings can be indexed".into(),
                )
                .into_err(x.1))
            }
            Expr::Assignment(x) | Expr::And(x) | Expr::Or(x) | Expr::In(x) => {
                return Err(PERR::MalformedIndexExpr(
                    "Only arrays, object maps and strings can be indexed".into(),
                )
                .into_err(x.2))
            }
            Expr::True(pos) | Expr::False(pos) | Expr::Unit(pos) => {
                return Err(PERR::MalformedIndexExpr(
                    "Only arrays, object maps and strings can be indexed".into(),
                )
                .into_err(pos))
            }

            _ => (),
        },

        // lhs[string]
        Expr::StringConstant(x) => match lhs {
            Expr::Map(_) => (),

            Expr::Array(_) | Expr::StringConstant(_) => {
                return Err(PERR::MalformedIndexExpr(
                    "Array or string expects numeric index, not a string".into(),
                )
                .into_err(x.1))
            }

            #[cfg(not(feature = "no_float"))]
            Expr::FloatConstant(x) => {
                return Err(PERR::MalformedIndexExpr(
                    "Only arrays, object maps and strings can be indexed".into(),
                )
                .into_err(x.1))
            }

            Expr::CharConstant(x) => {
                return Err(PERR::MalformedIndexExpr(
                    "Only arrays, object maps and strings can be indexed".into(),
                )
                .into_err(x.1))
            }

            Expr::Assignment(x) | Expr::And(x) | Expr::Or(x) | Expr::In(x) => {
                return Err(PERR::MalformedIndexExpr(
                    "Only arrays, object maps and strings can be indexed".into(),
                )
                .into_err(x.2))
            }

            Expr::True(pos) | Expr::False(pos) | Expr::Unit(pos) => {
                return Err(PERR::MalformedIndexExpr(
                    "Only arrays, object maps and strings can be indexed".into(),
                )
                .into_err(pos))
            }

            _ => (),
        },

        // lhs[float]
        #[cfg(not(feature = "no_float"))]
        Expr::FloatConstant(x) => {
            return Err(PERR::MalformedIndexExpr(
                "Array access expects integer index, not a float".into(),
            )
            .into_err(x.1))
        }
        // lhs[char]
        Expr::CharConstant(x) => {
            return Err(PERR::MalformedIndexExpr(
                "Array access expects integer index, not a character".into(),
            )
            .into_err(x.1))
        }
        // lhs[??? = ??? ]
        Expr::Assignment(x) => {
            return Err(PERR::MalformedIndexExpr(
                "Array access expects integer index, not ()".into(),
            )
            .into_err(x.2))
        }
        // lhs[()]
        Expr::Unit(pos) => {
            return Err(PERR::MalformedIndexExpr(
                "Array access expects integer index, not ()".into(),
            )
            .into_err(*pos))
        }
        // lhs[??? && ???], lhs[??? || ???], lhs[??? in ???]
        Expr::And(x) | Expr::Or(x) | Expr::In(x) => {
            return Err(PERR::MalformedIndexExpr(
                "Array access expects integer index, not a boolean".into(),
            )
            .into_err(x.2))
        }
        // lhs[true], lhs[false]
        Expr::True(pos) | Expr::False(pos) => {
            return Err(PERR::MalformedIndexExpr(
                "Array access expects integer index, not a boolean".into(),
            )
            .into_err(*pos))
        }
        // All other expressions
        _ => (),
    }

    // Check if there is a closing bracket
    match input.peek().unwrap() {
        (Token::RightBracket, _) => {
            eat_token(input, Token::RightBracket);

            // Any more indexing following?
            match input.peek().unwrap() {
                // If another indexing level, right-bind it
                (Token::LeftBracket, _) => {
                    let idx_pos = eat_token(input, Token::LeftBracket);
                    // Recursively parse the indexing chain, right-binding each
                    let idx = parse_index_chain(input, stack, idx_expr, idx_pos, allow_stmt_expr)?;
                    // Indexing binds to right
                    Ok(Expr::Index(Box::new((lhs, idx, pos))))
                }
                // Otherwise terminate the indexing chain
                _ => Ok(Expr::Index(Box::new((lhs, idx_expr, pos)))),
            }
        }
        (Token::LexError(err), pos) => return Err(PERR::BadInput(err.to_string()).into_err(*pos)),
        (_, pos) => Err(PERR::MissingToken(
            Token::RightBracket.into(),
            "for a matching [ in this index expression".into(),
        )
        .into_err(*pos)),
    }
}

/// Parse an array literal.
fn parse_array_literal<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    pos: Position,
    allow_stmt_expr: bool,
) -> Result<Expr, Box<ParseError>> {
    let mut arr = StaticVec::new();

    if !match_token(input, Token::RightBracket)? {
        while !input.peek().unwrap().0.is_eof() {
            arr.push(parse_expr(input, stack, allow_stmt_expr)?);

            match input.peek().unwrap() {
                (Token::Comma, _) => eat_token(input, Token::Comma),
                (Token::RightBracket, _) => {
                    eat_token(input, Token::RightBracket);
                    break;
                }
                (Token::EOF, pos) => {
                    return Err(PERR::MissingToken(
                        Token::RightBracket.into(),
                        "to end this array literal".into(),
                    )
                    .into_err(*pos))
                }
                (Token::LexError(err), pos) => {
                    return Err(PERR::BadInput(err.to_string()).into_err(*pos))
                }
                (_, pos) => {
                    return Err(PERR::MissingToken(
                        Token::Comma.into(),
                        "to separate the items of this array literal".into(),
                    )
                    .into_err(*pos))
                }
            };
        }
    }

    Ok(Expr::Array(Box::new((arr, pos))))
}

/// Parse a map literal.
fn parse_map_literal<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    pos: Position,
    allow_stmt_expr: bool,
) -> Result<Expr, Box<ParseError>> {
    let mut map = StaticVec::new();

    if !match_token(input, Token::RightBrace)? {
        while !input.peek().unwrap().0.is_eof() {
            const MISSING_RBRACE: &str = "to end this object map literal";

            let (name, pos) = match input.next().unwrap() {
                (Token::Identifier(s), pos) => (s, pos),
                (Token::StringConst(s), pos) => (s, pos),
                (Token::LexError(err), pos) => {
                    return Err(PERR::BadInput(err.to_string()).into_err(pos))
                }
                (_, pos) if map.is_empty() => {
                    return Err(
                        PERR::MissingToken(Token::RightBrace.into(), MISSING_RBRACE.into())
                            .into_err(pos),
                    )
                }
                (Token::EOF, pos) => {
                    return Err(
                        PERR::MissingToken(Token::RightBrace.into(), MISSING_RBRACE.into())
                            .into_err(pos),
                    )
                }
                (_, pos) => return Err(PERR::PropertyExpected.into_err(pos)),
            };

            match input.next().unwrap() {
                (Token::Colon, _) => (),
                (Token::LexError(err), pos) => {
                    return Err(PERR::BadInput(err.to_string()).into_err(pos))
                }
                (_, pos) => {
                    return Err(PERR::MissingToken(
                        Token::Colon.into(),
                        format!(
                            "to follow the property '{}' in this object map literal",
                            name
                        ),
                    )
                    .into_err(pos))
                }
            };

            let expr = parse_expr(input, stack, allow_stmt_expr)?;

            map.push(((name, pos), expr));

            match input.peek().unwrap() {
                (Token::Comma, _) => {
                    eat_token(input, Token::Comma);
                }
                (Token::RightBrace, _) => {
                    eat_token(input, Token::RightBrace);
                    break;
                }
                (Token::Identifier(_), pos) => {
                    return Err(PERR::MissingToken(
                        Token::Comma.into(),
                        "to separate the items of this object map literal".into(),
                    )
                    .into_err(*pos))
                }
                (Token::LexError(err), pos) => {
                    return Err(PERR::BadInput(err.to_string()).into_err(*pos))
                }
                (_, pos) => {
                    return Err(
                        PERR::MissingToken(Token::RightBrace.into(), MISSING_RBRACE.into())
                            .into_err(*pos),
                    )
                }
            }
        }
    }

    // Check for duplicating properties
    map.iter()
        .enumerate()
        .try_for_each(|(i, ((k1, _), _))| {
            map.iter()
                .skip(i + 1)
                .find(|((k2, _), _)| k2 == k1)
                .map_or_else(|| Ok(()), |((k2, pos), _)| Err((k2, *pos)))
        })
        .map_err(|(key, pos)| PERR::DuplicatedProperty(key.to_string()).into_err(pos))?;

    Ok(Expr::Map(Box::new((map, pos))))
}

/// Parse a primary expression.
fn parse_primary<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    allow_stmt_expr: bool,
) -> Result<Expr, Box<ParseError>> {
    let (token, pos) = match input.peek().unwrap() {
        // { - block statement as expression
        (Token::LeftBrace, pos) if allow_stmt_expr => {
            let pos = *pos;
            return parse_block(input, stack, false, allow_stmt_expr)
                .map(|block| Expr::Stmt(Box::new((block, pos))));
        }
        (Token::EOF, pos) => return Err(PERR::UnexpectedEOF.into_err(*pos)),
        _ => input.next().unwrap(),
    };

    let mut root_expr = match token {
        Token::IntegerConstant(x) => Expr::IntegerConstant(Box::new((x, pos))),
        #[cfg(not(feature = "no_float"))]
        Token::FloatConstant(x) => Expr::FloatConstant(Box::new((x, pos))),
        Token::CharConstant(c) => Expr::CharConstant(Box::new((c, pos))),
        Token::StringConst(s) => Expr::StringConstant(Box::new((s, pos))),
        Token::Identifier(s) => {
            let index = stack.find(&s);
            Expr::Variable(Box::new(((s, pos), None, 0, index)))
        }
        Token::LeftParen => parse_paren_expr(input, stack, pos, allow_stmt_expr)?,
        #[cfg(not(feature = "no_index"))]
        Token::LeftBracket => parse_array_literal(input, stack, pos, allow_stmt_expr)?,
        #[cfg(not(feature = "no_object"))]
        Token::MapStart => parse_map_literal(input, stack, pos, allow_stmt_expr)?,
        Token::True => Expr::True(pos),
        Token::False => Expr::False(pos),
        Token::LexError(err) => return Err(PERR::BadInput(err.to_string()).into_err(pos)),
        token => {
            return Err(PERR::BadInput(format!("Unexpected '{}'", token.syntax())).into_err(pos))
        }
    };

    // Tail processing all possible postfix operators
    loop {
        let (token, _) = input.peek().unwrap();

        if !root_expr.is_valid_postfix(token) {
            break;
        }

        let (token, token_pos) = input.next().unwrap();

        root_expr = match (root_expr, token) {
            // Function call
            (Expr::Variable(x), Token::LeftParen) => {
                let ((name, pos), modules, _, _) = *x;
                parse_call_expr(input, stack, name, modules, pos, allow_stmt_expr)?
            }
            (Expr::Property(_), _) => unreachable!(),
            // module access
            #[cfg(not(feature = "no_module"))]
            (Expr::Variable(x), Token::DoubleColon) => match input.next().unwrap() {
                (Token::Identifier(id2), pos2) => {
                    let ((name, pos), mut modules, _, index) = *x;
                    if let Some(ref mut modules) = modules {
                        modules.push((name, pos));
                    } else {
                        let mut m: ModuleRef = Default::default();
                        m.push((name, pos));
                        modules = Some(Box::new(m));
                    }

                    Expr::Variable(Box::new(((id2, pos2), modules, 0, index)))
                }
                (_, pos2) => return Err(PERR::VariableExpected.into_err(pos2)),
            },
            // Indexing
            #[cfg(not(feature = "no_index"))]
            (expr, Token::LeftBracket) => {
                parse_index_chain(input, stack, expr, token_pos, allow_stmt_expr)?
            }
            // Unknown postfix operator
            (expr, token) => panic!("unknown postfix operator {:?} for {:?}", token, expr),
        }
    }

    match &mut root_expr {
        // Cache the hash key for module-qualified variables
        #[cfg(not(feature = "no_module"))]
        Expr::Variable(x) if x.1.is_some() => {
            let ((name, _), modules, hash, _) = x.as_mut();
            let modules = modules.as_mut().unwrap();

            // Qualifiers + variable name
            *hash = calc_fn_hash(modules.iter().map(|(v, _)| v.as_str()), name, empty());
            modules.set_index(stack.find_module(&modules.get(0).0));
        }
        _ => (),
    }

    Ok(root_expr)
}

/// Parse a potential unary operator.
fn parse_unary<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    allow_stmt_expr: bool,
) -> Result<Expr, Box<ParseError>> {
    match input.peek().unwrap() {
        // If statement is allowed to act as expressions
        (Token::If, pos) => {
            let pos = *pos;
            Ok(Expr::Stmt(Box::new((
                parse_if(input, stack, false, allow_stmt_expr)?,
                pos,
            ))))
        }
        // -expr
        (Token::UnaryMinus, _) => {
            let pos = eat_token(input, Token::UnaryMinus);

            match parse_unary(input, stack, allow_stmt_expr)? {
                // Negative integer
                Expr::IntegerConstant(x) => {
                    let (num, pos) = *x;

                    num.checked_neg()
                        .map(|i| Expr::IntegerConstant(Box::new((i, pos))))
                        .or_else(|| {
                            #[cfg(not(feature = "no_float"))]
                            {
                                Some(Expr::FloatConstant(Box::new((-(x.0 as FLOAT), pos))))
                            }
                            #[cfg(feature = "no_float")]
                            {
                                None
                            }
                        })
                        .ok_or_else(|| {
                            PERR::BadInput(
                                LexError::MalformedNumber(format!("-{}", x.0)).to_string(),
                            )
                            .into_err(pos)
                        })
                }

                // Negative float
                #[cfg(not(feature = "no_float"))]
                Expr::FloatConstant(x) => Ok(Expr::FloatConstant(Box::new((-x.0, x.1)))),

                // Call negative function
                expr => {
                    let op = "-";
                    let hash = calc_fn_hash(empty(), op, repeat(EMPTY_TYPE_ID()).take(2));
                    let mut args = StaticVec::new();
                    args.push(expr);

                    Ok(Expr::FnCall(Box::new((
                        (op.into(), pos),
                        None,
                        hash,
                        args,
                        None,
                    ))))
                }
            }
        }
        // +expr
        (Token::UnaryPlus, _) => {
            eat_token(input, Token::UnaryPlus);
            parse_unary(input, stack, allow_stmt_expr)
        }
        // !expr
        (Token::Bang, _) => {
            let pos = eat_token(input, Token::Bang);
            let mut args = StaticVec::new();
            args.push(parse_primary(input, stack, allow_stmt_expr)?);

            let op = "!";
            let hash = calc_fn_hash(empty(), op, repeat(EMPTY_TYPE_ID()).take(2));

            Ok(Expr::FnCall(Box::new((
                (op.into(), pos),
                None,
                hash,
                args,
                Some(false.into()), // NOT operator, when operating on invalid operand, defaults to false
            ))))
        }
        // <EOF>
        (Token::EOF, pos) => Err(PERR::UnexpectedEOF.into_err(*pos)),
        // All other tokens
        _ => parse_primary(input, stack, allow_stmt_expr),
    }
}

fn make_assignment_stmt<'a>(
    stack: &mut Stack,
    lhs: Expr,
    rhs: Expr,
    pos: Position,
) -> Result<Expr, Box<ParseError>> {
    match &lhs {
        Expr::Variable(x) if x.3.is_none() => Ok(Expr::Assignment(Box::new((lhs, rhs, pos)))),
        Expr::Variable(x) => {
            let ((name, name_pos), _, _, index) = x.as_ref();
            match stack[(stack.len() - index.unwrap().get())].1 {
                ScopeEntryType::Normal => Ok(Expr::Assignment(Box::new((lhs, rhs, pos)))),
                // Constant values cannot be assigned to
                ScopeEntryType::Constant => {
                    Err(PERR::AssignmentToConstant(name.clone()).into_err(*name_pos))
                }
                ScopeEntryType::Module => unreachable!(),
            }
        }
        Expr::Index(x) | Expr::Dot(x) => match &x.0 {
            Expr::Variable(x) if x.3.is_none() => Ok(Expr::Assignment(Box::new((lhs, rhs, pos)))),
            Expr::Variable(x) => {
                let ((name, name_pos), _, _, index) = x.as_ref();
                match stack[(stack.len() - index.unwrap().get())].1 {
                    ScopeEntryType::Normal => Ok(Expr::Assignment(Box::new((lhs, rhs, pos)))),
                    // Constant values cannot be assigned to
                    ScopeEntryType::Constant => {
                        Err(PERR::AssignmentToConstant(name.clone()).into_err(*name_pos))
                    }
                    ScopeEntryType::Module => unreachable!(),
                }
            }
            _ => Err(PERR::AssignmentToCopy.into_err(x.0.position())),
        },
        expr if expr.is_constant() => {
            Err(PERR::AssignmentToConstant("".into()).into_err(lhs.position()))
        }
        _ => Err(PERR::AssignmentToCopy.into_err(lhs.position())),
    }
}

/// Parse an operator-assignment expression.
fn parse_op_assignment_stmt<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    lhs: Expr,
    allow_stmt_expr: bool,
) -> Result<Expr, Box<ParseError>> {
    let (op, pos) = match *input.peek().unwrap() {
        (Token::Equals, _) => {
            let pos = eat_token(input, Token::Equals);
            let rhs = parse_expr(input, stack, allow_stmt_expr)?;
            return make_assignment_stmt(stack, lhs, rhs, pos);
        }
        (Token::PlusAssign, pos) => ("+", pos),
        (Token::MinusAssign, pos) => ("-", pos),
        (Token::MultiplyAssign, pos) => ("*", pos),
        (Token::DivideAssign, pos) => ("/", pos),
        (Token::LeftShiftAssign, pos) => ("<<", pos),
        (Token::RightShiftAssign, pos) => (">>", pos),
        (Token::ModuloAssign, pos) => ("%", pos),
        (Token::PowerOfAssign, pos) => ("~", pos),
        (Token::AndAssign, pos) => ("&", pos),
        (Token::OrAssign, pos) => ("|", pos),
        (Token::XOrAssign, pos) => ("^", pos),
        (_, _) => return Ok(lhs),
    };

    input.next();

    let lhs_copy = lhs.clone();
    let rhs = parse_expr(input, stack, allow_stmt_expr)?;

    // lhs op= rhs -> lhs = op(lhs, rhs)
    let mut args = StaticVec::new();
    args.push(lhs_copy);
    args.push(rhs);

    let hash = calc_fn_hash(empty(), op, repeat(EMPTY_TYPE_ID()).take(args.len()));
    let rhs_expr = Expr::FnCall(Box::new(((op.into(), pos), None, hash, args, None)));

    make_assignment_stmt(stack, lhs, rhs_expr, pos)
}

/// Make a dot expression.
fn make_dot_expr(
    lhs: Expr,
    rhs: Expr,
    op_pos: Position,
    is_index: bool,
) -> Result<Expr, Box<ParseError>> {
    Ok(match (lhs, rhs) {
        // idx_lhs[idx_rhs].rhs
        // Attach dot chain to the bottom level of indexing chain
        (Expr::Index(x), rhs) => {
            Expr::Index(Box::new((x.0, make_dot_expr(x.1, rhs, op_pos, true)?, x.2)))
        }
        // lhs.id
        (lhs, Expr::Variable(x)) if x.1.is_none() => {
            let (name, pos) = x.0;
            let lhs = if is_index { lhs.into_property() } else { lhs };

            let getter = make_getter(&name);
            let setter = make_setter(&name);
            let rhs = Expr::Property(Box::new(((name, getter, setter), pos)));

            Expr::Dot(Box::new((lhs, rhs, op_pos)))
        }
        (lhs, Expr::Property(x)) => {
            let lhs = if is_index { lhs.into_property() } else { lhs };
            let rhs = Expr::Property(x);
            Expr::Dot(Box::new((lhs, rhs, op_pos)))
        }
        // lhs.module::id - syntax error
        (_, Expr::Variable(x)) if x.1.is_some() => {
            #[cfg(feature = "no_module")]
            unreachable!();
            #[cfg(not(feature = "no_module"))]
            return Err(PERR::PropertyExpected.into_err(x.1.unwrap().get(0).1));
        }
        // lhs.dot_lhs.dot_rhs
        (lhs, Expr::Dot(x)) => {
            let (dot_lhs, dot_rhs, pos) = *x;
            Expr::Dot(Box::new((
                lhs,
                Expr::Dot(Box::new((
                    dot_lhs.into_property(),
                    dot_rhs.into_property(),
                    pos,
                ))),
                op_pos,
            )))
        }
        // lhs.idx_lhs[idx_rhs]
        (lhs, Expr::Index(x)) => {
            let (idx_lhs, idx_rhs, pos) = *x;
            Expr::Dot(Box::new((
                lhs,
                Expr::Index(Box::new((
                    idx_lhs.into_property(),
                    idx_rhs.into_property(),
                    pos,
                ))),
                op_pos,
            )))
        }
        // lhs.rhs
        (lhs, rhs) => Expr::Dot(Box::new((lhs, rhs.into_property(), op_pos))),
    })
}

/// Make an 'in' expression.
fn make_in_expr(lhs: Expr, rhs: Expr, op_pos: Position) -> Result<Expr, Box<ParseError>> {
    match (&lhs, &rhs) {
        (_, Expr::IntegerConstant(x)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression expects a string, array or object map".into(),
            )
            .into_err(x.1))
        }

        (_, Expr::And(x)) | (_, Expr::Or(x)) | (_, Expr::In(x)) | (_, Expr::Assignment(x)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression expects a string, array or object map".into(),
            )
            .into_err(x.2))
        }

        (_, Expr::True(pos)) | (_, Expr::False(pos)) | (_, Expr::Unit(pos)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression expects a string, array or object map".into(),
            )
            .into_err(*pos))
        }

        #[cfg(not(feature = "no_float"))]
        (_, Expr::FloatConstant(x)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression expects a string, array or object map".into(),
            )
            .into_err(x.1))
        }

        // "xxx" in "xxxx", 'x' in "xxxx" - OK!
        (Expr::StringConstant(_), Expr::StringConstant(_))
        | (Expr::CharConstant(_), Expr::StringConstant(_)) => (),

        // 123.456 in "xxxx"
        #[cfg(not(feature = "no_float"))]
        (Expr::FloatConstant(x), Expr::StringConstant(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for a string expects a string, not a float".into(),
            )
            .into_err(x.1))
        }
        // 123 in "xxxx"
        (Expr::IntegerConstant(x), Expr::StringConstant(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for a string expects a string, not a number".into(),
            )
            .into_err(x.1))
        }
        // (??? && ???) in "xxxx", (??? || ???) in "xxxx", (??? in ???) in "xxxx",
        (Expr::And(x), Expr::StringConstant(_))
        | (Expr::Or(x), Expr::StringConstant(_))
        | (Expr::In(x), Expr::StringConstant(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for a string expects a string, not a boolean".into(),
            )
            .into_err(x.2))
        }
        //  true in "xxxx", false in "xxxx"
        (Expr::True(pos), Expr::StringConstant(_))
        | (Expr::False(pos), Expr::StringConstant(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for a string expects a string, not a boolean".into(),
            )
            .into_err(*pos))
        }
        // [???, ???, ???] in "xxxx"
        (Expr::Array(x), Expr::StringConstant(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for a string expects a string, not an array".into(),
            )
            .into_err(x.1))
        }
        // #{...} in "xxxx"
        (Expr::Map(x), Expr::StringConstant(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for a string expects a string, not an object map".into(),
            )
            .into_err(x.1))
        }
        // (??? = ???) in "xxxx"
        (Expr::Assignment(x), Expr::StringConstant(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for a string expects a string, not ()".into(),
            )
            .into_err(x.2))
        }
        // () in "xxxx"
        (Expr::Unit(pos), Expr::StringConstant(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for a string expects a string, not ()".into(),
            )
            .into_err(*pos))
        }

        // "xxx" in #{...}, 'x' in #{...} - OK!
        (Expr::StringConstant(_), Expr::Map(_)) | (Expr::CharConstant(_), Expr::Map(_)) => (),

        // 123.456 in #{...}
        #[cfg(not(feature = "no_float"))]
        (Expr::FloatConstant(x), Expr::Map(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for an object map expects a string, not a float".into(),
            )
            .into_err(x.1))
        }
        // 123 in #{...}
        (Expr::IntegerConstant(x), Expr::Map(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for an object map expects a string, not a number".into(),
            )
            .into_err(x.1))
        }
        // (??? && ???) in #{...}, (??? || ???) in #{...}, (??? in ???) in #{...},
        (Expr::And(x), Expr::Map(_))
        | (Expr::Or(x), Expr::Map(_))
        | (Expr::In(x), Expr::Map(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for an object map expects a string, not a boolean".into(),
            )
            .into_err(x.2))
        }
        // true in #{...}, false in #{...}
        (Expr::True(pos), Expr::Map(_)) | (Expr::False(pos), Expr::Map(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for an object map expects a string, not a boolean".into(),
            )
            .into_err(*pos))
        }
        // [???, ???, ???] in #{..}
        (Expr::Array(x), Expr::Map(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for an object map expects a string, not an array".into(),
            )
            .into_err(x.1))
        }
        // #{...} in #{..}
        (Expr::Map(x), Expr::Map(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for an object map expects a string, not an object map".into(),
            )
            .into_err(x.1))
        }
        // (??? = ???) in #{...}
        (Expr::Assignment(x), Expr::Map(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for an object map expects a string, not ()".into(),
            )
            .into_err(x.2))
        }
        // () in #{...}
        (Expr::Unit(pos), Expr::Map(_)) => {
            return Err(PERR::MalformedInExpr(
                "'in' expression for an object map expects a string, not ()".into(),
            )
            .into_err(*pos))
        }

        _ => (),
    }

    Ok(Expr::In(Box::new((lhs, rhs, op_pos))))
}

/// Parse a binary expression.
fn parse_binary_op<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    parent_precedence: u8,
    lhs: Expr,
    allow_stmt_expr: bool,
) -> Result<Expr, Box<ParseError>> {
    let mut current_lhs = lhs;

    loop {
        let (current_precedence, bind_right) = input.peek().map_or_else(
            || (0, false),
            |(current_op, _)| (current_op.precedence(), current_op.is_bind_right()),
        );

        // Bind left to the parent lhs expression if precedence is higher
        // If same precedence, then check if the operator binds right
        if current_precedence < parent_precedence
            || (current_precedence == parent_precedence && !bind_right)
        {
            return Ok(current_lhs);
        }

        let (op_token, pos) = input.next().unwrap();

        let rhs = parse_unary(input, stack, allow_stmt_expr)?;

        let next_precedence = input.peek().unwrap().0.precedence();

        // Bind to right if the next operator has higher precedence
        // If same precedence, then check if the operator binds right
        let rhs = if (current_precedence == next_precedence && bind_right)
            || current_precedence < next_precedence
        {
            parse_binary_op(input, stack, current_precedence, rhs, allow_stmt_expr)?
        } else {
            // Otherwise bind to left (even if next operator has the same precedence)
            rhs
        };

        let cmp_def = Some(false.into());
        let op = op_token.syntax();
        let hash = calc_fn_hash(empty(), &op, repeat(EMPTY_TYPE_ID()).take(2));

        let mut args = StaticVec::new();
        args.push(current_lhs);
        args.push(rhs);

        current_lhs = match op_token {
            Token::Plus => Expr::FnCall(Box::new(((op, pos), None, hash, args, None))),
            Token::Minus => Expr::FnCall(Box::new(((op, pos), None, hash, args, None))),
            Token::Multiply => Expr::FnCall(Box::new(((op, pos), None, hash, args, None))),
            Token::Divide => Expr::FnCall(Box::new(((op, pos), None, hash, args, None))),

            Token::LeftShift => Expr::FnCall(Box::new(((op, pos), None, hash, args, None))),
            Token::RightShift => Expr::FnCall(Box::new(((op, pos), None, hash, args, None))),
            Token::Modulo => Expr::FnCall(Box::new(((op, pos), None, hash, args, None))),
            Token::PowerOf => Expr::FnCall(Box::new(((op, pos), None, hash, args, None))),

            // Comparison operators default to false when passed invalid operands
            Token::EqualsTo => Expr::FnCall(Box::new(((op, pos), None, hash, args, cmp_def))),
            Token::NotEqualsTo => Expr::FnCall(Box::new(((op, pos), None, hash, args, cmp_def))),
            Token::LessThan => Expr::FnCall(Box::new(((op, pos), None, hash, args, cmp_def))),
            Token::LessThanEqualsTo => {
                Expr::FnCall(Box::new(((op, pos), None, hash, args, cmp_def)))
            }
            Token::GreaterThan => Expr::FnCall(Box::new(((op, pos), None, hash, args, cmp_def))),
            Token::GreaterThanEqualsTo => {
                Expr::FnCall(Box::new(((op, pos), None, hash, args, cmp_def)))
            }

            Token::Or => {
                let rhs = args.pop();
                let current_lhs = args.pop();
                Expr::Or(Box::new((current_lhs, rhs, pos)))
            }
            Token::And => {
                let rhs = args.pop();
                let current_lhs = args.pop();
                Expr::And(Box::new((current_lhs, rhs, pos)))
            }
            Token::Ampersand => Expr::FnCall(Box::new(((op, pos), None, hash, args, None))),
            Token::Pipe => Expr::FnCall(Box::new(((op, pos), None, hash, args, None))),
            Token::XOr => Expr::FnCall(Box::new(((op, pos), None, hash, args, None))),

            Token::In => {
                let rhs = args.pop();
                let current_lhs = args.pop();
                make_in_expr(current_lhs, rhs, pos)?
            }

            #[cfg(not(feature = "no_object"))]
            Token::Period => {
                let mut rhs = args.pop();
                let current_lhs = args.pop();

                match &mut rhs {
                    // current_lhs.rhs(...) - method call
                    Expr::FnCall(x) => {
                        let ((id, _), _, hash, args, _) = x.as_mut();
                        // Recalculate function call hash because there is an additional argument
                        let args_iter = repeat(EMPTY_TYPE_ID()).take(args.len() + 1);
                        *hash = calc_fn_hash(empty(), id, args_iter);
                    }
                    _ => (),
                }

                make_dot_expr(current_lhs, rhs, pos, false)?
            }

            token => return Err(PERR::UnknownOperator(token.into()).into_err(pos)),
        };
    }
}

/// Parse an expression.
fn parse_expr<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    allow_stmt_expr: bool,
) -> Result<Expr, Box<ParseError>> {
    let lhs = parse_unary(input, stack, allow_stmt_expr)?;
    parse_binary_op(input, stack, 1, lhs, allow_stmt_expr)
}

/// Make sure that the expression is not a statement expression (i.e. wrapped in `{}`).
fn ensure_not_statement_expr<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    type_name: &str,
) -> Result<(), Box<ParseError>> {
    match input.peek().unwrap() {
        // Disallow statement expressions
        (Token::LeftBrace, pos) | (Token::EOF, pos) => {
            Err(PERR::ExprExpected(type_name.to_string()).into_err(*pos))
        }
        // No need to check for others at this time - leave it for the expr parser
        _ => Ok(()),
    }
}

/// Make sure that the expression is not a mis-typed assignment (i.e. `a = b` instead of `a == b`).
fn ensure_not_assignment<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
) -> Result<(), Box<ParseError>> {
    match input.peek().unwrap() {
        (Token::Equals, pos) => {
            return Err(PERR::BadInput("Possibly a typo of '=='?".to_string()).into_err(*pos))
        }
        (Token::PlusAssign, pos)
        | (Token::MinusAssign, pos)
        | (Token::MultiplyAssign, pos)
        | (Token::DivideAssign, pos)
        | (Token::LeftShiftAssign, pos)
        | (Token::RightShiftAssign, pos)
        | (Token::ModuloAssign, pos)
        | (Token::PowerOfAssign, pos)
        | (Token::AndAssign, pos)
        | (Token::OrAssign, pos)
        | (Token::XOrAssign, pos) => {
            return Err(PERR::BadInput(
                "Expecting a boolean expression, not an assignment".to_string(),
            )
            .into_err(*pos))
        }

        _ => Ok(()),
    }
}

/// Parse an if statement.
fn parse_if<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    breakable: bool,
    allow_stmt_expr: bool,
) -> Result<Stmt, Box<ParseError>> {
    // if ...
    eat_token(input, Token::If);

    // if guard { if_body }
    ensure_not_statement_expr(input, "a boolean")?;
    let guard = parse_expr(input, stack, allow_stmt_expr)?;
    ensure_not_assignment(input)?;
    let if_body = parse_block(input, stack, breakable, allow_stmt_expr)?;

    // if guard { if_body } else ...
    let else_body = if match_token(input, Token::Else).unwrap_or(false) {
        Some(if let (Token::If, _) = input.peek().unwrap() {
            // if guard { if_body } else if ...
            parse_if(input, stack, breakable, allow_stmt_expr)?
        } else {
            // if guard { if_body } else { else-body }
            parse_block(input, stack, breakable, allow_stmt_expr)?
        })
    } else {
        None
    };

    Ok(Stmt::IfThenElse(Box::new((guard, if_body, else_body))))
}

/// Parse a while loop.
fn parse_while<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    allow_stmt_expr: bool,
) -> Result<Stmt, Box<ParseError>> {
    // while ...
    eat_token(input, Token::While);

    // while guard { body }
    ensure_not_statement_expr(input, "a boolean")?;
    let guard = parse_expr(input, stack, allow_stmt_expr)?;
    ensure_not_assignment(input)?;
    let body = parse_block(input, stack, true, allow_stmt_expr)?;

    Ok(Stmt::While(Box::new((guard, body))))
}

/// Parse a loop statement.
fn parse_loop<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    allow_stmt_expr: bool,
) -> Result<Stmt, Box<ParseError>> {
    // loop ...
    eat_token(input, Token::Loop);

    // loop { body }
    let body = parse_block(input, stack, true, allow_stmt_expr)?;

    Ok(Stmt::Loop(Box::new(body)))
}

/// Parse a for loop.
fn parse_for<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    allow_stmt_expr: bool,
) -> Result<Stmt, Box<ParseError>> {
    // for ...
    eat_token(input, Token::For);

    // for name ...
    let name = match input.next().unwrap() {
        // Variable name
        (Token::Identifier(s), _) => s,
        // Bad identifier
        (Token::LexError(err), pos) => return Err(PERR::BadInput(err.to_string()).into_err(pos)),
        // EOF
        (Token::EOF, pos) => return Err(PERR::VariableExpected.into_err(pos)),
        // Not a variable name
        (_, pos) => return Err(PERR::VariableExpected.into_err(pos)),
    };

    // for name in ...
    match input.next().unwrap() {
        (Token::In, _) => (),
        (Token::LexError(err), pos) => return Err(PERR::BadInput(err.to_string()).into_err(pos)),
        (_, pos) => {
            return Err(
                PERR::MissingToken(Token::In.into(), "after the iteration variable".into())
                    .into_err(pos),
            )
        }
    }

    // for name in expr { body }
    ensure_not_statement_expr(input, "a boolean")?;
    let expr = parse_expr(input, stack, allow_stmt_expr)?;

    let prev_len = stack.len();
    stack.push((name.clone(), ScopeEntryType::Normal));

    let body = parse_block(input, stack, true, allow_stmt_expr)?;

    stack.truncate(prev_len);

    Ok(Stmt::For(Box::new((name, expr, body))))
}

/// Parse a variable definition statement.
fn parse_let<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    var_type: ScopeEntryType,
    allow_stmt_expr: bool,
) -> Result<Stmt, Box<ParseError>> {
    // let/const... (specified in `var_type`)
    input.next();

    // let name ...
    let (name, pos) = match input.next().unwrap() {
        (Token::Identifier(s), pos) => (s, pos),
        (Token::LexError(err), pos) => return Err(PERR::BadInput(err.to_string()).into_err(pos)),
        (_, pos) => return Err(PERR::VariableExpected.into_err(pos)),
    };

    // let name = ...
    if match_token(input, Token::Equals)? {
        // let name = expr
        let init_value = parse_expr(input, stack, allow_stmt_expr)?;

        match var_type {
            // let name = expr
            ScopeEntryType::Normal => {
                stack.push((name.clone(), ScopeEntryType::Normal));
                Ok(Stmt::Let(Box::new(((name, pos), Some(init_value)))))
            }
            // const name = { expr:constant }
            ScopeEntryType::Constant if init_value.is_constant() => {
                stack.push((name.clone(), ScopeEntryType::Constant));
                Ok(Stmt::Const(Box::new(((name, pos), init_value))))
            }
            // const name = expr - error
            ScopeEntryType::Constant => {
                Err(PERR::ForbiddenConstantExpr(name).into_err(init_value.position()))
            }
            // Variable cannot be a module
            ScopeEntryType::Module => unreachable!(),
        }
    } else {
        // let name
        match var_type {
            ScopeEntryType::Normal => {
                stack.push((name.clone(), ScopeEntryType::Normal));
                Ok(Stmt::Let(Box::new(((name, pos), None))))
            }
            ScopeEntryType::Constant => {
                stack.push((name.clone(), ScopeEntryType::Constant));
                Ok(Stmt::Const(Box::new(((name, pos), Expr::Unit(pos)))))
            }
            // Variable cannot be a module
            ScopeEntryType::Module => unreachable!(),
        }
    }
}

/// Parse an import statement.
fn parse_import<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    allow_stmt_expr: bool,
) -> Result<Stmt, Box<ParseError>> {
    // import ...
    let pos = eat_token(input, Token::Import);

    // import expr ...
    let expr = parse_expr(input, stack, allow_stmt_expr)?;

    // import expr as ...
    match input.next().unwrap() {
        (Token::As, _) => (),
        (_, pos) => {
            return Err(
                PERR::MissingToken(Token::As.into(), "in this import statement".into())
                    .into_err(pos),
            )
        }
    }

    // import expr as name ...
    let (name, _) = match input.next().unwrap() {
        (Token::Identifier(s), pos) => (s, pos),
        (Token::LexError(err), pos) => return Err(PERR::BadInput(err.to_string()).into_err(pos)),
        (_, pos) => return Err(PERR::VariableExpected.into_err(pos)),
    };

    stack.push((name.clone(), ScopeEntryType::Module));
    Ok(Stmt::Import(Box::new((expr, (name, pos)))))
}

/// Parse an export statement.
fn parse_export<'a>(input: &mut Peekable<TokenIterator<'a>>) -> Result<Stmt, Box<ParseError>> {
    eat_token(input, Token::Export);

    let mut exports = StaticVec::new();

    loop {
        let (id, id_pos) = match input.next().unwrap() {
            (Token::Identifier(s), pos) => (s.clone(), pos),
            (Token::LexError(err), pos) => {
                return Err(PERR::BadInput(err.to_string()).into_err(pos))
            }
            (_, pos) => return Err(PERR::VariableExpected.into_err(pos)),
        };

        let rename = if match_token(input, Token::As)? {
            match input.next().unwrap() {
                (Token::Identifier(s), pos) => Some((s.clone(), pos)),
                (_, pos) => return Err(PERR::VariableExpected.into_err(pos)),
            }
        } else {
            None
        };

        exports.push(((id, id_pos), rename));

        match input.peek().unwrap() {
            (Token::Comma, _) => {
                eat_token(input, Token::Comma);
            }
            (Token::Identifier(_), pos) => {
                return Err(PERR::MissingToken(
                    Token::Comma.into(),
                    "to separate the list of exports".into(),
                )
                .into_err(*pos))
            }
            _ => break,
        }
    }

    // Check for duplicating parameters
    exports
        .iter()
        .enumerate()
        .try_for_each(|(i, ((id1, _), _))| {
            exports
                .iter()
                .skip(i + 1)
                .find(|((id2, _), _)| id2 == id1)
                .map_or_else(|| Ok(()), |((id2, pos), _)| Err((id2, *pos)))
        })
        .map_err(|(id2, pos)| PERR::DuplicatedExport(id2.to_string()).into_err(pos))?;

    Ok(Stmt::Export(Box::new(exports)))
}

/// Parse a statement block.
fn parse_block<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    breakable: bool,
    allow_stmt_expr: bool,
) -> Result<Stmt, Box<ParseError>> {
    // Must start with {
    let pos = match input.next().unwrap() {
        (Token::LeftBrace, pos) => pos,
        (Token::LexError(err), pos) => return Err(PERR::BadInput(err.to_string()).into_err(pos)),
        (_, pos) => {
            return Err(PERR::MissingToken(
                Token::LeftBrace.into(),
                "to start a statement block".into(),
            )
            .into_err(pos))
        }
    };

    let mut statements = StaticVec::new();
    let prev_len = stack.len();

    while !match_token(input, Token::RightBrace)? {
        // Parse statements inside the block
        let stmt = parse_stmt(input, stack, breakable, false, allow_stmt_expr)?;

        // See if it needs a terminating semicolon
        let need_semicolon = !stmt.is_self_terminated();

        statements.push(stmt);

        match input.peek().unwrap() {
            // { ... stmt }
            (Token::RightBrace, _) => {
                eat_token(input, Token::RightBrace);
                break;
            }
            // { ... stmt;
            (Token::SemiColon, _) if need_semicolon => {
                eat_token(input, Token::SemiColon);
            }
            // { ... { stmt } ;
            (Token::SemiColon, _) if !need_semicolon => (),
            // { ... { stmt } ???
            (_, _) if !need_semicolon => (),
            // { ... stmt <error>
            (Token::LexError(err), pos) => {
                return Err(PERR::BadInput(err.to_string()).into_err(*pos))
            }
            // { ... stmt ???
            (_, pos) => {
                // Semicolons are not optional between statements
                return Err(PERR::MissingToken(
                    Token::SemiColon.into(),
                    "to terminate this statement".into(),
                )
                .into_err(*pos));
            }
        }
    }

    stack.truncate(prev_len);

    Ok(Stmt::Block(Box::new((statements, pos))))
}

/// Parse an expression as a statement.
fn parse_expr_stmt<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    allow_stmt_expr: bool,
) -> Result<Stmt, Box<ParseError>> {
    let expr = parse_expr(input, stack, allow_stmt_expr)?;
    let expr = parse_op_assignment_stmt(input, stack, expr, allow_stmt_expr)?;
    Ok(Stmt::Expr(Box::new(expr)))
}

/// Parse a single statement.
fn parse_stmt<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    breakable: bool,
    is_global: bool,
    allow_stmt_expr: bool,
) -> Result<Stmt, Box<ParseError>> {
    let (token, pos) = match input.peek().unwrap() {
        (Token::EOF, pos) => return Ok(Stmt::Noop(*pos)),
        x => x,
    };

    match token {
        // Semicolon - empty statement
        Token::SemiColon => Ok(Stmt::Noop(*pos)),

        Token::LeftBrace => parse_block(input, stack, breakable, allow_stmt_expr),

        // fn ...
        Token::Fn if !is_global => Err(PERR::WrongFnDefinition.into_err(*pos)),
        Token::Fn => unreachable!(),

        Token::If => parse_if(input, stack, breakable, allow_stmt_expr),
        Token::While => parse_while(input, stack, allow_stmt_expr),
        Token::Loop => parse_loop(input, stack, allow_stmt_expr),
        Token::For => parse_for(input, stack, allow_stmt_expr),

        Token::Continue if breakable => {
            let pos = eat_token(input, Token::Continue);
            Ok(Stmt::Continue(pos))
        }
        Token::Break if breakable => {
            let pos = eat_token(input, Token::Break);
            Ok(Stmt::Break(pos))
        }
        Token::Continue | Token::Break => Err(PERR::LoopBreak.into_err(*pos)),

        Token::Return | Token::Throw => {
            let pos = *pos;

            let return_type = match input.next().unwrap() {
                (Token::Return, _) => ReturnType::Return,
                (Token::Throw, _) => ReturnType::Exception,
                _ => panic!("token should be return or throw"),
            };

            match input.peek().unwrap() {
                // `return`/`throw` at <EOF>
                (Token::EOF, pos) => Ok(Stmt::ReturnWithVal(Box::new(((return_type, *pos), None)))),
                // `return;` or `throw;`
                (Token::SemiColon, _) => {
                    Ok(Stmt::ReturnWithVal(Box::new(((return_type, pos), None))))
                }
                // `return` or `throw` with expression
                (_, _) => {
                    let expr = parse_expr(input, stack, allow_stmt_expr)?;
                    let pos = expr.position();

                    Ok(Stmt::ReturnWithVal(Box::new((
                        (return_type, pos),
                        Some(expr),
                    ))))
                }
            }
        }

        Token::Let => parse_let(input, stack, ScopeEntryType::Normal, allow_stmt_expr),
        Token::Const => parse_let(input, stack, ScopeEntryType::Constant, allow_stmt_expr),

        #[cfg(not(feature = "no_module"))]
        Token::Import => parse_import(input, stack, allow_stmt_expr),

        #[cfg(not(feature = "no_module"))]
        Token::Export if !is_global => Err(PERR::WrongExport.into_err(*pos)),

        #[cfg(not(feature = "no_module"))]
        Token::Export => parse_export(input),

        _ => parse_expr_stmt(input, stack, allow_stmt_expr),
    }
}

/// Parse a function definition.
fn parse_fn<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    stack: &mut Stack,
    access: FnAccess,
    allow_stmt_expr: bool,
) -> Result<FnDef, Box<ParseError>> {
    let pos = eat_token(input, Token::Fn);

    let name = match input.next().unwrap() {
        (Token::Identifier(s), _) => s,
        (_, pos) => return Err(PERR::FnMissingName.into_err(pos)),
    };

    match input.peek().unwrap() {
        (Token::LeftParen, _) => eat_token(input, Token::LeftParen),
        (_, pos) => return Err(PERR::FnMissingParams(name).into_err(*pos)),
    };

    let mut params = Vec::new();

    if !match_token(input, Token::RightParen)? {
        let end_err = format!("to close the parameters list of function '{}'", name);
        let sep_err = format!("to separate the parameters of function '{}'", name);

        loop {
            match input.next().unwrap() {
                (Token::Identifier(s), pos) => {
                    stack.push((s.clone(), ScopeEntryType::Normal));
                    params.push((s, pos))
                }
                (Token::LexError(err), pos) => {
                    return Err(PERR::BadInput(err.to_string()).into_err(pos))
                }
                (_, pos) => {
                    return Err(PERR::MissingToken(Token::RightParen.into(), end_err).into_err(pos))
                }
            }

            match input.next().unwrap() {
                (Token::RightParen, _) => break,
                (Token::Comma, _) => (),
                (Token::Identifier(_), pos) => {
                    return Err(PERR::MissingToken(Token::Comma.into(), sep_err).into_err(pos))
                }
                (Token::LexError(err), pos) => {
                    return Err(PERR::BadInput(err.to_string()).into_err(pos))
                }
                (_, pos) => {
                    return Err(PERR::MissingToken(Token::Comma.into(), sep_err).into_err(pos))
                }
            }
        }
    }

    // Check for duplicating parameters
    params
        .iter()
        .enumerate()
        .try_for_each(|(i, (p1, _))| {
            params
                .iter()
                .skip(i + 1)
                .find(|(p2, _)| p2 == p1)
                .map_or_else(|| Ok(()), |(p2, pos)| Err((p2, *pos)))
        })
        .map_err(|(p, pos)| {
            PERR::FnDuplicatedParam(name.to_string(), p.to_string()).into_err(pos)
        })?;

    // Parse function body
    let body = match input.peek().unwrap() {
        (Token::LeftBrace, _) => parse_block(input, stack, false, allow_stmt_expr)?,
        (_, pos) => return Err(PERR::FnMissingBody(name).into_err(*pos)),
    };

    let params = params.into_iter().map(|(p, _)| p).collect();

    Ok(FnDef {
        name,
        access,
        params,
        body,
        pos,
    })
}

pub fn parse_global_expr<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    engine: &Engine,
    scope: &Scope,
    optimization_level: OptimizationLevel,
) -> Result<AST, Box<ParseError>> {
    let mut stack = Stack::new();
    let expr = parse_expr(input, &mut stack, false)?;

    match input.peek().unwrap() {
        (Token::EOF, _) => (),
        // Return error if the expression doesn't end
        (token, pos) => {
            return Err(PERR::BadInput(format!("Unexpected '{}'", token.syntax())).into_err(*pos))
        }
    }

    Ok(
        // Optimize AST
        optimize_into_ast(
            engine,
            scope,
            vec![Stmt::Expr(Box::new(expr))],
            vec![],
            optimization_level,
        ),
    )
}

/// Parse the global level statements.
fn parse_global_level<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
) -> Result<(Vec<Stmt>, HashMap<u64, FnDef>), Box<ParseError>> {
    let mut statements = Vec::<Stmt>::new();
    let mut functions = HashMap::<u64, FnDef>::new();
    let mut stack = Stack::new();

    while !input.peek().unwrap().0.is_eof() {
        // Collect all the function definitions
        #[cfg(not(feature = "no_function"))]
        {
            let mut access = FnAccess::Public;
            let mut must_be_fn = false;

            if match_token(input, Token::Private)? {
                access = FnAccess::Private;
                must_be_fn = true;
            }

            match input.peek().unwrap() {
                (Token::Fn, _) => {
                    let mut stack = Stack::new();
                    let func = parse_fn(input, &mut stack, access, true)?;

                    // Qualifiers (none) + function name + argument `TypeId`'s
                    let hash = calc_fn_hash(
                        empty(),
                        &func.name,
                        repeat(EMPTY_TYPE_ID()).take(func.params.len()),
                    );

                    functions.insert(hash, func);
                    continue;
                }
                (_, pos) if must_be_fn => {
                    return Err(PERR::MissingToken(
                        Token::Fn.into(),
                        format!("following '{}'", Token::Private.syntax()),
                    )
                    .into_err(*pos))
                }
                _ => (),
            }
        }
        // Actual statement
        let stmt = parse_stmt(input, &mut stack, false, true, true)?;

        let need_semicolon = !stmt.is_self_terminated();

        statements.push(stmt);

        match input.peek().unwrap() {
            // EOF
            (Token::EOF, _) => break,
            // stmt ;
            (Token::SemiColon, _) if need_semicolon => {
                eat_token(input, Token::SemiColon);
            }
            // stmt ;
            (Token::SemiColon, _) if !need_semicolon => (),
            // { stmt } ???
            (_, _) if !need_semicolon => (),
            // stmt <error>
            (Token::LexError(err), pos) => {
                return Err(PERR::BadInput(err.to_string()).into_err(*pos))
            }
            // stmt ???
            (_, pos) => {
                // Semicolons are not optional between statements
                return Err(PERR::MissingToken(
                    Token::SemiColon.into(),
                    "to terminate this statement".into(),
                )
                .into_err(*pos));
            }
        }
    }

    Ok((statements, functions))
}

/// Run the parser on an input stream, returning an AST.
pub fn parse<'a>(
    input: &mut Peekable<TokenIterator<'a>>,
    engine: &Engine,
    scope: &Scope,
    optimization_level: OptimizationLevel,
) -> Result<AST, Box<ParseError>> {
    let (statements, functions) = parse_global_level(input)?;

    let fn_lib = functions.into_iter().map(|(_, v)| v).collect();
    Ok(
        // Optimize AST
        optimize_into_ast(engine, scope, statements, fn_lib, optimization_level),
    )
}

/// Map a `Dynamic` value to an expression.
///
/// Returns Some(expression) if conversion is successful.  Otherwise None.
pub fn map_dynamic_to_expr(value: Dynamic, pos: Position) -> Option<Expr> {
    match value.0 {
        #[cfg(not(feature = "no_float"))]
        Union::Float(value) => Some(Expr::FloatConstant(Box::new((value, pos)))),

        Union::Unit(_) => Some(Expr::Unit(pos)),
        Union::Int(value) => Some(Expr::IntegerConstant(Box::new((value, pos)))),
        Union::Char(value) => Some(Expr::CharConstant(Box::new((value, pos)))),
        Union::Str(value) => Some(Expr::StringConstant(Box::new(((*value).clone(), pos)))),
        Union::Bool(true) => Some(Expr::True(pos)),
        Union::Bool(false) => Some(Expr::False(pos)),
        #[cfg(not(feature = "no_index"))]
        Union::Array(array) => {
            let items: Vec<_> = array
                .into_iter()
                .map(|x| map_dynamic_to_expr(x, pos))
                .collect();

            if items.iter().all(Option::is_some) {
                Some(Expr::Array(Box::new((
                    items.into_iter().map(Option::unwrap).collect(),
                    pos,
                ))))
            } else {
                None
            }
        }
        #[cfg(not(feature = "no_object"))]
        Union::Map(map) => {
            let items: Vec<_> = map
                .into_iter()
                .map(|(k, v)| ((k, pos), map_dynamic_to_expr(v, pos)))
                .collect();

            if items.iter().all(|(_, expr)| expr.is_some()) {
                Some(Expr::Map(Box::new((
                    items
                        .into_iter()
                        .map(|((k, pos), expr)| ((k, pos), expr.unwrap()))
                        .collect(),
                    pos,
                ))))
            } else {
                None
            }
        }

        _ => None,
    }
}
