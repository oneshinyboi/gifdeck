use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::providers::GifResult;

/// Timeout for every favorites HTTP request.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A favorite item as stored on the server.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FavItem {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub use_count: u64,
    #[serde(default)]
    pub added_at: Option<String>,
    #[serde(default)]
    pub last_used: Option<String>,
}

/// One page of the favorites list plus the server-reported total.
#[derive(Debug, Clone)]
pub struct FavsPage {
    pub items: Vec<FavItem>,
    /// Total favorites on the server (`X-Total-Count`); `None` when the
    /// server predates the header.
    pub total: Option<usize>,
}

/// Response shape of `PATCH /favorites/{id}/use`.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // consumed by later sessions when use-tracking is surfaced in the TUI
pub struct UseCount {
    pub id: String,
    #[serde(default)]
    pub use_count: u64,
    #[serde(default)]
    pub last_used: Option<String>,
}

/// Minimal error body the server returns on 4xx.
#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    error: String,
}

/// Client for the self-hosted favorites server.
#[derive(Debug, Clone)]
pub struct FavsClient {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl FavsClient {
    pub fn new(cfg: &Config) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build http client: {e}"))?;
        Ok(FavsClient {
            http,
            base: cfg.favorites_api().trim_end_matches('/').to_string(),
            token: cfg.favorites_token.clone(),
        })
    }

