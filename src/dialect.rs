//! The SQL flavour to emit.
//!
//! The AST walk is driver-neutral; everything that differs between databases —
//! placeholder syntax, identifier quoting, string concatenation, the regex
//! operator — comes through this trait.
//!
//! These types describe *SQL text* only. They are always available, with no
//! Cargo feature and no sqlx dependency, so this crate can be used as a plain
//! CEL-to-SQL transpiler. Binding the values it produces is separate: that
//! needs the matching driver feature. See [`Value`](crate::Value).

/// The SQL flavour [`transpile`](crate::transpile) emits.
///
/// Implement this for a database that is not one of the three built in, or to
/// override a detail of one that is.
pub trait Dialect {
    /// The dialect's name, used in error messages.
    fn name(&self) -> &'static str;

    /// The placeholder for the `index`-th bind value, 1-based and already
    /// offset by [`Options::param_offset`](crate::Options).
    ///
    /// Dialects that use a positional `?` ignore `index` — but the values are
    /// still emitted in the order they must be bound.
    fn placeholder(&self, index: usize) -> String;

    /// Whether every placeholder renders alike, so that a bind cannot be
    /// referenced twice.
    ///
    /// This crate never needs it: [`transpile`](crate::transpile) binds each
    /// literal once and never points at an earlier one. It matters to callers
    /// that splice a fragment referencing a bind more than once — a key-set
    /// cursor predicate pins each more-significant column in every clause after
    /// the first:
    ///
    /// ```sql
    /// -- numbered: $1 is bound once, referenced twice
    /// ("title" > $1) OR ("title" = $1 AND "id" > $2)
    /// -- positional: each ? consumes its own bind, so the value list repeats
    /// ("title" > ?)  OR ("title" = ?  AND "id" > ?)
    /// ```
    ///
    /// The default asks [`placeholder`](Dialect::placeholder) to render two
    /// adjacent indices and compares them, which is right for any dialect that
    /// distinguishes parameters at all. That includes the awkward middle case:
    /// a dialect emitting SQLite's numbered `?1` / `?2` form is positional in
    /// syntax but still addressable, renders the two differently, and is
    /// correctly reported as *not* positional.
    ///
    /// Override it to answer without rendering anything.
    fn is_positional(&self) -> bool {
        self.placeholder(1) == self.placeholder(2)
    }

    /// Quotes a possibly-dotted column path, one segment at a time.
    ///
    /// Defaults to ANSI double quotes with embedded `"` doubled, which is what
    /// Postgres's `quote_ident` and SQLite both use:
    ///
    /// ```text
    /// volumes.read_count  →  "volumes"."read_count"
    /// we"ird              →  "we""ird"
    /// ```
    fn quote_ident(&self, column: &str) -> String {
        quote_delimited(column, '"')
    }

    /// Renders `s.contains(x)`, `s.startsWith(x)` and `s.endsWith(x)` as a
    /// `LIKE` pattern, with `%` on the requested sides.
    ///
    /// Defaults to SQL-standard `||` concatenation.
    ///
    /// Note that `LIKE` is case-*sensitive* in Postgres and SQLite (for ASCII),
    /// matching CEL's own `contains`, but case-*insensitive* in MySQL under the
    /// usual `_ci` collations. That is a property of the column's collation
    /// rather than of this fragment, so it is left alone rather than papered
    /// over with a cast.
    fn like(&self, lhs: &str, rhs: &str, leading: bool, trailing: bool) -> String {
        let mut sql = format!("{lhs} LIKE ");
        if leading {
            sql.push_str("'%' || ");
        }
        sql.push_str(rhs);
        if trailing {
            sql.push_str(" || '%'");
        }
        sql
    }

    /// Renders `s.matches(re)`, or `None` if the dialect has no regex operator —
    /// in which case `matches` is rejected with
    /// [`Error::UnsupportedByDialect`](crate::Error::UnsupportedByDialect).
    fn regex(&self, lhs: &str, rhs: &str) -> Option<String>;
}

impl<D: Dialect + ?Sized> Dialect for &D {
    fn name(&self) -> &'static str {
        (**self).name()
    }
    fn placeholder(&self, index: usize) -> String {
        (**self).placeholder(index)
    }
    fn is_positional(&self) -> bool {
        (**self).is_positional()
    }
    fn quote_ident(&self, column: &str) -> String {
        (**self).quote_ident(column)
    }
    fn like(&self, lhs: &str, rhs: &str, leading: bool, trailing: bool) -> String {
        (**self).like(lhs, rhs, leading, trailing)
    }
    fn regex(&self, lhs: &str, rhs: &str) -> Option<String> {
        (**self).regex(lhs, rhs)
    }
}

