use cel::common::ast::{
    CallExpr, ComprehensionExpr, Expr, IdedExpr, LiteralValue, SelectExpr, operators,
};
use cel::parser::Expression;

use crate::column::Columns;
use crate::dialect::Dialect;
use crate::duration;
use crate::error::Error;
use crate::value::Value;

/// A SQL `WHERE` fragment and the values its placeholders bind.
///
/// What [`transpile`] produces. The fragment carries no enclosing parentheses
/// and no `WHERE` keyword, so it splices into SQL the caller wrote by hand.
///
/// Despite the name it is simply a boolean SQL expression, and is equally valid
/// anywhere one is: a `HAVING` clause, a `CHECK` constraint, a partial index
/// predicate.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # use sqlx_cel::{WhereFragment, dialect};
/// let program = cel::Program::compile(r#"title == "demo""#)?;
/// let fragment =
///     sqlx_cel::transpile(program.expression(), &[("title", "t")], dialect::Postgres)?;
///
/// assert_eq!(fragment.sql, r#""t" = $1"#);
///
/// // Or destructure it. `..` is required: the struct is non-exhaustive.
/// let WhereFragment { sql, values, .. } = fragment;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct WhereFragment {
    /// The SQL text, with placeholders in the target dialect's syntax.
    pub sql: String,
    /// The values the placeholders bind, in the order they must be bound.
    pub values: Vec<Value>,
}

/// Knobs on a [`transpile_with`] call.
///
/// Construct with struct-update syntax so that a future field does not break
/// the call:
///
/// ```
/// # use sqlx_cel::Options;
/// let options = Options { param_offset: 5, ..Default::default() };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// The number of the first emitted placeholder. Defaults to `1`.
    ///
    /// Placeholders are then `$offset`, `$offset + 1`, … so the fragment can be
    /// spliced into a query that already has bound values — a hand-written
    /// `WHERE tenant_id = $1` prefix, or a cursor whose placeholders are
    /// numbered after the filter's.
    ///
    /// `0` is treated as `1`, since Postgres has no `$0`.
    ///
    /// Dialects with positional `?` placeholders ignore this, since there is no
    /// number to shift. Bind order still matters there, so a fragment spliced
    /// after existing placeholders must have its values bound after theirs.
    pub param_offset: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self { param_offset: 1 }
    }
}

/// Transpiles a parsed CEL expression into a SQL `WHERE` fragment and the
/// values its placeholders bind.
///
/// The fragment carries no enclosing parentheses and no `WHERE` keyword; the
/// caller splices it into SQL it wrote by hand. `dialect` decides the SQL
/// flavour — see [`dialect`](crate::dialect) for what varies.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use sqlx_cel::dialect;
///
/// const VOLUME_COLUMNS: &[(&str, &str)] = &[
///     ("title", "volumes.title"),
///     ("read_count", "volumes.read_count"),
/// ];
///
/// let program = cel::Program::compile(r#"title == "demo" && read_count > 3"#)?;
///
/// let postgres =
///     sqlx_cel::transpile(program.expression(), VOLUME_COLUMNS, dialect::Postgres)?;
/// assert_eq!(postgres.sql, r#"("volumes"."title" = $1 AND "volumes"."read_count" > $2)"#);
///
/// let sqlite =
///     sqlx_cel::transpile(program.expression(), VOLUME_COLUMNS, dialect::Sqlite)?;
/// assert_eq!(sqlite.sql, r#"("volumes"."title" = ? AND "volumes"."read_count" > ?)"#);
///
/// let mysql =
///     sqlx_cel::transpile(program.expression(), VOLUME_COLUMNS, dialect::MySql)?;
/// assert_eq!(mysql.sql, "(`volumes`.`title` = ? AND `volumes`.`read_count` > ?)");
///
/// // The values are the same whichever dialect produced the text.
/// assert_eq!(
///     postgres.values,
///     vec![sqlx_cel::Value::Text("demo".into()), sqlx_cel::Value::Int(3)],
/// );
/// assert_eq!(postgres.values, sqlite.values);
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns [`Error`] for any path absent from `columns`, and for every CEL
/// construct outside the supported set. Nothing is ever silently dropped — a
/// dropped predicate would widen the result set, which is a wrong answer rather
/// than an error.
pub fn transpile<'c>(
    expr: &Expression,
    columns: impl Into<Columns<'c>>,
    dialect: impl Dialect,
) -> Result<WhereFragment, Error> {
    transpile_with(expr, columns, dialect, Options::default())
}

