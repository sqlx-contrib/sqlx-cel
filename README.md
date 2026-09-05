# sqlx-cel

Transpiles a [CEL](https://cel.dev) expression into a Postgres `WHERE`
fragment with positional bind values, for [sqlx](https://github.com/launchbadge/sqlx).

The Rust counterpart of [pgxcel](https://github.com/pgx-contrib/pgxcel), and
the filter half of [sqlx-aip](https://github.com/sqlx-contrib/sqlx-aip).

```rust
let program = cel::Program::compile(r#"title == "demo" && read_count > 3"#)?;

let (sql, values) = sqlx_cel::transpile(program.expression(), VOLUME_COLUMNS)?;
// sql:    ("volumes"."title" = $1 AND "volumes"."read_count" > $2)
// values: [Value::Text("demo".into()), Value::Int(3)]

let volumes = sqlx::query_as::<_, Volume>(AssertSqlSafe(
        format!("SELECT * FROM volumes WHERE {sql}")))
    .bind_all(values)
    .fetch_all(&pool)
    .await?;
```

The expression is plain CEL, not the [AIP-160](https://google.aip.dev/160)
grammar, so it works for any caller with a CEL expression and a Postgres
table — AIP is one such caller, not a requirement.

## Status

**Not implemented.** This repository currently holds the design only. The
specification in `docs/` is complete enough to implement against, and every
external API it cites was verified against the published sources rather than
recalled.

| Document | What it settles |
| --- | --- |
| [docs/transpiler.md](docs/transpiler.md) | The AST walk, operator coverage, what is rejected, quoting, parameter numbering |
| [docs/values.md](docs/values.md) | The `Value` enum and how it reaches sqlx, time types, nulls, version floor |
| [docs/columns.md](docs/columns.md) | The fail-closed path → column allow-list, and why it is the security boundary |

Start with `docs/transpiler.md`. Read
[pgxcel](https://github.com/pgx-contrib/pgxcel)'s `transpiler.go` alongside
it — the port is close to a transcription, and `transpiler_test.go` gives the
assertion pairs for free.

## Scope

**In.** Comparison, boolean, `in`, the four string matchers, `timestamp` and
`duration` literals, nested field paths, column-to-column comparison.

**Out.** Arithmetic, ternary, indexing, `size`, `has`, and the comprehension
macros (`exists`, `all`, `map`, `filter`) — all rejected with an error, never
silently dropped. Query execution, `SELECT` generation, migrations. Databases
other than Postgres: the `$N` placeholders, `quote_ident` quoting, `~` regex
and `PgInterval` are all Postgres-shaped, and going generic over
`sqlx::Database` would buy little for what it costs.

## License

[MIT](LICENSE)
