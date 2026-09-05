use std::fmt;

/// Everything that can go wrong while transpiling.
///
/// The transpiler fails loudly. A predicate it cannot translate is never
/// silently dropped, because a dropped predicate widens the result set — a
/// wrong answer rather than an error.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A path that is not in the column map.
    ///
    /// This is the fail-closed gate: the CEL environment declares every field
    /// of a resource, and the column map decides which of them may actually
    /// reach SQL. A miss is an error, never a passthrough of the path as a
    /// column name.
    UnknownField {
        /// The dotted CEL path that was not found.
        path: String,
    },

    /// An expression in identifier position that is not an `Ident`/`Select`
    /// chain, so no dotted path can be reconstructed from it.
    NotAPath,

    /// An expression kind with no Postgres translation.
    UnsupportedExpression {
        /// The kind, named as CEL names it: `list`, `map`, `struct`, …
        kind: &'static str,
    },

    /// A CEL macro. Macros desugar into comprehensions at parse time, and a
    /// comprehension has no Postgres translation.
    UnsupportedMacro {
        /// The macro as written, recovered from the desugared shape.
        ///
        /// `map` and `filter` desugar to the same accumulator shape, so they
        /// are reported together as `map/filter`.
        name: &'static str,
    },

    /// A function outside the supported set — arithmetic, the ternary,
    /// indexing, `size`, and anything else CEL can call.
    UnsupportedFunction {
        /// The CEL function name, e.g. `_+_`.
        name: String,
    },

    /// A supported operator applied to the wrong number of arguments.
    Arity {
        /// The operator, named as it appears in the emitted SQL where there is
        /// one (`=`, `AND`), otherwise as CEL names it (`in`, `contains`).
        function: String,
        /// How many arguments the operator takes.
        expected: usize,
        /// How many it was given.
        actual: usize,
    },

    /// `in` whose right-hand side is not a list literal. `x in y` has no
    /// Postgres translation unless the elements are known at transpile time.
    NotAList,

    /// A `timestamp()` or `duration()` argument that is not a string literal.
    NotAStringLiteral {
        /// `timestamp` or `duration`.
        function: &'static str,
    },

    /// A `timestamp()` argument that is not RFC 3339.
    InvalidTimestamp {
        /// The string literal as written.
        literal: String,
        /// Why it did not parse.
        message: String,
    },

    /// A `duration()` argument that is not a Go duration string.
    InvalidDuration {
        /// The string literal as written.
        literal: String,
        /// Why it did not parse.
        message: String,
    },

    /// `null` outside a `==` or `!=` comparison, or on both sides of one.
    ///
    /// `x == null` becomes `x IS NULL`; there is nowhere else a NULL can go,
    /// because this crate never binds one. See the crate-level docs.
    UnexpectedNull,

    /// A function the target SQL dialect has no way to express.
    UnsupportedByDialect {
        /// The CEL function, e.g. `matches`.
        function: &'static str,
        /// The dialect that cannot express it.
        dialect: &'static str,
    },

    /// A function whose support is behind a Cargo feature that is off.
    FeatureDisabled {
        /// The CEL function that needs it.
        function: &'static str,
        /// The feature to enable.
        feature: &'static str,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnknownField { path } => write!(f, "unknown field {path:?}"),
            Error::NotAPath => f.write_str("unsupported identifier expression"),
            Error::UnsupportedExpression { kind } => {
                write!(f, "unsupported expression kind {kind}")
            }
            Error::UnsupportedMacro { name } => {
                write!(f, "the {name} macro has no Postgres translation")
            }
            Error::UnsupportedFunction { name } => write!(f, "unsupported function {name:?}"),
            Error::Arity {
                function,
                expected,
                actual,
            } => write!(
                f,
                "{function} expects {expected} argument{}, got {actual}",
                if *expected == 1 { "" } else { "s" }
            ),
            Error::NotAList => f.write_str("in requires a list literal on the right-hand side"),
            Error::NotAStringLiteral { function } => {
                write!(f, "{function} argument must be a string literal")
            }
            Error::InvalidTimestamp { literal, message } => {
                write!(f, "timestamp({literal:?}): {message}")
            }
            Error::InvalidDuration { literal, message } => {
                write!(f, "duration({literal:?}): {message}")
            }
            Error::UnexpectedNull => f.write_str(
                "null is only supported as one side of == or !=, which becomes IS NULL / IS NOT NULL",
            ),
            Error::UnsupportedByDialect { function, dialect } => write!(
                f,
                "{function}() has no equivalent in the {dialect} dialect"
            ),
            Error::FeatureDisabled { function, feature } => write!(
                f,
                "{function}() requires the {feature:?} feature of sqlx-cel to be enabled"
            ),
        }
    }
}

impl std::error::Error for Error {}
