//! The whole transpiler: CEL text in, a SQL boolean fragment plus bind values
//! out.
//!
//! The pipeline is three passes over one small AST, and they are worth naming
//! because the module is organised in that order:
//!
//! 1. **Parse.** [`Filter::compile`] hands the source to cel's parser and keeps
//!    the resulting `IdedExpr`. Nothing SQL-shaped happens here.
//! 2. **Resolve.** Every leaf becomes an [`Operand`] -- a column, a constant, or
//!    `NULL`. Constants are produced by *evaluating* the subtree with cel's own
//!    interpreter (see [`Operand::resolve`]), which is how `timestamp()` and
//!    `duration()` work without this crate owning a date parser.
//! 3. **Emit.** Predicates are written into a [`SqlSink`], which is either sqlx's
//!    `QueryBuilder` or our own [`SqlFragment`]. Both delegate placeholder formatting to
//!    `Arguments::format_placeholder`, so `$1` vs `?` is never our problem.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

use cel::common::ast::{Expr, IdedExpr, operators};
use cel::parser::Parser;
use chrono::{DateTime, Utc};
use sqlx::database::Database;
use sqlx::encode::{Encode, IsNull};
use sqlx::error::BoxDynError;
use sqlx::types::Type;
use sqlx::{Arguments, IntoArguments, QueryBuilder};

/// The `ESCAPE` character used by every `LIKE` this crate emits.
///
/// Not backslash. A backslash escape has to be written `'\'` on PostgreSQL and
/// SQLite but `'\\'` on MySQL, because MySQL treats backslash as an escape
/// inside string literals too. `'!'` is spelled the same way everywhere, and
/// since we escape occurrences of it in the pattern, the choice is otherwise
/// arbitrary.
const LIKE_ESCAPE: char = '!';

/// Bound on how deeply the parser will nest before giving up.
///
/// cel's default is 96. This is lower because the emitter recurses over the
/// same tree, so the parser's limit is also the bound on our stack depth, and a
/// filter language has no business being 96 levels deep.
const MAX_DEPTH: u16 = 32;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything that can go wrong between CEL text and a SQL fragment.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The source is not valid CEL.
    Parse(cel::ParseErrors),
    /// A constant subexpression could not be evaluated -- for example
    /// `timestamp('not a date')`.
    Eval(cel::ExecutionError),
    /// The expression names something the [`Schema`] does not expose. The
    /// payload is the dotted CEL path, not a SQL identifier.
    UnknownColumn(String),
    /// Valid CEL that this crate declines to translate.
    Unsupported(&'static str),
    /// A function or method with no SQL equivalent registered here.
    UnknownFunction(String),
    /// A comparison between a column and a constant of the wrong type.
    TypeMismatch {
        /// The dotted CEL path of the column.
        column: String,
        /// What the schema says the column holds.
        expected: ColumnType,
        /// What the expression tried to compare it against.
        actual: ColumnType,
    },
    /// A `uint` literal above `i64::MAX`. No supported database has an
    /// unsigned 64-bit bind type to widen into.
    IntegerOverflow(u64),
    /// The driver refused to encode a bind value.
    Encode(BoxDynError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(f, "invalid CEL: {error}"),
            Self::Eval(error) => write!(f, "cannot evaluate constant expression: {error}"),
            Self::UnknownColumn(path) => write!(f, "no such column: `{path}`"),
            Self::Unsupported(what) => write!(f, "unsupported in a SQL filter: {what}"),
            Self::UnknownFunction(name) => write!(f, "no SQL translation for function `{name}`"),
            Self::TypeMismatch {
                column,
                expected,
                actual,
            } => write!(
                f,
                "column `{column}` is {expected}, compared against {actual}"
            ),
            Self::IntegerOverflow(value) => write!(f, "unsigned literal {value} exceeds i64::MAX"),
            Self::Encode(error) => write!(f, "cannot encode bind value: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::Eval(error) => Some(error),
            Self::Encode(error) => Some(&**error),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// The SQL type family of a column, and the whole of this crate's type system.
///
/// cel-rust has no checker -- `Program::compile` only parses -- so a [`Schema`]
/// is the only thing standing between `age > 'tuesday'` and a database error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColumnType {
    /// `BOOLEAN`.
    Bool,
    /// Any signed integer type. Bound as `i64`.
    Int,
    /// Any floating-point type. Bound as `f64`.
    Float,
    /// Any character type.
    Text,
    /// `BYTEA`, `BLOB`, `VARBINARY`.
    Bytes,
    /// A timestamp, with or without a zone. Bound as `DateTime<Utc>`.
    Timestamp,
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Bool => "bool",
            Self::Int => "int",
            Self::Float => "float",
            Self::Text => "text",
            Self::Bytes => "bytes",
            Self::Timestamp => "timestamp",
        })
    }
}

/// One column a filter is allowed to mention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// The column name as the database spells it, *unquoted*. The dialect adds
    /// quoting, so a name containing the quote character is handled correctly
    /// rather than becoming an injection point.
    pub name: Cow<'static, str>,
    /// What the column holds.
    pub ty: ColumnType,
}

impl Column {
    /// A column whose SQL name is `name`.
    #[must_use]
    pub fn new(name: impl Into<Cow<'static, str>>, ty: ColumnType) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }
}

/// The allow-list. Fail-closed: a path this returns `None` for is rejected, so
/// the default posture of an unconfigured schema is "nothing is filterable".
///
/// `path` is the CEL path split on `.`, so `user.email` arrives as
/// `["user", "email"]`. Flattening it to a single column, mapping it to a JSON
/// extraction, or refusing it are all your call.
pub trait Schema {
    /// Resolve a CEL path to a column, or `None` to reject it.
    fn resolve(&self, path: &[&str]) -> Option<Column>;
}

impl<S: Schema + ?Sized> Schema for &S {
    fn resolve(&self, path: &[&str]) -> Option<Column> {
        (**self).resolve(path)
    }
}

/// A [`Schema`] built from an explicit list of columns.
///
/// ```
/// use sqlx_cel::{ColumnType, Table};
///
/// let users = Table::new()
///     .column("id", ColumnType::Int)
///     .column("email", ColumnType::Text)
///     .aliased("createdAt", "created_at", ColumnType::Timestamp);
/// ```
#[derive(Debug, Clone, Default)]
pub struct Table {
    columns: BTreeMap<String, Column>,
}

impl Table {
    /// An empty table. Every path is rejected until one is added.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Expose `field`, using the same name on both sides.
    #[must_use]
    pub fn column(self, field: impl Into<String>, ty: ColumnType) -> Self {
        let field = field.into();
        let name = field.clone();
        self.aliased(field, name, ty)
    }

    /// Expose CEL path `field` as SQL column `name`.
    ///
    /// Use this when the API and the database disagree about spelling, which is
    /// the usual case for `camelCase` request fields over `snake_case` columns.
    #[must_use]
    pub fn aliased(
        mut self,
        field: impl Into<String>,
        name: impl Into<Cow<'static, str>>,
        ty: ColumnType,
    ) -> Self {
        self.columns.insert(field.into(), Column::new(name, ty));
        self
    }
}

impl Schema for Table {
    fn resolve(&self, path: &[&str]) -> Option<Column> {
        self.columns.get(path.join(".").as_str()).cloned()
    }
}

// ---------------------------------------------------------------------------
// Dialect
// ---------------------------------------------------------------------------

mod sealed {
    /// Keeps [`Dialect`](super::Dialect) closed. Adding an associated item to
    /// it should not be a breaking change, and there is no fourth driver to
    /// implement it for.
    pub trait Sealed {}
}

/// The per-driver syntax this crate has to vary.
///
/// Notably absent: anything about placeholders. `$1` vs `?` is
/// `Arguments::format_placeholder`'s job, and both output paths
/// route through it, so numbering and offsets are structurally impossible to
/// get wrong here.
pub trait Dialect: Database + sealed::Sealed {
    /// Identifier quote character. Doubled to escape itself.
    const QUOTE: char;
    /// How the dialect spells a true literal.
    const TRUE: &'static str;
    /// How the dialect spells a false literal.
    const FALSE: &'static str;
    /// Function returning a string length in characters.
    const LENGTH: &'static str;
    /// Infix regex-match operator, or `None` if the dialect has none, in which
    /// case `matches()` is rejected rather than silently approximated.
    const REGEX: Option<&'static str>;

