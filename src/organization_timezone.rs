use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OrganizationTimezone(Tz);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidOrganizationTimezone;

impl OrganizationTimezone {
    pub(crate) fn parse(value: &str) -> Result<Self, InvalidOrganizationTimezone> {
        if value.trim() != value || value.is_empty() {
            return Err(InvalidOrganizationTimezone);
        }
        value
            .parse::<Tz>()
            .map(Self)
            .map_err(|_| InvalidOrganizationTimezone)
    }

    pub(crate) fn local_date(self, now: DateTime<Utc>) -> NaiveDate {
        now.with_timezone(&self.0).date_naive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone};

    #[test]
    fn resolves_iana_zone_and_uses_local_day_across_midnight() {
        let timezone = OrganizationTimezone::parse("America/Los_Angeles").unwrap();
        let before_midnight = Utc.with_ymd_and_hms(2026, 3, 8, 7, 59, 59).unwrap();
        let after_midnight = Utc.with_ymd_and_hms(2026, 3, 8, 8, 0, 0).unwrap();

        assert_eq!(
            timezone.local_date(before_midnight),
            NaiveDate::from_ymd_opt(2026, 3, 7).unwrap()
        );
        assert_eq!(
            timezone.local_date(after_midnight),
            NaiveDate::from_ymd_opt(2026, 3, 8).unwrap()
        );
    }

    #[test]
    fn resolves_dst_transition_without_host_timezone_assumptions() {
        let timezone = OrganizationTimezone::parse("America/Los_Angeles").unwrap();
        let before_transition = Utc.with_ymd_and_hms(2026, 3, 8, 9, 59, 59).unwrap();
        let after_transition = Utc.with_ymd_and_hms(2026, 3, 8, 10, 0, 0).unwrap();

        assert_eq!(
            timezone.local_date(before_transition),
            NaiveDate::from_ymd_opt(2026, 3, 8).unwrap()
        );
        assert_eq!(
            timezone.local_date(after_transition),
            NaiveDate::from_ymd_opt(2026, 3, 8).unwrap()
        );
        assert!(OrganizationTimezone::parse("Not/A_Timezone").is_err());
        assert!(OrganizationTimezone::parse(" UTC").is_err());
    }
}
