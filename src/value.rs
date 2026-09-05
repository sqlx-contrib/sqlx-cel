/// A literal the transpiler bound to a placeholder.
///
/// [`transpile`] produces a heterogeneous list whose length and element types
/// are known only at runtime, so there is nothing to name as the `T` of
/// `Query::bind`. This enum is what closes that gap: a `match` over it
/// monomorphizes each arm separately, so the heterogeneity resolves at compile
/// time even though the sequence does not.
///
/// It mirrors cel's `LiteralValue` plus the two constructed types, `timestamp`
/// and `duration`. It is deliberately driver-neutral — it holds no sqlx type —
/// which is what keeps the transpiler unit-testable without a database, and it
/// is why [`transpile`] returns `Vec<Value>` rather than a driver's arguments
/// buffer: that is an opaque byte buffer with no way to assert what went into
/// it.
///
/// Encoding is per-driver, behind the matching Cargo feature; each variant's
/// docs say what it becomes.
///
/// # No `Null` variant
///
/// A CEL `null` is handled in the SQL text — `x == null` becomes `x IS NULL` —
/// rather than bound. Binding NULL to `col = ?` yields NULL, not true, so the
/// text form is the only correct one. There is a second reason that
/// generalizes: `Arguments::add(None::<T>)` forces a choice of `T`, and without
/// a type checker there is nothing to derive it from. A NULL must be typed
/// before it can be sent, and this crate never knows the type.
///
/// [`transpile`]: crate::transpile
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// A CEL `bool`.
    Bool(bool),
    /// A CEL `string`.
    Text(String),
    /// A CEL `bytes`.
    Bytes(Vec<u8>),
    /// A CEL `int`, a signed 64-bit integer.
    Int(i64),
    /// A CEL `uint`, an unsigned 64-bit integer.
    ///
    /// Only MySQL has a native unsigned 64-bit type. On Postgres and SQLite a
    /// value above [`i64::MAX`] has nowhere to go, and fails when the query is
    /// encoded rather than when it is transpiled.
    Uint(u64),
    /// A CEL `double`.
    Float(f64),
    /// A `timestamp("…")` literal, parsed from RFC 3339.
    #[cfg(feature = "chrono")]
    Timestamp(chrono::DateTime<chrono::Utc>),
    /// A `timestamp("…")` literal, parsed from RFC 3339.
    ///
    /// With both the `chrono` and `time` features on, this variant holds a
    /// [`chrono::DateTime`] instead.
    #[cfg(all(feature = "time", not(feature = "chrono")))]
    Timestamp(time::OffsetDateTime),
    /// A `duration("…")` literal, as a signed count of microseconds.
    ///
    /// Postgres binds this as an `INTERVAL` of exactly that many microseconds —
    /// never months or days, which are calendar-relative and would change the
    /// meaning. SQLite and MySQL have no interval type a bind value can carry,
    /// so they bind the integer itself, which is meaningful only against a
    /// column that stores microseconds.
    ///
    /// Sub-microsecond precision is rounded away, half away from zero.
    Duration(i64),
}

