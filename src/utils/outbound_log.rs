use anyhow::{Context, Result, bail};
use axum::http::{Request, Uri};
use std::time::Instant;
use tower_http::trace::MakeSpan;
use tracing::{Level, Span};
use url::Url;

const MAX_LOG_BODY_CHARS: usize = 2_048;
const SENSITIVE_QUERY_KEYS: &[&str] = &["key", "apikey", "token", "access_token"];
const DEVICE_KEY_QUERY: &str = "device_key";
const NOTIFICATION_PATH_MARKER: &str = "/notifications/";

#[derive(Clone, Copy)]
pub(crate) struct RedactingMakeSpan;

impl<B> MakeSpan<B> for RedactingMakeSpan {
    fn make_span(&mut self, request: &Request<B>) -> Span {
        tracing::span!(
            tracing::Level::INFO,
            "request",
            method = %request.method(),
            uri = %redact_http_uri(request.uri()),
        )
    }
}

pub(crate) fn redact_url(url: &Url) -> String {
    let mut redacted = url.clone();
    let pairs = redacted
        .query_pairs()
        .map(|(key, value)| {
            if key.eq_ignore_ascii_case(DEVICE_KEY_QUERY) {
                (key.into_owned(), mask_middle(&value))
            } else if is_sensitive_query_key(&key) {
                (key.into_owned(), "***".to_string())
            } else {
                (key.into_owned(), value.into_owned())
            }
        })
        .collect::<Vec<_>>();
    if !pairs.is_empty() {
        redacted.query_pairs_mut().clear();
        redacted.query_pairs_mut().extend_pairs(
            pairs
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
    }
    let path = mask_notification_path(redacted.path());
    redacted.set_path(&path);
    redacted.to_string()
}

pub(crate) fn redact_http_uri(uri: &Uri) -> String {
    let displayed = uri.to_string();
    if let Ok(url) = Url::parse(&displayed) {
        return redact_url(&url);
    }
    match Url::parse(&format!("http://log.invalid{displayed}")) {
        Ok(url) => redact_url(&url)
            .strip_prefix("http://log.invalid")
            .unwrap_or(displayed.as_str())
            .to_string(),
        Err(_) => mask_notification_path(&displayed),
    }
}

pub(crate) fn truncate_log_body(body: &str) -> String {
    let mut chars = body.chars();
    let truncated = chars.by_ref().take(MAX_LOG_BODY_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...truncated")
    } else {
        truncated
    }
}

pub(crate) fn mask_json_for_log(value: &serde_json::Value) -> String {
    let mut cloned = value.clone();
    if let Some(object) = cloned.as_object_mut() {
        if let Some(device_key) = object
            .get("device_key")
            .and_then(serde_json::Value::as_str)
            .map(mask_middle)
        {
            object.insert(
                "device_key".to_string(),
                serde_json::Value::String(device_key),
            );
        }
        if let Some(detail_url) = object.get("url").and_then(serde_json::Value::as_str) {
            object.insert(
                "url".to_string(),
                serde_json::Value::String(redact_url_str(detail_url)),
            );
        }
    }
    truncate_log_body(&cloned.to_string())
}

pub(crate) fn transport_error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_request() {
        "request"
    } else {
        "transport"
    }
}

struct OutboundHttpLog<'a> {
    target: &'static str,
    method: &'static str,
    url: &'a str,
    status: Option<u16>,
    elapsed_ms: u64,
    request_body: Option<&'a str>,
    response_body: Option<&'a str>,
    error: Option<&'a str>,
}

impl OutboundHttpLog<'_> {
    fn emit(self, level: Level) {
        if level == Level::DEBUG {
            tracing::debug!(
                event = "outbound.http",
                target = self.target,
                method = self.method,
                url = self.url,
                status = self.status,
                elapsed_ms = self.elapsed_ms,
                request_body = self.request_body,
                response_body = self.response_body,
                error = self.error,
                "outbound.http"
            );
        } else {
            tracing::info!(
                event = "outbound.http",
                target = self.target,
                method = self.method,
                url = self.url,
                status = self.status,
                elapsed_ms = self.elapsed_ms,
                request_body = self.request_body,
                response_body = self.response_body,
                error = self.error,
                "outbound.http"
            );
        }
    }
}

