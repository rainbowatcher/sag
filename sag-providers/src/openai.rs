use async_trait::async_trait;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use sag_core::{
    AppError, ChatCompletionRequest, ChatCompletionResponse, ImageGenerationRequest,
    ImageGenerationResponse,
};
use tracing::{debug, error, instrument, warn};

use crate::LlmProvider;

/// OpenAI-compatible provider adapter.
///
/// Uses the OpenAI Chat Completions API (and any drop-in compatible endpoints).
pub struct OpenAiProvider {
    name: String,
    base_url: String,
    api_key: String,
    http_client: reqwest::Client,
    supported_params: Vec<String>,
}

impl OpenAiProvider {
    /// Create a new OpenAI provider from configuration.
    pub fn new(name: String, config: &sag_config::ProviderConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .expect("failed to build reqwest client");

        Self {
            name,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key: config.api_key.clone(),
            http_client,
            supported_params: config.supported_params.clone(),
        }
    }

    /// Validate that the request does not contain unsupported parameters.
    /// Returns `Ok(())` if all extra keys are in `supported_params`,
    /// or `Err(AppError::BadRequest)` with details of the offending keys.
    fn validate_params(&self, request: &ChatCompletionRequest) -> Result<(), AppError> {
        if self.supported_params.is_empty() {
            // No restriction — allow everything
            return Ok(());
        }

        let supported_set: std::collections::HashSet<&str> =
            self.supported_params.iter().map(|s| s.as_str()).collect();

        let unsupported: Vec<String> = request
            .extra
            .keys()
            .filter(|k| !supported_set.contains(k.as_str())).cloned()
            .collect();

        if !unsupported.is_empty() {
            return Err(AppError::BadRequest(format!(
                "provider '{}' does not support parameters: {}",
                self.name,
                unsupported.join(", ")
            )));
        }

        Ok(())
    }

    /// Build the full upstream URL from the base URL and an API endpoint path.
    fn build_url(&self, api_endpoint: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = api_endpoint.trim_start_matches('/');
        format!("{}/{}", base, path)
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn supported_params(&self) -> &[String] {
        &self.supported_params
    }

    #[instrument(skip(self, request), fields(provider = %self.name, model = %real_model))]
    async fn chat(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        api_endpoint: &str,
    ) -> sag_core::Result<ChatCompletionResponse> {
        // Validate parameters
        self.validate_params(request)?;

        // Clone request and override model with the real model name
        let mut upstream_request = request.clone();
        upstream_request.model = real_model.to_string();

        let url = self.build_url(api_endpoint);
        debug!(%url, "sending non-streaming chat request");

        let response = self
            .http_client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .json(&upstream_request)
            .send()
            .await
            .map_err(AppError::from)?;

        let status = response.status().as_u16();
        let body_text = response.text().await.map_err(AppError::from)?;

        if !(200..300).contains(&status) {
            warn!(status, %body_text, "upstream returned error");
            return Err(AppError::Upstream {
                provider: self.name.clone(),
                status,
                body: body_text,
            });
        }

        let completion: ChatCompletionResponse = serde_json::from_str(&body_text)
            .map_err(|e| AppError::Internal(format!("failed to parse upstream response: {e}")))?;

        Ok(completion)
    }

    #[instrument(skip(self, request), fields(provider = %self.name, model = %real_model))]
    async fn chat_stream(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        api_endpoint: &str,
    ) -> sag_core::Result<Response<Body>> {
        // Validate parameters
        self.validate_params(request)?;

        // Clone request and override model with the real model name
        let mut upstream_request = request.clone();
        upstream_request.model = real_model.to_string();

        let url = self.build_url(api_endpoint);
        debug!(%url, "sending streaming chat request");

        let response = self
            .http_client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .json(&upstream_request)
            .send()
            .await
            .map_err(AppError::from)?;

        let status = response.status();

        if !status.is_success() {
            let status_code = status.as_u16();
            let body_text = response.text().await.map_err(AppError::from)?;
            warn!(status = status_code, %body_text, "upstream returned error on stream");
            return Err(AppError::Upstream {
                provider: self.name.clone(),
                status: status_code,
                body: body_text,
            });
        }

        // Build an Axum response that streams the SSE chunks byte-by-byte.
        let response_builder = axum::response::Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Connection", "keep-alive");

        let byte_stream = response.bytes_stream().map(|result| {
            result.map_err(|e| {
                error!(?e, "error reading upstream stream chunk");
                std::io::Error::other(e)
            })
        });

        let body = Body::from_stream(byte_stream);

        response_builder
            .body(body)
            .map_err(|e| AppError::Internal(format!("failed to build stream response: {e}")))
    }

    #[instrument(skip(self, request), fields(provider = %self.name, model = %real_model))]
    async fn chat_image(
        &self,
        request: &ImageGenerationRequest,
        real_model: &str,
    ) -> sag_core::Result<ImageGenerationResponse> {
        let mut upstream = request.clone();
        upstream.model = real_model.to_string();

        let url = self.build_url("/images/generations");
        debug!(%url, "sending image generation request");

        let response = self
            .http_client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .json(&upstream)
            .send()
            .await
            .map_err(AppError::from)?;

        let status = response.status().as_u16();
        let body_text = response.text().await.map_err(AppError::from)?;

        if !(200..300).contains(&status) {
            warn!(status, %body_text, "upstream image generation returned error");
            return Err(AppError::Upstream {
                provider: self.name.clone(),
                status,
                body: body_text,
            });
        }

        serde_json::from_str(&body_text).map_err(|e| {
            AppError::Internal(format!("failed to parse image generation response: {e}"))
        })
    }
}
