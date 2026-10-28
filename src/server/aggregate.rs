use serde_json::Value;

pub(super) fn reduce(stat: Option<&str>, values: &[f64]) -> f64 {
    match stat {
        Some("sum") => values.iter().sum(),
        Some("min") => values.iter().cloned().fold(f64::INFINITY, f64::min),
        Some("max") => values.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        Some("count") => values.len() as f64,
        // "mean" and anything unrecognized both default to a plain average.
        _ => values.iter().sum::<f64>() / values.len() as f64,
    }
}

/// Most frequent value across sampled zones, for non-numeric properties
/// (e.g. `gi_dggs_stat: "majority"` on a categorical column like a country
/// name). Ties break on whichever value was seen first.
pub(super) fn majority(values: &[Value]) -> Value {
    let mut counts: Vec<(Value, usize)> = Vec::new();
    for v in values {
        match counts.iter_mut().find(|(existing, _)| existing == v) {
            Some((_, count)) => *count += 1,
            None => counts.push((v.clone(), 1)),
        }
    }
    // Not `max_by_key`: it returns the *last* of equal maxima, which would
    // break ties on the value seen last instead.
    let mut best: Option<(Value, usize)> = None;
    for (value, count) in counts {
        if best.as_ref().is_none_or(|(_, best_count)| count > *best_count) {
            best = Some((value, count));
        }
    }
    best.map(|(value, _)| value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduce_uses_the_configured_statistic() {
        let values = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(reduce(Some("mean"), &values), 2.5);
        assert_eq!(reduce(Some("sum"), &values), 10.0);
        assert_eq!(reduce(Some("min"), &values), 1.0);
        assert_eq!(reduce(Some("max"), &values), 4.0);
        assert_eq!(reduce(Some("count"), &values), 4.0);
        // Unknown or absent stat hints default to a plain average.
        assert_eq!(reduce(None, &values), 2.5);
        assert_eq!(reduce(Some("bogus"), &values), 2.5);
    }

    #[test]
    fn majority_picks_the_most_common_value() {
        let values = vec![
            Value::String("CA".to_string()),
            Value::String("CA".to_string()),
            Value::String("US".to_string()),
        ];
        assert_eq!(majority(&values), Value::String("CA".to_string()));
    }

    #[test]
    fn majority_tie_breaks_on_first_seen() {
        let values = vec![
            Value::String("CA".to_string()),
            Value::String("US".to_string()),
            Value::String("US".to_string()),
            Value::String("CA".to_string()),
        ];
        assert_eq!(majority(&values), Value::String("CA".to_string()));
    }

    #[test]
    fn majority_of_empty_is_null() {
        assert_eq!(majority(&[]), Value::Null);
    }
}