    /// Append `value` to a `QueryBuilder`, placeholder included.
    ///
    /// This exists so the transpiler can stay generic over `DB: Dialect`
    /// without every function in it also carrying
    /// `Value: for<'q> Encode<'q, DB> + Type<DB>`. A where-clause on a trait is
    /// not an implied bound for that trait's users -- it becomes an obligation
    /// at each use site -- so pushing the one place that needs it down into the
    /// three concrete impls, where the bound is satisfied by inspection, keeps
    /// it out of every signature above.
    ///
    /// # Panics
    ///
    /// If the driver fails to encode. `QueryBuilder::push_bind` offers no
    /// fallible form; see [`Dialect::bind`] for one.
    fn push_bind(query: &mut QueryBuilder<Self>, value: Value);

    /// Append `value` to an argument list.
    ///
    /// # Errors
    ///
    /// Whatever the driver's encoder returns.
    fn bind(arguments: &mut Self::Arguments, value: Value) -> Result<(), BoxDynError>;
}

#[cfg(feature = "postgres")]
impl sealed::Sealed for sqlx::Postgres {}

#[cfg(feature = "postgres")]
#[cfg_attr(docsrs, doc(cfg(feature = "postgres")))]
impl Dialect for sqlx::Postgres {
    const QUOTE: char = '"';
    const TRUE: &'static str = "TRUE";
    const FALSE: &'static str = "FALSE";
    const LENGTH: &'static str = "length";
    const REGEX: Option<&'static str> = Some("~");

    fn push_bind(query: &mut QueryBuilder<Self>, value: Value) {
        query.push_bind(value);
    }

    fn bind(arguments: &mut Self::Arguments, value: Value) -> Result<(), BoxDynError> {
        arguments.add(value)
    }
}

#[cfg(feature = "mysql")]
impl sealed::Sealed for sqlx::MySql {}

#[cfg(feature = "mysql")]
#[cfg_attr(docsrs, doc(cfg(feature = "mysql")))]
impl Dialect for sqlx::MySql {
    const QUOTE: char = '`';
    const TRUE: &'static str = "TRUE";
    const FALSE: &'static str = "FALSE";
    // `LENGTH` on MySQL counts bytes; CEL's `size()` on a string counts
    // characters, and `CHAR_LENGTH` is the one that agrees.
    const LENGTH: &'static str = "CHAR_LENGTH";
    const REGEX: Option<&'static str> = Some("REGEXP");

    fn push_bind(query: &mut QueryBuilder<Self>, value: Value) {
        query.push_bind(value);
    }

    fn bind(arguments: &mut Self::Arguments, value: Value) -> Result<(), BoxDynError> {
        arguments.add(value)
    }
}

#[cfg(feature = "sqlite")]
impl sealed::Sealed for sqlx::Sqlite {}

#[cfg(feature = "sqlite")]
#[cfg_attr(docsrs, doc(cfg(feature = "sqlite")))]
impl Dialect for sqlx::Sqlite {
    const QUOTE: char = '"';
    // SQLite only grew the `TRUE`/`FALSE` keywords in 3.23. `1`/`0` works on
    // every build, including whatever an old system library provides.
    const TRUE: &'static str = "1";
    const FALSE: &'static str = "0";
    const LENGTH: &'static str = "length";
    // `REGEXP` parses, but SQLite ships no implementation: the operator errors
    // at runtime unless the application registered a user function. Rejecting
    // at transpile time is the honest answer.
    const REGEX: Option<&'static str> = None;

    fn push_bind(query: &mut QueryBuilder<Self>, value: Value) {
        query.push_bind(value);
    }

    fn bind(arguments: &mut Self::Arguments, value: Value) -> Result<(), BoxDynError> {
        arguments.add(value)
    }
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// A bind value produced by the transpiler.
///
/// There is deliberately no `Null` variant. `x == null` becomes `IS NULL`,
/// which is both what the caller means and the only form that behaves under
/// SQL's three-valued logic -- a bound `NULL` would make the comparison
/// unknown rather than true.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// Bound as `bool`.
    Bool(bool),
    /// Bound as `i64`.
    Int(i64),
    /// Bound as `f64`.
    Float(f64),
    /// Bound as `String`.
    Text(String),
    /// Bound as `Vec<u8>`.
    Bytes(Vec<u8>),
    /// Bound as `DateTime<Utc>`. cel produces `DateTime<FixedOffset>`, but
    /// MySQL has no `Encode` impl for that, and UTC is the one instant type all
    /// three drivers accept.
    Timestamp(DateTime<Utc>),
}

impl Value {
    /// The type family this value can be compared against.
    #[must_use]
    pub fn ty(&self) -> ColumnType {
        match self {
            Self::Bool(_) => ColumnType::Bool,
            Self::Int(_) => ColumnType::Int,
            Self::Float(_) => ColumnType::Float,
            Self::Text(_) => ColumnType::Text,
            Self::Bytes(_) => ColumnType::Bytes,
            Self::Timestamp(_) => ColumnType::Timestamp,
        }
    }

    /// Check this value against a column, widening `int` to `float` where the
    /// column asks for it. Everything else must match exactly: coercing text to
    /// timestamps or numbers is how a filter language quietly starts returning
    /// the wrong rows.
    fn coerce(self, column: &Column, path: &str) -> Result<Self, Error> {
        match (column.ty, self) {
            (ColumnType::Bool, value @ Self::Bool(_))
            | (ColumnType::Int, value @ Self::Int(_))
            | (ColumnType::Float, value @ Self::Float(_))
            | (ColumnType::Text, value @ Self::Text(_))
            | (ColumnType::Bytes, value @ Self::Bytes(_))
            | (ColumnType::Timestamp, value @ Self::Timestamp(_)) => Ok(value),
            #[allow(clippy::cast_precision_loss)]
            (ColumnType::Float, Self::Int(value)) => Ok(Self::Float(value as f64)),
            (expected, actual) => Err(Error::TypeMismatch {
                column: path.to_owned(),
                expected,
                actual: actual.ty(),
            }),
        }
    }