/// [`transpile`], with control over where placeholder numbering starts.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use sqlx_cel::{Options, dialect};
///
/// let program = cel::Program::compile(r#"title == "demo""#)?;
/// let fragment = sqlx_cel::transpile_with(
///     program.expression(),
///     &[("title", "volumes.title")],
///     dialect::Postgres,
///     Options { param_offset: 5, ..Default::default() },
/// )?;
///
/// assert_eq!(fragment.sql, r#""volumes"."title" = $5"#);
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// As [`transpile`].
pub fn transpile_with<'c>(
    expr: &Expression,
    columns: impl Into<Columns<'c>>,
    dialect: impl Dialect,
    options: Options,
) -> Result<WhereFragment, Error> {
    let mut transpiler = Transpiler {
        columns: columns.into(),
        dialect,
        values: Vec::new(),
        param_offset: options.param_offset.max(1),
    };
    let sql = transpiler.expr(expr)?;
    Ok(WhereFragment {
        sql,
        values: transpiler.values,
    })
}

struct Transpiler<'c, D> {
    columns: Columns<'c>,
    dialect: D,
    values: Vec<Value>,
    param_offset: usize,
}

impl<D: Dialect> Transpiler<'_, D> {
    /// Binds `value` and returns the placeholder that refers to it.
    fn placeholder(&mut self, value: Value) -> String {
        self.values.push(value);
        self.dialect
            .placeholder(self.param_offset + self.values.len() - 1)
    }

    fn expr(&mut self, node: &IdedExpr) -> Result<String, Error> {
        // `id` is for source mapping; this crate reports errors by path.
        match &node.expr {
            Expr::Literal(literal) => self.literal(literal),
            Expr::Ident(_) | Expr::Select(_) => self.ident(node),
            Expr::Call(call) => self.call(call),
            Expr::Comprehension(comprehension) => Err(Error::UnsupportedMacro {
                name: macro_name(comprehension),
            }),
            Expr::List(_) => Err(Error::UnsupportedExpression { kind: "list" }),
            Expr::Map(_) => Err(Error::UnsupportedExpression { kind: "map" }),
            Expr::Struct(_) => Err(Error::UnsupportedExpression { kind: "struct" }),
            Expr::Unspecified => Err(Error::UnsupportedExpression {
                kind: "unspecified",
            }),
        }
    }

    /// Resolves an identifier chain through the column map and quotes the
    /// result. The mapped column is the only caller-influenced text that
    /// reaches the SQL, and it never comes from request data.
    fn ident(&mut self, expr: &IdedExpr) -> Result<String, Error> {
        let path = ident_path(expr)?;
        let column = self
            .columns
            .get(&path)
            .ok_or(Error::UnknownField { path })?;
        Ok(self.dialect.quote_ident(column))
    }

    fn literal(&mut self, literal: &LiteralValue) -> Result<String, Error> {
        let value = match literal {
            LiteralValue::Boolean(v) => Value::Bool(**v),
            LiteralValue::Bytes(v) => Value::Bytes(v.to_vec()),
            LiteralValue::Double(v) => Value::Float(**v),
            LiteralValue::Int(v) => Value::Int(**v),
            // cel's String is a newtype over std's, derefing to `str`.
            LiteralValue::String(v) => Value::Text((**v).to_owned()),
            LiteralValue::UInt(v) => Value::Uint(**v),
            // Only `x == null` / `x != null` are meaningful, and those are
            // handled in `comparison` before the literal is ever reached.
            LiteralValue::Null => return Err(Error::UnexpectedNull),
        };
        Ok(self.placeholder(value))
    }

    fn call(&mut self, call: &CallExpr) -> Result<String, Error> {
        match call.func_name.as_str() {
            operators::EQUALS => self.comparison(call, "="),
            operators::NOT_EQUALS => self.comparison(call, "!="),
            operators::LESS => self.comparison(call, "<"),
            operators::LESS_EQUALS => self.comparison(call, "<="),
            operators::GREATER => self.comparison(call, ">"),
            operators::GREATER_EQUALS => self.comparison(call, ">="),

            operators::LOGICAL_AND => self.binary(call, "AND"),
            operators::LOGICAL_OR => self.binary(call, "OR"),
            operators::LOGICAL_NOT => self.not(call),

            operators::IN => self.in_list(call),

            "contains" => self.like(call, true, true),
            "startsWith" => self.like(call, false, true),
            "endsWith" => self.like(call, true, false),
            "matches" => self.matches(call),

            "timestamp" => self.timestamp(call),
            "duration" => self.duration(call),

            // A macro only survives as a `Call` when it did not expand — a bad
            // arity, say. Name it as the client wrote it either way.
            operators::HAS => Err(Error::UnsupportedMacro { name: "has" }),
            operators::EXISTS => Err(Error::UnsupportedMacro { name: "exists" }),
            operators::ALL => Err(Error::UnsupportedMacro { name: "all" }),
            operators::MAP => Err(Error::UnsupportedMacro { name: "map" }),
            operators::FILTER => Err(Error::UnsupportedMacro { name: "filter" }),
            operators::EXISTS_ONE | "existsOne" => {
                Err(Error::UnsupportedMacro { name: "exists_one" })
            }

            name => Err(Error::UnsupportedFunction {
                name: name.to_string(),
            }),
        }
    }

    /// `=`, `!=`, `<`, `<=`, `>`, `>=`. Both sides recurse, so `created_at <
    /// updated_at` compares two columns and `3 > read_count` binds on the left.
    fn comparison(&mut self, call: &CallExpr, op: &str) -> Result<String, Error> {
        let (lhs, rhs) = binary_args(call, op)?;

        if matches!(op, "=" | "!=") {
            let lhs_null = is_null(lhs);
            let rhs_null = is_null(rhs);
            if lhs_null || rhs_null {
                // `col = $1` with a NULL bind is NULL, not true. The only
                // correct translation is in the SQL text.
                if lhs_null && rhs_null {
                    return Err(Error::UnexpectedNull);
                }
                let operand = self.expr(if lhs_null { rhs } else { lhs })?;
                let test = if op == "=" { "IS NULL" } else { "IS NOT NULL" };
                return Ok(format!("{operand} {test}"));
            }
        }

        let lhs = self.expr(lhs)?;
        let rhs = self.expr(rhs)?;
        Ok(format!("{lhs} {op} {rhs}"))
    }

    fn binary(&mut self, call: &CallExpr, op: &str) -> Result<String, Error> {
        let (lhs, rhs) = binary_args(call, op)?;
        let lhs = self.expr(lhs)?;
        let rhs = self.expr(rhs)?;
        Ok(format!("({lhs} {op} {rhs})"))
    }

    fn not(&mut self, call: &CallExpr) -> Result<String, Error> {
        let [operand] = exact_args(call, "NOT")?;
        let operand = self.expr(operand)?;
        Ok(format!("(NOT {operand})"))
    }

    /// `x in [a, b, c]` becomes `x IN ($1, $2, $3)`. The right-hand side must
    /// be a list literal; an empty one yields `FALSE`, since `x IN ()` is not
    /// valid SQL. Elements recurse, so a list may hold column references.
    fn in_list(&mut self, call: &CallExpr) -> Result<String, Error> {
        let (lhs, rhs) = binary_args(call, "in")?;

        // Transpiling the left-hand side may bind values. If the list turns out
        // to be empty its SQL is discarded, so those bindings must go with it —
        // an orphan in `values` would desynchronize every later placeholder.
        let mark = self.values.len();
        let lhs = self.expr(lhs)?;

        let Expr::List(list) = &rhs.expr else {
            return Err(Error::NotAList);
        };
        if list.elements.is_empty() {
            self.values.truncate(mark);
            return Ok("FALSE".to_string());
        }

        let elements = list
            .elements
            .iter()
            .map(|element| self.expr(element))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(format!("{lhs} IN ({})", elements.join(", ")))
    }

    /// `contains`, `startsWith` and `endsWith` as `LIKE` patterns:
    ///
    /// ```text
    /// contains    leading, trailing   →  LIKE '%' || $N || '%'
    /// startsWith           trailing   →  LIKE $N || '%'
    /// endsWith    leading             →  LIKE '%' || $N
    /// ```
    ///
    /// `LIKE`, not `ILIKE`. The dialect renders the concatenation, since MySQL
    /// spells it `CONCAT`.
    fn like(&mut self, call: &CallExpr, leading: bool, trailing: bool) -> Result<String, Error> {
        let (lhs, rhs) = string_method_args(call)?;
        let lhs = self.expr(lhs)?;
        let rhs = self.expr(rhs)?;
        Ok(self.dialect.like(&lhs, &rhs, leading, trailing))
    }

    /// `s.matches(re)` as a regex match, spelled however the dialect spells it.
    ///
    /// CEL's `matches` is RE2 and no SQL dialect implements RE2; they agree on
    /// the common syntax and diverge at the edges, so a pattern that leans on
    /// RE2 specifics may behave differently or raise a database error.
    fn matches(&mut self, call: &CallExpr) -> Result<String, Error> {
        let (lhs, rhs) = string_method_args(call)?;
        let lhs = self.expr(lhs)?;
        let rhs = self.expr(rhs)?;
        self.dialect
            .regex(&lhs, &rhs)
            .ok_or_else(|| Error::UnsupportedByDialect {
                function: "matches",
                dialect: self.dialect.name(),
            })
    }

    fn timestamp(&mut self, call: &CallExpr) -> Result<String, Error> {
        let literal = time_func_arg(call, "timestamp")?;
        let value = parse_timestamp(literal)?;
        Ok(self.placeholder(value))
    }

    fn duration(&mut self, call: &CallExpr) -> Result<String, Error> {
        let literal = time_func_arg(call, "duration")?;
        let micros = duration::parse(literal).map_err(|message| Error::InvalidDuration {
            literal: literal.to_string(),
            message,
        })?;
        Ok(self.placeholder(Value::Duration(micros)))
    }
}

