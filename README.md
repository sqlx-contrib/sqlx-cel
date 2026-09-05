# sqlx-cel

Transpiles a [CEL](https://cel.dev) expression into a SQL `WHERE` fragment
with bind values, for [sqlx](https://github.com/launchbadge/sqlx).

The Rust counterpart of [pgxcel](https://github.com/pgx-contrib/pgxcel), and
the filter half of [sqlx-aip](https://github.com/sqlx-contrib/sqlx-aip).

```rust
use sqlx::AssertSqlSafe;
use sqlx_cel::{BindAll, dialect};

let program = cel::Program::compile(r#"title == "demo" && read_count > 3"#)?;

let (sql, values) =
    sqlx_cel::transpile(program.expression(), VOLUME_COLUMNS, dialect::Postgres)?;
// sql:    ("volumes"."title" = $1 AND "volumes"."read_count" > $2)
// values: [Value::Text("demo".into()), Value::Int(3)]

let volumes = sqlx::query_as::<_, Volume>(AssertSqlSafe(
        format!("SELECT * FROM volumes WHERE {sql}")))
    .bind_all(values)
    .fetch_all(&pool)
    .await?;
```

The expression is plain CEL, not the [AIP-160](https://google.aip.dev/160)
grammar, so it works for any caller with a CEL expression and a database
table — AIP is one such caller, not a requirement.

## Dialects

The AST walk is driver-neutral. Everything that differs between databases
comes through the `Dialect` trait, and the same expression transpiles to any
of them with an identical value list:

| | placeholders | quoting | concat | regex |
| --- | --- | --- | --- | --- |
| `dialect::Postgres` | `$1` | `"ident"` | `\|\|` | `~` |
| `dialect::Sqlite` | `?` | `"ident"` | `\|\|` | `REGEXP` † |
| `dialect::MySql` | `?` | `` `ident` `` | `CONCAT` | `REGEXP` |

† SQLite parses `REGEXP` but resolves it to a function the application must
register; without one it fails at execution with `no such function: REGEXP`.

Dialects are pure text and always available. *Binding* the values needs the
matching Cargo feature — `postgres` (default), `sqlite`, `mysql` — which
supplies `Encode`/`Type` for `Value` and enables `bind_all`. With no driver
feature at all the crate has no sqlx dependency and is a plain CEL-to-SQL
transpiler.

## The column map is the security boundary

`transpile` takes a CEL-path → column allow-list, and lookup is
**fail-closed**: a path that is absent is an error, so an empty map rejects
every expression.

```rust
const VOLUME_COLUMNS: &[(&str, &str)] = &[
    ("title",       "volumes.title"),
    ("read_count",  "volumes.read_count"),
    ("create_time", "volumes.created_at"),
];
```

This matters because a CEL environment generated from a proto declares
*every* field of the resource, so the parser will happily accept
`internal_notes == "x"`. The column map is what stops it reaching SQL. Note
the last entry: the CEL path and the column name differ, which is why this
is a map rather than a set of allowed paths.

## Scope

**In.** Comparison, boolean, `in`, the four string matchers, `timestamp` and
`duration` literals, nested field paths, column-to-column comparison, and
`x == null` → `IS NULL`.

**Out.** Arithmetic, ternary, indexing, `size`, `has`, and the comprehension
macros (`exists`, `all`, `map`, `filter`) — all rejected with an error, never
silently dropped, because a dropped predicate widens the result set. Query
execution, `SELECT` generation, migrations.

## Design notes

| Document | What it settles |
| --- | --- |
| [docs/transpiler.md](docs/transpiler.md) | The AST walk, operator coverage, what is rejected, quoting, parameter numbering |
| [docs/values.md](docs/values.md) | The `Value` enum and how it reaches sqlx, time types, nulls, version floor |
| [docs/columns.md](docs/columns.md) | The fail-closed path → column allow-list, and why it is the security boundary |

These were written before the implementation and are largely still accurate;
where the code diverges, each document says so inline. The two substantive
divergences are that the crate is not Postgres-only, and that `Value` carries
a driver-neutral microsecond count rather than a `PgInterval`.

## Development

sqlx 0.9 declares `rust-version = "1.94"`, so this crate does too.

```sh
cargo test --features sqlite,mysql   # all drivers, incl. end-to-end SQLite
cargo clippy --all-targets --features sqlite,mysql
```

## License

[MIT](LICENSE)