struct PendingOutboundRequest<'a> {
    target: &'static str,
    method: &'static str,
    log_url: &'a str,
    started: Instant,
    request_body: Option<&'a str>,
    max_bytes: usize,
    read_context: &'static str,
    level: Level,
}

impl PendingOutboundRequest<'_> {
    fn emit_transport_error(self, kind: &'static str) {
        OutboundHttpLog {
            target: self.target,
            method: self.method,
            url: self.log_url,
            status: None,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            request_body: self.request_body,
            response_body: None,
            error: Some(kind),
        }
        .emit(self.level);
    }

    async fn finish(self, response: reqwest::Response) -> Result<(reqwest::StatusCode, Vec<u8>)> {
        let status = response.status();
        match read_limited_body(response, self.max_bytes, self.read_context).await {
            Ok(body) => {
                let body_text = String::from_utf8_lossy(&body);
                let truncated = truncate_log_body(&body_text);
                OutboundHttpLog {
                    target: self.target,
                    method: self.method,
                    url: self.log_url,
                    status: Some(status.as_u16()),
                    elapsed_ms: self.started.elapsed().as_millis() as u64,
                    request_body: self.request_body,
                    response_body: Some(&truncated),
                    error: None,
                }
                .emit(self.level);
                Ok((status, body))
            }
            Err(error) => {
                let message = error.to_string();
                OutboundHttpLog {
                    target: self.target,
                    method: self.method,
                    url: self.log_url,
                    status: Some(status.as_u16()),
                    elapsed_ms: self.started.elapsed().as_millis() as u64,
                    request_body: self.request_body,
                    response_body: None,
                    error: Some(&message),
                }
                .emit(self.level);
                Err(error)
            }
        }
    }
}

pub(crate) async fn get_logged(
    client: &reqwest::Client,
    target: &'static str,
    url: Url,
    max_bytes: usize,
    read_context: &'static str,
) -> Result<(reqwest::StatusCode, Vec<u8>)> {
    get_logged_at(client, target, url, max_bytes, read_context, Level::INFO).await
}

pub(crate) async fn get_logged_at(
    client: &reqwest::Client,
    target: &'static str,
    url: Url,
    max_bytes: usize,
    read_context: &'static str,
    level: Level,
) -> Result<(reqwest::StatusCode, Vec<u8>)> {
    let started = Instant::now();
    let log_url = redact_url(&url);
    let pending = PendingOutboundRequest {
        target,
        method: "GET",
        log_url: &log_url,
        started,
        request_body: None,
        max_bytes,
        read_context,
        level,
    };
    match client.get(url).send().await {
        Ok(response) => pending.finish(response).await,
        Err(error) => {
            let kind = transport_error_kind(&error);
            pending.emit_transport_error(kind);
            bail!("{target} request failed ({kind})")
        }
    }
}

pub(crate) async fn post_json_logged(
    client: &reqwest::Client,
    target: &'static str,
    url: &str,
    payload: &serde_json::Value,
    max_bytes: usize,
    read_context: &'static str,
) -> Result<(reqwest::StatusCode, Vec<u8>)> {
    let started = Instant::now();
    let log_url = redact_url_str(url);
    let request_body = mask_json_for_log(payload);
    let pending = PendingOutboundRequest {
        target,
        method: "POST",
        log_url: &log_url,
        started,
        request_body: Some(&request_body),
        max_bytes,
        read_context,
        level: Level::INFO,
    };
    match client.post(url).json(payload).send().await {
        Ok(response) => pending.finish(response).await,
        Err(error) => {
            let kind = transport_error_kind(&error);
            pending.emit_transport_error(kind);
            bail!("{target} request failed ({kind})")
        }
    }
}

