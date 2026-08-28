use crate::config::Config;
use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use url::Url;
use zeroize::Zeroizing;

const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MIN_REQUEST_INTERVAL: Duration = Duration::from_secs(1);
const MAX_CACHE_ENTRIES: usize = 1_024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReverseGeocodeResult {
    pub(crate) province: String,
    pub(crate) city: String,
    pub(crate) district: String,
}

#[derive(Clone)]
pub(crate) struct ReverseGeocoder {
    enabled: Option<Arc<EnabledGeocoder>>,
}

struct EnabledGeocoder {
    nominatim_endpoint: Url,
    amap: Option<AmapProvider>,
    client: reqwest::Client,
    state: Arc<Mutex<GeocoderState>>,
    coordinate_locks: Arc<StdMutex<HashMap<CoordinateKey, Weak<Mutex<()>>>>>,
}

struct AmapProvider {
    endpoint: Url,
    key: Zeroizing<String>,
}

#[derive(Default)]
struct GeocoderState {
    cache: HashMap<CoordinateKey, CacheEntry>,
    cache_order: VecDeque<CoordinateKey>,
    last_nominatim_request: Option<Instant>,
}

struct CacheEntry {
    value: ReverseGeocodeResult,
    stored_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CoordinateKey {
    latitude: i32,
    longitude: i32,
}

#[derive(Deserialize)]
struct NominatimResponse {
    #[serde(default)]
    address: NominatimAddress,
    #[serde(default)]
    display_name: Option<String>,
    error: Option<String>,
}

#[derive(Default, Deserialize)]
struct NominatimAddress {
    state: Option<String>,
    province: Option<String>,
    region: Option<String>,
    city: Option<String>,
    town: Option<String>,
    municipality: Option<String>,
    county: Option<String>,
    city_district: Option<String>,
    district: Option<String>,
    borough: Option<String>,
    suburb: Option<String>,
}

#[derive(Deserialize)]
struct AmapResponse {
    #[serde(deserialize_with = "deserialize_amap_text")]
    status: String,
    #[serde(default, deserialize_with = "deserialize_amap_text")]
    info: String,
    #[serde(default, deserialize_with = "deserialize_amap_text")]
    infocode: String,
    regeocode: Option<AmapRegeocode>,
}

#[derive(Deserialize)]
struct AmapRegeocode {
    #[serde(default, rename = "addressComponent")]
    address_component: Option<AmapAddressComponent>,
}

#[derive(Default, Deserialize)]
struct AmapAddressComponent {
    #[serde(default, deserialize_with = "deserialize_amap_text")]
    province: String,
    #[serde(default, deserialize_with = "deserialize_amap_text")]
    city: String,
    #[serde(default, deserialize_with = "deserialize_amap_text")]
    district: String,
}

impl ReverseGeocoder {
    pub(crate) fn new(config: &Config) -> Result<Self> {
        Self::from_settings(
            config.reverse_geocoding_enabled,
            &config.reverse_geocoding_url,
            &config.amap_regeo_url,
            config.amap_key.as_ref().map(|key| key.expose()),
        )
    }

    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self { enabled: None }
    }

    fn from_settings(
        enabled: bool,
        reverse_geocoding_url: &str,
        amap_regeo_url: &str,
        amap_key: Option<&str>,
    ) -> Result<Self> {
        if !enabled {
            return Ok(Self { enabled: None });
        }
        let nominatim_endpoint =
            Url::parse(reverse_geocoding_url).context("failed to parse reverse geocoding URL")?;
        let amap_key = amap_key.map(str::trim).filter(|key| !key.is_empty());
        let amap = match amap_key {
            Some(key) => Some(AmapProvider {
                endpoint: Url::parse(amap_regeo_url)
                    .context("failed to parse Amap reverse geocoding URL")?,
                key: Zeroizing::new(key.to_string()),
            }),
            None => None,
        };
        let client = reqwest::Client::builder()
            .user_agent("disaster-alert/1.0 (https://github.com/luyi2008/disaster-alert)")
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(2)
            .build()?;
        Ok(Self {
            enabled: Some(Arc::new(EnabledGeocoder {
                nominatim_endpoint,
                amap,
                client,
                state: Arc::new(Mutex::new(GeocoderState::default())),
                coordinate_locks: Arc::new(StdMutex::new(HashMap::new())),
            })),
        })
    }

