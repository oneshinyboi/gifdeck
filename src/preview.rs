use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, DynamicImage};
use ratatui::layout::{Rect, Size};
use ratatui::widgets::Paragraph;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{FilterType, Resize};
use tokio::sync::mpsc;

/// Hard ceiling for the preview cache, guarding against absurd terminal
/// sizes (e.g. 500x200 cells). Doubles as the cap when the visible grid is
/// somehow larger than this.
pub const MAX_CACHE_CAP: usize = 4096;
pub const MAX_FRAMES: usize = 24;
pub const PREVIEW_SIZE: Size = Size::new(12, 4);
const MIN_FRAME_DELAY: Duration = Duration::from_millis(40);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const GIF_MAGIC: &[u8; 6] = b"GIF89a";

pub struct CachedPreview {
    frames: Vec<Protocol>,
    delays: Vec<Duration>,
    loaded_at: Instant,
    /// Raw GIF bytes, kept so the enlarged view can re-decode at a bigger
    /// size without re-fetching.
    bytes: Arc<Vec<u8>>,
}

impl CachedPreview {
    pub fn frame(&self, now: Instant) -> &Protocol {
        let elapsed = now.saturating_duration_since(self.loaded_at);
        let idx = frame_index(&self.delays, elapsed);
        &self.frames[idx.min(self.frames.len().saturating_sub(1))]
    }

    #[cfg(test)]
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    pub fn raw_bytes(&self) -> &Arc<Vec<u8>> {
        &self.bytes
    }
}

pub fn frame_index(delays: &[Duration], elapsed: Duration) -> usize {
    if delays.is_empty() {
        return 0;
    }
    let total_ms: u128 = delays.iter().map(|d| d.as_millis()).sum();
    if total_ms == 0 {
        return 0;
    }
    let t_ms = elapsed.as_millis() % total_ms;
    let mut acc: u128 = 0;
    for (i, d) in delays.iter().enumerate() {
        acc += d.as_millis();
        if t_ms < acc {
            return i;
        }
    }
    0
}

pub enum PreviewEntry {
    Requested,
    Ready(CachedPreview),
    /// The failure reason is stored for future status surfacing (session 3).
    Failed(#[allow(dead_code)] String),
}

pub struct PreviewCache {
    map: HashMap<String, PreviewEntry>,
    lru: VecDeque<String>,
    loading: HashSet<String>,
    cap: usize,
}

impl PreviewCache {
    pub fn new(cap: usize) -> Self {
        PreviewCache {
            map: HashMap::new(),
            lru: VecDeque::new(),
            loading: HashSet::new(),
            cap: cap.max(1),
        }
    }

    fn touch(&mut self, url: &str) {
        if let Some(pos) = self.lru.iter().position(|u| u == url) {
            self.lru.remove(pos);
        }
        self.lru.push_back(url.to_string());
    }

    /// Evict least-recently-used entries while over the cap. In-flight
    /// (`Requested`) entries are never evicted: the loader cannot be
    /// cancelled, so dropping the bookkeeping would let the eventual result
    /// re-insert and evict a visible entry (the load/unload thrash loop).
    fn evict(&mut self) {
        while self.map.len() > self.cap {
            let Some(idx) = self.lru.iter().position(|u| !self.loading.contains(u)) else {
                break;
            };
            let oldest = self.lru.remove(idx).expect("index from iter() is valid");
            self.map.remove(&oldest);
        }
    }

    pub fn try_request(&mut self, url: &str) -> bool {
        if url.is_empty() || self.map.contains_key(url) || self.loading.contains(url) {
            return false;
        }
        self.map.insert(url.to_string(), PreviewEntry::Requested);
        self.loading.insert(url.to_string());
        self.touch(url);
        self.evict();
        true
    }

    /// Store a completed load. Results for URLs that are no longer tracked
    /// as in-flight (evicted via `clear`, or a stale duplicate) are dropped
    /// instead of re-inserted — a late insert would overshoot the cap and
    /// evict a currently visible entry.
    pub fn insert_ready(&mut self, url: &str, cached: CachedPreview) {
        if !self.loading.remove(url) {
            return;
        }
        self.map
            .insert(url.to_string(), PreviewEntry::Ready(cached));
        self.touch(url);
        self.evict();
    }