/// Reconstructs a dotted path from a chain of `Ident`/`Select` expressions:
/// `Select { operand: Ident("author"), field: "name" }` → `author.name`.
fn ident_path(node: &IdedExpr) -> Result<String, Error> {
    match &node.expr {
        Expr::Ident(name) => Ok(name.clone()),
        // `has(a.b)` desugars to a `Select` with `test` set rather than to a
        // comprehension, so it arrives here rather than in `expr`.
        Expr::Select(SelectExpr { test: true, .. }) => Err(Error::UnsupportedMacro { name: "has" }),
        Expr::Select(select) => {
            let operand = ident_path(&select.operand)?;
            Ok(format!("{operand}.{}", select.field))
        }
        _ => Err(Error::NotAPath),
    }
}

fn is_null(node: &IdedExpr) -> bool {
    matches!(&node.expr, Expr::Literal(LiteralValue::Null))
}

/// Borrows exactly `N` arguments, or reports the arity mismatch.
///
/// `&[T]` converts straight to `&[T; N]`, so this costs no allocation on a path
/// that every operator node walks.
fn exact_args<'e, const N: usize>(
    call: &'e CallExpr,
    function: &str,
) -> Result<&'e [IdedExpr; N], Error> {
    call.args.as_slice().try_into().map_err(|_| Error::Arity {
        function: function.to_string(),
        expected: N,
        actual: call.args.len(),
    })
}