/// PostgreSQL: `$1` placeholders, `"ident"` quoting, `~` for regex.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Postgres;

impl Dialect for Postgres {
    fn name(&self) -> &'static str {
        "postgres"
    }

    fn placeholder(&self, index: usize) -> String {
        format!("${index}")
    }

    /// `$1` names a parameter, so one bind can be referenced from several
    /// places in the same statement.
    fn is_positional(&self) -> bool {
        false
    }

    /// POSIX regex. CEL's `matches` is RE2 and Postgres's `~` is POSIX ERE;
    /// they agree on common syntax and diverge at the edges, so a pattern that
    /// leans on RE2 specifics may behave differently or raise a Postgres error.
    fn regex(&self, lhs: &str, rhs: &str) -> Option<String> {
        Some(format!("{lhs} ~ {rhs}"))
    }
}

/// SQLite: `?` placeholders, `"ident"` quoting, `REGEXP` for regex.
///
/// Two things to know:
///
/// - SQLite has no interval type. A `duration("…")` literal binds as an integer
///   count of microseconds, which is meaningful only against a column that
///   stores one.
/// - `REGEXP` is *not* built in. SQLite parses it but resolves it to a
///   user-defined function that the application must register, so
///   `title.matches("^a")` fails at execution with `no such function: REGEXP`
///   unless one is installed. That is left as a loud runtime error rather than
///   a transpile-time rejection, so callers who do register one can use it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sqlite;

impl Dialect for Sqlite {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    fn placeholder(&self, _index: usize) -> String {
        "?".to_string()
    }

    /// SQLite also accepts a numbered `?NNN`, which *is* addressable, but this
    /// dialect emits the anonymous form.
    fn is_positional(&self) -> bool {
        true
    }

    fn regex(&self, lhs: &str, rhs: &str) -> Option<String> {
        Some(format!("{lhs} REGEXP {rhs}"))
    }
}

/// MySQL and MariaDB: `?` placeholders, `` `ident` `` quoting, `CONCAT` rather
/// than `||`, `REGEXP` for regex.
///
/// MySQL has no interval type that a bind value can carry, so a `duration("…")`
/// literal binds as an integer count of microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MySql;

impl Dialect for MySql {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn placeholder(&self, _index: usize) -> String {
        "?".to_string()
    }

    fn is_positional(&self) -> bool {
        true
    }

    /// Backticks, because `"…"` is a string literal in MySQL's default
    /// `ANSI_QUOTES`-off mode. An embedded backtick is doubled.
    fn quote_ident(&self, column: &str) -> String {
        quote_delimited(column, '`')
    }

    /// `||` is logical OR in MySQL unless `PIPES_AS_CONCAT` is set, so string
    /// concatenation has to go through `CONCAT`.
    fn like(&self, lhs: &str, rhs: &str, leading: bool, trailing: bool) -> String {
        match (leading, trailing) {
            (true, true) => format!("{lhs} LIKE CONCAT('%', {rhs}, '%')"),
            (false, true) => format!("{lhs} LIKE CONCAT({rhs}, '%')"),
            (true, false) => format!("{lhs} LIKE CONCAT('%', {rhs})"),
            (false, false) => format!("{lhs} LIKE {rhs}"),
        }
    }

    fn regex(&self, lhs: &str, rhs: &str) -> Option<String> {
        Some(format!("{lhs} REGEXP {rhs}"))
    }
}