    pub fn insert_failed(&mut self, url: &str, why: String) {
        if !self.loading.remove(url) {
            return;
        }
        self.map.insert(url.to_string(), PreviewEntry::Failed(why));
        self.touch(url);
        self.evict();
    }

    /// Resize the cap to cover the currently visible window. Grow keeps all
    /// entries; shrink evicts least-recently-used entries down to the new size.
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.clamp(1, MAX_CACHE_CAP);
        self.evict();
    }

    /// Drop all entries and in-flight request bookkeeping (used when the
    /// whole item list is replaced, e.g. on a page change).
    pub fn clear(&mut self) {
        self.map.clear();
        self.lru.clear();
        self.loading.clear();
    }

    pub fn get(&self, url: &str) -> Option<&PreviewEntry> {
        self.map.get(url)
    }

    /// Newest `Ready` entry whose key belongs to `url` at any decode size
    /// (focus-cache keys embed the size; see [`focus_key`]). Used to keep
    /// showing the previous frames while a re-decode for a new size runs.
    pub fn newest_ready_for(&self, url: &str) -> Option<&CachedPreview> {
        self.lru.iter().rev().find_map(|key| {
            if !key_for_url(key, url) {
                return None;
            }
            match self.map.get(key) {
                Some(PreviewEntry::Ready(c)) => Some(c),
                _ => None,
            }
        })
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// Whether `key` is a focus-cache key built from `url` (any size).
fn key_for_url(key: &str, url: &str) -> bool {
    key.starts_with(url) && key[url.len()..].starts_with('#')
}

/// Focus-cache key: the preview URL plus the decode size, so a terminal
/// resize produces a fresh entry instead of being deduped against frames
/// decoded for the old size. `#WxH` cannot occur inside a URL, so keys of
/// different URLs never collide.
fn focus_key(url: &str, size: Size) -> String {
    format!("{url}#{}x{}", size.width, size.height)
}

/// A decode job for the loader task: a regular grid preview, or a
/// large-size decode for the enlarged overlay.
enum LoadRequest {
    Preview { url: String },
    Focus { url: String, size: Size },
}

pub struct PreviewLoader {
    tx: mpsc::UnboundedSender<LoadRequest>,
}

impl PreviewLoader {
    pub fn new(
        client: reqwest::Client,
        cache: Arc<Mutex<PreviewCache>>,
        focus_cache: Arc<Mutex<PreviewCache>>,
        picker: Picker,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<LoadRequest>();
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                match req {
                    LoadRequest::Preview { url } => {
                        load_preview(&client, &url, &picker, &cache).await;
                    }
                    LoadRequest::Focus { url, size } => {
                        load_focus(&client, &url, size, &picker, &cache, &focus_cache).await;
                    }
                }
            }
        });
        PreviewLoader { tx }
    }

    pub fn request(&self, url: &str) {
        let _ = self.tx.send(LoadRequest::Preview {
            url: url.to_string(),
        });
    }

    pub fn request_focus(&self, url: &str, size: Size) {
        let _ = self.tx.send(LoadRequest::Focus {
            url: url.to_string(),
            size,
        });
    }
}

pub fn ensure_requested(cache: &Arc<Mutex<PreviewCache>>, loader: &PreviewLoader, url: &str) {
    let send = {
        let mut guard = cache.lock().unwrap();
        guard.try_request(url)
    };
    if send {
        loader.request(url);
    }
}

/// Request an enlarged decode for `url` at `size` into `focus_cache`,
/// deduped per (url, size): a different size (e.g. after a terminal
/// resize) starts a fresh decode while the old frames keep rendering.
pub fn ensure_focus_requested(
    focus_cache: &Arc<Mutex<PreviewCache>>,
    loader: &PreviewLoader,
    url: &str,
    size: Size,
) {
    let key = focus_key(url, size);
    let send = {
        let mut guard = focus_cache.lock().unwrap();
        guard.try_request(&key)
    };
    if send {
        loader.request_focus(url, size);
    }
}

