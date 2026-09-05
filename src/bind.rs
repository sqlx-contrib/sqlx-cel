use sqlx::query::{Query, QueryAs, QueryScalar};
use sqlx::{Database, Encode, Type};

use crate::value::Value;

/// Binds a whole [`Vec<Value>`](Value) to a query in one call.
///
/// [`transpile`](crate::transpile) hands back the values in placeholder order,
/// so binding them is a fold over `bind`. This is that fold, and nothing more.
///
// The example is Postgres-shaped, so it is only compiled when that driver is
// available. It still type-checks under the default features and in CI.
#[cfg_attr(feature = "postgres", doc = "```no_run")]
#[cfg_attr(not(feature = "postgres"), doc = "```ignore")]
/// # async fn example(pool: sqlx::PgPool) -> Result<(), Box<dyn std::error::Error>> {
/// use sqlx::AssertSqlSafe;
/// use sqlx_cel::{BindAll, dialect};
///
/// let program = cel::Program::compile(r#"title == "demo""#)?;
/// let filter = sqlx_cel::transpile(
///     program.expression(),
///     &[("title", "volumes.title")],
///     dialect::Postgres,
/// )?;
///
/// // The filter's placeholders are $1..$N, so the limit's follows them.
/// let limit = filter.values.len() + 1;
/// let sql = format!("SELECT title FROM volumes WHERE {} LIMIT ${limit}", filter.sql);
///
/// let titles: Vec<String> = sqlx::query_scalar(AssertSqlSafe(sql))
///     .bind_all(filter.values)
///     .bind(50i64)
///     .fetch_all(&pool)
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// # `AssertSqlSafe` is not optional
///
/// sqlx 0.9's `query`/`query_as` take `impl SqlSafeStr`, which is implemented
/// for `&'static str` and *not* for `&str` or `String`. Runtime-assembled SQL —
/// the entire point of this crate — has to be wrapped. Without the wrap the
/// error reads as though a `String` should have coerced.
///
/// The assertion is honest here. The only caller-influenced text in the
/// fragment is column names, and those come from a fail-closed map, never from
/// request data; everything out of the expression itself is a bind value. What
/// you must not do is interpolate anything else into `sql` yourself.
///
/// Implemented for every sqlx query type and every database this crate can
/// encode [`Value`] for.
pub trait BindAll: Sized {
    /// Binds each value in turn, in order.
    #[must_use]
    fn bind_all<I: IntoIterator<Item = Value>>(self, values: I) -> Self;
}

impl<DB> BindAll for Query<'_, DB, DB::Arguments>
where
    DB: Database,
    Value: for<'a> Encode<'a, DB> + Type<DB>,
{
    fn bind_all<I: IntoIterator<Item = Value>>(self, values: I) -> Self {
        values.into_iter().fold(self, Query::bind)
    }
}

impl<DB, O> BindAll for QueryAs<'_, DB, O, DB::Arguments>
where
    DB: Database,
    Value: for<'a> Encode<'a, DB> + Type<DB>,
{
    fn bind_all<I: IntoIterator<Item = Value>>(self, values: I) -> Self {
        values.into_iter().fold(self, QueryAs::bind)
    }
}

impl<DB, O> BindAll for QueryScalar<'_, DB, O, DB::Arguments>
where
    DB: Database,
    Value: for<'a> Encode<'a, DB> + Type<DB>,
{
    fn bind_all<I: IntoIterator<Item = Value>>(self, values: I) -> Self {
        values.into_iter().fold(self, QueryScalar::bind)
    }
}
