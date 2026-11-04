//! The `datetime` argument as the API takes it, and the query parameters
//! that go with it.

use rmcp::ErrorData as McpError;

/// How the backend picks imagery for a time range. Its default keeps one
/// scene, so a parent zone the scene only partly covers comes back mostly
/// empty (9 of 2,449 Jasper children had data); `mosaic` combines every
/// scene in the range. Collections that aren't STAC-backed ignore it.
const STAC_SEARCH_STRATEGY: &str = "mosaic";

/// Normalises a caller's `datetime` to the RFC 3339 instant or interval the
/// API parses. Models often write bare dates (`2024-08-01/2024-08-31`),
/// which the API rejects, so a `YYYY-MM-DD` side is expanded to cover its
/// whole day, and a single bare date becomes that day's interval. An empty
/// side or `..` is an open end; anything else is passed through for the
/// API to judge.
pub(crate) fn api_datetime(raw: &str) -> Result<String, McpError> {
    let invalid = || {
        McpError::invalid_params(
            format!("`datetime` must be an RFC 3339 instant or `start/end` interval, got {raw:?}"),
            None,
        )
    };
    let parts: Vec<&str> = raw.split('/').map(str::trim).collect();
    let (start, end) = match parts.as_slice() {
        [single] if is_date(single) => (*single, *single),
        [single] if !single.is_empty() => return Ok(single.to_string()),
        [start, end] => (*start, *end),
        _ => return Err(invalid()),
    };
    let side = |value: &str, time: &str| match value {
        "" | ".." => "..".to_string(),
        date if is_date(date) => format!("{date}T{time}Z"),
        other => other.to_string(),
    };
    let (start, end) = (side(start, "00:00:00"), side(end, "23:59:59"));
    if start == ".." && end == ".." {
        return Err(invalid());
    }
    Ok(format!("{start}/{end}"))
}

/// `YYYY-MM-DD`, with no time part.
fn is_date(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// The query parameters a data request carries for `datetime` (already
/// normalised by [`api_datetime`]): the range itself and, with it, the
/// imagery strategy. None without a range.
pub(crate) fn time_query(datetime: Option<&str>) -> Vec<(&'static str, String)> {
    match datetime {
        Some(dt) => vec![
            ("datetime", dt.to_string()),
            ("stac-search-strategy", STAC_SEARCH_STRATEGY.to_string()),
        ],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_dates_cover_whole_days() {
        assert_eq!(
            api_datetime("2024-08-01/2024-08-31").unwrap(),
            "2024-08-01T00:00:00Z/2024-08-31T23:59:59Z"
        );
        assert_eq!(api_datetime("2024-08-05").unwrap(), "2024-08-05T00:00:00Z/2024-08-05T23:59:59Z");
    }

    #[test]
    fn timestamps_and_open_ends_pass_through() {
        assert_eq!(api_datetime("2024-08-01T12:00:00Z").unwrap(), "2024-08-01T12:00:00Z");
        assert_eq!(
            api_datetime("2024-08-01T00:00:00Z/2024-08-31T23:59:59Z").unwrap(),
            "2024-08-01T00:00:00Z/2024-08-31T23:59:59Z"
        );
        assert_eq!(api_datetime("2024-08-01/..").unwrap(), "2024-08-01T00:00:00Z/..");
        assert_eq!(api_datetime("/2024-08-31").unwrap(), "../2024-08-31T23:59:59Z");
    }

    #[test]
    fn empty_or_malformed_ranges_are_rejected() {
        assert!(api_datetime("").is_err());
        assert!(api_datetime("../..").is_err());
        assert!(api_datetime("a/b/c").is_err());
    }

    #[test]
    fn a_range_brings_the_mosaic_strategy() {
        assert!(time_query(None).is_empty());
        assert_eq!(
            time_query(Some("2024-08-01T00:00:00Z/..")),
            vec![
                ("datetime", "2024-08-01T00:00:00Z/..".to_string()),
                ("stac-search-strategy", "mosaic".to_string()),
            ]
        );
    }
}
