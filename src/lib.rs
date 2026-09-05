//! Transpiles a CEL expression into a Postgres `WHERE` fragment with
//! positional bind values.
//!
//! Not implemented yet. The design lives in `docs/`, and
//! [pgxcel](https://github.com/pgx-contrib/pgxcel) is the reference
//! implementation this one ports:
//!
//! - `docs/transpiler.md` — the AST walk and operator coverage
//! - `docs/values.md` — how bind values reach sqlx
//! - `docs/columns.md` — the fail-closed path to column allow-list

#![deny(missing_docs)]
