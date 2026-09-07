use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Which GIF backend a result came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Giphy,
    Klipy,
}

/// Unified search result across providers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct GifResult {
    pub id: String,
    pub title: String,
    pub url: String,
    pub preview_url: String,
    pub provider: Provider,
}

/// Source selection for `search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Auto,
    Giphy,
    Klipy,
}

impl Source {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Source::Auto),
            "giphy" => Ok(Source::Giphy),
            // "tenor" is a historical alias for klipy.
            "klipy" | "tenor" => Ok(Source::Klipy),
            other => anyhow::bail!("unknown source '{other}' (expected auto|giphy|klipy|tenor)"),
        }
    }
}

// ---------------------------------------------------------------------------
// KLIPY
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct KlipyResponse {
    results: Vec<KlipyResult>,
}

#[derive(Debug, Deserialize, Clone)]
struct KlipyResult {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content_description: Option<String>,
    media_formats: KlipyMediaFormats,
}

#[derive(Debug, Deserialize, Clone)]
struct KlipyMediaFormats {
    gif: Option<KlipyMedia>,
    tinygif: Option<KlipyMedia>,
    preview: Option<KlipyMedia>,
}

#[derive(Debug, Deserialize, Clone)]
struct KlipyMedia {
    #[serde(default)]
    url: Option<String>,
}

/// Query KLIPY for GIFs matching `query`.
pub async fn klipy_search(
    client: &reqwest::Client,
    cfg: &Config,
    query: &str,
    limit: usize,
) -> anyhow::Result<Vec<GifResult>> {
    let key = cfg
        .klipy_api_key
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("KLIPY_API_KEY is not configured"))?;
    let url = "https://api.klipy.com/v2/search";
    let resp = client
        .get(url)
        .query(&[
            ("q", query),
            ("key", key),
            ("limit", &limit.to_string()),
            ("contentfilter", "low"),
            (
                "media_filter",
                "gif,tinygif,mediumgif,nanogif,preview",
            ),
        ])
        .send()
        .await?;
    let status = resp.status();
    let body = resp
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("klipy search failed ({status}): {e}"))?;
    let parsed: KlipyResponse = body.json().await?;
    Ok(parsed.results.into_iter().map(|r| r.into_gif()).collect())
}

impl KlipyResult {
    fn into_gif(self) -> GifResult {
        GifResult {
            id: self.id,
            title: self
                .title
                .or(self.content_description)
                .unwrap_or_else(|| "(untitled)".to_string()),
            url: self.media_formats.gif.and_then(|m| m.url).unwrap_or_default(),
            preview_url: self
                .media_formats
                .tinygif
                .and_then(|m| m.url)
                .or_else(|| self.media_formats.preview.and_then(|m| m.url))
                .unwrap_or_default(),
            provider: Provider::Klipy,
        }
    }
}

// ---------------------------------------------------------------------------
// GIPHY
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GiphyResponse {
    data: Vec<GiphyResult>,
}

#[derive(Debug, Deserialize)]
struct GiphyResult {
    id: String,
    #[serde(default)]
    title: Option<String>,
    images: GiphyImages,
}

#[derive(Debug, Deserialize)]
struct GiphyImages {
    #[serde(rename = "original")]
    original: Option<GiphyImage>,
    #[serde(rename = "preview_gif")]
    preview_gif: Option<GiphyImage>,
}

#[derive(Debug, Deserialize)]
struct GiphyImage {
    #[serde(default)]
    url: Option<String>,
}

/// Query GIPHY for GIFs matching `query`.
pub async fn giphy_search(
    client: &reqwest::Client,
    cfg: &Config,
    query: &str,
    limit: usize,
) -> anyhow::Result<Vec<GifResult>> {
    let key = cfg
        .giphy_api_key
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("GIPHY_API_KEY is not configured"))?;
    let url = "https://api.giphy.com/v1/gifs/search";
    let resp = client
        .get(url)
        .query(&[
            ("q", query),
            ("api_key", key),
            ("limit", &limit.to_string()),
            ("rating", "g"),
        ])
        .send()
        .await?;
    let status = resp.status();
    let body = resp
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("giphy search failed ({status}): {e}"))?;
    let parsed: GiphyResponse = body.json().await?;
    Ok(parsed
        .data
        .into_iter()
        .map(|r| GifResult {
            id: r.id,
            title: r.title.unwrap_or_else(|| "(untitled)".to_string()),
            url: r
                .images
                .original
                .and_then(|i| i.url)
                .unwrap_or_default(),
            preview_url: r
                .images
                .preview_gif
                .and_then(|i| i.url)
                .unwrap_or_default(),
            provider: Provider::Giphy,
        })
        .collect())
}