/// Quotes each dot-separated segment of `column` in `delimiter`, doubling any
/// embedded occurrence of it. Every SQL identifier quote is its own terminator,
/// so one character covers both ends.
fn quote_delimited(column: &str, delimiter: char) -> String {
    let mut out = String::with_capacity(column.len() + 4);
    for (i, segment) in column.split('.').enumerate() {
        if i > 0 {
            out.push('.');
        }
        out.push(delimiter);
        for ch in segment.chars() {
            if ch == delimiter {
                out.push(delimiter);
            }
            out.push(ch);
        }
        out.push(delimiter);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Dialect, MySql, Postgres, Sqlite};

    #[test]
    fn quotes_each_segment_independently() {
        assert_eq!(
            Postgres.quote_ident("volumes.read_count"),
            r#""volumes"."read_count""#
        );
        assert_eq!(Sqlite.quote_ident("name"), r#""name""#);
        assert_eq!(MySql.quote_ident("volumes.title"), "`volumes`.`title`");
    }

    #[test]
    fn doubles_the_embedded_delimiter() {
        assert_eq!(Postgres.quote_ident(r#"we"ird"#), r#""we""ird""#);
        assert_eq!(Postgres.quote_ident(r#"a"b.c"d"#), r#""a""b"."c""d""#);
        assert_eq!(MySql.quote_ident("we`ird"), "`we``ird`");
        // A backtick is not special to Postgres, nor a double quote to MySQL.
        assert_eq!(Postgres.quote_ident("we`ird"), r#""we`ird""#);
        assert_eq!(MySql.quote_ident(r#"we"ird"#), "`we\"ird`");
    }

    #[test]
    fn placeholders_follow_the_driver() {
        assert_eq!(Postgres.placeholder(1), "$1");
        assert_eq!(Postgres.placeholder(7), "$7");
        assert_eq!(Sqlite.placeholder(7), "?");
        assert_eq!(MySql.placeholder(7), "?");
    }

    /// The property a caller splicing a bind into two places depends on.
    #[test]
    fn positional_dialects_are_the_ones_that_render_every_placeholder_alike() {
        assert!(!Postgres.is_positional());
        assert!(Sqlite.is_positional());
        assert!(MySql.is_positional());
    }

    /// The three override the default, so nothing checks the default against
    /// them unless something does it here. A divergence would mean a custom
    /// dialect that does not override gets a different answer to a built-in
    /// with the same placeholder syntax.
    #[test]
    fn the_default_inference_agrees_with_every_explicit_answer() {
        fn inferred(dialect: &impl Dialect) -> bool {
            dialect.placeholder(1) == dialect.placeholder(2)
        }
        assert_eq!(inferred(&Postgres), Postgres.is_positional());
        assert_eq!(inferred(&Sqlite), Sqlite.is_positional());
        assert_eq!(inferred(&MySql), MySql.is_positional());
    }

    /// A dialect that does not override gets the inference -- including the
    /// case the inference exists for: `?1` looks positional and is not.
    #[test]
    fn a_custom_dialect_falls_back_to_the_inference() {
        // Calls through the blanket impl for a reference, which has to forward
        // the new method or it silently falls back to the default.
        fn by_reference(dialect: impl Dialect) -> bool {
            dialect.is_positional()
        }

        struct Anonymous;
        impl Dialect for Anonymous {
            fn name(&self) -> &'static str {
                "anonymous"
            }
            fn placeholder(&self, _: usize) -> String {
                "?".to_string()
            }
            fn regex(&self, _: &str, _: &str) -> Option<String> {
                None
            }
        }

        struct Numbered;
        impl Dialect for Numbered {
            fn name(&self) -> &'static str {
                "numbered"
            }
            fn placeholder(&self, index: usize) -> String {
                format!("?{index}")
            }
            fn regex(&self, _: &str, _: &str) -> Option<String> {
                None
            }
        }

        assert!(Anonymous.is_positional());
        assert!(!Numbered.is_positional(), "?1 is addressable");
        assert!(!by_reference(&Numbered));
    }

    #[test]
    fn mysql_concatenates_with_concat_not_pipes() {
        assert_eq!(
            Postgres.like("\"t\"", "$1", true, true),
            r#""t" LIKE '%' || $1 || '%'"#
        );
        assert_eq!(
            MySql.like("`t`", "?", true, true),
            "`t` LIKE CONCAT('%', ?, '%')"
        );
        assert_eq!(
            MySql.like("`t`", "?", false, true),
            "`t` LIKE CONCAT(?, '%')"
        );
        assert_eq!(
            MySql.like("`t`", "?", true, false),
            "`t` LIKE CONCAT('%', ?)"
        );
    }

    #[test]
    fn regex_operator_follows_the_driver() {
        assert_eq!(
            Postgres.regex("\"t\"", "$1").as_deref(),
            Some(r#""t" ~ $1"#)
        );
        assert_eq!(
            Sqlite.regex("\"t\"", "?").as_deref(),
            Some(r#""t" REGEXP ?"#)
        );
        assert_eq!(MySql.regex("`t`", "?").as_deref(), Some("`t` REGEXP ?"));
    }
}
