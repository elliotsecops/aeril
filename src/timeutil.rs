//! Tiempo: epoch en milisegundos, formateo relativo e ISO-8601 UTC.
//! Sin dependencias de zona horaria (local-first, un solo usuario).

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn format_relative(ms: i64) -> String {
    let delta_s = ((now_ms() - ms).max(0)) / 1000;
    if delta_s < 60 {
        format!("hace {delta_s}s")
    } else if delta_s < 3600 {
        format!("hace {}m", delta_s / 60)
    } else if delta_s < 86400 {
        format!("hace {}h", delta_s / 3600)
    } else {
        format!("hace {}d", delta_s / 86400)
    }
}

pub fn iso8601_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Algoritmo civil-from-days (Howard Hinnant), válido para todo `i64` práctico.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_epoch() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_000_000_000_000), "2001-09-09T01:46:40Z");
    }
}