    /// Narrow one of cel's runtime values to something bindable.
    fn from_cel(value: cel::Value) -> Result<Self, Error> {
        Ok(match value {
            cel::Value::Bool(value) => Self::Bool(value),
            cel::Value::Int(value) => Self::Int(value),
            cel::Value::UInt(value) => {
                Self::Int(i64::try_from(value).map_err(|_| Error::IntegerOverflow(value))?)
            }
            cel::Value::Float(value) => Self::Float(value),
            cel::Value::String(value) => Self::Text(value.as_str().to_owned()),
            cel::Value::Bytes(value) => Self::Bytes(value.as_ref().clone()),
            cel::Value::Timestamp(value) => Self::Timestamp(value.with_timezone(&Utc)),
            // A bare duration has no portable column type to compare against.
            // Inside `timestamp(..) - duration(..)` it never reaches here,
            // because the subtraction folds first.
            cel::Value::Duration(_) => {
                return Err(Error::Unsupported(
                    "a duration outside timestamp arithmetic",
                ));
            }
            cel::Value::Map(_) => return Err(Error::Unsupported("map values")),
            cel::Value::List(_) => return Err(Error::Unsupported("nested list values")),
            _ => return Err(Error::Unsupported("this constant value")),
        })
    }
}

/// `Encode` + `Type` for one driver.
///
/// The bodies are identical across drivers because neither needs a driver
/// specific type constant: `produces()` asks the *concrete* Rust type for its
/// `TypeInfo`, and both `PgArguments::add` and `MySqlArguments::add` prefer
/// `produces()` over `Type::type_info()`. That is what lets a dynamic enum bind
/// correctly despite `Type::type_info()` being an associated fn with no `self`.
macro_rules! impl_bind {
    ($db:ty) => {
        impl Type<$db> for Value {
            fn type_info() -> <$db as Database>::TypeInfo {
                // Only a fallback; `produces()` overrides it per value.
                <String as Type<$db>>::type_info()
            }

            fn compatible(_: &<$db as Database>::TypeInfo) -> bool {
                true
            }
        }

        impl Encode<'_, $db> for Value {
            fn encode_by_ref(
                &self,
                buf: &mut <$db as Database>::ArgumentBuffer,
            ) -> Result<IsNull, BoxDynError> {
                match self {
                    Value::Bool(value) => <bool as Encode<'_, $db>>::encode_by_ref(value, buf),
                    Value::Int(value) => <i64 as Encode<'_, $db>>::encode_by_ref(value, buf),
                    Value::Float(value) => <f64 as Encode<'_, $db>>::encode_by_ref(value, buf),
                    Value::Text(value) => <String as Encode<'_, $db>>::encode_by_ref(value, buf),
                    Value::Bytes(value) => <Vec<u8> as Encode<'_, $db>>::encode_by_ref(value, buf),
                    Value::Timestamp(value) => {
                        <DateTime<Utc> as Encode<'_, $db>>::encode_by_ref(value, buf)
                    }
                }
            }

            fn produces(&self) -> Option<<$db as Database>::TypeInfo> {
                Some(match self {
                    Value::Bool(_) => <bool as Type<$db>>::type_info(),
                    Value::Int(_) => <i64 as Type<$db>>::type_info(),
                    Value::Float(_) => <f64 as Type<$db>>::type_info(),
                    Value::Text(_) => <String as Type<$db>>::type_info(),
                    Value::Bytes(_) => <Vec<u8> as Type<$db>>::type_info(),
                    Value::Timestamp(_) => <DateTime<Utc> as Type<$db>>::type_info(),
                })
            }

            fn size_hint(&self) -> usize {
                match self {
                    Value::Bool(_) => 1,
                    Value::Int(_) | Value::Float(_) | Value::Timestamp(_) => 8,
                    Value::Text(value) => value.len(),
                    Value::Bytes(value) => value.len(),
                }
            }
        }
    };
}

#[cfg(feature = "postgres")]
impl_bind!(sqlx::Postgres);
#[cfg(feature = "mysql")]
impl_bind!(sqlx::MySql);
#[cfg(feature = "sqlite")]
impl_bind!(sqlx::Sqlite);

// ---------------------------------------------------------------------------
// Output sinks
// ---------------------------------------------------------------------------

/// Somewhere a fragment can be written.
///
/// Private on purpose: it exists so the emitter is written once against both
/// sqlx's `QueryBuilder` and our own [`SqlFragment`], not as an extension point.
trait SqlSink<DB: Dialect> {
    /// Append literal SQL. Never reachable from user input.
    fn push_sql(&mut self, sql: &str);

    /// Append a placeholder and record the value behind it.
    fn push_value(&mut self, value: Value) -> Result<(), Error>;
}

impl<DB: Dialect> SqlSink<DB> for QueryBuilder<DB> {
    fn push_sql(&mut self, sql: &str) {
        self.push(sql);
    }

    fn push_value(&mut self, value: Value) -> Result<(), Error> {
        // `push_bind` panics rather than returning on an encode failure. Every
        // `Value` variant delegates to an infallible primitive encoder, so the
        // panic is unreachable -- but it is why `Filter::push_to` documents one
        // and `Filter::to_sql` does not.
        DB::push_bind(self, value);
        Ok(())
    }
}

/// A rendered fragment and the arguments that go with it.
///
/// Produced by [`Filter::to_sql`], for callers not building through
/// `QueryBuilder`.
///
/// ### Placeholders start at the beginning
///
/// The arguments here are a complete set, numbered from one. Splicing the
/// fragment after binds you made yourself will not renumber it. If the filter
/// is not the only source of binds, use [`Filter::push_to`] instead, which
/// appends to your builder's existing argument list.
pub struct SqlFragment<DB: Database> {
    sql: String,
    arguments: DB::Arguments,
}

impl<DB: Database> SqlFragment<DB> {
    /// The SQL text. A boolean expression, already parenthesised where it
    /// needs to be, suitable to drop after `WHERE`, `AND`, `HAVING`, or into a
    /// `CHECK`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.sql
    }

    /// Split into text and arguments.
    #[must_use]
    pub fn into_parts(self) -> (String, DB::Arguments) {
        (self.sql, self.arguments)
    }
}

impl<DB: Database> AsRef<str> for SqlFragment<DB> {
    fn as_ref(&self) -> &str {
        &self.sql
    }
}

impl<DB: Database> fmt::Debug for SqlFragment<DB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `arguments` is deliberately omitted: `DB::Arguments` is not `Debug`,
        // and bind values are user data that has no business in a log line.
        f.debug_struct("SqlFragment")
            .field("sql", &self.sql)
            .finish_non_exhaustive()
    }
}

impl<DB: Database> IntoArguments<DB> for SqlFragment<DB> {
    fn into_arguments(self) -> DB::Arguments {
        self.arguments
    }
}

impl<DB: Dialect> SqlSink<DB> for SqlFragment<DB> {
    fn push_sql(&mut self, sql: &str) {
        self.sql.push_str(sql);
    }

