//! One JSON object per line on stderr.

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn emit(level: &str, msg: &str, fields: &[(&str, String)]) {
    let mut line = format!(
        "{{\"time\":\"{}\",\"level\":\"{level}\",\"msg\":{}",
        timestamp(),
        json(msg)
    );
    for (k, v) in fields {
        let _ = write!(line, ",\"{k}\":{}", json(v));
    }
    line.push_str("}\n");
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

fn json(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// RFC 3339 UTC with milliseconds, without a date library.
fn timestamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        now.subsec_millis()
    )
}

#[macro_export]
macro_rules! info {
    ($msg:expr $(, $k:ident = $v:expr)* $(,)?) => {
        $crate::logging::emit("info", $msg, &[$((stringify!($k), format!("{}", $v))),*])
    };
}

#[macro_export]
macro_rules! warn {
    ($msg:expr $(, $k:ident = $v:expr)* $(,)?) => {
        $crate::logging::emit("warn", $msg, &[$((stringify!($k), format!("{}", $v))),*])
    };
}

#[macro_export]
macro_rules! error {
    ($msg:expr $(, $k:ident = $v:expr)* $(,)?) => {
        $crate::logging::emit("error", $msg, &[$((stringify!($k), format!("{}", $v))),*])
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamp_shape() {
        let t = super::timestamp();
        assert_eq!(t.len(), 24);
        assert!(t.starts_with("20") && t.ends_with('Z'));
    }
}
