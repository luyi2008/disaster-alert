pub(super) fn f64(data: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| data.get(*key).and_then(as_f64))
}

pub(super) fn as_f64(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .filter(|number| number.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_continue_after_invalid_values() {
        let data = serde_json::json!({
            "Latitude": "invalid",
            "lat": 35.5
        });
        assert_eq!(f64(&data, &["Latitude", "lat"]), Some(35.5));
    }
}