fn binary_args<'e>(
    call: &'e CallExpr,
    function: &str,
) -> Result<(&'e IdedExpr, &'e IdedExpr), Error> {
    let [lhs, rhs] = exact_args(call, function)?;
    Ok((lhs, rhs))
}

/// Unpacks a CEL string method into `(receiver, argument)`, accepting both the
/// method form (`s.contains(x)` — a target and one argument) and the function
/// form (`contains(s, x)` — no target and two).
fn string_method_args(call: &CallExpr) -> Result<(&IdedExpr, &IdedExpr), Error> {
    if let Some(target) = &call.target {
        let [arg] = exact_args(call, &call.func_name)?;
        return Ok((target, arg));
    }
    binary_args(call, &call.func_name)
}

fn time_func_arg<'e>(call: &'e CallExpr, function: &'static str) -> Result<&'e str, Error> {
    let [arg] = exact_args(call, function)?;
    match &arg.expr {
        Expr::Literal(LiteralValue::String(literal)) => Ok(literal),
        _ => Err(Error::NotAStringLiteral { function }),
    }
}

#[cfg(feature = "chrono")]
fn parse_timestamp(literal: &str) -> Result<Value, Error> {
    chrono::DateTime::parse_from_rfc3339(literal)
        .map(|parsed| Value::Timestamp(parsed.with_timezone(&chrono::Utc)))
        .map_err(|error| Error::InvalidTimestamp {
            literal: literal.to_string(),
            message: error.to_string(),
        })
}

#[cfg(all(feature = "time", not(feature = "chrono")))]
fn parse_timestamp(literal: &str) -> Result<Value, Error> {
    time::OffsetDateTime::parse(literal, &time::format_description::well_known::Rfc3339)
        .map(Value::Timestamp)
        .map_err(|error| Error::InvalidTimestamp {
            literal: literal.to_string(),
            message: error.to_string(),
        })
}

#[cfg(not(any(feature = "chrono", feature = "time")))]
fn parse_timestamp(_literal: &str) -> Result<Value, Error> {
    Err(Error::FeatureDisabled {
        function: "timestamp",
        feature: "chrono",
    })
}

/// Recovers the macro a comprehension was desugared from, so the error names
/// what the client actually wrote. `map` and `filter` accumulate into the same
/// empty list and cannot be told apart in every case, so they are reported
/// together.
fn macro_name(comprehension: &ComprehensionExpr) -> &'static str {
    let step = match &comprehension.loop_step.expr {
        Expr::Call(call) => call.func_name.as_str(),
        _ => "",
    };
    match &comprehension.accu_init.expr {
        Expr::Literal(LiteralValue::Boolean(seed)) if !**seed && step == operators::LOGICAL_OR => {
            "exists"
        }
        Expr::Literal(LiteralValue::Boolean(seed)) if **seed && step == operators::LOGICAL_AND => {
            "all"
        }
        Expr::Literal(LiteralValue::Int(_)) => "exists_one",
        Expr::List(list) if list.elements.is_empty() => "map/filter",
        _ => "comprehension",
    }
}

