# Bind values

The Go predecessor returns `[]any` and hands it to pgx, which takes `...any`.
sqlx has no equivalent: `Query::bind` is typed at the call site. This document
is the design that replaces it. It is the only part of the port that is a
design rather than a transcription.

## The problem

`bind` is monomorphic:

```rust
pub fn bind<T: 'q + Encode<'q, DB> + Type<DB>>(self, value: T) -> Self
```

A transpiler produces a heterogeneous list whose length and types are known
only at runtime, so there is nothing to name as `T`.

The escape hatch is that `Arguments::add` is generic *per call site*
(`sqlx-core-0.9.0/src/arguments.rs:20`):

```rust
fn add<'t, T>(&mut self, value: T) -> Result<(), BoxDynError>
where T: Encode<'t, Self::Database> + Type<Self::Database>;
```

A `match` over an owned enum monomorphizes each arm separately, so the
heterogeneity is resolved at compile time even though the sequence is not.

## `Value`

```rust
/// A literal the transpiler bound to a placeholder.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Value {
    Bool(bool),
    Text(String),
    Bytes(Vec<u8>),
    Int(i64),
    Uint(u64),
    Float(f64),
    Timestamp(DateTime<Utc>),   // feature `chrono`
    Interval(PgInterval),
}
```

Mirrors `cel::common::ast::LiteralValue` plus the two constructed types.
There is deliberately **no `Null` variant** — see [Nulls](#nulls).

`PartialEq` is not decoration. It is what makes the transpiler unit-testable
without a database, and it is why `transpile` returns `Vec<Value>` rather than
a `PgArguments`: `PgArguments` is an opaque byte buffer with no way to assert
what went into it.

## Three layers

Expose all three. Each is a few lines once `Value` exists, and they serve
different callers.

**1. The core.** Driver-neutral, testable, what everything else is built on:

```rust
pub fn transpile(expr: &Expression, columns: &Columns) -> Result<(String, Vec<Value>), Error>;
```

**2. `impl Encode<'_, Postgres> + Type<Postgres> for Value`**, so `.bind(v)`
works directly. The trick is `Encode::produces`
(`sqlx-core-0.9.0/src/encode.rs:48`), a hook for a value-dependent type OID:
`PgArguments::add` prefers it over `T::type_info()`. So `Type::type_info`
returns any placeholder and `Type::compatible` returns `true` broadly, while
`produces` returns the real per-variant OID.

**3. A `bind_all` extension trait**, for the common case:

```rust
let sql = format!("SELECT * FROM volumes WHERE {where_sql} ORDER BY {order_sql} LIMIT $3");
let books = sqlx::query_as::<_, Volume>(AssertSqlSafe(sql))
    .bind_all(values)
    .bind(page_size)
    .fetch_all(&pool)
    .await?;
```

## `AssertSqlSafe` is not optional

sqlx 0.9's `query`/`query_as` take `impl SqlSafeStr`, which is implemented for
`&'static str` and not for `&str` or `String`
(`sqlx-core-0.9.0/src/sql_str.rs:47`). Runtime-assembled SQL — the entire
point of this crate — must be wrapped:

```rust
use sqlx::sql_str::AssertSqlSafe;
sqlx::query_as::<_, Volume>(AssertSqlSafe(sql))
```

Put this in the first README example. The trait carries a
`#[diagnostic::on_unimplemented]` message, but a user who has not seen it will
read the error as "my `String` should coerce" and lose ten minutes.

The assertion is honest here: the only caller-influenced text in the fragment
is column names, and those come from a fail-closed map, never from request
data. Say so in the docs where the wrap is recommended, so it is a reasoned
assertion rather than a ritual.

## Time types

pgx binds `time.Time` and `time.Duration` natively. sqlx needs a choice.

**Timestamps.** Feature-gate `chrono` and `time`, mirroring sqlx's own
features, defaulting to `chrono`. `timestamp("…")` takes an RFC 3339 string;
parse it to the enabled type. If both features are on, prefer `chrono` for the
`Value` variant rather than adding two variants.

**Durations.** Postgres `INTERVAL` is `PgInterval`, which implements
`Type<Postgres>` and has `TryFrom<std::time::Duration>`
(`sqlx-postgres-0.9.0/src/types/interval.rs:95`).

CEL permits `duration("-1h")`, and `std::time::Duration` cannot hold it. Do
not route through `std::time::Duration`; construct `PgInterval { months: 0,
days: 0, microseconds }` directly from the parsed nanoseconds. This is a real
case, not a hypothetical: `read_time > duration("-1h")` compiles in cel-rust.

CEL's duration grammar is Go's `time.ParseDuration` syntax (`1h30m`, `-5s`,
`300ms`, `1.5h`). No Rust crate parses it; write the parser, it is ~40 lines,
and test `1.5h`, `-1h`, `1h30m`, bare `0`, and overflow.

Sub-microsecond precision is lost — `PgInterval` is microseconds and Postgres
`INTERVAL` stores no finer. Round, do not truncate silently, and document it.

## Nulls

`Value` has no `Null` variant, and `transpile` never emits a NULL bind. A CEL
`null` literal is handled in the SQL text instead — `x == null` becomes
`x IS NULL` — because binding NULL to `col = $1` yields NULL, not true, which
is a wrong-answer bug rather than an error.

There is a second reason, and it is the one that generalizes: `args.add(None::<T>)`
forces a choice of `T`, and without a type checker there is nothing to derive
it from. A NULL has to be typed before it can be sent, and this crate never
knows the type. Any future feature that wants to bind a NULL runs into this
first. The same constraint drives the cursor decision in `sqlx-aip` — see
`sqlx-aip/docs/query.md`.

## Version floor

sqlx **0.9**. The `SqlSafeStr` gate and the `add<'t, T>` signature are both
0.9; 0.8's `add<T>` carries a `T: 'q` bound tied to the arguments' lifetime,
which is satisfiable with owned values but constrains the API for no gain.
Supporting both is not worth a feature flag on a crate with no users yet.
