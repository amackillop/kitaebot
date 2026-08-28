//! `OpenRouter` endpoint-pricing client (spec 27).
//!
//! `GET {base}/{model}/endpoints` lists every upstream endpoint
//! serving a model with its prices. Unauthenticated, same host as the
//! chat API, so it rides the existing egress allowlist. Pure parsing
//! lives in [`interpret_pricing`]; IO is a stored closure, mirroring
//! [`super::chat_completion::CompletionsClient`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tracing::{debug, error};

use super::RawResponse;
use crate::config::ModelRates;
use crate::error::ProviderError;

type GetResult = Result<RawResponse, ProviderError>;
type GetFuture = Pin<Box<dyn Future<Output = GetResult> + Send>>;
type GetFn = Arc<dyn Fn(String) -> GetFuture + Send + Sync>;

/// Pricing lookups are report-side garnish; never let one hang /usage.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// One upstream endpoint's identity and prices for a model.
#[derive(Debug, PartialEq)]
pub struct EndpointRates {
    /// Matches the chat response's `provider` field.
    pub provider_name: String,
    pub rates: ModelRates,
}

/// Client for the `OpenRouter` model-endpoints pricing API.
#[derive(Clone)]
pub struct PricingClient {
    get: GetFn,
}

impl PricingClient {
    /// `base` is the models API root, e.g.
    /// `https://openrouter.ai/api/v1/models`.
    pub fn new(base: String) -> Self {
        #[cfg(feature = "mock-network")]
        super::assert_loopback(&base);
        let client = super::http_client(reqwest::Client::builder().timeout(FETCH_TIMEOUT));
        Self {
            get: Arc::new(move |url| {
                let client = client.clone();
                let url = format!("{base}/{url}");
                Box::pin(async move {
                    let resp = client.get(&url).send().await.map_err(|e| {
                        error!("Pricing fetch failed for {url}: {e}");
                        ProviderError::Network(e.to_string())
                    })?;
                    let status = resp.status().as_u16();
                    let bytes = resp.bytes().await.map_err(|e| {
                        error!("Pricing body read failed for {url}: {e}");
                        ProviderError::Network(e.to_string())
                    })?;
                    Ok(RawResponse {
                        status,
                        body: bytes.to_vec(),
                        // Not plumbed: this client never retries.
                        retry_after_secs: None,
                    })
                })
            }),
        }
    }

    /// Test constructor — inject an arbitrary closure keyed by the
    /// path suffix (`<model>/endpoints`).
    #[cfg(test)]
    pub fn from_fn<F, Fut>(f: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = GetResult> + Send + 'static,
    {
        Self {
            get: Arc::new(move |path| Box::pin(f(path))),
        }
    }

    /// Every endpoint currently serving `model`, with prices.
    pub async fn endpoint_rates(&self, model: &str) -> Result<Vec<EndpointRates>, ProviderError> {
        let raw = (self.get)(format!("{model}/endpoints")).await?;
        interpret_pricing(&raw)
    }
}

/// Parse a raw endpoints response into per-endpoint rates. Pure.
fn interpret_pricing(raw: &RawResponse) -> Result<Vec<EndpointRates>, ProviderError> {
    debug!(status = raw.status, "Endpoint pricing response");
    if !(200..=299).contains(&raw.status) {
        let body = String::from_utf8_lossy(&raw.body);
        return Err(ProviderError::BadRequest(format!("{}: {body}", raw.status)));
    }
    let parsed: PricingResponse = serde_json::from_slice(&raw.body)
        .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;
    Ok(parsed
        .data
        .endpoints
        .into_iter()
        .filter_map(|e| {
            let input_per_mtok = per_mtok(&e.pricing.prompt)?;
            // No cache-read price means the endpoint bills cache hits
            // at the full input rate: a real spread of zero, not a
            // parse failure.
            let cache_read_per_mtok = e
                .pricing
                .input_cache_read
                .as_deref()
                .and_then(per_mtok)
                .unwrap_or(input_per_mtok);
            Some(EndpointRates {
                provider_name: e.provider_name,
                rates: ModelRates {
                    input_per_mtok,
                    cache_read_per_mtok,
                },
            })
        })
        .collect())
}

/// The API prices in USD per token, as strings; config and the report
/// speak USD per million tokens.
fn per_mtok(per_token: &str) -> Option<f64> {
    per_token.parse::<f64>().ok().map(|p| p * 1e6)
}

#[derive(Deserialize)]
struct PricingResponse {
    data: PricingData,
}

#[derive(Deserialize)]
struct PricingData {
    #[serde(default)]
    endpoints: Vec<WireEndpoint>,
}

#[derive(Deserialize)]
struct WireEndpoint {
    provider_name: String,
    pricing: WirePricing,
}

#[derive(Deserialize)]
struct WirePricing {
    prompt: String,
    #[serde(default)]
    input_cache_read: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(status: u16, body: &str) -> RawResponse {
        RawResponse {
            status,
            body: body.as_bytes().to_vec(),
            retry_after_secs: None,
        }
    }

    /// Shape captured from the live API (2026-08-27).
    const FIXTURE: &str = r#"{"data":{"id":"z-ai/glm-5.2","endpoints":[
        {"provider_name":"Sail Research","context_length":1048576,
         "pricing":{"prompt":"0.0000005","completion":"0.00000315","input_cache_read":"0.000000115"}},
        {"provider_name":"Ambient",
         "pricing":{"prompt":"0.0000006","completion":"0.000002"}}
    ]}}"#;

    #[test]
    fn parses_endpoints_and_converts_to_per_mtok() {
        let rates = interpret_pricing(&raw(200, FIXTURE)).unwrap();
        assert_eq!(rates.len(), 2);
        assert_eq!(rates[0].provider_name, "Sail Research");
        assert!((rates[0].rates.input_per_mtok - 0.5).abs() < 1e-9);
        assert!((rates[0].rates.cache_read_per_mtok - 0.115).abs() < 1e-9);
    }

    /// An endpoint without a cache-read price bills hits at the full
    /// input rate: spread zero, present, not dropped.
    #[test]
    fn missing_cache_read_means_zero_spread() {
        let rates = interpret_pricing(&raw(200, FIXTURE)).unwrap();
        assert_eq!(rates[1].provider_name, "Ambient");
        assert!((rates[1].rates.cache_read_per_mtok - rates[1].rates.input_per_mtok).abs() < 1e-9);
    }

    #[test]
    fn non_2xx_is_an_error_carrying_the_body() {
        let err = interpret_pricing(&raw(404, "no such model")).unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[tokio::test]
    async fn client_requests_the_model_endpoint_path() {
        let client = PricingClient::from_fn(|path| async move {
            assert_eq!(path, "z-ai/glm-5.2/endpoints");
            Ok(raw(200, FIXTURE))
        });
        let rates = client.endpoint_rates("z-ai/glm-5.2").await.unwrap();
        assert_eq!(rates.len(), 2);
    }
}
