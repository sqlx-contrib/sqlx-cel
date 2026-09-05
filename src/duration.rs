//! A parser for CEL's duration grammar, which is Go's `time.ParseDuration`
//! syntax: an optional sign, then one or more decimal-with-unit runs.
//!
//! `1h30m`, `-5s`, `300ms`, `1.5h`, and a bare `0` are all valid. No Rust crate
//! parses this, and routing through [`std::time::Duration`] is not an option
//! because CEL permits negative durations and `std::time::Duration` cannot hold
//! one — `read_time > duration("-1h")` compiles in cel-rust.

/// Nanoseconds per unit, longest name first so `ms` is not read as `m`.
const UNITS: &[(&str, i128)] = &[
    ("ns", 1),
    ("us", 1_000),
    ("\u{b5}s", 1_000),  // micro sign
    ("\u{3bc}s", 1_000), // greek small letter mu
    ("ms", 1_000_000),
    ("s", 1_000_000_000),
    ("m", 60 * 1_000_000_000),
    ("h", 3_600 * 1_000_000_000),
];

/// Parses a Go duration string into a signed count of microseconds.
///
/// Microseconds is the common denominator across the supported drivers:
/// Postgres `INTERVAL` stores nothing finer, and SQLite and MySQL have no
/// interval type a bind value can carry at all.
///
/// Sub-microsecond precision is therefore lost. The remainder is rounded half
/// away from zero rather than truncated, so `duration("1500ns")` binds as `2`
/// and `duration("-1500ns")` as `-2`.
///
/// # Errors
///
/// Returns a message describing why the string is not a valid duration.
pub(crate) fn parse(input: &str) -> Result<i64, String> {
    let nanos = parse_nanos(input)?;
    let micros = if nanos >= 0 {
        (nanos + 500) / 1_000
    } else {
        (nanos - 500) / 1_000
    };
    i64::try_from(micros).map_err(|_| format!("{input:?} is out of range in microseconds"))
}

/// The Go grammar, in nanoseconds:
///
/// ```text
/// [-+]? ( "0" | ( [0-9]*("."[0-9]*)? unit )+ )
/// ```
fn parse_nanos(input: &str) -> Result<i128, String> {
    let mut rest = input;

    let negative = match rest.as_bytes().first() {
        Some(b'-') => {
            rest = &rest[1..];
            true
        }
        Some(b'+') => {
            rest = &rest[1..];
            false
        }
        _ => false,
    };

    // Go's one special case: a bare zero needs no unit.
    if rest == "0" {
        return Ok(0);
    }
    if rest.is_empty() {
        return Err("empty duration".to_string());
    }

    let mut total: i128 = 0;
    while !rest.is_empty() {
        let (whole, tail) = take_digits(rest);
        let (fraction, scale, tail) = if let Some(tail) = tail.strip_prefix('.') {
            let (digits, tail) = take_digits(tail);
            // Anything past the 18th fraction digit is finer than a nanosecond
            // even when the unit is an hour, so it cannot affect the result.
            // Go likewise stops accumulating once the fraction overflows.
            let kept = &digits[..digits.len().min(18)];
            let scale = 10i128.pow(u32::try_from(kept.len()).unwrap_or(0));
            (kept, scale, tail)
        } else {
            ("", 1, tail)
        };

        if whole.is_empty() && fraction.is_empty() {
            return Err(format!("expected a number at {rest:?}"));
        }

        let (unit_name, unit) = UNITS
            .iter()
            .find(|(name, _)| tail.starts_with(name))
            .ok_or_else(|| {
                if tail.is_empty() {
                    "missing unit; expected one of ns, us, ms, s, m, h".to_string()
                } else {
                    format!("unknown unit at {tail:?}; expected one of ns, us, ms, s, m, h")
                }
            })?;
        rest = &tail[unit_name.len()..];

        let whole: i128 = if whole.is_empty() {
            0
        } else {
            whole
                .parse()
                .map_err(|_| format!("{whole:?} is out of range"))?
        };
        let fraction: i128 = if fraction.is_empty() {
            0
        } else {
            fraction
                .parse()
                .map_err(|_| format!("{fraction:?} is out of range"))?
        };

        let scaled = fraction
            .checked_mul(*unit)
            .map(|fraction| fraction / scale)
            .and_then(|fraction| whole.checked_mul(*unit)?.checked_add(fraction))
            .ok_or_else(|| "duration out of range".to_string())?;
        total = total
            .checked_add(scaled)
            .ok_or_else(|| "duration out of range".to_string())?;
    }

    if negative {
        total = -total;
    }

    // Go's `time.Duration` is an i64 nanosecond count, and CEL inherits that
    // range. Anything wider was never a valid CEL duration to begin with.
    if total > i128::from(i64::MAX) || total < i128::from(i64::MIN) {
        return Err("duration out of range".to_string());
    }
    Ok(total)
}

fn take_digits(input: &str) -> (&str, &str) {
    let end = input
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(input.len());
    input.split_at(end)
}

#[cfg(test)]
mod tests {
    use super::{parse, parse_nanos};

    #[test]
    fn parses_the_shapes_go_accepts() {
        assert_eq!(parse_nanos("0"), Ok(0));
        assert_eq!(parse_nanos("-0"), Ok(0));
        assert_eq!(parse_nanos("+0"), Ok(0));
        assert_eq!(parse_nanos("1h30m"), Ok(5_400_000_000_000));
        assert_eq!(parse_nanos("1.5h"), Ok(5_400_000_000_000));
        assert_eq!(parse_nanos("-1h"), Ok(-3_600_000_000_000));
        assert_eq!(parse_nanos("300ms"), Ok(300_000_000));
        assert_eq!(parse_nanos("-5s"), Ok(-5_000_000_000));
        assert_eq!(parse_nanos("1ns"), Ok(1));
        assert_eq!(parse_nanos("1us"), Ok(1_000));
        assert_eq!(parse_nanos("1\u{b5}s"), Ok(1_000));
        assert_eq!(parse_nanos("1\u{3bc}s"), Ok(1_000));
        assert_eq!(parse_nanos(".5s"), Ok(500_000_000));
        assert_eq!(parse_nanos("1h30m10.5s"), Ok(5_410_500_000_000));
    }

    #[test]
    fn ms_is_not_read_as_m() {
        assert_eq!(parse_nanos("1ms"), Ok(1_000_000));
        assert_eq!(parse_nanos("1m"), Ok(60_000_000_000));
    }

    #[test]
    fn rejects_what_go_rejects() {
        assert!(parse_nanos("").is_err());
        assert!(parse_nanos("-").is_err());
        assert!(parse_nanos("1").is_err(), "a bare number has no unit");
        assert!(parse_nanos("1x").is_err());
        assert!(parse_nanos("h").is_err());
        assert!(parse_nanos("1h30").is_err());
    }

    #[test]
    fn rejects_overflow() {
        assert!(parse_nanos("2562048h").is_err());
        assert!(parse_nanos("99999999999999999999h").is_err());
    }

    #[test]
    fn rounds_sub_microsecond_precision_half_away_from_zero() {
        assert_eq!(parse("1500ns"), Ok(2));
        assert_eq!(parse("-1500ns"), Ok(-2));
        assert_eq!(parse("1499ns"), Ok(1));
        assert_eq!(parse("400ns"), Ok(0));
    }

    #[test]
    fn converts_to_microseconds() {
        assert_eq!(parse("720h"), Ok(720 * 3_600 * 1_000_000));
        assert_eq!(parse("1h30m"), Ok(5_400_000_000));
    }
}
