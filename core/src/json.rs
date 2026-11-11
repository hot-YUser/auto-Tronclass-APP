//! Reading tenant JSON. Tenants disagree on field names and on whether an id is a string or a
//! number, so every lookup here takes the candidate keys and normalizes what it finds.

use serde_json::Value;

/// An id as text. The same id arrives as `"42"` from one endpoint and `42` from the next; blank
/// strings and non-integers are not ids.
pub fn id(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim())
            .filter(|text| !text.is_empty())
            .map(str::to_string),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some(number.to_string()),
        _ => None,
    }
}

/// The id under the first of `keys` that holds one.
pub fn id_at(object: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| object.get(key).and_then(id))
}

/// The array under the first of `keys` that holds one; a bare top-level array is its own list.
pub fn array_at<'a>(value: &'a Value, keys: &[&str]) -> &'a [Value] {
    keys.iter()
        .find_map(|key| value.get(key).and_then(Value::as_array))
        .or_else(|| value.as_array())
        .map_or(&[], Vec::as_slice)
}

/// The first non-empty string among `keys`, or `""`.
pub fn text_at<'a>(object: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter()
        .find_map(|key| {
            object
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        })
        .unwrap_or("")
}

/// The boolean at `key`, or `default` when it is absent or not a boolean.
pub fn flag(object: &Value, key: &str, default: bool) -> bool {
    object.get(key).and_then(Value::as_bool).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn id_accepts_strings_and_integers_only() {
        assert_eq!(id(&json!("42")), Some("42".into()));
        assert_eq!(id(&json!(" 42 ")), Some("42".into()));
        assert_eq!(id(&json!(42)), Some("42".into()));
        assert_eq!(id(&json!(u64::MAX)), Some(u64::MAX.to_string()));
        for not_an_id in [json!(""), json!("  "), json!(1.5), json!(null), json!([1])] {
            assert_eq!(id(&not_an_id), None, "{not_an_id}");
        }
    }

    #[test]
    fn id_at_skips_keys_that_hold_no_id() {
        let object = json!({ "rollcall_id": null, "id": 7 });
        assert_eq!(id_at(&object, &["rollcall_id", "id"]), Some("7".into()));
        assert_eq!(id_at(&object, &["missing"]), None);
    }

    #[test]
    fn array_at_falls_back_to_a_bare_array() {
        assert_eq!(
            array_at(&json!({ "items": [1, 2] }), &["data", "items"]).len(),
            2
        );
        assert_eq!(array_at(&json!([1, 2, 3]), &["items"]).len(), 3);
        assert!(array_at(&json!({ "items": "no" }), &["items"]).is_empty());
    }

    #[test]
    fn text_at_skips_blank_and_non_string_fields() {
        let subject = json!({ "description": "", "content": 5, "stem": "Q" });
        assert_eq!(text_at(&subject, &["description", "content", "stem"]), "Q");
        assert_eq!(text_at(&subject, &["missing"]), "");
    }
}