    fn push_value(&mut self, value: Value) -> Result<(), Error> {
        DB::bind(&mut self.arguments, value).map_err(Error::Encode)?;
        // Same call `QueryBuilder::push_bind` makes, and the reason this crate
        // has no notion of `$1` vs `?`. Writing to a `String` cannot fail.
        let _ = self.arguments.format_placeholder(&mut self.sql);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Operands
// ---------------------------------------------------------------------------

/// A resolved leaf: what one side of a comparison turned out to be.
#[derive(Debug)]
enum Operand {
    /// A column reference, plus the CEL path it came from for error messages.
    Column(Column, String),
    /// `size(col)` -- a column under a length function.
    Length(Column, String),
    /// A constant.
    Value(Value),
    /// A list constant, for `in`.
    List(Vec<Value>),
    /// The `null` literal.
    Null,
}

impl Operand {
    /// Resolve one side of an operator.
    ///
    /// The first branch is the whole reason this crate enables cel's `chrono`
    /// feature: a subtree that mentions no variables is a constant, and cel can
    /// evaluate it far more correctly than we could re-implement it. That
    /// covers literals, `timestamp('...')`, `duration('24h')`, list literals,
    /// and arithmetic between any of them.
    fn resolve<S: Schema>(
        expr: &IdedExpr,
        schema: &S,
        context: &cel::Context<'_>,
    ) -> Result<Self, Error> {
        if expr.references().variables().is_empty() {
            let value = cel::Value::resolve(expr, context).map_err(Error::Eval)?;
            return match value {
                cel::Value::Null => Ok(Self::Null),
                cel::Value::List(items) => items
                    .iter()
                    .map(|item| Value::from_cel(item.clone()))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Self::List),
                other => Value::from_cel(other).map(Self::Value),
            };
        }

        match &expr.expr {
            Expr::Ident(_) | Expr::Select(_) => {
                let (column, path) = lookup(expr, schema)?;
                Ok(Self::Column(column, path))
            }
            Expr::Call(call) if call.func_name == "size" => {
                // `size(x)` puts the operand in `args`; `x.size()` puts it in
                // `target`. Both mean the same thing.
                let ((Some(target), []) | (None, [target])) =
                    (call.target.as_deref(), call.args.as_slice())
                else {
                    return Err(Error::Unsupported("size() with more than one argument"));
                };
                let (column, path) = lookup(target, schema)?;
                Ok(Self::Length(column, path))
            }
            Expr::Call(call) => Err(Error::UnknownFunction(call.func_name.clone())),
            Expr::Comprehension(_) => Err(Error::Unsupported(
                "macros (all, exists, exists_one, map, filter)",
            )),
            _ => Err(Error::Unsupported(
                "this expression as a comparison operand",
            )),
        }
    }
}

/// Walk a chain of selects down to the root identifier, collecting the path.
fn collect<'e>(expr: &'e IdedExpr, path: &mut Vec<&'e str>) -> Result<(), Error> {
    match &expr.expr {
        Expr::Ident(name) => {
            path.push(name);
            Ok(())
        }
        // `test` is set by `has(x.y)`, which is a predicate, not a path.
        Expr::Select(select) if !select.test => {
            collect(&select.operand, path)?;
            path.push(&select.field);
            Ok(())
        }
        _ => Err(Error::Unsupported("this expression as a column reference")),
    }
}

/// Resolve a CEL path against the schema. Returns the column and the dotted
/// path, which errors and type mismatches quote back at the caller.
fn lookup<S: Schema>(expr: &IdedExpr, schema: &S) -> Result<(Column, String), Error> {
    let mut path = Vec::new();
    collect(expr, &mut path)?;
    let joined = path.join(".");
    schema
        .resolve(&path)
        .map(|column| (column, joined.clone()))
        .ok_or(Error::UnknownColumn(joined))
}

// ---------------------------------------------------------------------------
// Filter
// ---------------------------------------------------------------------------

/// A parsed CEL filter, ready to be rendered against any schema and driver.
///
/// Compiling is independent of both, so a filter parsed once can be reused
/// across tables and databases.
#[derive(Debug, Clone)]
pub struct Filter {
    expr: IdedExpr,
}

impl Filter {
    /// Parse CEL source.
    ///
    /// Optional syntax (`a.?b`, `[?x]`) and backtick-escaped identifiers stay
    /// off: neither has a SQL lowering here, and rejecting them at the parser
    /// is clearer than rejecting them three passes later.
    ///
    /// # Errors
    ///
    /// [`Error::Parse`] if the source is not valid CEL, including if it nests
    /// deeper than 32 levels.
    pub fn compile(source: &str) -> Result<Self, Error> {
        Parser::new()
            .max_recursion_depth(MAX_DEPTH)
            .parse(source)
            .map(|expr| Self { expr })
            .map_err(Error::Parse)
    }

    /// Every variable the expression mentions, for validating a filter against
    /// an allow-list before committing to transpile it.
    ///
    /// Owned rather than borrowed because cel's `ExpressionReferences` ties its
    /// `&str`s to itself, not to the expression it read. Order is unspecified.
    #[must_use]
    pub fn variables(&self) -> Vec<String> {
        self.expr
            .references()
            .variables()
            .into_iter()
            .map(ToOwned::to_owned)
            .collect()
    }

    /// Append this filter to a query under construction.
    ///
    /// The preferred entry point. The builder owns the argument list, so the
    /// fragment's placeholders continue your numbering instead of restarting.
    ///
    /// ```
    /// # #[cfg(feature = "postgres")] {
    /// use sqlx::{Postgres, QueryBuilder};
    /// use sqlx_cel::{ColumnType, Filter, Table};
    ///
    /// let users = Table::new().column("age", ColumnType::Int);
    /// let filter = Filter::compile("age > 21").unwrap();
    ///
    /// let mut query = QueryBuilder::<Postgres>::new("SELECT * FROM users WHERE tenant = ");
    /// query.push_bind(7_i64);
    /// query.push(" AND ");
    /// filter.push_to(&users, &mut query).unwrap();
    ///
    /// assert!(query.sql().as_str().ends_with("AND \"age\" > $2"));
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Any [`Error`] except [`Error::Parse`]: unknown columns, type mismatches,
    /// and constructs with no SQL lowering.
    ///
    /// # Panics
    ///
    /// If the driver fails to encode a bind value, because
    /// `QueryBuilder::push_bind` panics rather than returning. Every [`Value`]
    /// delegates to an infallible primitive encoder, so this is unreachable;
    /// [`Filter::to_sql`] returns [`Error::Encode`] instead if you would rather
    /// not rely on that.
    pub fn push_to<DB: Dialect, S: Schema>(
        &self,
        schema: &S,
        query: &mut QueryBuilder<DB>,
    ) -> Result<(), Error> {
        predicate(&self.expr, schema, &cel::Context::default(), query)
    }

