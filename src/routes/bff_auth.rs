use axum::http::{HeaderMap, HeaderName, StatusCode, header::AUTHORIZATION};

pub(crate) const BFF_AUTH_FAILED_MESSAGE: &str = "服务凭证无效";
pub(crate) static X_BFF_SERVICE_TOKEN: HeaderName = HeaderName::from_static("x-bff-service-token");

pub(crate) fn require_bff_service_token(
    headers: &HeaderMap,
    expected: &str,
) -> Result<(), (StatusCode, String)> {
    let failed = || {
        (
            StatusCode::UNAUTHORIZED,
            BFF_AUTH_FAILED_MESSAGE.to_string(),
        )
    };
    let Some(provided) = extract_service_token(headers) else {
        return Err(failed());
    };
    if provided.is_empty() || !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return Err(failed());
    }
    Ok(())
}

/// Current BFF sends `X-BFF-Service-Token` on write APIs and may also put the
/// Bark device key in `Authorization`. Prefer the dedicated header so those
/// requests are not compared against the service token as a Bark bearer.
fn extract_service_token(headers: &HeaderMap) -> Option<&str> {
    if let Some(value) = headers.get(&X_BFF_SERVICE_TOKEN) {
        let text = value.to_str().ok()?.trim();
        if !text.is_empty() {
            return Some(text);
        }
    }
    let value = headers.get(AUTHORIZATION)?;
    let text = value.to_str().ok()?.trim();
    let (scheme, rest) = text.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    Some(rest.trim())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::{BFF_AUTH_FAILED_MESSAGE, require_bff_service_token};
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION};

    fn bearer(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let header = format!("Bearer {value}");
        if let Ok(value) = HeaderValue::from_str(&header) {
            headers.insert(AUTHORIZATION, value);
        }
        headers
    }

    #[test]
    fn missing_header_is_unauthorized() {
        let error = require_bff_service_token(&HeaderMap::new(), "expected-token").err();
        assert_eq!(
            error,
            Some((
                StatusCode::UNAUTHORIZED,
                BFF_AUTH_FAILED_MESSAGE.to_string()
            ))
        );
    }

    #[test]
    fn bark_token_bearer_is_rejected() {
        let error = require_bff_service_token(&bearer("abc123"), "expected-token").err();
        assert_eq!(
            error,
            Some((
                StatusCode::UNAUTHORIZED,
                BFF_AUTH_FAILED_MESSAGE.to_string()
            ))
        );
    }

    #[test]
    fn matching_service_token_is_accepted() {
        assert_eq!(
            require_bff_service_token(&bearer("expected-token"), "expected-token").ok(),
            Some(())
        );
    }

    #[test]
    fn scheme_is_case_insensitive_and_value_is_trimmed() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("bearer  expected-token  "),
        );
        assert!(require_bff_service_token(&headers, "expected-token").is_ok());
    }

    #[test]
    fn length_mismatch_is_rejected_without_accepting_prefix() {
        let error =
            require_bff_service_token(&bearer("expected-token-extra"), "expected-token").err();
        assert_eq!(
            error,
            Some((
                StatusCode::UNAUTHORIZED,
                BFF_AUTH_FAILED_MESSAGE.to_string()
            ))
        );
    }

    fn x_bff_service_token(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-bff-service-token",
            HeaderValue::from_str(value).expect("header value"),
        );
        headers
    }

    #[test]
    fn matching_x_bff_service_token_header_is_accepted() {
        assert!(
            require_bff_service_token(&x_bff_service_token("expected-token"), "expected-token")
                .is_ok()
        );
    }

    #[test]
    fn x_bff_service_token_is_preferred_over_bark_authorization() {
        let mut headers = x_bff_service_token("expected-token");
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer abc123"));
        assert!(require_bff_service_token(&headers, "expected-token").is_ok());
    }

    #[test]
    fn mismatched_x_bff_service_token_is_rejected() {
        let error =
            require_bff_service_token(&x_bff_service_token("other-token"), "expected-token").err();
        assert_eq!(
            error,
            Some((
                StatusCode::UNAUTHORIZED,
                BFF_AUTH_FAILED_MESSAGE.to_string()
            ))
        );
    }
}
