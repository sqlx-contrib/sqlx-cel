//! End-to-end tests against a real in-memory SQLite database.
//!
//! The unit tests assert the SQL text and the value list. These assert the part
//! that text cannot: that the fragment parses, that the placeholders line up
//! with the values, and that every `Value` variant actually encodes.

#![cfg(feature = "sqlite")]

use sqlx::{AssertSqlSafe, Row, SqlitePool};
use sqlx_cel::{BindAll, dialect};

const COLUMNS: &[(&str, &str)] = &[
    ("title", "volumes.title"),
    ("read_count", "volumes.read_count"),
    ("published", "volumes.published"),
    ("rating", "volumes.rating"),
    ("cover", "volumes.cover"),
];

async fn seeded_pool() -> SqlitePool {
    let pool = SqlitePool::connect(":memory:").await.unwrap();
    sqlx::query(
        "CREATE TABLE volumes (
            title      TEXT    NOT NULL,
            read_count INTEGER NOT NULL,
            published  BOOLEAN NOT NULL,
            rating     REAL,
            cover      BLOB
        )",
    )
    .execute(&pool)
    .await
    .unwrap();

    for (title, read_count, published, rating, cover) in [
        ("demo", 10i64, true, Some(4.5f64), Some(vec![1u8, 2, 3])),
        ("demo two", 2, false, Some(1.0), None),
        ("other", 7, true, None, None),
    ] {
        sqlx::query("INSERT INTO volumes VALUES (?, ?, ?, ?, ?)")
            .bind(title)
            .bind(read_count)
            .bind(published)
            .bind(rating)
            .bind(cover)
            .execute(&pool)
            .await
            .unwrap();
    }
    pool
}

/// Runs `filter` against the seeded table and returns the matching titles.
async fn titles(pool: &SqlitePool, filter: &str) -> Vec<String> {
    let program = cel::Program::compile(filter).expect("filter must parse");
    let (where_sql, values) = sqlx_cel::transpile(program.expression(), COLUMNS, dialect::Sqlite)
        .expect("must transpile");

    let sql = format!("SELECT title FROM volumes WHERE {where_sql} ORDER BY title");
    sqlx::query(AssertSqlSafe(sql))
        .bind_all(values)
        .fetch_all(pool)
        .await
        .expect("query must execute")
        .into_iter()
        .map(|row| row.get::<String, _>("title"))
        .collect()
}

#[tokio::test]
async fn filters_end_to_end() {
    let pool = seeded_pool().await;

    for (filter, expected) in [
        (r#"title == "demo""#, vec!["demo"]),
        ("read_count > 5", vec!["demo", "other"]),
        ("read_count >= 7 && published", vec!["demo", "other"]),
        (r#"title.startsWith("demo")"#, vec!["demo", "demo two"]),
        (r#"title.contains("emo")"#, vec!["demo", "demo two"]),
        (r#"title.endsWith("two")"#, vec!["demo two"]),
        (r#"title in ["demo", "other"]"#, vec!["demo", "other"]),
        ("title in []", vec![]),
        ("rating == null", vec!["other"]),
        ("rating != null", vec!["demo", "demo two"]),
        ("!published", vec!["demo two"]),
        ("rating > 2.0", vec!["demo"]),
        (r#"cover == b"\x01\x02\x03""#, vec!["demo"]),
        (
            r#"title == "demo" || (read_count < 5 && !published)"#,
            vec!["demo", "demo two"],
        ),
    ] {
        assert_eq!(titles(&pool, filter).await, expected, "for filter {filter}");
    }
}

/// The whole point of `param_offset`: a fragment spliced after placeholders the
/// caller wrote by hand. SQLite is positional, so what matters is bind order.
#[tokio::test]
async fn splices_after_a_hand_written_prefix() {
    let pool = seeded_pool().await;

    let program = cel::Program::compile("read_count > 1").unwrap();
    let (where_sql, values) =
        sqlx_cel::transpile(program.expression(), COLUMNS, dialect::Sqlite).unwrap();

    let sql =
        format!("SELECT title FROM volumes WHERE published = ? AND {where_sql} ORDER BY title");
    let rows: Vec<String> = sqlx::query(AssertSqlSafe(sql))
        .bind(true)
        .bind_all(values)
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get("title"))
        .collect();

    assert_eq!(rows, vec!["demo", "other"]);
}

/// An unmapped path must never reach SQL, even though the CEL parses fine.
#[tokio::test]
async fn an_unmapped_column_never_reaches_the_database() {
    let program = cel::Program::compile(r#"internal_notes == "secret""#).unwrap();
    let error = sqlx_cel::transpile(program.expression(), COLUMNS, dialect::Sqlite).unwrap_err();
    assert_eq!(
        error,
        sqlx_cel::Error::UnknownField {
            path: "internal_notes".into()
        },
    );
}
