use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::providers::{GifResult, Provider};
use crate::store::LocalStore;

/// Timeout for every favorites HTTP request.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A favorite item as stored on the server (and, in the same shape, in
/// the local favorites store).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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

impl FavItem {
    /// Build a favorite from a search result, as the server does on POST
    /// and the local store does on insert.
    pub fn from_gif(gif: &GifResult) -> Self {
        FavItem {
            id: gif.id.clone(),
            url: gif.url.clone(),
            preview: gif.preview_url.clone(),
            provider: gif.provider.label().to_string(),
            title: gif.title.clone(),
            use_count: 0,
            added_at: Some(now_epoch()),
            last_used: None,
        }
    }

    /// Reconstruct the `GifResult` needed to re-save this favorite
    /// (e.g. `favs --import` pushing the local store to the server).
    pub fn to_gif_result(&self) -> GifResult {
        GifResult {
            id: self.id.clone(),
            title: self.title.clone(),
            url: self.url.clone(),
            preview_url: self.preview.clone(),
            provider: Provider::from_label(&self.provider),
        }
    }
}

/// Wall-clock seconds since the Unix epoch, as a timestamp (kept opaque;
/// nothing parses it back).
pub(crate) fn now_epoch() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

/// One page of the favorites list plus the server-reported total.
#[derive(Debug, Clone)]
pub struct FavsPage {
    pub items: Vec<FavItem>,
    /// Total favorites on the server (`X-Total-Count`); `None` when the
    /// server predates the header.
    pub total: Option<usize>,
}