async fn fetch_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let resp = client
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("preview request failed: {url}"))?
        .error_for_status()
        .with_context(|| format!("preview request failed: {url}"))?;
    let bytes = resp.bytes().await?;
    Ok(bytes.to_vec())
}

async fn load_preview(
    client: &reqwest::Client,
    url: &str,
    picker: &Picker,
    cache: &Arc<Mutex<PreviewCache>>,
) {
    let bytes = match fetch_bytes(client, url).await {
        Ok(b) => b,
        Err(e) => {
            cache.lock().unwrap().insert_failed(url, format!("{e:#}"));
            return;
        }
    };

    let picker = picker.clone();
    let result = tokio::task::spawn_blocking(move || decode_preview(&bytes, &picker))
        .await
        .map_err(|e| anyhow::anyhow!("decode task failed: {e}"));

    let mut guard = cache.lock().unwrap();
    match result {
        Ok(Ok(Some(cached))) => guard.insert_ready(url, cached),
        Ok(Ok(None)) => guard.insert_failed(url, "not a gif".to_string()),
        Ok(Err(e)) => guard.insert_failed(url, format!("{e:#}")),
        Err(e) => guard.insert_failed(url, e.to_string()),
    }
}

/// Load an enlarged preview: reuse the raw bytes already cached for the
/// grid preview when possible, fetching only when they aren't there yet.
async fn load_focus(
    client: &reqwest::Client,
    url: &str,
    size: Size,
    picker: &Picker,
    cache: &Arc<Mutex<PreviewCache>>,
    focus_cache: &Arc<Mutex<PreviewCache>>,
) {
    let cached_bytes = {
        let guard = cache.lock().unwrap();
        match guard.get(url) {
            Some(PreviewEntry::Ready(c)) => {
                let bytes = c.raw_bytes();
                (!bytes.is_empty()).then(|| Arc::clone(bytes))
            }
            _ => None,
        }
    };
    let bytes = match cached_bytes {
        Some(b) => b,
        None => match fetch_bytes(client, url).await {
            Ok(b) => Arc::new(b),
            Err(e) => {
                let key = focus_key(url, size);
                focus_cache
                    .lock()
                    .unwrap()
                    .insert_failed(&key, format!("{e:#}"));
                return;
            }
        },
    };

    let picker = picker.clone();
    let result =
        tokio::task::spawn_blocking(move || decode_preview_at(&bytes, &picker, size, FOCUS_RESIZE))
            .await
            .map_err(|e| anyhow::anyhow!("decode task failed: {e}"));

    let key = focus_key(url, size);
    let mut guard = focus_cache.lock().unwrap();
    match result {
        Ok(Ok(Some(cached))) => guard.insert_ready(&key, cached),
        Ok(Ok(None)) => guard.insert_failed(&key, "not a gif".to_string()),
        Ok(Err(e)) => guard.insert_failed(&key, format!("{e:#}")),
        Err(e) => guard.insert_failed(&key, e.to_string()),
    }
}

/// Grid cells keep the source resolution: `Fit` never upscales, so small
/// GIFs stay crisp in 12x4 cells.
const GRID_RESIZE: Resize = Resize::Fit(None);
/// The enlarged overlay has to upscale small preview GIFs to fill the
/// window: `Fit` would leave them at natural size in the corner. Triangle
/// filtering keeps the enlargement smooth instead of blocky.
const FOCUS_RESIZE: Resize = Resize::Scale(Some(FilterType::Triangle));

pub fn decode_preview(bytes: &[u8], picker: &Picker) -> Result<Option<CachedPreview>> {
    decode_preview_at(bytes, picker, PREVIEW_SIZE, GRID_RESIZE)
}