#[cfg(test)]
mod tests {
    use super::{Options, WhereFragment, transpile, transpile_with};
    use crate::dialect::{MySql, Postgres, Sqlite};
    use crate::{Error, Value};

    const COLUMNS: &[(&str, &str)] = &[
        ("name", "name"),
        ("age", "age"),
        ("title", "book_title"),
        ("balance", "balance"),
        ("published", "published"),
        ("cover", "cover"),
        ("timeout", "timeout"),
        ("create_time", "created_at"),
        ("update_time", "updated_at"),
        ("read_count", "volumes.read_count"),
        ("author.name", "authors.name"),
    ];

    /// Transpiles for Postgres, or returns the error.
    fn pg(source: &str) -> Result<WhereFragment, Error> {
        let program = cel::Program::compile(source).expect("source must parse");
        transpile(program.expression(), COLUMNS, Postgres)
    }

    /// SQL and values as a pair, so an assertion can pin both at once.
    fn parts(source: &str) -> (String, Vec<Value>) {
        let fragment = pg(source).unwrap();
        (fragment.sql, fragment.values)
    }

    /// The SQL only, panicking on error.
    fn sql(source: &str) -> String {
        pg(source).unwrap().sql
    }

    /// The error only, panicking on success.
    fn err(source: &str) -> Error {
        pg(source).unwrap_err()
    }