    /// Attach auth header when a token is configured.
    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(tok) => req.header("X-Auth-Token", tok),
            None => req,
        }
    }

    /// GET /favorites → list sorted by the server.
    pub async fn list(&self) -> anyhow::Result<Vec<FavItem>> {
        Ok(self.list_page(0, 0).await?.items)
    }

    /// GET /favorites?limit=&offset= → one page of the list plus the
    /// server-reported total (`X-Total-Count` header; `None` when the
    /// server predates it). `limit = 0` means "no limit" server-side,
    /// matching `list()`.
    pub async fn list_page(&self, limit: usize, offset: usize) -> anyhow::Result<FavsPage> {
        let resp = self
            .authed(self.http.get(format!(
                "{}/favorites?limit={limit}&offset={offset}",
                self.base
            )))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(self.describe_error(resp, status, "list favorites").await);
        }
        let total = resp
            .headers()
            .get("X-Total-Count")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<usize>().ok());
        let items = resp
            .json::<Vec<FavItem>>()
            .await
            .map_err(|e| anyhow::anyhow!("failed to parse favorites list: {e}"))?;
        Ok(FavsPage { items, total })
    }

    /// POST /favorites → upsert a favorite from a GifResult.
    pub async fn save(&self, gif: &GifResult) -> anyhow::Result<FavItem> {
        #[derive(Serialize)]
        struct SaveBody<'a> {
            id: &'a str,
            url: &'a str,
            preview: &'a str,
            provider: &'a str,
            title: &'a str,
        }
        let body = SaveBody {
            id: &gif.id,
            url: &gif.url,
            preview: &gif.preview_url,
            provider: gif.provider.label(),
            title: &gif.title,
        };
        let resp = self
            .authed(
                self.http
                    .post(format!("{}/favorites", self.base))
                    .json(&body),
            )
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(self.describe_error(resp, status, "save favorite").await);
        }
        resp.json::<FavItem>()
            .await
            .map_err(|e| anyhow::anyhow!("failed to parse saved favorite: {e}"))
    }

    /// PATCH /favorites/{id}/use → increment usage.
    #[allow(dead_code)] // wired up in later sessions (use-tracking on pick)
    pub async fn increment_use(&self, id: &str) -> anyhow::Result<UseCount> {
        let path = urlenc(id);
        let resp = self
            .authed(
                self.http
                    .patch(format!("{}/favorites/{path}/use", self.base)),
            )
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(self
                .describe_error(resp, status, "increment favorite use")
                .await);
        }
        resp.json::<UseCount>()
            .await
            .map_err(|e| anyhow::anyhow!("failed to parse use-count response: {e}"))
    }

    /// DELETE /favorites/{id} → remove favorite.
    pub async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let path = urlenc(id);
        let resp = self
            .authed(self.http.delete(format!("{}/favorites/{path}", self.base)))
            .send()
            .await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NO_CONTENT || status.is_success() {
            return Ok(());
        }
        Err(self.describe_error(resp, status, "delete favorite").await)
    }

    /// Build a descriptive error from a non-success response.
    async fn describe_error(
        &self,
        resp: reqwest::Response,
        status: reqwest::StatusCode,
        action: &str,
    ) -> anyhow::Error {
        let mut msg = format!("failed to {action}: HTTP {}", status.as_u16());
        if let Ok(bytes) = resp.bytes().await {
            if let Ok(api) = serde_json::from_slice::<ApiError>(&bytes) {
                if !api.error.is_empty() {
                    msg.push_str(&format!(" ({})", api.error));
                }
            }
        }
        anyhow::anyhow!(msg)
    }
}

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Provider;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_gif() -> GifResult {
        GifResult {
            id: "abc123".into(),
            title: "test gif".into(),
            url: "https://static.klipy.com/gifs/abc123.gif".into(),
            preview_url: "https://static.klipy.com/previews/abc123.gif".into(),
            provider: Provider::Klipy,
        }
    }

    fn client_for(base: &str) -> FavsClient {
        let cfg = Config {
            klipy_api_key: None,
            giphy_api_key: None,
            favorites_api: Some(base.to_string()),
            favorites_token: Some("sekret".to_string()),
        };
        FavsClient::new(&cfg).unwrap()
    }

    #[tokio::test]
    async fn list_returns_items() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .and(header("X-Auth-Token", "sekret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
                "id": "a1",
                "url": "https://static.klipy.com/gifs/a1.gif",
                "preview": "https://static.klipy.com/previews/a1.gif",
                "provider": "klipy",
                "title": "cool gif",
                "use_count": 3,
                "added_at": "2026-01-01T00:00:00Z",
                "last_used": null
            })]))
            .mount(&server)
            .await;

        let client = client_for(&server.uri());
        let items = client.list().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "a1");
        assert_eq!(items[0].use_count, 3);
        assert_eq!(items[0].provider, "klipy");
    }

    #[tokio::test]
    async fn list_page_sends_limit_and_offset() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .and(query_param("limit", "50"))
            .and(query_param("offset", "50"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "137")
                    .set_body_json(json!([])),
            )
            .mount(&server)
            .await;

        let client = client_for(&server.uri());
        let page = client.list_page(50, 50).await.unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.total, Some(137));
    }

    #[tokio::test]
    async fn list_page_total_absent_is_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let client = client_for(&server.uri());
        let page = client.list_page(50, 0).await.unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.total, None);
    }

    #[tokio::test]
    async fn list_empty_is_ok() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let client = client_for(&server.uri());
        let items = client.list().await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn save_upserts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/favorites"))
            .and(header("X-Auth-Token", "sekret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "abc123",
                "url": "https://static.klipy.com/gifs/abc123.gif",
                "preview": "https://static.klipy.com/previews/abc123.gif",
                "provider": "klipy",
                "title": "test gif",
                "use_count": 1
            })))
            .mount(&server)
            .await;

        let client = client_for(&server.uri());
        let saved = client.save(&test_gif()).await.unwrap();
        assert_eq!(saved.id, "abc123");
        assert_eq!(saved.use_count, 1);
    }

    #[tokio::test]
    async fn increment_use() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/favorites/abc123/use"))
            .and(header("X-Auth-Token", "sekret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "abc123",
                "use_count": 7,
                "last_used": "2026-09-06T00:00:00Z"
            })))
            .mount(&server)
            .await;

        let client = client_for(&server.uri());
        let use_count = client.increment_use("abc123").await.unwrap();
        assert_eq!(use_count.use_count, 7);
    }

    #[tokio::test]
    async fn delete_no_content() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/favorites/abc123"))
            .and(header("X-Auth-Token", "sekret"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let client = client_for(&server.uri());
        client.delete("abc123").await.unwrap();
    }

    #[tokio::test]
    async fn unauthorized_returns_descriptive_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"error": "invalid token"})),
            )
            .mount(&server)
            .await;

        let client = client_for(&server.uri());
        let err = client.list().await.unwrap_err().to_string();
        assert!(err.contains("401"), "got: {err}");
        assert!(err.contains("invalid token"), "got: {err}");
    }

    #[tokio::test]
    async fn not_found_on_delete() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/favorites/nope"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({"error": "no such favorite"})),
            )
            .mount(&server)
            .await;

        let client = client_for(&server.uri());
        let err = client.delete("nope").await.unwrap_err().to_string();
        assert!(err.contains("404"), "got: {err}");
        assert!(err.contains("no such favorite"), "got: {err}");
    }

    #[tokio::test]
    async fn server_error_is_descriptive() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .respond_with(
                ResponseTemplate::new(502).set_body_json(json!({"error": "upstream down"})),
            )
            .mount(&server)
            .await;

        let client = client_for(&server.uri());
        let err = client.list().await.unwrap_err().to_string();
        assert!(err.contains("502"), "got: {err}");
        assert!(err.contains("upstream down"), "got: {err}");
    }
}