/// Decode a GIF into cached frames fitted to `size` cells (used for both
/// grid previews and the enlarged overlay).
pub fn decode_preview_at(
    bytes: &[u8],
    picker: &Picker,
    size: Size,
    resize: Resize,
) -> Result<Option<CachedPreview>> {
    if !bytes.starts_with(GIF_MAGIC) {
        return Ok(None);
    }
    let decoder = GifDecoder::new(Cursor::new(bytes))?;
    let mut frames = Vec::new();
    let mut delays = Vec::new();
    for frame in decoder.into_frames().take(MAX_FRAMES) {
        let frame = frame?;
        let dyn_img: DynamicImage = frame.buffer().clone().into();
        let protocol = picker.new_protocol(dyn_img, size, resize.clone())?;
        let (numer, denom) = frame.delay().numer_denom_ms();
        let delay = Duration::from_millis((numer as u64).saturating_div(denom.max(1) as u64));
        frames.push(protocol);
        delays.push(delay.max(MIN_FRAME_DELAY));
    }
    if frames.is_empty() {
        return Ok(None);
    }
    Ok(Some(CachedPreview {
        frames,
        delays,
        loaded_at: Instant::now(),
        bytes: Arc::new(bytes.to_vec()),
    }))
}

pub fn render_preview(
    frame: &mut ratatui::Frame,
    area: Rect,
    cache: &Arc<Mutex<PreviewCache>>,
    url: &str,
    center: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let guard = cache.lock().unwrap();
    match guard.get(url) {
        Some(PreviewEntry::Ready(cached)) => render_entry(frame, area, cached, center),
        _ => render_placeholder(frame, area, url),
    }
}

/// Render the enlarged overlay image for `url` at the overlay's current
/// cell size. If no decode for that exact size is ready yet (e.g. right
/// after a terminal resize), fall back to the newest frames decoded for
/// any size so the overlay never goes blank mid-resize.
pub fn render_focus_preview(
    frame: &mut ratatui::Frame,
    area: Rect,
    focus_cache: &Arc<Mutex<PreviewCache>>,
    url: &str,
    want: Size,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let key = focus_key(url, want);
    let guard = focus_cache.lock().unwrap();
    let cached = match guard.get(&key) {
        Some(PreviewEntry::Ready(cached)) => Some(cached),
        _ => guard.newest_ready_for(url),
    };
    match cached {
        Some(cached) => render_entry(frame, area, cached, true),
        None => render_placeholder(frame, area, url),
    }
}

fn render_entry(frame: &mut ratatui::Frame, area: Rect, cached: &CachedPreview, center: bool) {
    let protocol = cached.frame(Instant::now());
    let area = if center {
        centered_area(area, protocol.size())
    } else {
        area
    };
    let widget = ratatui_image::Image::new(protocol).allow_clipping(true);
    frame.render_widget(widget, area);
}

fn render_placeholder(frame: &mut ratatui::Frame, area: Rect, url: &str) {
    let label = if url.is_empty() { "" } else { "…" };
    let paragraph = Paragraph::new(label)
        .alignment(ratatui::layout::Alignment::Center)
        .style(ratatui::style::Style::default().dim());
    frame.render_widget(paragraph, area);
}

