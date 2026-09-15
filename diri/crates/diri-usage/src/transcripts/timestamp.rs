/// Parse the ISO-8601 forms accepted by the Swift formatters, returning Unix seconds.
pub fn parse_timestamp(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }

    let year = number(bytes, 0, 4)?;
    let month = number(bytes, 5, 7)?;
    let day = number(bytes, 8, 10)?;
    let hour = number(bytes, 11, 13)?;
    let minute = number(bytes, 14, 16)?;
    let second = number(bytes, 17, 19)?;
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut cursor = 19;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let fraction_start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == fraction_start {
            return None;
        }
    }

    let offset = match bytes.get(cursor).copied()? {
        b'Z' if cursor + 1 == bytes.len() => 0,
        sign @ (b'+' | b'-') if cursor + 6 == bytes.len() && bytes[cursor + 3] == b':' => {
            let hours = number(bytes, cursor + 1, cursor + 3)?;
            let minutes = number(bytes, cursor + 4, cursor + 6)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let seconds = i64::from(hours * 3_600 + minutes * 60);
            if sign == b'+' { seconds } else { -seconds }
        }
        _ => return None,
    };

    Some(
        days_from_civil(year, month, day) * 86_400 + i64::from(hour * 3_600 + minute * 60 + second)
            - offset,
    )
}

fn number(bytes: &[u8], start: usize, end: usize) -> Option<i32> {
    let mut result = 0_i32;
    for byte in bytes.get(start..end)? {
        if !byte.is_ascii_digit() {
            return None;
        }
        result = result * 10 + i32::from(byte - b'0');
    }
    Some(result)
}

const fn days_in_month(year: i32, month: i32) -> i32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Howard Hinnant's civil-date conversion, shifted to the Unix epoch.
pub(crate) fn days_from_civil(year: i32, month: i32, day: i32) -> i64 {
    let adjusted_year = year - i32::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    i64::from(era * 146_097 + day_of_era - 719_468)
}
