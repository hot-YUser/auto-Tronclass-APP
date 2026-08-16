use crate::protocol::CourseSnapshot;
use crate::providers::Endpoints;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;

pub async fn list(client: &Client, base_url: &str) -> Result<Vec<CourseSnapshot>, String> {
    let endpoint = Endpoints::derive(base_url).my_courses();
    let response = client
        .get(&endpoint)
        .send()
        .await
        .map_err(|_| "course_transport_failed".to_string())?;
    if response.status().as_u16() == 401 || crate::rollcall::response_url_is_login(response.url()) {
        return Err("course_session_expired".to_string());
    }
    if !response.status().is_success() {
        return Err(format!("course_http_{}", response.status().as_u16()));
    }
    let body = crate::http::read_bounded(response, crate::http::MAX_API_JSON, "list courses")
        .await
        .map_err(|_| "course_response_too_large".to_string())?;
    let value: Value =
        serde_json::from_slice(&body).map_err(|_| "course_response_invalid".to_string())?;
    let items = course_array(&value).ok_or_else(|| "course_response_invalid".to_string())?;
    let mut seen = HashSet::new();
    let mut courses = Vec::new();
    for item in items {
        let Some(course_id) = string_id(item, &["id", "course_id", "courseId"]) else {
            continue;
        };
        if !seen.insert(course_id.clone()) {
            continue;
        }
        let name = ["name", "course_name", "courseName", "title"]
            .iter()
            .find_map(|key| item.get(*key).and_then(Value::as_str))
            .unwrap_or(&course_id)
            .trim()
            .to_string();
        courses.push(CourseSnapshot { course_id, name });
    }
    courses.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.course_id.cmp(&right.course_id))
    });
    Ok(courses)
}

fn course_array(value: &Value) -> Option<&Vec<Value>> {
    value
        .as_array()
        .or_else(|| value.get("courses").and_then(Value::as_array))
        .or_else(|| value.get("items").and_then(Value::as_array))
        .or_else(|| {
            value.get("data").and_then(|data| {
                data.as_array()
                    .or_else(|| data.get("courses").and_then(Value::as_array))
                    .or_else(|| data.get("items").and_then(Value::as_array))
            })
        })
}

fn string_id(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|field| {
            field
                .as_str()
                .map(str::to_string)
                .or_else(|| field.as_i64().map(|number| number.to_string()))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_nested_courses_deduplicates_and_sorts() {
        let value = json!({ "data": { "courses": [
            { "id": 2, "name": "B" },
            { "course_id": "1", "course_name": "A" },
            { "id": 2, "name": "duplicate" }
        ] } });
        let items = course_array(&value).unwrap();
        let mut seen = HashSet::new();
        let parsed: Vec<_> = items
            .iter()
            .filter_map(|item| string_id(item, &["id", "course_id", "courseId"]))
            .filter(|course_id| seen.insert(course_id.clone()))
            .collect();
        assert_eq!(parsed, ["2", "1"]);
    }
}