/// A `size`-sized rect centered inside `area` (shrunk and offset; never
/// larger than `area`). Used so an enlarged frame that is smaller than the
/// overlay still appears in the middle instead of the top-left corner.
fn centered_area(area: Rect, size: Size) -> Rect {
    let w = size.width.min(area.width);
    let h = size.height.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

/// Breakdown of cache states for status display.
pub fn preview_stats(cache: &Arc<Mutex<PreviewCache>>) -> (usize, usize, usize) {
    let guard = cache.lock().unwrap();
    let mut loaded = 0;
    let mut requested = 0;
    let mut failed = 0;
    for entry in guard.map.values() {
        match entry {
            PreviewEntry::Ready(_) => loaded += 1,
            PreviewEntry::Requested => requested += 1,
            PreviewEntry::Failed(_) => failed += 1,
        }
    }
    (loaded, requested, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    fn cache(cap: usize) -> PreviewCache {
        PreviewCache::new(cap)
    }

    #[test]
    fn frame_index_wraps_and_selects() {
        let delays = [Duration::from_millis(100), Duration::from_millis(100)];
        assert_eq!(frame_index(&delays, Duration::from_millis(0)), 0);
        assert_eq!(frame_index(&delays, Duration::from_millis(99)), 0);
        assert_eq!(frame_index(&delays, Duration::from_millis(100)), 1);
        assert_eq!(frame_index(&delays, Duration::from_millis(199)), 1);
        assert_eq!(frame_index(&delays, Duration::from_millis(200)), 0);
        assert_eq!(frame_index(&delays, Duration::from_millis(299)), 0);
    }

    #[test]
    fn frame_index_edge_cases() {
        assert_eq!(frame_index(&[], Duration::from_secs(5)), 0);
        assert_eq!(
            frame_index(&[Duration::from_millis(200)], Duration::from_secs(3)),
            0
        );
        let zero = [Duration::ZERO, Duration::ZERO];
        assert_eq!(frame_index(&zero, Duration::from_millis(50)), 0);
    }

    #[test]
    fn cache_insert_and_evict_at_cap() {
        let mut c = cache(3);
        for k in ["a", "b", "c"] {
            c.try_request(k);
        }
        // Complete the loads so entries become evictable.
        for k in ["a", "b", "c"] {
            c.insert_ready(k, empty_cached());
        }
        assert!(c.try_request("d"));
        assert_eq!(c.len(), 3);
        assert!(c.get("a").is_none());
        assert!(c.get("b").is_some());
        assert!(c.get("d").is_some());
    }

    #[test]
    fn evict_never_drops_in_flight_entries() {
        let mut c = cache(2);
        c.try_request("a");
        c.try_request("b");
        c.insert_ready("a", empty_cached());
        // 'b' is still in flight: over cap it must not be evicted.
        c.try_request("c");
        assert!(c.get("b").is_some(), "in-flight entry kept");
        assert!(c.get("a").is_none(), "lru ready entry evicted instead");
        assert!(c.get("c").is_some());
    }

    #[test]
    fn late_results_after_clear_are_dropped() {
        let mut c = cache(4);
        c.try_request("a");
        c.clear();
        c.insert_ready(
            "a",
            CachedPreview {
                frames: Vec::new(),
                delays: Vec::new(),
                loaded_at: Instant::now(),
                bytes: Arc::new(Vec::new()),
            },
        );
        c.insert_failed("a", "boom".to_string());
        assert!(c.get("a").is_none(), "stale result must not re-insert");
    }

    fn empty_cached() -> CachedPreview {
        CachedPreview {
            frames: Vec::new(),
            delays: Vec::new(),
            loaded_at: Instant::now(),
            bytes: Arc::new(Vec::new()),
        }
    }

    #[test]
    fn cache_ready_bumps_lru_position() {
        let mut c = cache(2);
        c.try_request("a");
        c.try_request("b");
        let picker = Picker::halfblocks();
        let img: DynamicImage = ImageBuffer::from_pixel(10, 10, Rgba([255u8, 0, 0, 255])).into();
        let proto = picker
            .new_protocol(img, PREVIEW_SIZE, Resize::Fit(None))
            .unwrap();
        c.insert_ready(
            "a",
            CachedPreview {
                frames: vec![proto],
                delays: vec![Duration::from_millis(100)],
                loaded_at: Instant::now(),
                bytes: Arc::new(Vec::new()),
            },
        );
        c.insert_ready("b", empty_cached());
        assert!(c.try_request("c"));
        assert_eq!(c.len(), 2);
        assert!(c.get("a").is_none(), "least-recently-used 'a' evicted");
        assert!(c.get("b").is_some());
        assert!(c.get("c").is_some());
    }

    #[test]
    fn set_cap_grows_and_keeps_entries() {
        let mut c = cache(2);
        c.try_request("a");
        c.try_request("b");
        c.set_cap(5);
        c.try_request("c");
        c.try_request("d");
        c.try_request("e");
        assert_eq!(c.len(), 5);
        for k in ["a", "b", "c", "d", "e"] {
            assert!(c.get(k).is_some(), "{k} should be kept after grow");
        }
    }

    #[test]
    fn set_cap_shrinks_evicting_lru() {
        let mut c = cache(10);
        for k in ["a", "b", "c", "d", "e"] {
            c.try_request(k);
            c.insert_ready(k, empty_cached());
        }
        c.set_cap(3);
        assert_eq!(c.len(), 3);
        // 'a','b' are least recently used and must go.
        assert!(c.get("a").is_none());
        assert!(c.get("b").is_none());
        for k in ["c", "d", "e"] {
            assert!(c.get(k).is_some());
        }
    }

    #[test]
    fn set_cap_clamps_to_ceiling() {
        let mut c = cache(2);
        c.set_cap(usize::MAX);
        c.try_request("a");
        c.try_request("b");
        c.try_request("c");
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn request_dedupes_in_flight_and_failed() {
        let mut c = cache(4);
        assert!(c.try_request("a"));
        assert!(!c.try_request("a"));
        c.insert_failed("a", "boom".to_string());
        assert!(!c.try_request("a"), "failed entries are not refetched");
        c.try_request("b");
        c.insert_ready(
            "b",
            CachedPreview {
                frames: Vec::new(),
                delays: Vec::new(),
                loaded_at: Instant::now(),
                bytes: Arc::new(Vec::new()),
            },
        );
        assert!(!c.try_request("b"), "ready entries are not refetched");
    }

    #[test]
    fn decode_non_gif_returns_none() {
        let picker = Picker::halfblocks();
        let png = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert!(decode_preview(&png, &picker).unwrap().is_none());
        assert!(decode_preview(b"", &picker).unwrap().is_none());
        assert!(
            decode_preview_at(&png, &picker, Size::new(40, 20), FOCUS_RESIZE)
                .unwrap()
                .is_none()
        );
    }

    fn two_frame_gif() -> Vec<u8> {
        let mut gif_bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif_bytes);
            encoder
                .set_repeat(image::codecs::gif::Repeat::Infinite)
                .unwrap();
            let f1: image::RgbaImage = ImageBuffer::from_pixel(20, 10, Rgba([10, 10, 10, 255]));
            let f2: image::RgbaImage = ImageBuffer::from_pixel(20, 10, Rgba([200, 200, 200, 255]));
            encoder.encode_frame(image::Frame::new(f1)).unwrap();
            encoder.encode_frame(image::Frame::new(f2)).unwrap();
        }
        gif_bytes
    }

    #[test]
    fn decode_gif_frame_counts() {
        let picker = Picker::halfblocks();
        let gif_bytes = two_frame_gif();
        let cached = decode_preview(&gif_bytes, &picker)
            .unwrap()
            .expect("should decode");
        assert_eq!(cached.frame_count(), 2);
    }

    #[test]
    fn decode_preview_at_larger_size_keeps_frames_and_bytes() {
        let picker = Picker::halfblocks();
        let gif_bytes = two_frame_gif();
        let cached = decode_preview_at(&gif_bytes, &picker, Size::new(40, 20), FOCUS_RESIZE)
            .unwrap()
            .expect("should decode");
        assert_eq!(cached.frame_count(), 2);
        assert_eq!(cached.raw_bytes().as_slice(), gif_bytes.as_slice());
        // The enlarged decode fits within the requested cell size.
        let size = cached.frame(Instant::now()).size();
        assert!(size.width <= 40 && size.height <= 20);
    }

    #[test]
    fn focus_resize_upscales_small_gifs_but_fit_does_not() {
        let picker = Picker::halfblocks();
        let gif_bytes = two_frame_gif();

        // Fit: the 20x10px source already fits 40x20 cells, so it stays at
        // its natural size (this was the "tiny gif in the overlay corner"
        // bug).
        let fitted = decode_preview_at(&gif_bytes, &picker, Size::new(40, 20), GRID_RESIZE)
            .unwrap()
            .expect("should decode");
        let fitted_size = fitted.frame(Instant::now()).size();
        assert!(
            fitted_size.width < 40,
            "fit keeps natural size, got {fitted_size:?}"
        );

        // Scale: the same source is upscaled towards the requested size.
        let scaled = decode_preview_at(&gif_bytes, &picker, Size::new(40, 20), FOCUS_RESIZE)
            .unwrap()
            .expect("should decode");
        let scaled_size = scaled.frame(Instant::now()).size();
        assert!(
            scaled_size.width > fitted_size.width,
            "scale enlarges beyond natural size, got {scaled_size:?} vs {fitted_size:?}"
        );
    }

    #[test]
    fn centered_area_centers_and_clamps() {
        let area = Rect::new(10, 5, 20, 10);
        assert_eq!(
            centered_area(area, Size::new(10, 4)),
            Rect::new(15, 8, 10, 4)
        );
        // Oversized images clamp to the full area.
        assert_eq!(
            centered_area(area, Size::new(50, 50)),
            Rect::new(10, 5, 20, 10)
        );
        // Odd offsets floor the position.
        assert_eq!(centered_area(area, Size::new(5, 5)), Rect::new(17, 7, 5, 5));
    }

    #[test]
    fn focus_key_separates_sizes_without_url_collisions() {
        assert_eq!(focus_key("u", Size::new(10, 5)), "u#10x5");
        assert_ne!(
            focus_key("u", Size::new(10, 5)),
            focus_key("u", Size::new(11, 5))
        );
        // Different urls at sizes that could tile a suffix never collide.
        assert_ne!(
            focus_key("u#1x1", Size::new(2, 2)),
            focus_key("u", Size::new(1, 1))
        );
        assert!(key_for_url("u#10x5", "u"));
        assert!(!key_for_url("u#10x5", "v"));
        assert!(!key_for_url("u", "u"));
    }

    fn one_frame_cached() -> CachedPreview {
        let picker = Picker::halfblocks();
        let img: DynamicImage = ImageBuffer::from_pixel(10, 10, Rgba([255u8, 0, 0, 255])).into();
        let proto = picker
            .new_protocol(img, PREVIEW_SIZE, Resize::Fit(None))
            .unwrap();
        CachedPreview {
            frames: vec![proto],
            delays: vec![Duration::from_millis(100)],
            loaded_at: Instant::now(),
            bytes: Arc::new(Vec::new()),
        }
    }

    #[test]
    fn resize_starts_new_decode_while_old_frames_render() {
        let mut c = cache(4);
        let small = focus_key("u", Size::new(10, 5));
        let large = focus_key("u", Size::new(20, 10));

        // Initial decode at the small size.
        assert!(c.try_request(&small));
        c.insert_ready(&small, empty_cached());
        assert!(matches!(c.get(&small), Some(PreviewEntry::Ready(_))));

        // Terminal resized: the large key is a brand-new request (not
        // deduped against the small entry)…
        assert!(c.try_request(&large));
        // …and while it is in flight the newest ready frames for the url
        // (the small decode, 0 frames) still render.
        assert_eq!(c.newest_ready_for("u").unwrap().frame_count(), 0);

        // Once the large decode lands it becomes the newest.
        c.insert_ready(&large, one_frame_cached());
        assert_eq!(c.newest_ready_for("u").unwrap().frame_count(), 1);
        assert!(matches!(c.get(&large), Some(PreviewEntry::Ready(_))));
        // The old-size entry survives until LRU eviction.
        assert!(matches!(c.get(&small), Some(PreviewEntry::Ready(_))));
    }

    #[test]
    fn newest_ready_for_ignores_other_urls() {
        let mut c = cache(4);
        c.try_request("other#1x1");
        c.insert_ready("other#1x1", empty_cached());
        c.try_request(&focus_key("u", Size::new(2, 2)));
        c.insert_ready(&focus_key("u", Size::new(2, 2)), empty_cached());
        assert!(c.newest_ready_for("u").is_some());
        assert!(c.newest_ready_for("v").is_none());
    }
}
