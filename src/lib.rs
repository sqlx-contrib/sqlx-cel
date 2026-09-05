//! Transpiles a [CEL](https://cel.dev) expression into a SQL `WHERE` fragment
//! with bind values, for [sqlx](https://github.com/launchbadge/sqlx).
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use sqlx_cel::{Value, dialect};
//!
//! const VOLUME_COLUMNS: &[(&str, &str)] = &[
//!     ("title", "volumes.title"),
//!     ("read_count", "volumes.read_count"),
//! ];
//!
//! let program = cel::Program::compile(r#"title == "demo" && read_count > 3"#)?;
//! let (sql, values) =
//!     sqlx_cel::transpile(program.expression(), VOLUME_COLUMNS, dialect::Postgres)?;
//!
//! assert_eq!(sql, r#"("volumes"."title" = $1 AND "volumes"."read_count" > $2)"#);
//! assert_eq!(values, vec![Value::Text("demo".into()), Value::Int(3)]);
//! # Ok(())
//! # }
//! ```
//!
//! The expression is plain CEL, not the [AIP-160](https://google.aip.dev/160)
//! grammar, so it works for any caller with a CEL expression and a database
//! table — AIP is one such caller, not a requirement.
//!
//! # The column map is the security boundary
//!
//! [`transpile`] takes a CEL-path → column allow-list, and lookup is
//! **fail-closed**: a path that is absent is an error, so a transpiler built
//! with an empty map rejects every expression. This matters because a CEL
//! environment generated from a proto declares *every* field of the resource,
//! so the parser will happily accept `internal_notes == "x"`. The column map is
//! what stops it reaching SQL. See [`Columns`].
//!
//! The mapped column is the only caller-influenced text in the fragment, and it
//! is emitted after identifier quoting only. Everything from the expression
//! itself becomes a bind value. That is what makes wrapping the assembled query
//! in `AssertSqlSafe` a reasoned assertion rather than a ritual — see
//! [`BindAll`].
//!
//! # Scope
//!
//! **In.** Comparison, boolean, `in`, the four string matchers, `timestamp` and
//! `duration` literals, nested field paths, column-to-column comparison.
//!
//! | CEL | SQL |
//! | --- | --- |
//! | `==`, `!=`, `<`, `<=`, `>`, `>=` | `col op $N`, or `col op col` |
//! | `&&`, `\|\|` | `(lhs AND rhs)`, `(lhs OR rhs)` |
//! | `!` | `(NOT expr)` |
//! | `x == null`, `x != null` | `x IS NULL`, `x IS NOT NULL` |
//! | `x in [a, b, c]` | `x IN ($1, $2, $3)`; empty list → `FALSE` |
//! | `s.contains(x)` | `s LIKE '%' \|\| $N \|\| '%'` |
//! | `s.startsWith(x)` | `s LIKE $N \|\| '%'` |
//! | `s.endsWith(x)` | `s LIKE '%' \|\| $N` |
//! | `s.matches(re)` | `s ~ $N`, or `s REGEXP ?` |
//! | `timestamp("…")` | `$N`, bound as a timestamp |
//! | `duration("…")` | `$N`, bound as microseconds |
//!
//! **Out.** Arithmetic, the ternary, indexing, `size`, `has`, and the
//! comprehension macros (`exists`, `all`, `map`, `filter`) — every one of them
//! rejected with an error, never silently dropped. A dropped predicate widens
//! the result set, which is a wrong answer rather than an error. Also out:
//! query execution, `SELECT` generation, migrations.
//!
//! # Dialects
//!
//! The AST walk is driver-neutral. Placeholder syntax, identifier quoting,
//! string concatenation and the regex operator come through the [`Dialect`]
//! trait, which ships implementations for [`Postgres`], [`Sqlite`] and
//! [`MySql`].
//!
//! Dialects are pure text and always available. *Binding* the values needs the
//! matching Cargo feature — `postgres` (on by default), `sqlite`, `mysql` —
//! which is what supplies `Encode`/`Type` for [`Value`] and makes [`BindAll`]
//! usable. With no driver feature at all, this crate has no sqlx dependency and
//! is a plain CEL-to-SQL transpiler.
//!
//! # There is no type checker
//!
//! `cel::Program::compile` is parse-only; cel-rust has no equivalent of
//! cel-go's checking phase. The transpiler walks the expression structurally,
//! so it loses no *capability* — but it loses the diagnosis. `title == 3` is
//! caught by cel-go's checker at the RPC boundary; here it transpiles cleanly
//! to `"title" = $1` and fails at the database. Surface that error faithfully
//! rather than wrapping it in something vaguer; it is the only diagnosis
//! available.
//!
//! [`Postgres`]: dialect::Postgres
//! [`Sqlite`]: dialect::Sqlite
//! [`MySql`]: dialect::MySql

#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod dialect;

mod column;
mod duration;
mod error;
mod transpiler;
mod value;

#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
mod bind;

pub use column::Columns;
pub use dialect::Dialect;
pub use error::Error;
pub use transpiler::{Options, transpile, transpile_with};
pub use value::Value;

#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
pub use bind::BindAll;
