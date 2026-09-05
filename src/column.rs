/// The fail-closed CEL-path → database-column allow-list.
///
/// This is the security boundary. A CEL environment generated from a proto
/// declares every field of the resource that has a CEL type, so the parser
/// happily accepts `internal_notes == "x"`; the column map is what stops it
/// reaching SQL. A path that is absent is [`Error::UnknownField`], never a
/// skipped predicate and never a passthrough of the path as a column name.
///
/// The maps are small — a resource has perhaps ten queryable fields — and known
/// at compile time, so this wraps a slice of pairs and scans it linearly. That
/// beats hashing at this size, allocates nothing, and stays `const`-constructible,
/// which matters because the natural home for one of these is a `const` next to
/// the query or in generated code.
///
/// ```
/// const VOLUME_COLUMNS: &[(&str, &str)] = &[
///     ("name", "volumes.name"),
///     ("title", "volumes.title"),
///     ("read_count", "volumes.read_count"),
///     ("create_time", "volumes.created_at"),
/// ];
/// ```
///
/// Note the last entry: the CEL path and the column name differ. That is the
/// normal case, and it is why this is a map rather than a set of allowed paths.
///
/// # Nested paths
///
/// `author.name` is a valid path. It resolves through the map like any other,
/// so `("author.name", "authors.name")` works given that the caller's SQL joins
/// `authors`. Mapping a path onto a JSON member is *not* supported: quoting is
/// per-segment, so the map can only ever name a table and a column — you get
/// `"author"."name"`, never `"author" -> 'name'`.
///
/// [`Error::UnknownField`]: crate::Error::UnknownField
#[derive(Debug, Clone, Copy, Default)]
pub struct Columns<'a> {
    entries: &'a [(&'a str, &'a str)],
}

impl<'a> Columns<'a> {
    /// Wraps a slice of `(cel_path, database_column)` pairs.
    #[must_use]
    pub const fn new(entries: &'a [(&'a str, &'a str)]) -> Self {
        Self { entries }
    }

    /// Returns the database column `path` maps to, or `None` if it is absent.
    ///
    /// On a duplicate path the first entry wins.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&'a str> {
        self.entries
            .iter()
            .find(|(cel_path, _)| *cel_path == path)
            .map(|(_, column)| *column)
    }

    /// The underlying pairs.
    #[must_use]
    pub const fn entries(&self) -> &'a [(&'a str, &'a str)] {
        self.entries
    }
}

impl<'a> From<&'a [(&'a str, &'a str)]> for Columns<'a> {
    fn from(entries: &'a [(&'a str, &'a str)]) -> Self {
        Self::new(entries)
    }
}

impl<'a, const N: usize> From<&'a [(&'a str, &'a str); N]> for Columns<'a> {
    fn from(entries: &'a [(&'a str, &'a str); N]) -> Self {
        Self::new(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::Columns;

    #[test]
    fn lookup_is_fail_closed() {
        let columns = Columns::new(&[("title", "volumes.title")]);
        assert_eq!(columns.get("title"), Some("volumes.title"));
        assert_eq!(columns.get("internal_notes"), None);
        assert_eq!(Columns::default().get("title"), None);
    }
}
