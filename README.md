# sqlx-cel

> Transpile a CEL expression into a SQL `WHERE` fragment with bind values —
> Postgres, SQLite and MySQL, behind a fail-closed column allow-list.

[![CI](https://github.com/sqlx-contrib/sqlx-cel/actions/workflows/ci.yml/badge.svg)](https://github.com/sqlx-contrib/sqlx-cel/actions/workflows/ci.yml)
[![Crate](https://img.shields.io/crates/v/sqlx-cel)](https://crates.io/crates/sqlx-cel)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Transpiles a [CEL](https://cel.dev) expression into a SQL `WHERE` fragment
with bind values, for [sqlx](https://github.com/launchbadge/sqlx).

The expression is plain CEL, not the [AIP-160](https://google.aip.dev/160)
grammar, so it works for any caller with a CEL expression and a database
table — AIP is one such caller, not a requirement.

The Rust counterpart of [pgxcel](https://github.com/pgx-contrib/pgxcel), and
the filter half of [sqlx-aip](https://github.com/sqlx-contrib/sqlx-aip).

## Status

Rewritten and working. The whole crate is one module, `src/filter.rs`; the
rationale lives with the code — `cargo doc --open` — rather than here.

```rust
let users = Table::new()
    .column("age", ColumnType::Int)
    .aliased("createdAt", "created_at", ColumnType::Timestamp);

let filter = Filter::compile("age > 21 && createdAt > timestamp('2024-01-01T00:00:00Z')")?;

let mut query = QueryBuilder::<Postgres>::new("SELECT * FROM users WHERE ");
filter.push_to(&users, &mut query)?;
```

Prior attempts are on branches: `main` for the first cut, and
`filter-predicate-refactor` for a second. Neither is a target to reproduce.

## Development

sqlx 0.9 declares `rust-version = "1.94"`, so this crate does too.
`rust-toolchain.toml` pins the dev toolchain to 1.95.0, so plain `cargo` picks
the right one even when the machine's default stable is older than the MSRV.

```sh
cargo test --features sqlite,mysql
cargo clippy --all-targets --features sqlite,mysql
```

`clippy::all` and `clippy::pedantic` are denied rather than warned, because
several consumers in this ecosystem deny pedantic at the workspace level: a
lint this crate tolerates is one they cannot.

There is a Nix flake and a devcontainer for a batteries-included shell — the
pinned toolchain plus the `sqlite3` CLI:

```sh
nix develop
```

The devcontainer installs Nix and does the same thing, so "Reopen in Container"
lands in the same environment. The flake exposes only a dev shell: this is a
library crate with no binary, and `buildRustPackage` would want a committed
`Cargo.lock`, which a library deliberately does not have.

## License

[MIT](LICENSE)
