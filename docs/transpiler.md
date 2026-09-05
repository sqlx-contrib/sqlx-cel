# CEL to SQL

> **Status: implemented.** This was the design written before the code, and it
> is still accurate except where marked. The one structural change is that the
> crate is no longer Postgres-only: the walk is driver-neutral and everything
> dialect-specific goes through the `Dialect` trait. See
> [What changed in the implementation](#what-changed-in-the-implementation).

This is the normative specification for the transpiler. It is a port of
[pgxcel](https://github.com/pgx-contrib/pgxcel) — read `transpiler.go` there
before starting; it is ~440 lines and this crate is the same walk with a
different AST type and a different way of carrying bind values.

The port is closer to a transcription than a redesign. The section
[What actually differs](#what-actually-differs) is the whole list of places
where it is not.

## Scope

One function. A parsed CEL expression, plus an AIP-path → database-column
allow-list, becomes a Postgres `WHERE` fragment and the values its
placeholders bind:

```rust
let program = cel::Program::compile(r#"title == "demo" && read_count > 3"#)?;

let (sql, values) =
    sqlx_cel::transpile(program.expression(), COLUMNS, dialect::Postgres)?;
// sql:    ("volumes"."title" = $1 AND "volumes"."read_count" > $2)
// values: [Value::Text("demo"), Value::Int(3)]
```

The third argument is the dialect; `Sqlite` and `MySql` produce the same value
list with `?` placeholders and their own quoting.

No connection, no query execution, no `SELECT`. The caller splices the
fragment into SQL it wrote by hand. Value binding is specified separately in
[values.md](values.md).

## The AST

`cel` 0.14 exposes everything needed. `Program::expression()` is public
(`cel-0.14.4/src/lib.rs:209`) and returns `&IdedExpr`, defined with all
variants public in `cel-0.14.4/src/common/ast/mod.rs`:

```rust
pub struct IdedExpr { pub id: u64, pub expr: Expr }

pub enum Expr {
    Unspecified,
    Call(CallExpr),                     // { func_name, target: Option<Box<IdedExpr>>, args }
    Comprehension(Box<ComprehensionExpr>),
    Ident(String),
    List(ListExpr),                     // { elements, optional_indices }
    Literal(LiteralValue),              // Boolean | Bytes | Double | Int | Null | String | UInt
    Map(MapExpr),
    Select(SelectExpr),                 // { operand: Box<IdedExpr>, field, test }
    Struct(StructExpr),
}
```

Walk `IdedExpr.expr`. Ignore `id` — it exists for source mapping and this
crate reports errors by path, not by offset.

Function names match cel-go's exactly. This was verified by compiling each
supported form and dumping the AST, not assumed:

| Source | `Expr` produced |
| --- | --- |
| `a == b && c > 3` | `Call("_&&_")` over `Call("_==_")`, `Call("_>_")` |
| `!published` | `Call("!_")` |
| `genre in [1, 2, 3]` | `Call("@in")`, `args[1]` is `Expr::List` |
| `title.contains("x")` | `Call("contains")`, `target: Some(Ident("title"))` |
| `title.matches("^a")` | `Call("matches")`, receiver-style |
| `timestamp("2025-01-02T03:04:05Z")` | `Call("timestamp")` with one string literal arg |
| `author.name` | `Select { operand: Ident("author"), field: "name" }` |
| `read_count == -3` | `Literal(Int(-3))` — **folded, no `-_` call** |
| `cover != null` | `Call("_!=_")` with `Literal(Null)` |
| `tags.exists(t, t == "x")` | `Comprehension(..)` |

The operator name constants are in `cel::common::ast::operators`. Use them
rather than string literals.

## Operator coverage

Everything below matches pgxcel's output byte for byte. The tests in
`pgxcel/transpiler_test.go` port directly as assertion pairs.

| CEL | Postgres fragment |
| --- | --- |
| `==`, `!=`, `<`, `<=`, `>`, `>=` | `col op $N`, or `col op col` |
| `&&`, `\|\|` | `(lhs AND rhs)`, `(lhs OR rhs)` |
| `!` | `(NOT expr)` |
| `x in [a, b, c]` | `x IN ($1, $2, $3)`; empty list → `FALSE` |
| `s.contains(x)` | `s LIKE '%' \|\| $N \|\| '%'` |
| `s.startsWith(x)` | `s LIKE $N \|\| '%'` |
| `s.endsWith(x)` | `s LIKE '%' \|\| $N` |
| `s.matches(re)` | `s ~ $N` (POSIX regex), or `s REGEXP ?` |
| `timestamp("…")` | `$N`, bound as a timestamp |
| `duration("…")` | `$N`, bound as an interval |

`LIKE`, not `ILIKE`. pgxaip's README claims `ILIKE` but its own test asserts
`LIKE` (`pgxaip/query_test.go:144`); the README is stale from before the
einride removal. Match the test.

Both sides of a comparison recurse, so `created_at < updated_at` compares two
columns and `3 > read_count` binds on the left. Same for `in` elements — a
list may hold column references, not only literals.

## What is rejected

Fail loudly. Every one of these is an error, never a silently-dropped
predicate:

- `Expr::Map`, `Expr::Struct`, `Expr::Comprehension`, `Expr::Unspecified`
- any `func_name` not in the coverage table, including arithmetic (`_+_`,
  `_-_`, `_*_`, `_/_`, `_%_`), `_?_:_`, `_[_]`, `size`, `has`
- `@in` whose right-hand side is not a list literal
- an identifier or `Select` chain absent from the column map
- an arity mismatch on any supported operator
- a `timestamp` / `duration` argument that is not a string literal, or a
  string that does not parse

Macros (`exists`, `all`, `map`, `filter`) desugar to `Comprehension` and are
therefore rejected by the first rule. That is the intended outcome — they have
no Postgres translation here — but the error message should say so by name
rather than saying "unsupported expression kind", because the client wrote
`tags.exists(...)` and will not recognise itself in the desugared form.

## Identifiers

An `Ident`, or a `Select` chain over one, reconstructs to a dotted path:
`Select { operand: Ident("author"), field: "name" }` → `author.name`. This is
`identPath` in `transpiler.go:138`. Anything else in operand position is not a
path and is an error.

The path is then looked up in the column map. **Lookup is fail-closed**: a
path that is absent is an error, and a transpiler built with an empty map
therefore rejects every expression. This is the only gate on what a client may
filter by, because the generated CEL environment declares every field of the
resource — see [columns.md](columns.md).

The mapped value is emitted into SQL after identifier quoting only. Never
accept a column name from request data.

### Quoting

Quote each dot-separated segment of the mapped column independently, doubling
any embedded `"`, matching `quote_ident` semantics. This is `Dialect::quote_ident`,
which MySQL overrides with backticks:

```
volumes.read_count  →  "volumes"."read_count"
we"ird              →  "we""ird"
```

pgxaip's `identifier.go` is the whole implementation; it is 8 lines.

## Parameter numbering

Placeholders are `$1`-based and assigned in walk order. The transpiler takes a
starting offset so a fragment can be spliced into a query that already has
bound values:

```rust
pub struct Options { pub param_offset: usize }  // default 1
```

`sqlx-aip` uses this to number cursor placeholders after the filter's, and any
caller with a hand-written `WHERE tenant_id = $1` prefix uses it too.

Postgres numbering is what sqlx uses for `Postgres` as well, so the fragment
drops in unchanged. Dialects with positional `?` ignore the offset — there is no
number to shift — but bind order still matters.

## No type checker

`Program::compile` is parse-only — it calls `Parser::parse` and stops
(`cel-0.14.4/src/lib.rs:182`). There is no cel-go equivalent of
`env.Compile`'s checking phase in cel-rust at all.

pgxcel requires `ast.IsChecked()` but never reads the type map or the
reference map; it walks the expression structurally. So this crate loses no
*capability* by walking an unchecked AST. What it loses is the diagnosis:
`title == 3` is rejected by cel-go's checker at the RPC boundary, and here it
transpiles cleanly to `"title" = $1` and fails at Postgres with SQLSTATE
42883.

Do not try to reconstruct a type checker. Do surface the Postgres error
faithfully rather than wrapping it in something vaguer — it is the only
diagnosis available.

## What actually differs from pgxcel

1. **No `-_` handling.** cel-rust folds unary minus into the literal at parse
   time, so `transpileUnaryMinus` (`transpiler.go:389`) has no counterpart.
2. **No function alias map.** `WithFunctions` existed to accept einride's
   `"="` / `"AND"` naming. einride was dropped from the Go side in `940a89d`,
   and cel-rust emits cel-go's names natively. Do not port the option.
3. **`null` is expressible.** `LiteralValue::Null` has no cel-go `Constant`
   counterpart that pgxcel handles — `transpileConst` (`transpiler.go:165`)
   errors on it. Here, translate `x == null` → `x IS NULL` and `x != null` →
   `x IS NOT NULL`, and reject `null` in any other position. Binding a NULL as
   a parameter is not equivalent: `col = $1` with a NULL bind is NULL, not
   true.
4. **Bind values are an enum, not `any`.** See [values.md](values.md).

## What changed in the implementation

1. **Not Postgres-only.** Placeholder syntax, identifier quoting, `LIKE`
   concatenation and the regex operator go through a `Dialect` trait, with
   `Postgres`, `Sqlite` and `MySql` shipped. The AST walk itself is unchanged
   and driver-neutral.
2. **`has()` needs its own rejection.** It does *not* desugar to a
   comprehension — cel-rust turns `has(a.b)` into `Select { test: true }`, so it
   arrives in identifier position and is rejected there.
3. **`map` and `filter` are reported together.** Both desugar to the same
   empty-list accumulator, and a three-argument `map` is shaped exactly like a
   `filter`, so the error names them as `map/filter` rather than guessing.
4. **`x in []` rolls back its left-hand side.** pgxcel transpiles the left side
   before discovering the list is empty, then discards the SQL — leaving any
   value it bound orphaned in the args list. Here the bindings are truncated
   along with the discarded SQL.

## Resolved from "open for the implementer"

- `transpile` takes `impl Into<Columns<'_>>`, which covers `&[(&str, &str)]` and
  `&[(&str, &str); N]`. The slice was the right answer.
- `Options` is a plain struct with a hand-written `Default` (`param_offset: 1`).
  It is deliberately *not* `#[non_exhaustive]`, because that would forbid the
  `..Default::default()` that makes it worth being a struct.
- One error enum, `#[non_exhaustive]`, hand-written `Display` and `Error`, no
  `thiserror`.