    pub(crate) async fn resolve(
        &self,
        latitude: f64,
        longitude: f64,
    ) -> Result<ReverseGeocodeResult> {
        let enabled = self
            .enabled
            .as_ref()
            .context("reverse geocoding is disabled")?;
        let key = CoordinateKey::new(latitude, longitude)?;
        let coordinate_lock = enabled.coordinate_lock(key);
        let _coordinate_guard = coordinate_lock.lock().await;
        if let Some(cached) = enabled.cached(key).await {
            return Ok(cached);
        }

        if enabled.amap.is_some() {
            let started = Instant::now();
            match enabled.lookup_amap(latitude, longitude).await {
                Ok(value) => {
                    enabled.cache(key, value.clone()).await;
                    return Ok(value);
                }
                Err(error) => {
                    tracing::warn!(
                        event = "reverse_geocode.amap_failed",
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        error = %error,
                        "reverse_geocode.amap_failed"
                    );
                }
            }
        }

        enabled.acquire_nominatim_slot().await;
        let value = enabled.lookup_nominatim(latitude, longitude).await?;
        enabled.cache(key, value.clone()).await;
        Ok(value)
    }
}

impl EnabledGeocoder {
    fn coordinate_lock(&self, key: CoordinateKey) -> Arc<Mutex<()>> {
        let mut locks = self
            .coordinate_locks
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        locks.retain(|_, weak| weak.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    async fn cached(&self, key: CoordinateKey) -> Option<ReverseGeocodeResult> {
        let state = self.state.lock().await;
        state
            .cache
            .get(&key)
            .filter(|entry| entry.stored_at.elapsed() <= CACHE_TTL)
            .map(|entry| entry.value.clone())
    }

    async fn acquire_nominatim_slot(&self) {
        loop {
            let delay = {
                let mut state = self.state.lock().await;
                match state.last_nominatim_request {
                    Some(last_request) => {
                        let elapsed = last_request.elapsed();
                        if elapsed < MIN_REQUEST_INTERVAL {
                            Some(MIN_REQUEST_INTERVAL - elapsed)
                        } else {
                            state.last_nominatim_request = Some(Instant::now());
                            None
                        }
                    }
                    None => {
                        state.last_nominatim_request = Some(Instant::now());
                        None
                    }
                }
            };
            let Some(delay) = delay else {
                return;
            };
            tokio::time::sleep(delay).await;
        }
    }

    async fn lookup_amap(&self, latitude: f64, longitude: f64) -> Result<ReverseGeocodeResult> {
        let amap = self
            .amap
            .as_ref()
            .context("amap reverse geocoding is not configured")?;
        let mut url = amap.endpoint.clone();
        url.query_pairs_mut()
            .append_pair("key", amap.key.as_str())
            .append_pair("location", &format!("{longitude:.6},{latitude:.6}"))
            .append_pair("extensions", "base")
            .append_pair("output", "json");
        let response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(error) => {
                bail!(
                    "amap reverse geocoding request failed ({})",
                    reqwest_error_kind(&error)
                );
            }
        };
        let status = response.status();
        if !status.is_success() {
            bail!("amap reverse geocoding service returned an error ({status})");
        }
        let parsed = limited_response_json::<AmapResponse>(response).await?;
        // #region agent log
        agent_debug_log(
            "F",
            "reverse_geocoder.rs:lookup_amap",
            "amap json status",
            serde_json::json!({
                "status": parsed.status,
                "info": parsed.info,
                "infocode": parsed.infocode,
                "has_regeocode": parsed.regeocode.is_some(),
            }),
        );
        // #endregion
        parsed.into_result()
    }

    async fn lookup_nominatim(
        &self,
        latitude: f64,
        longitude: f64,
    ) -> Result<ReverseGeocodeResult> {
        let mut url = self.nominatim_endpoint.clone();
        url.query_pairs_mut()
            .append_pair("format", "jsonv2")
            .append_pair("addressdetails", "1")
            .append_pair("accept-language", "zh-CN,zh,en")
            .append_pair("zoom", "14")
            .append_pair("lat", &latitude.to_string())
            .append_pair("lon", &longitude.to_string());
        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("reverse geocoding request failed")?
            .error_for_status()
            .context("reverse geocoding service returned an error")?;
        let response = limited_response_json::<NominatimResponse>(response).await?;
        anyhow::ensure!(
            response.error.as_deref().is_none_or(str::is_empty),
            "reverse geocoding service rejected the coordinates"
        );
        Ok(response.into_result())
    }

    async fn cache(&self, key: CoordinateKey, value: ReverseGeocodeResult) {
        let mut state = self.state.lock().await;
        state.cache.insert(
            key,
            CacheEntry {
                value: value.clone(),
                stored_at: Instant::now(),
            },
        );
        state.cache_order.retain(|cached| *cached != key);
        state.cache_order.push_back(key);
        while state.cache_order.len() > MAX_CACHE_ENTRIES {
            if let Some(expired) = state.cache_order.pop_front() {
                state.cache.remove(&expired);
            }
        }
    }
}

async fn limited_response_json<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        bail!("reverse geocoding response exceeded size limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read reverse geocoding response")?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            bail!("reverse geocoding response exceeded size limit");
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).context("invalid reverse geocoding response")
}

