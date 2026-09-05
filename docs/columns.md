# The column map

> **Status: implemented as specified.** `Columns<'a>` wraps
> `&'a [(&'a str, &'a str)]` and scans it linearly, and `transpile` takes
> `impl Into<Columns<'_>>`.

The AIP-path → database-column allow-list. It is the security boundary of both
crates, so it gets its own document.

## Why it is the boundary

`protoc-gen-rust-aip` declares **every** field of the resource that has a CEL
type, in `QUERY_FIELDS`. Its own documentation is explicit that this is not an
authorization decision:

> Which of them a client may *actually* query is decided by the AIP-path to
> database-column map at the query layer, which is fail-closed; this is only
> what parses.
>
> — `protoc-gen-rust-aip/src/emit/query.rs:162`

So `parse_filter` accepts `internal_notes == "x"` if the proto declares the
field. The column map is what stops it reaching SQL. A miss must be an error,
never a skipped predicate and never a passthrough of the path as a column
name.

## Shape

Go uses `map[string]string`. Don't reflexively port that to `HashMap`.

The maps are small — a resource has perhaps ten queryable fields — and they
are known at compile time. `&'static [(&'static str, &'static str)]` with a
linear scan beats hashing at that size, costs no allocation, and is a
`const`-constructible literal, which matters because the natural place for it
is generated code or a `const` next to the query.

```rust
const VOLUME_COLUMNS: &[(&str, &str)] = &[
    ("name",        "volumes.name"),
    ("title",       "volumes.title"),
    ("read_count",  "volumes.read_count"),
    ("create_time", "volumes.created_at"),
];
```

Accept `impl Into<Columns<'_>>` over both the slice and a `HashMap` if it
proves worth it, but design for the slice.

Note the last entry: the AIP path and the column name differ. This is the
normal case, not the exception, and it is why the map exists rather than a
bare `HashSet` of allowed paths.

## One map, not two

`protoc-gen-go-aip-query` emits separate `*FilterColumns` and
`*OrderByColumns`, and pgxaip's README tells the caller to union them,
observing that the same path maps to the same column by construction so the
union is safe.

Skip that. Take one map. The union was an artifact of Go codegen emitting two,
and a caller who genuinely wants a field filterable but not orderable is
better served by an explicit second map than by a convention.

## The codegen gap

Go gets these maps from
[protoc-gen-go-aip-query](https://github.com/protoc-contrib/protoc-gen-go-aip-query).
`protoc-gen-rust-aip` **does not emit a column map** — it emits `QUERY_FIELDS`
and stops, deliberately, because the mapping is a database concern that the
proto does not carry.

Until that changes, the map is hand-written. That is fine and arguably better:
it is the one place where a human decides what is exposed, and it is short.

If it does change, the natural shape is a sibling constant driven by a field
option:

```rust
impl ListVolumesRequest {
    pub const QUERY_FIELDS: &'static [&'static str] = &[/* … */];
    pub const QUERY_COLUMNS: &'static [(&'static str, &'static str)] = &[/* … */];
}
```

which is why the slice-of-pairs shape above is the one to design for. Neither
crate should depend on that happening.

## Nested paths

`author.name` is a valid path and quotes to `"author"."name"` only if the map
sends it somewhere sensible. Two cases the implementer must decide and
document:

- **Joined column.** `("author.name", "authors.name")` — works today, given
  the caller's SQL joins `authors`.
- **JSONB.** `("author.name", …)` has no answer, because the quoting rule
  produces `"author"."name"`, not `"author" -> 'name'`. Out of scope for v1;
  say so rather than half-supporting it. Documented as such on `Columns`.