    /// Render a standalone fragment and its arguments.
    ///
    /// For callers assembling SQL some other way. Note that the placeholders
    /// start at one; see [`SqlFragment`].
    ///
    /// ```
    /// # #[cfg(feature = "postgres")] {
    /// use sqlx::Postgres;
    /// use sqlx_cel::{ColumnType, Filter, Table};
    ///
    /// let users = Table::new().column("email", ColumnType::Text);
    /// let filter = Filter::compile("email.endsWith('@example.com')").unwrap();
    /// let sql = filter.to_sql::<Postgres, _>(&users).unwrap();
    ///
    /// assert_eq!(sql.as_str(), r#""email" LIKE $1 ESCAPE '!'"#);
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Any [`Error`] except [`Error::Parse`].
    pub fn to_sql<DB: Dialect, S: Schema>(&self, schema: &S) -> Result<SqlFragment<DB>, Error> {
        let mut fragment = SqlFragment {
            sql: String::new(),
            arguments: DB::Arguments::default(),
        };
        predicate(&self.expr, schema, &cel::Context::default(), &mut fragment)?;
        Ok(fragment)
    }
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// Write `expr` as a SQL boolean expression.
fn predicate<DB, S, K>(
    expr: &IdedExpr,
    schema: &S,
    context: &cel::Context<'_>,
    out: &mut K,
) -> Result<(), Error>
where
    DB: Dialect,
    S: Schema,
    K: SqlSink<DB>,
{
    // A predicate mentioning no columns is a constant. Fold it rather than
    // emitting `$1 = $2`, and insist the answer is a boolean -- `"age" AND 3`
    // should not reach the database to be argued about there.
    if expr.references().variables().is_empty() {
        return match cel::Value::resolve(expr, context).map_err(Error::Eval)? {
            cel::Value::Bool(true) => {
                out.push_sql(DB::TRUE);
                Ok(())
            }
            cel::Value::Bool(false) => {
                out.push_sql(DB::FALSE);
                Ok(())
            }
            _ => Err(Error::Unsupported("a non-boolean constant as a predicate")),
        };
    }

    match &expr.expr {
        Expr::Call(call) => call_predicate(call, expr, schema, context, out),

        // `has(x.y)`. CEL asks whether the field is present; the closest SQL
        // question is whether the column is non-null.
        Expr::Select(select) if select.test => {
            let mut path = Vec::new();
            collect(&select.operand, &mut path)?;
            path.push(&select.field);
            let joined = path.join(".");
            let column = schema
                .resolve(&path)
                .ok_or(Error::UnknownColumn(joined.clone()))?;
            push_column::<DB, K>(&column, out);
            out.push_sql(" IS NOT NULL");
            Ok(())
        }

        // A bare column used as a predicate: `is_active`.
        Expr::Ident(_) | Expr::Select(_) => {
            let (column, path) = lookup(expr, schema)?;
            if column.ty != ColumnType::Bool {
                return Err(Error::TypeMismatch {
                    column: path,
                    expected: column.ty,
                    actual: ColumnType::Bool,
                });
            }
            push_column::<DB, K>(&column, out);
            Ok(())
        }

        Expr::Comprehension(_) => Err(Error::Unsupported(
            "macros (all, exists, exists_one, map, filter)",
        )),

        _ => Err(Error::Unsupported("this expression as a predicate")),
    }
}

/// The operator table. Split out of [`predicate`] only because the match is
/// long, not because it is a separate concern.
fn call_predicate<DB, S, K>(
    call: &cel::common::ast::CallExpr,
    expr: &IdedExpr,
    schema: &S,
    context: &cel::Context<'_>,
    out: &mut K,
) -> Result<(), Error>
where
    DB: Dialect,
    S: Schema,
    K: SqlSink<DB>,
{
    let args = call.args.as_slice();

    match (call.func_name.as_str(), call.target.as_deref(), args) {
        (operators::LOGICAL_AND, None, [lhs, rhs]) => {
            infix(lhs, rhs, " AND ", schema, context, out)
        }
        (operators::LOGICAL_OR, None, [lhs, rhs]) => infix(lhs, rhs, " OR ", schema, context, out),

        (operators::LOGICAL_NOT, None, [inner]) => {
            out.push_sql("NOT ");
            group(inner, schema, context, out)
        }

        (operators::CONDITIONAL, None, [cond, yes, no]) => {
            out.push_sql("CASE WHEN ");
            predicate(cond, schema, context, out)?;
            out.push_sql(" THEN ");
            predicate(yes, schema, context, out)?;
            out.push_sql(" ELSE ");
            predicate(no, schema, context, out)?;
            out.push_sql(" END");
            Ok(())
        }

        (operators::IN, None, [needle, haystack]) => {
            in_list(needle, haystack, schema, context, out)
        }

        (name, None, [lhs, rhs]) if operator(name).is_some() => {
            let symbol = operator(name).unwrap_or_default();
            compare(name, symbol, lhs, rhs, schema, context, out)
        }

        (name @ ("startsWith" | "endsWith" | "contains"), Some(target), [pattern]) => {
            like(name, target, pattern, schema, context, out)
        }

        ("matches", Some(target), [pattern]) => {
            let Some(symbol) = DB::REGEX else {
                return Err(Error::Unsupported(
                    "matches() on a driver with no regex operator",
                ));
            };
            let (column, path) = lookup(target, schema)?;
            let Operand::Value(value) = Operand::resolve(pattern, schema, context)? else {
                return Err(Error::Unsupported("matches() with a non-constant pattern"));
            };
            let value = value.coerce(&column, &path)?;
            push_column::<DB, K>(&column, out);
            out.push_sql(" ");
            out.push_sql(symbol);
            out.push_sql(" ");
            out.push_value(value)
        }

        // `size(x) > 0` arrives here when `size` is the whole predicate, which
        // it cannot be -- it is an int. Fall through to the operand resolver so
        // the error names the real problem.
        _ => {
            drop(Operand::resolve(expr, schema, context)?);
            Err(Error::Unsupported("this call as a predicate"))
        }
    }
}

/// `lhs OP rhs`, each side parenthesised so precedence survives the round trip.
fn infix<DB, S, K>(
    lhs: &IdedExpr,
    rhs: &IdedExpr,
    symbol: &str,
    schema: &S,
    context: &cel::Context<'_>,
    out: &mut K,
) -> Result<(), Error>
where
    DB: Dialect,
    S: Schema,
    K: SqlSink<DB>,
{
    group(lhs, schema, context, out)?;
    out.push_sql(symbol);
    group(rhs, schema, context, out)
}

/// A predicate wrapped in parentheses.
fn group<DB, S, K>(
    expr: &IdedExpr,
    schema: &S,
    context: &cel::Context<'_>,
    out: &mut K,
) -> Result<(), Error>
where
    DB: Dialect,
    S: Schema,
    K: SqlSink<DB>,
{
    out.push_sql("(");
    predicate(expr, schema, context, out)?;
    out.push_sql(")");
    Ok(())
}

/// Map a cel operator name to its SQL spelling.
fn operator(name: &str) -> Option<&'static str> {
    Some(match name {
        operators::EQUALS => "=",
        operators::NOT_EQUALS => "<>",
        operators::LESS => "<",
        operators::LESS_EQUALS => "<=",
        operators::GREATER => ">",
        operators::GREATER_EQUALS => ">=",
        _ => return None,
    })
}

/// A binary comparison, after both sides have been resolved.
#[allow(clippy::too_many_arguments)]
fn compare<DB, S, K>(
    name: &str,
    symbol: &str,
    lhs: &IdedExpr,
    rhs: &IdedExpr,
    schema: &S,
    context: &cel::Context<'_>,
    out: &mut K,
) -> Result<(), Error>
where
    DB: Dialect,
    S: Schema,
    K: SqlSink<DB>,
{
    let lhs = Operand::resolve(lhs, schema, context)?;
    let rhs = Operand::resolve(rhs, schema, context)?;

    match (lhs, rhs) {
        // Null is not a value you can bind: `col = NULL` is unknown, never
        // true. Only equality has a null form; `col < null` is nonsense.
        (Operand::Column(column, _), Operand::Null)
        | (Operand::Null, Operand::Column(column, _)) => {
            push_column::<DB, K>(&column, out);
            out.push_sql(match name {
                operators::EQUALS => " IS NULL",
                operators::NOT_EQUALS => " IS NOT NULL",
                _ => return Err(Error::Unsupported("ordering a column against null")),
            });
            Ok(())
        }

        (Operand::Column(column, path), Operand::Value(value)) => {
            let value = value.coerce(&column, &path)?;
            push_column::<DB, K>(&column, out);
            surround(symbol, out);
            out.push_value(value)
        }

        (Operand::Value(value), Operand::Column(column, path)) => {
            let value = value.coerce(&column, &path)?;
            out.push_value(value)?;
            surround(symbol, out);
            push_column::<DB, K>(&column, out);
            Ok(())
        }

        // `size(name) > 0`. The length is an int whatever the column is, so it
        // is checked against `Int` rather than the column's own type.
        (Operand::Length(column, path), Operand::Value(value)) => {
            let value = value.coerce(&Column::new("", ColumnType::Int), &path)?;
            push_length::<DB, K>(&column, out);
            surround(symbol, out);
            out.push_value(value)
        }

        (Operand::Value(value), Operand::Length(column, path)) => {
            let value = value.coerce(&Column::new("", ColumnType::Int), &path)?;
            out.push_value(value)?;
            surround(symbol, out);
            push_length::<DB, K>(&column, out);
            Ok(())
        }

        (Operand::Column(lhs, _), Operand::Column(rhs, _)) => {
            push_column::<DB, K>(&lhs, out);
            surround(symbol, out);
            push_column::<DB, K>(&rhs, out);
            Ok(())
        }

        _ => Err(Error::Unsupported("this comparison")),
    }
}