impl CoordinateKey {
    fn new(latitude: f64, longitude: f64) -> Result<Self> {
        if !latitude.is_finite()
            || !longitude.is_finite()
            || !(-90.0..=90.0).contains(&latitude)
            || !(-180.0..=180.0).contains(&longitude)
        {
            bail!("invalid coordinates");
        }
        Ok(Self {
            latitude: (latitude * 10_000.0).round() as i32,
            longitude: (longitude * 10_000.0).round() as i32,
        })
    }
}

impl NominatimResponse {
    fn into_result(self) -> ReverseGeocodeResult {
        self.address.into_result(self.display_name.as_deref())
    }
}

impl NominatimAddress {
    fn into_result(self, display_name: Option<&str>) -> ReverseGeocodeResult {
        let province = first_non_empty([self.state, self.province, self.region]);
        let mut city = first_non_empty([self.city, self.town, self.municipality]);
        let mut district = first_non_empty([
            self.city_district,
            self.district,
            self.borough,
            self.suburb,
            self.county,
        ]);
        if looks_like_chinese_district(&city) {
            if district.is_empty() || looks_like_chinese_subdistrict(&district) {
                district = city.clone();
            }
            if let Some(prefecture) = chinese_prefecture_from_display_name(display_name) {
                city = prefecture;
            }
        }
        ReverseGeocodeResult {
            province,
            city,
            district,
        }
    }
}

impl AmapResponse {
    fn into_result(self) -> Result<ReverseGeocodeResult> {
        anyhow::ensure!(
            self.status == "1",
            "amap reverse geocoding rejected the coordinates (status={} info={} infocode={})",
            self.status,
            self.info,
            self.infocode
        );
        let component = self
            .regeocode
            .and_then(|regeocode| regeocode.address_component)
            .unwrap_or_default();
        let province = component.province.trim().to_string();
        let mut city = component.city.trim().to_string();
        let district = component.district.trim().to_string();
        if city.is_empty() {
            city = province.clone();
        }
        anyhow::ensure!(
            !(province.is_empty() && city.is_empty() && district.is_empty()),
            "amap reverse geocoding returned an empty address"
        );
        Ok(ReverseGeocodeResult {
            province,
            city,
            district,
        })
    }
}

fn looks_like_chinese_district(value: &str) -> bool {
    value.ends_with('区') || value.ends_with('县') || value.ends_with('旗')
}

fn looks_like_chinese_subdistrict(value: &str) -> bool {
    value.ends_with("街道") || value.ends_with('镇') || value.ends_with('乡')
}

fn chinese_prefecture_from_display_name(display_name: Option<&str>) -> Option<String> {
    display_name?
        .split([',', '，'])
        .map(str::trim)
        .find(|part| part.ends_with('市') && !part.ends_with('区') && part.chars().count() >= 2)
        .map(ToOwned::to_owned)
}

fn first_non_empty<const N: usize>(values: [Option<String>; N]) -> String {
    values
        .into_iter()
        .flatten()
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

fn reqwest_error_kind(error: &reqwest::Error) -> &'static str {
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

fn deserialize_amap_text<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(String::new()),
        Some(serde_json::Value::String(text)) => Ok(text),
        Some(serde_json::Value::Number(number)) => Ok(number.to_string()),
        Some(serde_json::Value::Array(items)) if items.is_empty() => Ok(String::new()),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected Amap string, number, or empty array, got {other}"
        ))),
    }
}