    #[test]
    fn equality_binds_the_literal_and_quotes_the_column() {
        assert_eq!(
            parts(r#"name == "Alice""#),
            (
                r#""name" = $1"#.to_string(),
                vec![Value::Text("Alice".into())]
            ),
        );
    }

    #[test]
    fn maps_cel_paths_to_their_backing_columns() {
        assert_eq!(
            parts(r#"title == "The Go Programming Language""#),
            (
                r#""book_title" = $1"#.to_string(),
                vec![Value::Text("The Go Programming Language".into())],
            ),
        );
    }

    #[test]
    fn quotes_a_dotted_column_segment_by_segment() {
        assert_eq!(sql("read_count > 3"), r#""volumes"."read_count" > $1"#);
    }

    #[test]
    fn resolves_a_dotted_cel_path_through_the_map() {
        assert_eq!(sql(r#"author.name == "x""#), r#""authors"."name" = $1"#);
    }

    #[test]
    fn combines_and_or_with_parentheses_per_branch() {
        assert_eq!(
            parts(r#"name == "Alice" && age > 30"#),
            (
                r#"("name" = $1 AND "age" > $2)"#.to_string(),
                vec![Value::Text("Alice".into()), Value::Int(30)],
            ),
        );
        assert_eq!(
            sql(r#"name == "Alice" || name == "Bob""#),
            r#"("name" = $1 OR "name" = $2)"#,
        );
    }

    #[test]
    fn wraps_not_in_parentheses() {
        assert_eq!(sql(r#"!(name == "Alice")"#), r#"(NOT "name" = $1)"#);
    }

    #[test]
    fn a_bare_column_is_a_predicate() {
        // Which is what makes `!published` mean anything.
        assert_eq!(sql("!published"), r#"(NOT "published")"#);
        assert_eq!(sql("published"), r#""published""#);
    }

    #[test]
    fn compares_two_columns_without_binding_anything() {
        assert_eq!(
            parts("update_time > create_time"),
            (r#""updated_at" > "created_at""#.to_string(), vec![]),
        );
    }

    #[test]
    fn binds_on_the_left_when_the_literal_is_on_the_left() {
        assert_eq!(sql("3 > age"), r#"$1 > "age""#);
    }

    #[test]
    fn every_comparison_operator() {
        for (cel, op) in [
            ("==", "="),
            ("!=", "!="),
            ("<", "<"),
            ("<=", "<="),
            (">", ">"),
            (">=", ">="),
        ] {
            assert_eq!(sql(&format!("age {cel} 30")), format!(r#""age" {op} $1"#));
        }
    }

    #[test]
    fn folds_unary_minus_on_literals() {
        // cel-rust folds the sign into the literal at parse time, so there is
        // no `-_` call to handle.
        assert_eq!(
            parts("balance > -5"),
            (r#""balance" > $1"#.to_string(), vec![Value::Int(-5)]),
        );
        assert_eq!(
            pg("balance > -2.5").unwrap().values,
            vec![Value::Float(-2.5)]
        );
    }

    #[test]
    fn negating_a_column_is_arithmetic_and_is_rejected() {
        assert_eq!(
            err("-age > 3"),
            Error::UnsupportedFunction { name: "-_".into() },
        );
    }

    #[test]
    fn binds_every_literal_kind() {
        assert_eq!(
            pg("published == true").unwrap().values,
            vec![Value::Bool(true)]
        );
        assert_eq!(pg("age == 42").unwrap().values, vec![Value::Int(42)]);
        assert_eq!(pg("age == 42u").unwrap().values, vec![Value::Uint(42)]);
        assert_eq!(pg("age == 2.75").unwrap().values, vec![Value::Float(2.75)]);
        assert_eq!(
            pg(r#"cover == b"abc""#).unwrap().values,
            vec![Value::Bytes(vec![97, 98, 99])],
        );
    }

    #[test]
    fn renders_in_over_a_list_literal() {
        assert_eq!(
            parts(r#"name in ["Alice", "Bob", "Carol"]"#),
            (
                r#""name" IN ($1, $2, $3)"#.to_string(),
                vec![
                    Value::Text("Alice".into()),
                    Value::Text("Bob".into()),
                    Value::Text("Carol".into()),
                ],
            ),
        );
    }

    #[test]
    fn in_elements_may_be_columns_too() {
        assert_eq!(sql("age in [age, 3]"), r#""age" IN ("age", $1)"#,);
    }

    #[test]
    fn an_empty_in_list_becomes_false_and_binds_nothing() {
        // `x IN ()` is not valid SQL. The discarded left-hand side must not
        // leave an orphan in `values` -- that would desynchronize every
        // placeholder after it.
        assert_eq!(parts("name in []"), ("FALSE".to_string(), vec![]));
        assert_eq!(
            parts(r#"("z" in []) && name == "Alice""#),
            (
                r#"(FALSE AND "name" = $1)"#.to_string(),
                vec![Value::Text("Alice".into())],
            ),
        );
    }

    #[test]
    fn in_still_validates_the_left_hand_side_of_an_empty_list() {
        assert_eq!(
            err("missing in []"),
            Error::UnknownField {
                path: "missing".into()
            },
        );
    }

    #[test]
    fn renders_the_string_matchers_as_like() {
        assert_eq!(
            sql(r#"name.contains("ali")"#),
            r#""name" LIKE '%' || $1 || '%'"#,
        );
        assert_eq!(sql(r#"name.startsWith("ali")"#), r#""name" LIKE $1 || '%'"#);
        assert_eq!(sql(r#"name.endsWith("ali")"#), r#""name" LIKE '%' || $1"#);
        assert_eq!(sql(r#"name.matches("^a")"#), r#""name" ~ $1"#);
    }

    #[test]
    fn accepts_the_function_style_call_shape() {
        // `contains(s, x)` rather than `s.contains(x)`.
        assert_eq!(
            sql(r#"contains(name, "ali")"#),
            r#""name" LIKE '%' || $1 || '%'"#,
        );
    }

    #[test]
    fn null_becomes_is_null_rather_than_a_bound_null() {
        // Binding NULL to `col = $1` yields NULL, not true.
        assert_eq!(
            parts("cover == null"),
            (r#""cover" IS NULL"#.to_string(), vec![])
        );
        assert_eq!(sql("cover != null"), r#""cover" IS NOT NULL"#);
        assert_eq!(sql("null == cover"), r#""cover" IS NULL"#);
    }

    #[test]
    fn null_anywhere_else_is_rejected() {
        assert_eq!(err("cover > null"), Error::UnexpectedNull);
        assert_eq!(err("null == null"), Error::UnexpectedNull);
        assert_eq!(err("cover in [null]"), Error::UnexpectedNull);
    }

    #[test]
    fn placeholders_start_at_one_and_follow_the_offset() {
        assert_eq!(sql(r#"name == "Alice""#), r#""name" = $1"#);

        let program = cel::Program::compile(r#"name == "Alice" && age > 30"#).unwrap();
        let fragment = transpile_with(
            program.expression(),
            COLUMNS,
            Postgres,
            Options { param_offset: 5 },
        )
        .unwrap();
        assert_eq!(fragment.sql, r#"("name" = $5 AND "age" > $6)"#);
        assert_eq!(
            fragment.values,
            vec![Value::Text("Alice".into()), Value::Int(30)],
        );
    }

    #[test]
    fn a_zero_offset_is_treated_as_one_because_there_is_no_dollar_zero() {
        let program = cel::Program::compile(r#"name == "Alice""#).unwrap();
        let fragment = transpile_with(
            program.expression(),
            COLUMNS,
            Postgres,
            Options { param_offset: 0 },
        )
        .unwrap();
        assert_eq!(fragment.sql, r#""name" = $1"#);
    }

    #[test]
    fn fails_closed_on_an_unmapped_path() {
        assert_eq!(
            err(r#"internal_notes == "x""#),
            Error::UnknownField {
                path: "internal_notes".into()
            },
        );
        assert_eq!(
            err("age == other"),
            Error::UnknownField {
                path: "other".into()
            },
            "the right-hand side is checked too",
        );
    }

    #[test]
    fn an_empty_column_map_rejects_everything() {
        let program = cel::Program::compile(r#"name == "Alice""#).unwrap();
        let empty: &[(&str, &str)] = &[];
        assert_eq!(
            transpile(program.expression(), empty, Postgres).unwrap_err(),
            Error::UnknownField {
                path: "name".into()
            },
        );
    }

    #[test]
    fn rejects_arithmetic_and_the_other_unsupported_functions() {
        for (source, name) in [
            ("age + 1 == 2", "_+_"),
            ("age - 1 == 2", "_-_"),
            ("age * 2 == 4", "_*_"),
            ("age / 2 == 4", "_/_"),
            ("age % 2 == 0", "_%_"),
            ("size(name) > 1", "size"),
            ("published ? age : age", "_?_:_"),
        ] {
            assert_eq!(
                err(source),
                Error::UnsupportedFunction { name: name.into() },
                "for {source}",
            );
        }
    }

    #[test]
    fn rejects_indexing() {
        assert!(matches!(
            err(r#"name["x"] == "y""#),
            Error::UnsupportedFunction { .. },
        ));
    }

    #[test]
    fn names_the_macro_the_client_actually_wrote() {
        // These all desugar to a comprehension; the client will not recognise
        // itself in the desugared form.
        for (source, name) in [
            (r#"name.exists(t, t == "x")"#, "exists"),
            (r#"name.all(t, t == "x")"#, "all"),
            (r#"name.exists_one(t, t == "x")"#, "exists_one"),
            ("name.map(t, t)", "map/filter"),
            (r#"name.filter(t, t == "x")"#, "map/filter"),
        ] {
            assert_eq!(
                err(source),
                Error::UnsupportedMacro { name },
                "for {source}"
            );
        }
    }

    #[test]
    fn rejects_has_which_desugars_to_a_select_rather_than_a_comprehension() {
        assert_eq!(
            err("has(author.name)"),
            Error::UnsupportedMacro { name: "has" },
        );
    }

    #[test]
    fn rejects_a_map_or_list_literal_outside_in() {
        assert_eq!(
            err("age == [1, 2]"),
            Error::UnsupportedExpression { kind: "list" },
        );
        assert_eq!(
            err(r#"age == {"a": 1}"#),
            Error::UnsupportedExpression { kind: "map" },
        );
    }

    #[test]
    fn rejects_in_whose_right_hand_side_is_not_a_list() {
        assert_eq!(err(r#"name in "abc""#), Error::NotAList);
    }

    #[cfg(any(feature = "chrono", feature = "time"))]
    #[test]
    fn binds_timestamp_literals() {
        let (sql, values) = parts(r#"create_time > timestamp("2025-01-02T03:04:05Z")"#);
        assert_eq!(sql, r#""created_at" > $1"#);
        assert_eq!(values.len(), 1);
        #[cfg(feature = "chrono")]
        assert_eq!(
            values[0],
            Value::Timestamp(
                "2025-01-02T03:04:05Z"
                    .parse::<chrono::DateTime<chrono::Utc>>()
                    .unwrap()
            ),
        );
    }

    #[cfg(not(any(feature = "chrono", feature = "time")))]
    #[test]
    fn timestamp_says_which_feature_to_turn_on_rather_than_failing_obscurely() {
        assert_eq!(
            err(r#"create_time > timestamp("2025-01-02T03:04:05Z")"#),
            Error::FeatureDisabled {
                function: "timestamp",
                feature: "chrono",
            },
        );
    }

    #[test]
    fn binds_duration_literals_as_microseconds() {
        assert_eq!(
            parts(r#"timeout > duration("1h30m")"#),
            (
                r#""timeout" > $1"#.to_string(),
                vec![Value::Duration(90 * 60 * 1_000_000)],
            ),
        );
        // Negative durations are legal CEL and are why `std::time::Duration`
        // is not the carrier.
        assert_eq!(
            pg(r#"timeout > duration("-1h")"#).unwrap().values,
            vec![Value::Duration(-60 * 60 * 1_000_000)],
        );
    }

    #[test]
    fn rejects_bad_time_literals() {
        // The message is the timestamp library's own, so it is quoted rather
        // than asserted verbatim -- it is a diagnosis, not a contract.
        #[cfg(any(feature = "chrono", feature = "time"))]
        {
            let Error::InvalidTimestamp { literal, message } =
                err(r#"create_time > timestamp("not-a-date")"#)
            else {
                panic!("expected an InvalidTimestamp");
            };
            assert_eq!(literal, "not-a-date");
            assert!(!message.is_empty());
        }
        assert!(matches!(
            err(r#"timeout > duration("nope")"#),
            Error::InvalidDuration { .. },
        ));
        assert_eq!(
            err("create_time > timestamp(1)"),
            Error::NotAStringLiteral {
                function: "timestamp"
            },
        );
        assert_eq!(
            err("create_time > timestamp(name)"),
            Error::NotAStringLiteral {
                function: "timestamp"
            },
        );
    }

    #[test]
    fn reports_arity_by_the_name_the_caller_would_recognise() {
        assert_eq!(
            err(r#"timestamp("2025-01-02T03:04:05Z", "x")"#),
            Error::Arity {
                function: "timestamp".into(),
                expected: 1,
                actual: 2
            },
        );
        assert_eq!(
            err("contains(name)"),
            Error::Arity {
                function: "contains".into(),
                expected: 2,
                actual: 1
            },
        );
        assert_eq!(
            err(r#"name.contains("a", "b")"#),
            Error::Arity {
                function: "contains".into(),
                expected: 1,
                actual: 2
            },
        );
    }

    // -- dialects ---------------------------------------------------------

    fn for_dialect(source: &str, dialect: impl crate::Dialect) -> String {
        let program = cel::Program::compile(source).unwrap();
        transpile(program.expression(), COLUMNS, dialect)
            .unwrap()
            .sql
    }

    #[test]
    fn sqlite_uses_positional_placeholders_and_ansi_quoting() {
        assert_eq!(
            for_dialect(r#"name == "Alice" && age > 30"#, Sqlite),
            r#"("name" = ? AND "age" > ?)"#,
        );
        assert_eq!(
            for_dialect(r#"name in ["a", "b"]"#, Sqlite),
            r#""name" IN (?, ?)"#,
        );
        assert_eq!(
            for_dialect(r#"name.contains("ali")"#, Sqlite),
            r#""name" LIKE '%' || ? || '%'"#,
        );
        assert_eq!(
            for_dialect(r#"name.matches("^a")"#, Sqlite),
            r#""name" REGEXP ?"#,
        );
    }

    #[test]
    fn mysql_uses_backticks_and_concat() {
        assert_eq!(
            for_dialect(r#"name == "Alice" && age > 30"#, MySql),
            "(`name` = ? AND `age` > ?)",
        );
        assert_eq!(
            for_dialect("read_count > 3", MySql),
            "`volumes`.`read_count` > ?",
        );
        assert_eq!(
            for_dialect(r#"name.contains("ali")"#, MySql),
            "`name` LIKE CONCAT('%', ?, '%')",
        );
        assert_eq!(
            for_dialect(r#"name.matches("^a")"#, MySql),
            "`name` REGEXP ?",
        );
        assert_eq!(for_dialect("cover == null", MySql), "`cover` IS NULL");
    }

    #[test]
    fn the_values_are_the_same_whichever_dialect_produced_the_text() {
        let source = r#"name == "Alice" && age in [1, 2] && timeout > duration("1h")"#;
        let program = cel::Program::compile(source).unwrap();
        let pg = transpile(program.expression(), COLUMNS, Postgres)
            .unwrap()
            .values;
        let sqlite = transpile(program.expression(), COLUMNS, Sqlite)
            .unwrap()
            .values;
        let mysql = transpile(program.expression(), COLUMNS, MySql)
            .unwrap()
            .values;
        assert_eq!(pg, sqlite);
        assert_eq!(pg, mysql);
    }

    #[test]
    fn a_dialect_without_regex_rejects_matches_rather_than_guessing() {
        struct NoRegex;
        impl crate::Dialect for NoRegex {
            fn name(&self) -> &'static str {
                "no-regex"
            }
            fn placeholder(&self, _: usize) -> String {
                "?".to_string()
            }
            fn regex(&self, _: &str, _: &str) -> Option<String> {
                None
            }
        }

        let program = cel::Program::compile(r#"name.matches("^a")"#).unwrap();
        assert_eq!(
            transpile(program.expression(), COLUMNS, NoRegex).unwrap_err(),
            Error::UnsupportedByDialect {
                function: "matches",
                dialect: "no-regex"
            },
        );
    }
}
