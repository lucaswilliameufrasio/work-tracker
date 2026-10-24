use chrono::NaiveDate;

pub(crate) fn parse_date(value: &str) -> Option<NaiveDate> {
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    if date.format("%Y-%m-%d").to_string() != value {
        return None;
    }
    Some(date)
}

pub(crate) fn is_overdue(
    due_date: Option<NaiveDate>,
    status_category: &str,
    local_today: NaiveDate,
) -> Option<bool> {
    match status_category {
        "backlog" | "active" => Some(due_date.is_some_and(|date| date < local_today)),
        "done" | "canceled" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(value: &str) -> NaiveDate {
        parse_date(value).unwrap()
    }

    #[test]
    fn overdue_is_a_strict_local_date_comparison_excluding_terminal_categories() {
        let today = date("2026-03-08");
        assert!(is_overdue(Some(date("2026-03-07")), "active", today).unwrap());
        assert!(!is_overdue(Some(today), "active", today).unwrap());
        assert!(!is_overdue(Some(date("2026-03-09")), "backlog", today).unwrap());
        assert!(!is_overdue(None, "active", today).unwrap());
        assert!(!is_overdue(Some(date("2026-03-07")), "done", today).unwrap());
        assert!(!is_overdue(Some(date("2026-03-07")), "canceled", today).unwrap());
        assert!(is_overdue(Some(today), "unknown", today).is_none());
    }

    #[test]
    fn parses_only_real_canonical_civil_dates() {
        assert_eq!(date("2028-02-29").to_string(), "2028-02-29");
        assert!(parse_date("2026-02-29").is_none());
        assert!(parse_date("2026-2-09").is_none());
        assert!(parse_date("2026-02-09 ").is_none());
    }
}