/// `col IN (…)`.
fn in_list<DB, S, K>(
    needle: &IdedExpr,
    haystack: &IdedExpr,
    schema: &S,
    context: &cel::Context<'_>,
    out: &mut K,
) -> Result<(), Error>
where
    DB: Dialect,
    S: Schema,
    K: SqlSink<DB>,
{
    let (column, path) = lookup(needle, schema)?;
    let Operand::List(values) = Operand::resolve(haystack, schema, context)? else {
        return Err(Error::Unsupported("`in` against a non-constant list"));
    };

    // `col IN ()` is a syntax error everywhere. An empty CEL list matches
    // nothing, which is exactly `FALSE`.
    if values.is_empty() {
        out.push_sql(DB::FALSE);
        return Ok(());
    }

    push_column::<DB, K>(&column, out);
    out.push_sql(" IN (");
    for (index, value) in values.into_iter().enumerate() {
        if index > 0 {
            out.push_sql(", ");
        }
        out.push_value(value.coerce(&column, &path)?)?;
    }
    out.push_sql(")");
    Ok(())
}

/// `startsWith` / `endsWith` / `contains` as `LIKE`.
fn like<DB, S, K>(
    name: &str,
    target: &IdedExpr,
    pattern: &IdedExpr,
    schema: &S,
    context: &cel::Context<'_>,
    out: &mut K,
) -> Result<(), Error>
where
    DB: Dialect,
    S: Schema,
    K: SqlSink<DB>,
{
    let (column, path) = lookup(target, schema)?;
    if column.ty != ColumnType::Text {
        return Err(Error::TypeMismatch {
            column: path,
            expected: column.ty,
            actual: ColumnType::Text,
        });
    }

    let Operand::Value(Value::Text(text)) = Operand::resolve(pattern, schema, context)? else {
        return Err(Error::Unsupported(
            "startsWith/endsWith/contains with a non-constant string",
        ));
    };

    let (prefix, suffix) = match name {
        "startsWith" => (false, true),
        "endsWith" => (true, false),
        _ => (true, true),
    };

    push_column::<DB, K>(&column, out);
    out.push_sql(" LIKE ");
    out.push_value(Value::Text(escape(&text, prefix, suffix)))?;
    out.push_sql(" ESCAPE '");
    // A `char` literal we chose ourselves, not user input.
    out.push_sql(&LIKE_ESCAPE.to_string());
    out.push_sql("'");
    Ok(())
}

/// Turn a literal substring into a `LIKE` pattern, neutralising the wildcards
/// the user did not ask for. Without this, `contains('100%')` would match far
/// more than it should.
fn escape(text: &str, prefix: bool, suffix: bool) -> String {
    let mut pattern = String::with_capacity(text.len() + 2);
    if prefix {
        pattern.push('%');
    }
    for character in text.chars() {
        if matches!(character, '%' | '_') || character == LIKE_ESCAPE {
            pattern.push(LIKE_ESCAPE);
        }
        pattern.push(character);
    }
    if suffix {
        pattern.push('%');
    }
    pattern
}

/// Write a quoted identifier.
fn push_column<DB, K>(column: &Column, out: &mut K)
where
    DB: Dialect,
    K: SqlSink<DB>,
{
    let mut quoted = String::with_capacity(column.name.len() + 2);
    quoted.push(DB::QUOTE);
    for character in column.name.chars() {
        // Doubling is the escape in all three dialects: `""` and ` `` `.
        if character == DB::QUOTE {
            quoted.push(DB::QUOTE);
        }
        quoted.push(character);
    }
    quoted.push(DB::QUOTE);
    out.push_sql(&quoted);
}

/// Write `length(col)` under whatever the dialect calls it.
fn push_length<DB, K>(column: &Column, out: &mut K)
where
    DB: Dialect,
    K: SqlSink<DB>,
{
    out.push_sql(DB::LENGTH);
    out.push_sql("(");
    push_column::<DB, K>(column, out);
    out.push_sql(")");
}