/// Response shape of `PATCH /favorites/{id}/use`. Returned by
/// `FavsBackend::increment_use` for symmetry with the server; callers
/// currently ignore it (tests consume the fields).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
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
        let base = cfg.favorites_api().ok_or_else(|| {
            anyhow::anyhow!("GIFDECK_FAVORITES_API is not configured (required in server mode)")
        })?;
        Ok(FavsClient {
            http,
            base: base.trim_end_matches('/').to_string(),
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

/// Server-ordering parity: `use_count` DESC, then `last_used` DESC
/// (missing counts as lowest), tie-break `id` ASC — the same key the
/// server's SQL uses, applied to the local store's view so both modes
/// present identically.
pub(crate) fn sort_by_use(items: &mut [FavItem]) {
    items.sort_by(|a, b| {
        b.use_count
            .cmp(&a.use_count)
            .then_with(|| b.last_used.cmp(&a.last_used))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// Where favorites live: the self-hosted server, or the local store.
///
/// The two are alternatives, never a fallback for one another — the
/// backend is picked once from the config (token configured → server,
/// otherwise local). The only crossover between them is the explicit
/// `favs --export` / `favs --import` commands.
#[derive(Debug, Clone)]
pub enum FavsBackend {
    Server(FavsClient),
    Local(LocalStore),
}

impl FavsBackend {
    /// Backend implied by the config: an explicit `FAVORITES_MODE`
    /// (`"server"` / `"local"`) wins, otherwise the server is chosen when
    /// a favorites token is configured and the local store when not.
    pub fn from_config(cfg: &Config) -> Self {
        match cfg.favorites_mode.as_deref() {
            Some("local") => return FavsBackend::Local(LocalStore::new()),
            Some("server") => {
                if let Ok(client) = FavsClient::new(cfg) {
                    return FavsBackend::Server(client);
                }
                return FavsBackend::Local(LocalStore::new());
            }
            _ => {}
        }
        if cfg.use_server_favorites() {
            if let Ok(client) = FavsClient::new(cfg) {
                return FavsBackend::Server(client);
            }
        }
        FavsBackend::Local(LocalStore::new())
    }

    pub fn is_local(&self) -> bool {
        matches!(self, FavsBackend::Local(_))
    }

    /// GET-equivalent: the full favorites list.
    pub async fn list(&self) -> anyhow::Result<Vec<FavItem>> {
        match self {
            FavsBackend::Server(client) => client.list().await,
            FavsBackend::Local(store) => store.load(),
        }
    }

    /// One page of the list plus the total. The local backend paginates
    /// the file in memory (`limit = 0` means "no limit", like the server).
    pub async fn list_page(&self, limit: usize, offset: usize) -> anyhow::Result<FavsPage> {
        match self {
            FavsBackend::Server(client) => client.list_page(limit, offset).await,
            FavsBackend::Local(store) => {
                let mut items = store.load()?;
                sort_by_use(&mut items);
                let total = items.len();
                let page = items
                    .into_iter()
                    .skip(offset)
                    .take(if limit == 0 { usize::MAX } else { limit })
                    .collect();
                Ok(FavsPage {
                    items: page,
                    total: Some(total),
                })
            }
        }
    }

    /// Upsert a favorite from a GifResult.
    pub async fn save(&self, gif: &GifResult) -> anyhow::Result<FavItem> {
        match self {
            FavsBackend::Server(client) => client.save(gif).await,
            FavsBackend::Local(store) => store.upsert(gif),
        }
    }

    /// Remove a favorite by id (idempotent on the local backend).
    pub async fn delete(&self, id: &str) -> anyhow::Result<()> {
        match self {
            FavsBackend::Server(client) => client.delete(id).await,
            FavsBackend::Local(store) => store.remove(id),
        }
    }

    /// Record a "use" of a favorite: bump `use_count` and set `last_used`
    /// (PATCH /favorites/{id}/use on the server; a persisted update in the
    /// local store). A missing id is a silent no-op on the local backend.
    pub async fn increment_use(&self, id: &str) -> anyhow::Result<UseCount> {
        match self {
            FavsBackend::Server(client) => client.increment_use(id).await,
            FavsBackend::Local(store) => {
                let item = store.increment_use(id)?;
                Ok(UseCount {
                    id: item.id,
                    use_count: item.use_count,
                    last_used: item.last_used,
                })
            }
        }
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
            favorites_mode: None,
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

    /// A local store per test: tests run in parallel and each needs its own
    /// file (the atomic write would race on a shared temp path).
    fn local_store() -> (LocalStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "gifdeck-backend-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("favorites.json");
        let _ = std::fs::remove_file(&path);
        (LocalStore::at(&path), path)
    }

    #[test]
    fn fav_item_conversions_round_trip() {
        let fav = FavItem::from_gif(&test_gif());
        assert_eq!(fav.id, "abc123");
        assert_eq!(fav.provider, "klipy");
        assert!(fav.added_at.is_some());
        assert_eq!(fav.use_count, 0);
        let gif = fav.to_gif_result();
        assert_eq!(gif.id, "abc123");
        assert_eq!(gif.url, test_gif().url);
        assert_eq!(gif.preview_url, test_gif().preview_url);
        assert_eq!(gif.provider, Provider::Klipy);
        // Unknown provider labels parse back as Klipy.
        let mut weird = fav.clone();
        weird.provider = "someone-else".into();
        assert_eq!(weird.to_gif_result().provider, Provider::Klipy);
    }

    #[test]
    fn backend_from_config_picks_by_token() {
        // Token configured → server backend.
        let cfg = Config {
            favorites_api: Some("http://cfg-server/api".into()),
            favorites_token: Some("tok".into()),
            ..Config::default()
        };
        assert!(matches!(
            FavsBackend::from_config(&cfg),
            FavsBackend::Server(_)
        ));
        assert!(!FavsBackend::from_config(&cfg).is_local());

        // No token → local backend, even with an API configured.
        let cfg = Config {
            favorites_api: Some("http://cfg-server/api".into()),
            favorites_token: None,
            ..Config::default()
        };
        assert!(FavsBackend::from_config(&cfg).is_local());

        // Default config → local.
        assert!(FavsBackend::from_config(&Config::default()).is_local());
    }

    #[test]
    fn backend_from_config_mode_overrides_token_heuristic() {
        // Explicit local mode wins even with a token configured.
        let cfg = Config {
            favorites_api: Some("http://cfg-server/api".into()),
            favorites_token: Some("tok".into()),
            favorites_mode: Some("local".into()),
            ..Config::default()
        };
        assert!(FavsBackend::from_config(&cfg).is_local());

        // Explicit server mode wins without a token.
        let cfg = Config {
            favorites_api: Some("http://cfg-server/api".into()),
            favorites_token: None,
            favorites_mode: Some("server".into()),
            ..Config::default()
        };
        assert!(matches!(
            FavsBackend::from_config(&cfg),
            FavsBackend::Server(_)
        ));

        // An unset mode keeps the token-presence default (server mode
        // requires a configured API base).
        let cfg = Config {
            favorites_api: Some("http://cfg-server/api".into()),
            favorites_token: Some("tok".into()),
            ..Config::default()
        };
        assert!(matches!(
            FavsBackend::from_config(&cfg),
            FavsBackend::Server(_)
        ));

        // Token set but no API base configured: server mode is
        // impossible, so the local store is used.
        let cfg = Config {
            favorites_token: Some("tok".into()),
            ..Config::default()
        };
        assert!(
            FavsBackend::from_config(&cfg).is_local(),
            "server mode requires GIFDECK_FAVORITES_API"
        );
    }

    #[tokio::test]
    async fn local_backend_lists_pages_and_toggles() {
        let (store, path) = local_store();
        let backend = FavsBackend::Local(store.clone());
        assert!(backend.is_local());

        assert!(backend.list().await.unwrap().is_empty());

        backend.save(&test_gif()).await.unwrap();
        backend
            .save(&GifResult {
                id: "b1".into(),
                ..test_gif()
            })
            .await
            .unwrap();
        backend
            .save(&GifResult {
                id: "c1".into(),
                ..test_gif()
            })
            .await
            .unwrap();

        // Full list.
        assert_eq!(backend.list().await.unwrap().len(), 3);

        // Paging: limit 2 / offset 1 → items b1, c1 and the known total.
        let page = backend.list_page(2, 1).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].id, "b1");
        assert_eq!(page.items[1].id, "c1");
        assert_eq!(page.total, Some(3));

        // limit 0 means "no limit", like the server's list().
        assert_eq!(backend.list_page(0, 0).await.unwrap().items.len(), 3);

        // delete is idempotent.
        backend.delete("b1").await.unwrap();
        backend.delete("b1").await.unwrap();
        let items = backend.list().await.unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|f| f.id != "b1"));

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn local_backend_increment_use_persists() {
        let (store, path) = local_store();
        let backend = FavsBackend::Local(store.clone());
        backend.save(&test_gif()).await.unwrap();

        let first = backend.increment_use("abc123").await.unwrap();
        assert_eq!(first.id, "abc123");
        assert_eq!(first.use_count, 1);
        assert!(first.last_used.is_some());

        let second = backend.increment_use("abc123").await.unwrap();
        assert_eq!(second.use_count, 2);

        // Missing id is a silent no-op.
        let noop = backend.increment_use("nope").await.unwrap();
        assert_eq!(noop.use_count, 0);

        let items = store.load().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].use_count, 2, "bumps persisted");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn local_backend_lists_pages_in_use_order() {
        let (store, path) = local_store();
        let backend = FavsBackend::Local(store.clone());
        // Insert in a fixed file order: c, a, b.
        for id in ["c1", "a1", "b1"] {
            backend
                .save(&GifResult {
                    id: id.into(),
                    ..test_gif()
                })
                .await
                .unwrap();
        }
        // Use counts: b1 twice, c1 once, a1 never.
        backend.increment_use("b1").await.unwrap();
        backend.increment_use("b1").await.unwrap();
        backend.increment_use("c1").await.unwrap();

        let page = backend.list_page(0, 0).await.unwrap();
        let ids: Vec<&str> = page.items.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["b1", "c1", "a1"], "use_count DESC");

        // Tie-break: equal counts order by id ASC (a1, b1, c1 all unused).
        // A separate file — `local_store()` has a single per-thread path.
        let tie_path = path.with_extension("tie.json");
        let _ = std::fs::remove_file(&tie_path);
        let backend2 = FavsBackend::Local(LocalStore::at(&tie_path));
        for id in ["c1", "a1", "b1"] {
            backend2
                .save(&GifResult {
                    id: id.into(),
                    ..test_gif()
                })
                .await
                .unwrap();
        }
        let page = backend2.list_page(0, 0).await.unwrap();
        let ids: Vec<&str> = page.items.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["a1", "b1", "c1"], "id ASC tie-break");

        // Paging happens after sorting (offset 1 of the sorted view).
        let page = backend.list_page(1, 1).await.unwrap();
        assert_eq!(page.items[0].id, "c1");
        assert_eq!(page.total, Some(3));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&tie_path);
    }

    #[tokio::test]
    async fn local_backend_surfaces_damaged_store_as_error() {
        let (store, path) = local_store();
        std::fs::write(&path, "{ broken").unwrap();
        let backend = FavsBackend::Local(store);
        let err = backend.list().await.unwrap_err().to_string();
        assert!(err.contains("invalid local favorites store"), "got: {err}");
        // Toggles refuse rather than overwrite the damaged store.
        assert!(backend.save(&test_gif()).await.is_err());
        assert!(backend.delete("x").await.is_err());
        let _ = std::fs::remove_file(&path);
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
