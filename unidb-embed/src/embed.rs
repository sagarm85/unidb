//! Client-side embedding generation.
//!
//! Calls a **pluggable** HTTP embedding endpoint (OpenAI-compatible by
//! default) to turn a piece of text into a vector. This is the only place the
//! CLI reaches out to a model — deliberately kept entirely client-side so the
//! `unidb` engine never gains a model or network dependency (roadmap Track D).

use anyhow::{anyhow, Context, Result};
use reqwest::blocking::Client;
use serde_json::Value as Json;

/// Everything needed to call one embedding endpoint.
#[derive(Debug, Clone)]
pub struct EmbeddingClient {
    /// Full endpoint URL, e.g. `https://api.openai.com/v1/embeddings`.
    pub url: String,
    /// Model identifier sent as the request's `model` field.
    pub model: String,
    /// API key sent as `Authorization: Bearer <key>`. Empty ⇒ no auth header
    /// (useful for local, keyless embedding servers).
    pub api_key: String,
}

impl EmbeddingClient {
    /// Turn `text` into a vector by POSTing an OpenAI-style
    /// `{"model": ..., "input": text}` body and reading the embedding back out
    /// of the response.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let http = Client::new();
        let mut req = http
            .post(&self.url)
            .json(&serde_json::json!({ "model": self.model, "input": text }));
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req
            .send()
            .with_context(|| format!("calling embedding endpoint {}", self.url))?;
        let status = resp.status();
        let body: Json = resp
            .json()
            .context("embedding endpoint returned a non-JSON body")?;
        if !status.is_success() {
            return Err(anyhow!("embedding endpoint returned HTTP {status}: {body}"));
        }
        parse_embedding(&body)
    }
}

/// Extract the embedding vector from a response body.
///
/// Accepts either the OpenAI shape (`{"data": [{"embedding": [...]}]}`) or a
/// flatter `{"embedding": [...]}` some self-hosted servers return — so the
/// endpoint is genuinely pluggable, not OpenAI-locked.
fn parse_embedding(body: &Json) -> Result<Vec<f32>> {
    let arr = body
        .get("data")
        .and_then(|d| d.get(0))
        .and_then(|first| first.get("embedding"))
        .or_else(|| body.get("embedding"))
        .and_then(|e| e.as_array())
        .ok_or_else(|| anyhow!("no embedding array in response: {body}"))?;

    arr.iter()
        .map(|v| {
            v.as_f64()
                .map(|f| f as f32)
                .ok_or_else(|| anyhow!("non-numeric value in embedding: {v}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_openai_shape() {
        let body = json!({ "data": [{ "embedding": [0.1, 0.2, 0.3] }] });
        assert_eq!(parse_embedding(&body).unwrap(), vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn parses_flat_shape() {
        let body = json!({ "embedding": [1.0, -2.0] });
        assert_eq!(parse_embedding(&body).unwrap(), vec![1.0, -2.0]);
    }

    #[test]
    fn errors_on_missing_embedding() {
        assert!(parse_embedding(&json!({ "oops": true })).is_err());
    }

    #[test]
    fn errors_on_non_numeric_element() {
        let body = json!({ "embedding": [0.1, "nope"] });
        assert!(parse_embedding(&body).is_err());
    }
}