/// `" op "`, without allocating a format string per comparison.
fn surround<DB, K>(symbol: &str, out: &mut K)
where
    DB: Dialect,
    K: SqlSink<DB>,
{
    out.push_sql(" ");
    out.push_sql(symbol);
    out.push_sql(" ");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// One table exercising every [`ColumnType`], with one aliased column so
    /// the CEL-name/SQL-name split stays covered.
    fn users() -> Table {
        Table::new()
            .column("age", ColumnType::Int)
            .column("score", ColumnType::Float)
            .column("email", ColumnType::Text)
            .column("active", ColumnType::Bool)
            .column("avatar", ColumnType::Bytes)
            .aliased("createdAt", "created_at", ColumnType::Timestamp)
            .aliased("profile.city", "city", ColumnType::Text)
    }

    /// Transpile against `users()`, returning the fragment or the error.
    fn sql<DB: Dialect>(source: &str) -> Result<String, Error> {
        Filter::compile(source)?
            .to_sql::<DB, _>(&users())
            .map(|rendered| rendered.as_str().to_owned())
    }

    #[cfg(feature = "postgres")]
    fn pg(source: &str) -> String {
        sql::<sqlx::Postgres>(source).expect("should transpile")
    }

    #[cfg(feature = "postgres")]
    mod lowering {
        use super::*;
        use sqlx::Postgres;

        #[test]
        fn comparisons_cover_every_operator() {
            assert_eq!(pg("age == 21"), r#""age" = $1"#);
            assert_eq!(pg("age != 21"), r#""age" <> $1"#);
            assert_eq!(pg("age < 21"), r#""age" < $1"#);
            assert_eq!(pg("age <= 21"), r#""age" <= $1"#);
            assert_eq!(pg("age > 21"), r#""age" > $1"#);
            assert_eq!(pg("age >= 21"), r#""age" >= $1"#);
        }

        #[test]
        fn constant_on_the_left_keeps_its_side() {
            // Not rewritten to `"age" < $1`: preserving the written order keeps
            // the emitted SQL recognisable next to the CEL it came from.
            assert_eq!(pg("21 > age"), r#"$1 > "age""#);
        }

        #[test]
        fn logical_operators_parenthesise_their_operands() {
            assert_eq!(pg("age > 21 && active"), r#"("age" > $1) AND ("active")"#,);
            assert_eq!(pg("age > 21 || age < 5"), r#"("age" > $1) OR ("age" < $2)"#,);
            assert_eq!(pg("!active"), r#"NOT ("active")"#);
        }

        #[test]
        fn precedence_survives_the_round_trip() {
            // Without the parentheses this would bind as `a && (b || c)` when
            // re-parsed by the database.
            assert_eq!(
                pg("(age > 1 || age > 2) && active"),
                r#"(("age" > $1) OR ("age" > $2)) AND ("active")"#,
            );
        }

        #[test]
        fn ternary_becomes_a_case_expression() {
            assert_eq!(
                pg("active ? age > 21 : age > 65"),
                r#"CASE WHEN "active" THEN "age" > $1 ELSE "age" > $2 END"#,
            );
        }

        #[test]
        fn membership_becomes_in() {
            assert_eq!(pg("age in [1, 2, 3]"), r#""age" IN ($1, $2, $3)"#);
        }

        #[test]
        fn empty_membership_is_false_not_a_syntax_error() {
            // `"age" IN ()` does not parse on any of the three databases.
            assert_eq!(pg("age in []"), "FALSE");
        }

        #[test]
        fn null_comparison_becomes_is_null() {
            // A bound NULL would make the comparison unknown, never true.
            assert_eq!(pg("email == null"), r#""email" IS NULL"#);
            assert_eq!(pg("email != null"), r#""email" IS NOT NULL"#);
        }

        #[test]
        fn ordering_against_null_is_rejected() {
            assert!(matches!(
                sql::<Postgres>("age > null"),
                Err(Error::Unsupported(_))
            ));
        }

        #[test]
        fn has_becomes_is_not_null() {
            assert_eq!(pg("has(profile.city)"), r#""city" IS NOT NULL"#);
        }

        #[test]
        fn size_becomes_the_dialect_length_function() {
            assert_eq!(pg("size(email) > 0"), r#"length("email") > $1"#);
            assert_eq!(pg("email.size() > 0"), r#"length("email") > $1"#);
        }

        #[test]
        fn bare_boolean_column_is_a_predicate() {
            assert_eq!(pg("active"), r#""active""#);
        }

        #[test]
        fn non_boolean_column_is_not_a_predicate() {
            assert!(matches!(
                sql::<Postgres>("age"),
                Err(Error::TypeMismatch { .. })
            ));
        }

        #[test]
        fn column_to_column_comparison_binds_nothing() {
            assert_eq!(pg("age > score"), r#""age" > "score""#);
        }

        #[test]
        fn aliased_columns_use_their_sql_name() {
            assert_eq!(pg("profile.city == 'Sofia'"), r#""city" = $1"#);
        }

        #[test]
        fn constant_predicates_fold_rather_than_binding() {
            assert_eq!(pg("1 == 1"), "TRUE");
            assert_eq!(pg("2 < 1"), "FALSE");
            assert_eq!(pg("true"), "TRUE");
        }

        #[test]
        fn non_boolean_constant_is_not_a_predicate() {
            assert!(matches!(
                sql::<Postgres>("1 + 1"),
                Err(Error::Unsupported(_))
            ));
        }

        #[test]
        fn placeholders_are_numbered_across_the_whole_filter() {
            assert_eq!(
                pg("age > 1 && email == 'a' && age < 9"),
                r#"(("age" > $1) AND ("email" = $2)) AND ("age" < $3)"#,
            );
        }

        #[test]
        fn empty_schema_rejects_everything() {
            let filter = Filter::compile("age > 21").unwrap();
            assert!(matches!(
                filter.to_sql::<Postgres, _>(&Table::new()),
                Err(Error::UnknownColumn(path)) if path == "age"
            ));
        }

        #[test]
        fn variables_lists_what_the_filter_touches() {
            let filter = Filter::compile("age > 21 && profile.city == 'Sofia'").unwrap();
            let mut variables = filter.variables();
            variables.sort();
            assert_eq!(variables, vec!["age".to_owned(), "profile".to_owned()]);
        }

        #[test]
        fn schema_sees_the_full_dotted_path() {
            struct Recording;
            impl Schema for Recording {
                fn resolve(&self, path: &[&str]) -> Option<Column> {
                    assert_eq!(path, ["profile", "city"]);
                    Some(Column::new("city", ColumnType::Text))
                }
            }

            let filter = Filter::compile("profile.city == 'Sofia'").unwrap();
            filter.to_sql::<Postgres, _>(&Recording).unwrap();
        }
    }

    #[cfg(feature = "postgres")]
    mod strings {
        use super::*;
        use sqlx::Postgres;

        #[test]
        fn prefix_infix_and_suffix_matching() {
            assert_eq!(pg("email.startsWith('a')"), r#""email" LIKE $1 ESCAPE '!'"#,);
            assert_eq!(pg("email.endsWith('a')"), r#""email" LIKE $1 ESCAPE '!'"#);
            assert_eq!(pg("email.contains('a')"), r#""email" LIKE $1 ESCAPE '!'"#);
        }

        #[test]
        fn patterns_are_anchored_on_the_right_side() {
            assert_eq!(escape("a", false, true), "a%");
            assert_eq!(escape("a", true, false), "%a");
            assert_eq!(escape("a", true, true), "%a%");
        }

        #[test]
        fn wildcards_in_user_input_are_neutralised() {
            // Without this, `contains('100%')` would match anything starting
            // with `100`.
            assert_eq!(escape("100%", true, true), "%100!%%");
            assert_eq!(escape("a_b", true, true), "%a!_b%");
            // The escape character escapes itself.
            assert_eq!(escape("!", true, true), "%!!%");
        }

        #[test]
        fn matching_a_non_text_column_is_rejected() {
            assert!(matches!(
                sql::<Postgres>("age.startsWith('1')"),
                Err(Error::TypeMismatch { .. })
            ));
        }

        #[test]
        fn regex_uses_the_dialect_operator() {
            assert_eq!(pg("email.matches('^a')"), r#""email" ~ $1"#);
        }
    }

    #[cfg(feature = "postgres")]
    mod types {
        use super::*;
        use sqlx::Postgres;

        #[test]
        fn mismatched_constant_is_caught_here_not_by_the_database() {
            // cel-rust does not type-check, so this is the only pass that can
            // reject it.
            assert!(matches!(
                sql::<Postgres>("age > 'tuesday'"),
                Err(Error::TypeMismatch {
                    expected: ColumnType::Int,
                    actual: ColumnType::Text,
                    ..
                })
            ));
        }

        #[test]
        fn int_widens_into_a_float_column() {
            assert!(sql::<Postgres>("score > 1").is_ok());
        }

        #[test]
        fn float_does_not_narrow_into_an_int_column() {
            assert!(matches!(
                sql::<Postgres>("age > 1.5"),
                Err(Error::TypeMismatch { .. })
            ));
        }

        #[test]
        fn unsigned_beyond_i64_has_nowhere_to_go() {
            assert!(matches!(
                sql::<Postgres>("age == 9223372036854775808u"),
                Err(Error::IntegerOverflow(_))
            ));
        }

        #[test]
        fn in_list_elements_are_type_checked_too() {
            assert!(matches!(
                sql::<Postgres>("age in [1, 'two']"),
                Err(Error::TypeMismatch { .. })
            ));
        }
    }

    #[cfg(feature = "postgres")]
    mod temporal {
        use super::*;
        use sqlx::Postgres;

        #[test]
        fn timestamp_literals_fold_to_a_single_bind() {
            assert_eq!(
                pg("createdAt > timestamp('2024-01-01T00:00:00Z')"),
                r#""created_at" > $1"#,
            );
        }

        #[test]
        fn timestamp_arithmetic_folds_too() {
            // The whole right-hand side is closed, so cel evaluates it and we
            // bind one instant -- no portable date arithmetic to emit, and no
            // Go-duration parser for this crate to own.
            assert_eq!(
                pg("createdAt > timestamp('2024-01-02T00:00:00Z') - duration('24h')"),
                r#""created_at" > $1"#,
            );
        }

        #[test]
        fn a_bad_timestamp_is_an_evaluation_error() {
            assert!(matches!(
                sql::<Postgres>("createdAt > timestamp('not a date')"),
                Err(Error::Eval(_))
            ));
        }

        #[test]
        fn a_bare_duration_has_no_column_to_compare_against() {
            assert!(matches!(
                sql::<Postgres>("createdAt > duration('24h')"),
                Err(Error::Unsupported(_))
            ));
        }
    }

    #[cfg(feature = "postgres")]
    mod rejected {
        use super::*;
        use sqlx::Postgres;

        #[test]
        fn comprehension_macros_are_refused() {
            for source in [
                "[1, 2].all(x, x > 0)",
                "[1, 2].exists(x, x > 0)",
                "[1, 2].map(x, x * 2) == [2, 4]",
            ] {
                assert!(
                    matches!(sql::<Postgres>(source), Err(Error::Unsupported(_))),
                    "{source} should be rejected",
                );
            }
        }

        #[test]
        fn unknown_functions_name_themselves() {
            assert!(matches!(
                sql::<Postgres>("email.lowerAscii() == 'a'"),
                Err(Error::UnknownFunction(name)) if name == "lowerAscii"
            ));
        }

        #[test]
        fn optional_syntax_stays_off_at_the_parser() {
            assert!(matches!(
                Filter::compile("email.?x == 'a'"),
                Err(Error::Parse(_))
            ));
        }

        #[test]
        fn nonsense_is_a_parse_error() {
            assert!(matches!(Filter::compile("age >"), Err(Error::Parse(_))));
        }
    }

    /// The dialect differences, asserted side by side so a divergence is
    /// visible rather than buried in three separate files.
    mod dialects {
        use super::*;

        #[test]
        fn identifier_quoting() {
            #[cfg(feature = "postgres")]
            assert_eq!(sql::<sqlx::Postgres>("age > 1").unwrap(), r#""age" > $1"#,);
            #[cfg(feature = "sqlite")]
            assert_eq!(sql::<sqlx::Sqlite>("age > 1").unwrap(), r#""age" > ?"#);
            #[cfg(feature = "mysql")]
            assert_eq!(sql::<sqlx::MySql>("age > 1").unwrap(), "`age` > ?");
        }

        #[test]
        fn boolean_literals() {
            #[cfg(feature = "postgres")]
            assert_eq!(sql::<sqlx::Postgres>("age in []").unwrap(), "FALSE");
            // SQLite only grew the `FALSE` keyword in 3.23.
            #[cfg(feature = "sqlite")]
            assert_eq!(sql::<sqlx::Sqlite>("age in []").unwrap(), "0");
            #[cfg(feature = "mysql")]
            assert_eq!(sql::<sqlx::MySql>("age in []").unwrap(), "FALSE");
        }

        #[test]
        fn length_function() {
            #[cfg(feature = "postgres")]
            assert_eq!(
                sql::<sqlx::Postgres>("size(email) > 0").unwrap(),
                r#"length("email") > $1"#,
            );
            // `LENGTH` counts bytes on MySQL; CEL's `size()` counts characters.
            #[cfg(feature = "mysql")]
            assert_eq!(
                sql::<sqlx::MySql>("size(email) > 0").unwrap(),
                "CHAR_LENGTH(`email`) > ?",
            );
        }

        #[test]
        fn regex_support_is_not_universal() {
            #[cfg(feature = "postgres")]
            assert_eq!(
                sql::<sqlx::Postgres>("email.matches('^a')").unwrap(),
                r#""email" ~ $1"#,
            );
            #[cfg(feature = "mysql")]
            assert_eq!(
                sql::<sqlx::MySql>("email.matches('^a')").unwrap(),
                "`email` REGEXP ?",
            );
            // SQLite parses `REGEXP` but ships no implementation, so rejecting
            // beats emitting SQL that fails at runtime.
            #[cfg(feature = "sqlite")]
            assert!(matches!(
                sql::<sqlx::Sqlite>("email.matches('^a')"),
                Err(Error::Unsupported(_))
            ));
        }

        /// A `Schema` is trusted to name columns, but "trusted" is not
        /// "unquoted": a name is still doubled at the dialect's own quote
        /// character. The name here carries both quote characters, so each
        /// dialect escapes one and passes the other through untouched.
        struct Hostile;

        impl Schema for Hostile {
            fn resolve(&self, _: &[&str]) -> Option<Column> {
                Some(Column::new("a\"`b OR 1=1 --", ColumnType::Int))
            }
        }

        fn hostile<DB: Dialect>() -> String {
            Filter::compile("age > 1")
                .unwrap()
                .to_sql::<DB, _>(&Hostile)
                .unwrap()
                .as_str()
                .to_owned()
        }

        #[test]
        fn a_quote_in_a_column_name_is_escaped_not_injected() {
            #[cfg(feature = "postgres")]
            assert_eq!(hostile::<sqlx::Postgres>(), r#""a""`b OR 1=1 --" > $1"#);
            #[cfg(feature = "sqlite")]
            assert_eq!(hostile::<sqlx::Sqlite>(), r#""a""`b OR 1=1 --" > ?"#);
            #[cfg(feature = "mysql")]
            assert_eq!(hostile::<sqlx::MySql>(), r#"`a"``b OR 1=1 --` > ?"#);
        }
    }

    /// Proof that `push_to` continues the builder's numbering instead of
    /// restarting, which is the whole reason it is the preferred entry point.
    #[cfg(feature = "postgres")]
    #[test]
    fn push_to_continues_existing_placeholders() {
        use sqlx::{Postgres, QueryBuilder};

        let filter = Filter::compile("age > 21 && email == 'a'").unwrap();
        let mut query = QueryBuilder::<Postgres>::new("SELECT * FROM users WHERE tenant = ");
        query.push_bind(7_i64);
        query.push(" AND ");
        filter.push_to(&users(), &mut query).unwrap();

        assert_eq!(
            query.sql().as_str(),
            r#"SELECT * FROM users WHERE tenant = $1 AND ("age" > $2) AND ("email" = $3)"#,
        );
    }

    /// The only test that proves the bind *values* are right rather than just
    /// the SQL text. Everything above asserts on strings; this one asks a real
    /// database.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn round_trip_against_sqlite() {
        use sqlx::{Row, Sqlite, SqlitePool};

        let pool = SqlitePool::connect(":memory:").await.unwrap();
        sqlx::raw_sql(
            "CREATE TABLE users (age INTEGER, score REAL, email TEXT, active BOOLEAN, \
             avatar BLOB, created_at TEXT, city TEXT);
             INSERT INTO users VALUES (30, 1.5, 'a@example.com', 1, NULL, \
             '2024-06-01T00:00:00+00:00', 'Sofia');
             INSERT INTO users VALUES (10, 0.5, 'b@other.org', 0, NULL, \
             '2020-01-01T00:00:00+00:00', NULL);",
        )
        .execute(&pool)
        .await
        .unwrap();

        for (source, expected) in [
            ("age > 21", 1),
            ("age > 100", 0),
            ("age in [10, 30]", 2),
            ("email.endsWith('@example.com')", 1),
            // Escaping proof: a literal `%` must not behave as a wildcard.
            ("email.contains('%')", 0),
            ("has(profile.city)", 1),
            ("profile.city == null", 1),
            ("size(email) > 11", 1),
            ("active", 1),
            ("!active", 1),
            ("score > 1", 1),
            ("createdAt > timestamp('2024-01-01T00:00:00Z')", 1),
            (
                "createdAt > timestamp('2024-06-02T00:00:00Z') - duration('48h')",
                1,
            ),
            ("age > 21 || email.startsWith('b')", 2),
        ] {
            let filter = Filter::compile(source).unwrap();
            let mut query = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) FROM users WHERE ");
            filter.push_to(&users(), &mut query).unwrap();

            let count: i64 = query
                .build()
                .fetch_one(&pool)
                .await
                .unwrap_or_else(|error| panic!("`{source}` failed: {error}"))
                .get(0);

            assert_eq!(count, expected, "`{source}` matched the wrong rows");
        }
    }
}