async fn read_limited_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    context: &'static str,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        bail!("{context} exceeded size limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("failed to read {context}"))?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            bail!("{context} exceeded size limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn redact_url_str(value: &str) -> String {
    match Url::parse(value) {
        Ok(url) => redact_url(&url),
        Err(_) => mask_notification_path(value),
    }
}

fn mask_notification_path(path: &str) -> String {
    let Some(index) = path.rfind(NOTIFICATION_PATH_MARKER) else {
        return path.to_string();
    };
    let prefix = &path[..index.saturating_add(NOTIFICATION_PATH_MARKER.len())];
    let rest = &path[index.saturating_add(NOTIFICATION_PATH_MARKER.len())..];
    let token_len = rest.find('/').unwrap_or(rest.len());
    let token = &rest[..token_len];
    let suffix = &rest[token_len..];
    format!("{prefix}{}{suffix}", mask_middle(token))
}

pub(crate) fn mask_middle(value: &str) -> String {
    let value = value.trim();
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= 6 {
        "***".to_string()
    } else {
        let prefix = chars.iter().take(3).collect::<String>();
        let suffix = chars
            .iter()
            .skip(chars.len().saturating_sub(3))
            .collect::<String>();
        format!("{prefix}***{suffix}")
    }
}

fn is_sensitive_query_key(key: &str) -> bool {
    SENSITIVE_QUERY_KEYS
        .iter()
        .any(|sensitive| key.eq_ignore_ascii_case(sensitive))
}

#[cfg(test)]
mod tests {
    use super::{mask_json_for_log, mask_middle, redact_http_uri, redact_url, truncate_log_body};
    use axum::http::Uri;
    use url::Url;

    #[test]
    fn redacts_amap_key_and_keeps_coordinates() -> anyhow::Result<()> {
        let url = Url::parse(
            "https://restapi.amap.com/v3/geocode/regeo?key=secret-key&location=116.480881,39.989410&output=json",
        )?;
        let redacted = redact_url(&url);
        anyhow::ensure!(redacted.contains("key=***"));
        anyhow::ensure!(!redacted.contains("secret-key"));
        anyhow::ensure!(
            redacted.contains("location=116.480881%2C39.989410")
                || redacted.contains("location=116.480881,39.989410")
        );
        Ok(())
    }

    #[test]
    fn masks_notification_token_in_relative_uri() -> anyhow::Result<()> {
        let uri: Uri = "/api/incidents/evt-1/notifications/abcdefg.signaturetoken"
            .parse()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let redacted = redact_http_uri(&uri);
        anyhow::ensure!(redacted.contains("/api/incidents/evt-1/notifications/abc***ken"));
        anyhow::ensure!(!redacted.contains("abcdefg.signaturetoken"));
        Ok(())
    }

    #[test]
    fn masks_device_key_query_in_relative_uri() -> anyhow::Result<()> {
        let uri: Uri = "/api/admin/subscriptions?device_key=barkdevicekey"
            .parse()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let redacted = redact_http_uri(&uri);
        anyhow::ensure!(redacted.contains("device_key=bar***key"));
        anyhow::ensure!(!redacted.contains("barkdevicekey"));
        Ok(())
    }

    #[test]
    fn masks_device_key_and_detail_token_in_json() {
        let payload = serde_json::json!({
            "device_key": "barkdevicekey",
            "title": "地震",
            "url": "https://alert.example.com/incidents/e1/notifications/abcdefghijk.sig"
        });
        let logged = mask_json_for_log(&payload);
        assert!(logged.contains("bar***key"));
        assert!(!logged.contains("barkdevicekey"));
        assert!(logged.contains("abc***sig"));
        assert!(!logged.contains("abcdefghijk.sig"));
    }

    #[test]
    fn mask_middle_keeps_prefix_and_suffix() {
        assert_eq!(mask_middle("short"), "***");
        assert_eq!(mask_middle("barkdevicekey"), "bar***key");
    }

    #[test]
    fn truncates_long_bodies() {
        let body = "测".repeat(3_000);
        let truncated = truncate_log_body(&body);
        assert!(truncated.ends_with("...truncated"));
        assert!(truncated.chars().count() < body.chars().count());
    }
}
