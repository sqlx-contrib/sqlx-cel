//! Transpiles a [CEL] expression into a SQL `WHERE` fragment with bind values,
//! for [sqlx].
//!
//! ```
//! # #[cfg(feature = "postgres")] {
//! use sqlx::{Postgres, QueryBuilder};
//! use sqlx_cel::{ColumnType, Filter, Table};
//!
//! // The allow-list. Anything not named here is rejected, not passed through.
//! let users = Table::new()
//!     .column("age", ColumnType::Int)
//!     .column("email", ColumnType::Text)
//!     .aliased("createdAt", "created_at", ColumnType::Timestamp);
//!
//! let filter = Filter::compile("age > 21 && email.endsWith('@example.com')")?;
//!
//! let mut query = QueryBuilder::<Postgres>::new("SELECT * FROM users WHERE ");
//! filter.push_to(&users, &mut query)?;
//!
//! assert_eq!(
//!     query.sql().as_str(),
//!     r#"SELECT * FROM users WHERE ("age" > $1) AND ("email" LIKE $2 ESCAPE '!')"#,
//! );
//! # }
//! # Ok::<_, sqlx_cel::Error>(())
//! ```
//!
//! The expression is plain CEL, not the [AIP-160] grammar, so it works for any
//! caller with a CEL expression and a database table.
//!
//! # Two entry points
//!
//! [`Filter::push_to`] appends to a `QueryBuilder` you already own, and is the
//! one to reach for. The builder holds the argument list, so the fragment's
//! placeholders continue your numbering rather than restarting at `$1` — the
//! classic off-by-`$n` bug is not expressible.
//!
//! [`Filter::to_fragment`] renders a standalone [`SqlFragment`] for callers assembling
//! queries some other way. It implements [`IntoArguments`], so it drops
//! straight into `sqlx::query_with`.
//!
//! [`IntoArguments`]: sqlx::IntoArguments
//!
//! # What it will and will not translate
//!
//! Supported: `&&`, `||`, `!`, the ternary, the six comparisons, `in` over a
//! constant list, `has()`, `size()`, `startsWith`/`endsWith`/`contains` as
//! escaped `LIKE`, `matches()` where the driver has a regex operator, and
//! `timestamp()`/`duration()` arithmetic.
//!
//! Rejected, deliberately and with an error rather than an approximation: the
//! comprehension macros (`all`, `exists`, `map`, `filter`), map and struct
//! literals, and any column the [`Schema`] does not name.
//!
//! # Why a schema is mandatory
//!
//! cel-rust does not type-check. `Program::compile` parses and stops; there is
//! no equivalent of cel-go's checking phase. So `age > 'tuesday'` is a
//! perfectly good CEL program, and the only thing that can catch it before the
//! database does is the [`ColumnType`] a [`Schema`] attaches to each column.
//! The allow-list and the type checker are the same object because they have to
//! be.
//!
//! # Nulls
//!
//! `x == null` becomes `IS NULL` and `x != null` becomes `IS NOT NULL`. There
//! is no null [`Value`] to bind, which is both correct — a bound `NULL` makes a
//! comparison unknown, not true — and convenient, since PostgreSQL exposes no
//! public "unknown" type info for an untyped null bind.
//!
//! [CEL]: https://cel.dev
//! [sqlx]: https://github.com/launchbadge/sqlx
//! [AIP-160]: https://google.aip.dev/160

#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(not(any(feature = "postgres", feature = "mysql", feature = "sqlite")))]
compile_error!(
    "sqlx-cel needs at least one driver feature: `postgres`, `mysql`, or `sqlite`. \
     Without one there is no `Dialect` to transpile against."
);

mod filter;

pub use filter::{Column, ColumnType, Dialect, Error, Filter, Schema, SqlFragment, Table, Value};