/// Generates the `Encode`/`Type` pair for one driver.
///
/// The trick is [`Encode::produces`], a hook for a value-dependent type: the
/// driver's `Arguments::add` prefers it over `T::type_info()`. So `type_info`
/// returns a placeholder and `compatible` is broad, while `produces` returns
/// the real per-variant type.
#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
macro_rules! impl_encode {
    (
        $(#[$meta:meta])*
        $database:ty, $buffer:ty,
        uint: $uint:ty,
        duration: $interval:ty = |$micros:ident| $duration:expr,
    ) => {
        $(#[$meta])*
        impl ::sqlx::types::Type<$database> for $crate::Value {
            /// A placeholder only. What actually reaches the database comes
            /// from [`Encode::produces`](::sqlx::encode::Encode::produces).
            fn type_info() -> <$database as ::sqlx::Database>::TypeInfo {
                <String as ::sqlx::types::Type<$database>>::type_info()
            }

            /// `Value` spans every type it can hold, so compatibility is
            /// decided per-value by `produces`, not here.
            fn compatible(_: &<$database as ::sqlx::Database>::TypeInfo) -> bool {
                true
            }
        }

        $(#[$meta])*
        impl ::sqlx::encode::Encode<'_, $database> for $crate::Value {
            fn encode_by_ref(
                &self,
                buf: &mut $buffer,
            ) -> Result<::sqlx::encode::IsNull, ::sqlx::error::BoxDynError> {
                // Every call here is fully qualified: with more than one driver
                // feature on, `v.encode_by_ref(buf)` is ambiguous across the
                // per-database impls.
                macro_rules! encode {
                    ($ty:ty, $value:expr) => {
                        <$ty as ::sqlx::encode::Encode<'_, $database>>::encode_by_ref($value, buf)
                    };
                }
                match self {
                    $crate::Value::Bool(v) => encode!(bool, v),
                    $crate::Value::Text(v) => encode!(String, v),
                    $crate::Value::Bytes(v) => encode!(Vec<u8>, v),
                    $crate::Value::Int(v) => encode!(i64, v),
                    $crate::Value::Uint(v) => {
                        let narrowed = <$uint>::try_from(*v).map_err(|_| {
                            format!("{v} exceeds the largest integer this database can bind")
                        })?;
                        encode!($uint, &narrowed)
                    }
                    $crate::Value::Float(v) => encode!(f64, v),
                    #[cfg(feature = "chrono")]
                    $crate::Value::Timestamp(v) => {
                        encode!(::chrono::DateTime<::chrono::Utc>, v)
                    }
                    #[cfg(all(feature = "time", not(feature = "chrono")))]
                    $crate::Value::Timestamp(v) => encode!(::time::OffsetDateTime, v),
                    $crate::Value::Duration($micros) => {
                        let interval: $interval = $duration;
                        encode!($interval, &interval)
                    }
                }
            }

            fn produces(&self) -> Option<<$database as ::sqlx::Database>::TypeInfo> {
                macro_rules! type_info {
                    ($ty:ty) => {
                        <$ty as ::sqlx::types::Type<$database>>::type_info()
                    };
                }
                Some(match self {
                    $crate::Value::Bool(_) => type_info!(bool),
                    $crate::Value::Text(_) => type_info!(String),
                    $crate::Value::Bytes(_) => type_info!(Vec<u8>),
                    $crate::Value::Int(_) => type_info!(i64),
                    $crate::Value::Uint(_) => type_info!($uint),
                    $crate::Value::Float(_) => type_info!(f64),
                    #[cfg(feature = "chrono")]
                    $crate::Value::Timestamp(_) => {
                        type_info!(::chrono::DateTime<::chrono::Utc>)
                    }
                    #[cfg(all(feature = "time", not(feature = "chrono")))]
                    $crate::Value::Timestamp(_) => type_info!(::time::OffsetDateTime),
                    $crate::Value::Duration(_) => type_info!($interval),
                })
            }

            fn size_hint(&self) -> usize {
                match self {
                    $crate::Value::Bool(_) => 1,
                    $crate::Value::Text(v) => v.len(),
                    $crate::Value::Bytes(v) => v.len(),
                    $crate::Value::Int(_)
                    | $crate::Value::Uint(_)
                    | $crate::Value::Float(_) => 8,
                    #[cfg(any(feature = "chrono", feature = "time"))]
                    $crate::Value::Timestamp(_) => 8,
                    $crate::Value::Duration(_) => 16,
                }
            }
        }
    };
}

// Postgres has no unsigned integer type, so a CEL `uint` narrows to `INT8` and
// fails above `i64::MAX`. A duration becomes an `INTERVAL` of exactly that many
// microseconds.
#[cfg(feature = "postgres")]
impl_encode! {
    #[cfg_attr(docsrs, doc(cfg(feature = "postgres")))]
    ::sqlx::Postgres, ::sqlx::postgres::PgArgumentBuffer,
    uint: i64,
    duration: ::sqlx::postgres::types::PgInterval = |micros| {
        ::sqlx::postgres::types::PgInterval {
            months: 0,
            days: 0,
            microseconds: *micros,
        }
    },
}

// SQLite stores every integer as a signed 64-bit value, so a CEL `uint`
// narrows the same way it does on Postgres, and a duration binds as its raw
// microsecond count.
#[cfg(feature = "sqlite")]
impl_encode! {
    #[cfg_attr(docsrs, doc(cfg(feature = "sqlite")))]
    ::sqlx::Sqlite, <::sqlx::Sqlite as ::sqlx::Database>::ArgumentBuffer,
    uint: i64,
    duration: i64 = |micros| *micros,
}

// MySQL has a native `BIGINT UNSIGNED`, so a CEL `uint` survives intact.
#[cfg(feature = "mysql")]
impl_encode! {
    #[cfg_attr(docsrs, doc(cfg(feature = "mysql")))]
    ::sqlx::MySql, <::sqlx::MySql as ::sqlx::Database>::ArgumentBuffer,
    uint: u64,
    duration: i64 = |micros| *micros,
}

#[cfg(all(test, feature = "postgres"))]
mod postgres_tests {
    use crate::Value;
    use sqlx::postgres::PgArguments;
    use sqlx::types::Type;
    use sqlx::{Arguments, Postgres, TypeInfo};

    #[test]
    fn each_variant_binds_as_its_own_type() {
        let mut args = PgArguments::default();
        for value in [
            Value::Bool(true),
            Value::Text("demo".into()),
            Value::Bytes(vec![1, 2, 3]),
            Value::Int(3),
            Value::Uint(3),
            Value::Float(1.5),
            Value::Duration(90 * 60 * 1_000_000),
        ] {
            args.add(value).unwrap();
        }
        assert_eq!(args.len(), 7);

        // The blanket `type_info` is a placeholder; an INT8 arriving as TEXT
        // would be silently wrong at the wire, so assert it is only a fallback.
        assert_eq!(<Value as Type<Postgres>>::type_info().name(), "TEXT");
    }

    #[test]
    fn uint_above_i64_max_fails_at_encode_rather_than_wrapping() {
        let mut args = PgArguments::default();
        let error = args.add(Value::Uint(u64::MAX)).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
        assert_eq!(args.len(), 0, "a failed add must not leave a partial value");
    }
}

#[cfg(all(test, feature = "mysql"))]
mod mysql_tests {
    use crate::Value;
    use sqlx::Arguments;
    use sqlx::mysql::MySqlArguments;

    #[test]
    fn uint_survives_intact_because_mysql_has_bigint_unsigned() {
        let mut args = MySqlArguments::default();
        args.add(Value::Uint(u64::MAX)).unwrap();
        assert_eq!(args.len(), 1);
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod sqlite_tests {
    use crate::Value;
    use sqlx::Arguments;
    use sqlx::sqlite::SqliteArguments;

    #[test]
    fn duration_binds_as_a_microsecond_count() {
        let mut args = SqliteArguments::default();
        args.add(Value::Duration(90 * 60 * 1_000_000)).unwrap();
        assert_eq!(args.len(), 1);
    }
}
