//! Derived fields: values the API calculates for each zone from a
//! collection's stored fields, sent as `alias[name]=expression` on its data
//! routes. The expression is CQL2-Text arithmetic (numbers, field names,
//! `+ - * / % ^`, parentheses); this server passes it on unchanged.

use crate::server::enc;

/// The derived fields of one request, in the order they were given.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct DerivedFields(Vec<(String, String)>);

impl DerivedFields {
    /// No derived field.
    pub(super) const fn none() -> Self {
        Self(Vec::new())
    }

    /// `pairs` are `(name, expression)`.
    pub(super) fn new(pairs: Vec<(String, String)>) -> Self {
        Self(pairs)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The names of the calculated columns.
    pub(super) fn names(&self) -> Vec<String> {
        self.0.iter().map(|(name, _)| name.clone()).collect()
    }

    /// The query parameters that define the fields: `alias[name]` and its
    /// expression, not yet URL-encoded.
    pub(super) fn query_pairs(&self) -> Vec<(String, String)> {
        self.0
            .iter()
            .map(|(name, expression)| (format!("alias[{name}]"), expression.clone()))
            .collect()
    }

    /// The same parameters as URL-encoded `key=value` parts, for a request
    /// URL handed to a map.
    pub(super) fn url_parts(&self) -> Vec<String> {
        self.query_pairs()
            .iter()
            .map(|(key, expression)| format!("{}={}", enc(key), enc(expression)))
            .collect()
    }

    /// The stored fields the expressions read: every name in them for which
    /// `is_field` holds, each once, in order of first use.
    pub(super) fn input_fields(&self, is_field: impl Fn(&str) -> bool) -> Vec<String> {
        let mut fields: Vec<String> = Vec::new();
        for (_, expression) in &self.0 {
            for name in names_in(expression) {
                if is_field(name) && !fields.iter().any(|f| f == name) {
                    fields.push(name.to_string());
                }
            }
        }
        fields
    }
}

/// The names an expression mentions: its runs of letters, digits and `_`
/// that do not start with a digit (those are numbers).
fn names_in(expression: &str) -> impl Iterator<Item = &str> {
    expression
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|token| token.chars().next().is_some_and(|c| !c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ratio() -> DerivedFields {
        DerivedFields::new(vec![("ratio".to_string(), "(b8 - b4) / (b8 + b4 - 2000)".to_string())])
    }

    #[test]
    fn a_field_becomes_an_alias_parameter() {
        // Act
        let pairs = ratio().query_pairs();

        // Assert
        assert_eq!(pairs, vec![("alias[ratio]".to_string(), "(b8 - b4) / (b8 + b4 - 2000)".to_string())]);
        assert_eq!(ratio().names(), vec!["ratio"]);
    }

    #[test]
    fn url_parts_are_encoded() {
        assert_eq!(
            ratio().url_parts(),
            vec!["alias%5Bratio%5D=%28b8%20-%20b4%29%20%2F%20%28b8%20%2B%20b4%20-%202000%29"]
        );
    }

    #[test]
    fn input_fields_are_the_known_names_once_each() {
        // Arrange: `b9` is not a field of the collection.
        let derived = DerivedFields::new(vec![
            ("a".to_string(), "b8 * 0.0001 - b4".to_string()),
            ("b".to_string(), "b4 + b9 + B8A_20m".to_string()),
        ]);

        // Act
        let fields = derived.input_fields(|name| name != "b9");

        // Assert
        assert_eq!(fields, vec!["b8", "b4", "B8A_20m"]);
    }

    #[test]
    fn none_is_empty() {
        let none = DerivedFields::default();
        assert!(none.is_empty());
        assert!(none.query_pairs().is_empty());
        assert!(none.input_fields(|_| true).is_empty());
    }
}