/// Shared search dispatch honoring `--source` semantics.
///
/// auto = giphy if `GIPHY_API_KEY` set, else klipy; if both keys are
/// configured, try giphy first and fall back to klipy on error.
pub async fn search(
    client: &reqwest::Client,
    cfg: &Config,
    source: Source,
    query: &str,
    limit: usize,
) -> anyhow::Result<Vec<GifResult>> {
    match source {
        Source::Giphy => giphy_search(client, cfg, query, limit).await,
        Source::Klipy => klipy_search(client, cfg, query, limit).await,
        Source::Auto => {
            let has_giphy = cfg.giphy_api_key.is_some();
            let has_klipy = cfg.klipy_api_key.is_some();
            match (has_giphy, has_klipy) {
                (true, false) => giphy_search(client, cfg, query, limit).await,
                (false, _) => klipy_search(client, cfg, query, limit).await,
                (true, true) => match giphy_search(client, cfg, query, limit).await {
                    Ok(results) => Ok(results),
                    Err(_) => klipy_search(client, cfg, query, limit).await,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_parse() {
        assert_eq!(Source::parse("auto").unwrap(), Source::Auto);
        assert_eq!(Source::parse("GIPHY").unwrap(), Source::Giphy);
        assert_eq!(Source::parse("klipy").unwrap(), Source::Klipy);
        assert_eq!(Source::parse("tenor").unwrap(), Source::Klipy);
        assert!(Source::parse("bing").is_err());
    }

    #[test]
    fn klipy_preview_prefers_animated_tinygif() {
        let json = r#"
        {
          "results": [
            {
              "id": "k4",
              "content_description": "tiny animated cat",
              "tags": [],
              "media_formats": {
                "gif": { "url": "https://static.klipy.com/gifs/k4.gif" },
                "tinygif": { "url": "https://static.klipy.com/tiny/k4.gif" },
                "preview": { "url": "https://static.klipy.com/previews/k4.jpg" }
              }
            }
          ]
        }"#;
        let parsed: KlipyResponse = serde_json::from_str(json).unwrap();
        let gif = parsed.results[0].clone().into_gif();
        assert_eq!(gif.preview_url, "https://static.klipy.com/tiny/k4.gif");
    }

    #[test]
    fn klipy_preview_falls_back_to_preview_media() {
        let json = r#"
        {
          "results": [
            {
              "id": "k5",
              "content_description": "no tinygif",
              "tags": [],
              "media_formats": {
                "gif": { "url": "https://static.klipy.com/gifs/k5.gif" },
                "preview": { "url": "https://static.klipy.com/previews/k5.jpg" }
              }
            }
          ]
        }"#;
        let parsed: KlipyResponse = serde_json::from_str(json).unwrap();
        let gif = parsed.results[0].clone().into_gif();
        assert_eq!(gif.preview_url, "https://static.klipy.com/previews/k5.jpg");
    }

    #[test]
    fn parse_klipy_response() {
        let json = r#"
        {
          "results": [
            {
              "id": "k1",
              "title": "Cat dance",
              "content_description": "cat dance",
              "tags": ["cat"],
              "media_formats": {
                "gif": { "url": "https://static.klipy.com/gifs/k1.gif" },
                "preview": { "url": "https://static.klipy.com/previews/k1.gif" },
                "tinygif": { "url": "https://static.klipy.com/tiny/k1.gif" }
              }
            }
          ]
        }"#;
        let parsed: KlipyResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.results.len(), 1);
        let r = &parsed.results[0];
        assert_eq!(r.id, "k1");
        assert_eq!(
            r.media_formats.gif.as_ref().unwrap().url.as_deref(),
            Some("https://static.klipy.com/gifs/k1.gif")
        );
        assert_eq!(
            r.media_formats.preview.as_ref().unwrap().url.as_deref(),
            Some("https://static.klipy.com/previews/k1.gif")
        );
    }

    #[test]
    fn klipy_result_maps_fields_and_title_fallback() {
        let json = r#"
        {
          "results": [
            {
              "id": "k2",
              "content_description": "a very descriptive cat",
              "tags": [],
              "media_formats": {
                "gif": { "url": "https://static.klipy.com/gifs/k2.gif" },
                "preview": { "url": "https://static.klipy.com/previews/k2.gif" }
              }
            }
          ]
        }"#;
        let parsed: KlipyResponse = serde_json::from_str(json).unwrap();
        let gif = parsed.results[0].clone().into_gif();
        assert_eq!(gif.id, "k2");
        assert_eq!(gif.title, "a very descriptive cat");
        assert_eq!(gif.url, "https://static.klipy.com/gifs/k2.gif");
        assert_eq!(
            gif.preview_url,
            "https://static.klipy.com/previews/k2.gif"
        );
        assert_eq!(gif.provider, Provider::Klipy);
    }

    #[test]
    fn klipy_result_untitled_fallback() {
        let json = r#"
        {
          "results": [
            {
              "id": "k3",
              "tags": [],
              "media_formats": { "gif": { "url": "https://static.klipy.com/gifs/k3.gif" } }
            }
          ]
        }"#;
        let parsed: KlipyResponse = serde_json::from_str(json).unwrap();
        let gif = parsed.results[0].clone().into_gif();
        assert_eq!(gif.title, "(untitled)");
        assert_eq!(gif.provider, Provider::Klipy);
    }

    #[test]
    fn parse_giphy_response() {
        let json = r#"
        {
          "data": [
            {
              "id": "g1",
              "title": "happy cat",
              "images": {
                "original": { "url": "https://media.giphy.com/media/g1/original.gif" },
                "preview_gif": { "url": "https://media.giphy.com/media/g1/preview.gif" },
                "fixed_width_small": { "url": "https://media.giphy.com/media/g1/fw.gif" }
              }
            }
          ]
        }"#;
        let parsed: GiphyResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.data.len(), 1);
        let r = &parsed.data[0];
        assert_eq!(r.id, "g1");
        assert_eq!(
            r.images.original.as_ref().unwrap().url.as_deref(),
            Some("https://media.giphy.com/media/g1/original.gif")
        );
        assert_eq!(
            r.images.preview_gif.as_ref().unwrap().url.as_deref(),
            Some("https://media.giphy.com/media/g1/preview.gif")
        );
    }
}