use chrono::{DateTime, SecondsFormat, Utc};

pub(crate) fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Nanos, true)
}