// #region agent log
fn agent_debug_log(hypothesis_id: &str, location: &str, message: &str, data: serde_json::Value) {
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/Users/jon/Mangguo/disaster-alert/.cursor/debug-c37bf2.log")
    else {
        return;
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let payload = serde_json::json!({
        "sessionId": "c37bf2",
        "hypothesisId": hypothesis_id,
        "location": location,
        "message": message,
        "data": data,
        "timestamp": timestamp,
        "runId": "amap-status",
    });
    match std::io::Write::write_all(&mut file, format!("{payload}\n").as_bytes()) {
        Ok(()) => {}
        Err(_) => {}
    }
}
// #endregion

#[cfg(test)]
mod tests {
    use super::{
        AmapResponse, CoordinateKey, EnabledGeocoder, GeocoderState, MIN_REQUEST_INTERVAL,
        NominatimAddress, ReverseGeocoder,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::{Duration, Instant};
    use tokio::sync::Mutex;
    use url::Url;

    fn nominatim_only(
        enabled: bool,
        reverse_geocoding_url: &str,
    ) -> anyhow::Result<ReverseGeocoder> {
        ReverseGeocoder::from_settings(
            enabled,
            reverse_geocoding_url,
            "http://127.0.0.1:9/regeo",
            None,
        )
    }

    #[test]
    fn amap_rejection_includes_infocode() -> anyhow::Result<()> {
        let parsed: AmapResponse = serde_json::from_value(serde_json::json!({
            "status": "0",
            "info": "INVALID_USER_IP",
            "infocode": "10005"
        }))?;
        let error = parsed
            .into_result()
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected Amap rejection"))?;
        let message = error.to_string();
        anyhow::ensure!(message.contains("status=0"));
        anyhow::ensure!(message.contains("INVALID_USER_IP"));
        anyhow::ensure!(message.contains("10005"));
        Ok(())
    }

    #[test]
    fn disabled_geocoder_does_not_parse_its_endpoint() -> anyhow::Result<()> {
        let geocoder = nominatim_only(false, "not a URL")?;
        anyhow::ensure!(geocoder.enabled.is_none());
        Ok(())
    }

    #[test]
    fn maps_nominatim_address_fallbacks() -> anyhow::Result<()> {
        let result = NominatimAddress {
            state: Some("四川省".to_string()),
            province: None,
            region: None,
            city: None,
            town: Some("成都市".to_string()),
            municipality: None,
            county: None,
            city_district: Some("武侯区".to_string()),
            district: None,
            borough: None,
            suburb: None,
        }
        .into_result(None);
        anyhow::ensure!(result.province == "四川省");
        anyhow::ensure!(result.city == "成都市");
        anyhow::ensure!(result.district == "武侯区");
        Ok(())
    }

    #[test]
    fn county_is_a_district_fallback() -> anyhow::Result<()> {
        let result = NominatimAddress {
            city: Some("杭州市".to_string()),
            county: Some("余杭区".to_string()),
            ..NominatimAddress::default()
        }
        .into_result(None);
        anyhow::ensure!(result.city == "杭州市");
        anyhow::ensure!(result.district == "余杭区");
        Ok(())
    }

    #[test]
    fn chinese_district_in_city_uses_display_name_prefecture() -> anyhow::Result<()> {
        let result = NominatimAddress {
            state: Some("四川省".to_string()),
            city: Some("锦江区".to_string()),
            suburb: Some("沙河街道".to_string()),
            ..NominatimAddress::default()
        }
        .into_result(Some(
            "静康社区, 沙河街道, 锦江区, 成都市, 四川省, 610066, 中国",
        ));
        anyhow::ensure!(result.province == "四川省");
        anyhow::ensure!(result.city == "成都市");
        anyhow::ensure!(result.district == "锦江区");
        Ok(())
    }

    #[test]
    fn coordinate_cache_key_uses_four_decimal_places() -> anyhow::Result<()> {
        anyhow::ensure!(
            CoordinateKey::new(35.12344, 139.12344)? == CoordinateKey::new(35.12343, 139.12343)?
        );
        anyhow::ensure!(CoordinateKey::new(91.0, 0.0).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn rate_limit_wait_does_not_hold_the_state_lock() -> anyhow::Result<()> {
        let state = Arc::new(Mutex::new(GeocoderState {
            last_nominatim_request: Some(Instant::now()),
            ..GeocoderState::default()
        }));
        let geocoder = EnabledGeocoder {
            nominatim_endpoint: Url::parse("http://127.0.0.1:9/reverse")?,
            amap: None,
            client: reqwest::Client::new(),
            state: Arc::clone(&state),
            coordinate_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        };
        let task = tokio::spawn(async move { geocoder.acquire_nominatim_slot().await });
        tokio::task::yield_now().await;

        let guard = tokio::time::timeout(Duration::from_millis(100), state.lock()).await?;
        drop(guard);
        task.abort();
        drop(task.await);
        anyhow::ensure!(MIN_REQUEST_INTERVAL > Duration::from_millis(100));
        Ok(())
    }

    #[tokio::test]
    async fn provider_error_is_rejected_without_caching() -> anyhow::Result<()> {
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = Arc::clone(&calls);
        let app = axum::Router::new().route(
            "/reverse",
            axum::routing::get(move || {
                let calls = Arc::clone(&server_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({"error": "coordinates rejected"}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/reverse", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let geocoder = nominatim_only(true, &endpoint)?;

        anyhow::ensure!(geocoder.resolve(35.0, 105.0).await.is_err());
        let enabled = geocoder
            .enabled
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("test geocoder unexpectedly disabled"))?;
        anyhow::ensure!(enabled.state.lock().await.cache.is_empty());
        anyhow::ensure!(calls.load(Ordering::SeqCst) == 1);
        server.abort();
        drop(server.await);
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_same_coordinate_uses_one_upstream_request() -> anyhow::Result<()> {
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = Arc::clone(&calls);
        let app = axum::Router::new().route(
            "/reverse",
            axum::routing::get(move || {
                let calls = Arc::clone(&server_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    axum::Json(serde_json::json!({
                        "address": {"state": "四川省", "city": "成都市", "county": "武侯区"}
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/reverse", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let geocoder = nominatim_only(true, &endpoint)?;
        let first = geocoder.resolve(30.6, 104.0);
        let second = geocoder.resolve(30.6, 104.0);

        let (first, second) = tokio::join!(first, second);
        let first = first?;
        let second = second?;
        anyhow::ensure!(first.district == "武侯区");
        anyhow::ensure!(second.district == first.district);
        anyhow::ensure!(calls.load(Ordering::SeqCst) == 1);
        server.abort();
        drop(server.await);
        Ok(())
    }

    #[test]
    fn coordinate_lock_table_removes_expired_entries() -> anyhow::Result<()> {
        let geocoder = EnabledGeocoder {
            nominatim_endpoint: Url::parse("http://127.0.0.1:9/reverse")?,
            amap: None,
            client: reqwest::Client::new(),
            state: Arc::new(Mutex::new(GeocoderState::default())),
            coordinate_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        };
        drop(geocoder.coordinate_lock(CoordinateKey::new(1.0, 1.0)?));
        let active = geocoder.coordinate_lock(CoordinateKey::new(2.0, 2.0)?);
        let locks = geocoder
            .coordinate_locks
            .lock()
            .map_err(|error| anyhow::anyhow!("coordinate lock table poisoned: {error}"))?;
        anyhow::ensure!(locks.len() == 1);
        drop(locks);
        drop(active);
        Ok(())
    }

    #[tokio::test]
    async fn amap_success_skips_nominatim() -> anyhow::Result<()> {
        let amap_calls = Arc::new(AtomicUsize::new(0));
        let nominatim_calls = Arc::new(AtomicUsize::new(0));
        let amap_server_calls = Arc::clone(&amap_calls);
        let nominatim_server_calls = Arc::clone(&nominatim_calls);
        let app = axum::Router::new()
            .route(
                "/regeo",
                axum::routing::get(move || {
                    let calls = Arc::clone(&amap_server_calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({
                            "status": "1",
                            "regeocode": {
                                "addressComponent": {
                                    "province": "四川省",
                                    "city": "成都市",
                                    "district": "锦江区"
                                }
                            }
                        }))
                    }
                }),
            )
            .route(
                "/reverse",
                axum::routing::get(move || {
                    let calls = Arc::clone(&nominatim_server_calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({"error": "should not be called"}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let geocoder = ReverseGeocoder::from_settings(
            true,
            &format!("http://{addr}/reverse"),
            &format!("http://{addr}/regeo"),
            Some("test-key"),
        )?;

        let result = geocoder.resolve(30.6376, 104.1119).await?;
        anyhow::ensure!(result.province == "四川省");
        anyhow::ensure!(result.city == "成都市");
        anyhow::ensure!(result.district == "锦江区");
        anyhow::ensure!(amap_calls.load(Ordering::SeqCst) == 1);
        anyhow::ensure!(nominatim_calls.load(Ordering::SeqCst) == 0);
        server.abort();
        drop(server.await);
        Ok(())
    }

    #[tokio::test]
    async fn amap_empty_city_array_uses_province() -> anyhow::Result<()> {
        let app = axum::Router::new().route(
            "/regeo",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "status": "1",
                    "regeocode": {
                        "addressComponent": {
                            "province": "北京市",
                            "city": [],
                            "district": "朝阳区"
                        }
                    }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let geocoder = ReverseGeocoder::from_settings(
            true,
            "http://127.0.0.1:9/reverse",
            &format!("http://{addr}/regeo"),
            Some("test-key"),
        )?;

        let result = geocoder.resolve(39.99, 116.48).await?;
        anyhow::ensure!(result.province == "北京市");
        anyhow::ensure!(result.city == "北京市");
        anyhow::ensure!(result.district == "朝阳区");
        server.abort();
        drop(server.await);
        Ok(())
    }

    #[tokio::test]
    async fn amap_failure_falls_back_to_nominatim() -> anyhow::Result<()> {
        let nominatim_calls = Arc::new(AtomicUsize::new(0));
        let nominatim_server_calls = Arc::clone(&nominatim_calls);
        let app = axum::Router::new()
            .route(
                "/regeo",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({
                        "status": "0",
                        "info": "INVALID_USER_KEY"
                    }))
                }),
            )
            .route(
                "/reverse",
                axum::routing::get(move || {
                    let calls = Arc::clone(&nominatim_server_calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({
                            "display_name": "静康社区, 沙河街道, 锦江区, 成都市, 四川省",
                            "address": {
                                "state": "四川省",
                                "city": "锦江区",
                                "suburb": "沙河街道"
                            }
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let geocoder = ReverseGeocoder::from_settings(
            true,
            &format!("http://{addr}/reverse"),
            &format!("http://{addr}/regeo"),
            Some("test-key"),
        )?;

        let result = geocoder.resolve(30.6376, 104.1119).await?;
        anyhow::ensure!(result.province == "四川省");
        anyhow::ensure!(result.city == "成都市");
        anyhow::ensure!(result.district == "锦江区");
        anyhow::ensure!(nominatim_calls.load(Ordering::SeqCst) == 1);
        server.abort();
        drop(server.await);
        Ok(())
    }

    #[tokio::test]
    async fn both_providers_failing_is_an_error() -> anyhow::Result<()> {
        let app = axum::Router::new()
            .route(
                "/regeo",
                axum::routing::get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
            )
            .route(
                "/reverse",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({"error": "unable to geocode"}))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let geocoder = ReverseGeocoder::from_settings(
            true,
            &format!("http://{addr}/reverse"),
            &format!("http://{addr}/regeo"),
            Some("test-key"),
        )?;

        anyhow::ensure!(geocoder.resolve(30.6376, 104.1119).await.is_err());
        server.abort();
        drop(server.await);
        Ok(())
    }

    #[tokio::test]
    async fn missing_amap_key_uses_only_nominatim() -> anyhow::Result<()> {
        let amap_calls = Arc::new(AtomicUsize::new(0));
        let amap_server_calls = Arc::clone(&amap_calls);
        let app = axum::Router::new()
            .route(
                "/regeo",
                axum::routing::get(move || {
                    let calls = Arc::clone(&amap_server_calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({"status": "1"}))
                    }
                }),
            )
            .route(
                "/reverse",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({
                        "address": {"state": "浙江省", "city": "杭州市", "county": "余杭区"}
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let geocoder = ReverseGeocoder::from_settings(
            true,
            &format!("http://{addr}/reverse"),
            &format!("http://{addr}/regeo"),
            None,
        )?;

        let result = geocoder.resolve(30.3, 120.0).await?;
        anyhow::ensure!(result.city == "杭州市");
        anyhow::ensure!(amap_calls.load(Ordering::SeqCst) == 0);
        server.abort();
        drop(server.await);
        Ok(())
    }
}
