//! Inline images on the grid.
//!
//! The Engine marks every cell an image covers with an [`ImageAnchor`] link
//! (see `diri_proto::inline_image`), so placements arrive, scroll and vanish
//! with the rows that carry them. This module turns those anchors into
//! clipped image draws and keeps the decoded pixels: a per-terminal cache with
//! a hard byte budget that drops the least recently drawn image first. A key
//! the cache has never seen is handed once to the host's requester, which
//! reads the bytes from the Engine, decodes them off the main thread with
//! [`decode`] and stores the result.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use diri_proto::grid::LinkSpan;
use diri_proto::inline_image::{ImageAnchor, InlineImageFormat, is_anchor};
use gpui::{Bounds, Pixels, RenderImage, point, px, size};

/// Decoded bytes one terminal may keep before evicting.
pub const CACHE_BUDGET_BYTES: usize = 96 * 1024 * 1024;

type Requester = Box<dyn Fn(String) + Send + Sync>;

enum Entry {
    /// Requested; the bytes are on their way.
    Pending,
    /// Unknown to the Engine (evicted, deleted) or undecodable. Not retried.
    Failed,
    Ready(Arc<RenderImage>, usize),
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    /// Ready keys, least recently drawn first.
    order: VecDeque<String>,
    bytes: usize,
    /// Evicted images still in the sprite atlas, released at the next paint.
    retired: Vec<Arc<RenderImage>>,
}

/// Decoded images for one terminal. Without a requester (a remote session,
/// a preview) unknown keys stay unknown and nothing is drawn.
#[derive(Default)]
pub struct InlineImageCache {
    inner: Mutex<Inner>,
    requester: Mutex<Option<Requester>>,
}

impl InlineImageCache {
    /// Installs the fetcher for unknown keys. It must not block: it runs
    /// during prepaint.
    pub fn set_requester(&self, requester: impl Fn(String) + Send + Sync + 'static) {
        *lock(&self.requester) = Some(Box::new(requester));
    }

    /// The decoded image for `key`, marking it recently used. An unseen key is
    /// requested once.
    pub fn lookup(&self, key: &str) -> Option<Arc<RenderImage>> {
        let mut inner = lock(&self.inner);
        match inner.entries.get(key) {
            Some(Entry::Ready(image, _)) => {
                let image = Arc::clone(image);
                if inner.order.back().map(String::as_str) != Some(key)
                    && let Some(position) = inner.order.iter().position(|ready| ready == key)
                {
                    let key = inner.order.remove(position).expect("position");
                    inner.order.push_back(key);
                }
                Some(image)
            }
            Some(Entry::Pending | Entry::Failed) => None,
            None => {
                let requester = lock(&self.requester);
                if let Some(request) = requester.as_ref() {
                    inner.entries.insert(key.to_owned(), Entry::Pending);
                    drop(inner);
                    request(key.to_owned());
                }
                None
            }
        }
    }

    pub fn insert(&self, key: &str, image: Arc<RenderImage>) {
        let pixels = image.size(0);
        let size = pixels.width.0.max(0) as usize * pixels.height.0.max(0) as usize * 4;
        let mut inner = lock(&self.inner);
        while inner.bytes + size > CACHE_BUDGET_BYTES {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            // Back to unknown: drawn again later, it is fetched again.
            if let Some(Entry::Ready(image, bytes)) = inner.entries.remove(&oldest) {
                inner.bytes -= bytes;
                inner.retired.push(image);
            }
        }
        if let Some(Entry::Ready(previous, bytes)) = inner.entries.remove(key) {
            inner.bytes -= bytes;
            inner.retired.push(previous);
            inner.order.retain(|ready| ready != key);
        }
        inner.bytes += size;
        inner.order.push_back(key.to_owned());
        inner
            .entries
            .insert(key.to_owned(), Entry::Ready(image, size));
    }

    pub fn fail(&self, key: &str) {
        lock(&self.inner)
            .entries
            .insert(key.to_owned(), Entry::Failed);
    }

    /// Decoded bytes held.
    pub fn bytes(&self) -> usize {
        lock(&self.inner).bytes
    }

    pub(crate) fn take_retired(&self) -> Vec<Arc<RenderImage>> {
        std::mem::take(&mut lock(&self.inner).retired)
    }
}

impl std::fmt::Debug for InlineImageCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InlineImageCache")
            .field("bytes", &self.bytes())
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Decodes stored image bytes into a drawable image. Run it off the main
/// thread: a large PNG takes milliseconds.
pub fn decode(
    format: InlineImageFormat,
    compressed: bool,
    width: u32,
    height: u32,
    bytes: &[u8],
) -> Result<Arc<RenderImage>, String> {
    let rgba = match format.raw_channels() {
        Some(channels) => {
            let expected = width as usize * height as usize * channels;
            let inflated;
            let raw = if compressed {
                inflated = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(bytes, expected)
                    .map_err(|error| format!("inflate: {error:?}"))?;
                inflated.as_slice()
            } else {
                bytes
            };
            if raw.len() < expected {
                return Err("short pixel data".into());
            }
            let rgba = if channels == 4 {
                raw[..expected].to_vec()
            } else {
                raw[..expected]
                    .chunks_exact(3)
                    .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 0xff])
                    .collect()
            };
            image::RgbaImage::from_raw(width, height, rgba).ok_or("bad dimensions")?
        }
        None => {
            let format = match format {
                InlineImageFormat::Png => image::ImageFormat::Png,
                InlineImageFormat::Jpeg => image::ImageFormat::Jpeg,
                _ => image::ImageFormat::Gif,
            };
            image::load_from_memory_with_format(bytes, format)
                .map_err(|error| error.to_string())?
                .into_rgba8()
        }
    };
    let mut bgra = rgba;
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(bgra)])))
}

/// One clipped image draw.
#[derive(Clone, Debug)]
pub(crate) struct ImageDraw {
    pub image: Arc<RenderImage>,
    /// The whole image, fitted into its cell box.
    pub bounds: Bounds<Pixels>,
    /// The cells of the rows that still carry it.
    pub clip: Bounds<Pixels>,
}

/// Geometry shared by every row of a frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DrawGrid {
    pub origin: gpui::Point<Pixels>,
    pub cell_width: Pixels,
    pub line_height: Pixels,
    pub visible_cols: usize,
    pub scale_factor: f32,
}

/// Appends the draws for one window row's anchor links. Consecutive rows of
/// the same placement with the same span extend one clip.
pub(crate) fn append_row_draws(
    cache: &InlineImageCache,
    links: &[LinkSpan],
    window_row: usize,
    grid: DrawGrid,
    draws: &mut Vec<ImageDraw>,
) {
    for link in links {
        if !is_anchor(&link.uri) {
            continue;
        }
        let Some(anchor) = ImageAnchor::parse(&link.uri) else {
            continue;
        };
        let start = usize::from(link.start).min(grid.visible_cols);
        let end = usize::from(link.end).min(grid.visible_cols);
        if start >= end {
            continue;
        }
        let Some(image) = cache.lookup(&anchor.key) else {
            continue;
        };
        let top = window_row as f32 - f32::from(anchor.row);
        let cell_box = Bounds::new(
            point(
                grid.origin.x + grid.cell_width * f32::from(anchor.col),
                grid.origin.y + grid.line_height * top,
            ),
            size(
                grid.cell_width * f32::from(anchor.cols),
                grid.line_height * f32::from(anchor.rows),
            ),
        );
        let bounds = fit(&image, cell_box, grid.scale_factor);
        let clip = Bounds::new(
            point(
                grid.origin.x + grid.cell_width * start as f32,
                grid.origin.y + grid.line_height * window_row as f32,
            ),
            size(grid.cell_width * (end - start) as f32, grid.line_height),
        );
        if let Some(last) = draws.last_mut()
            && Arc::ptr_eq(&last.image, &image)
            && last.bounds == bounds
            && last.clip.origin.x == clip.origin.x
            && last.clip.size.width == clip.size.width
            && last.clip.bottom() == clip.top()
        {
            last.clip.size.height += clip.size.height;
            continue;
        }
        draws.push(ImageDraw {
            image,
            bounds,
            clip,
        });
    }
}

/// The image scaled to fit its cell box, top-left aligned, aspect kept. It is
/// never drawn larger than one image pixel per device pixel unless the box
/// asks for it, so a box rounded up to whole cells does not blur the image.
fn fit(image: &RenderImage, cell_box: Bounds<Pixels>, scale_factor: f32) -> Bounds<Pixels> {
    let pixels = image.size(0);
    let (width, height) = (pixels.width.0 as f32, pixels.height.0 as f32);
    if width <= 0.0 || height <= 0.0 {
        return cell_box;
    }
    let scale =
        (f32::from(cell_box.size.width) / width).min(f32::from(cell_box.size.height) / height);
    // Within one cell of native size the box only rounded up: stay sharp.
    let native = 1.0 / scale_factor.max(1.0);
    let scale = if scale > native && scale < native * 1.25 {
        native
    } else {
        scale
    };
    Bounds::new(cell_box.origin, size(px(width * scale), px(height * scale)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn decodes_png_and_raw_pixels_into_bgra() {
        let image = decode(InlineImageFormat::Png, false, 0, 0, &png(3, 2)).unwrap();
        assert_eq!(
            image.size(0),
            gpui::size(gpui::DevicePixels(3), gpui::DevicePixels(2))
        );
        assert_eq!(&image.as_bytes(0).unwrap()[..4], &[30, 20, 10, 255]);

        let rgb = [1, 2, 3, 4, 5, 6];
        let image = decode(InlineImageFormat::Rgb, false, 2, 1, &rgb).unwrap();
        assert_eq!(image.as_bytes(0).unwrap(), &[3, 2, 1, 255, 6, 5, 4, 255]);

        let rgba = [9, 8, 7, 6];
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&rgba, 6);
        let image = decode(InlineImageFormat::Rgba, true, 1, 1, &compressed).unwrap();
        assert_eq!(image.as_bytes(0).unwrap(), &[7, 8, 9, 6]);

        assert!(decode(InlineImageFormat::Rgba, false, 2, 2, &rgba).is_err());
        assert!(decode(InlineImageFormat::Png, false, 0, 0, b"junk").is_err());
    }

    #[test]
    fn unknown_keys_are_requested_once_and_failures_are_not_retried() {
        let cache = InlineImageCache::default();
        assert!(cache.lookup("k.1").is_none(), "no requester: nothing asked");
        let asked = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&asked);
        cache.set_requester(move |key| record.lock().unwrap().push(key));
        for _ in 0..3 {
            assert!(cache.lookup("k.2").is_none());
        }
        cache.fail("k.2");
        assert!(cache.lookup("k.2").is_none());
        assert_eq!(*asked.lock().unwrap(), vec!["k.2".to_owned()]);
        let image = decode(InlineImageFormat::Rgba, false, 1, 1, &[0; 4]).unwrap();
        cache.insert("k.2", Arc::clone(&image));
        assert!(Arc::ptr_eq(&cache.lookup("k.2").unwrap(), &image));
    }

    #[test]
    fn the_least_recently_drawn_image_is_evicted_past_the_budget() {
        let cache = InlineImageCache::default();
        cache.set_requester(|_| {});
        // 2048×2048 RGBA is 16 MiB; the budget holds six.
        let pixels = vec![0u8; 2048 * 2048 * 4];
        let image = || decode(InlineImageFormat::Rgba, false, 2048, 2048, &pixels).unwrap();
        for index in 0..6 {
            cache.insert(&format!("k.{index}"), image());
        }
        assert!(cache.lookup("k.0").is_some(), "drawn: now the newest");
        cache.insert("k.6", image());
        assert!(cache.bytes() <= CACHE_BUDGET_BYTES);
        assert!(cache.lookup("k.0").is_some());
        assert!(
            cache.lookup("k.1").is_none(),
            "least recently drawn went first"
        );
        assert_eq!(cache.take_retired().len(), 1);
    }

    #[test]
    fn rows_of_one_placement_merge_into_one_clipped_draw() {
        let cache = InlineImageCache::default();
        cache.set_requester(|_| {});
        let image = decode(
            InlineImageFormat::Rgba,
            false,
            40,
            40,
            &vec![0; 40 * 40 * 4],
        )
        .unwrap();
        cache.insert("k.1", image);
        let grid = DrawGrid {
            origin: point(px(0.0), px(0.0)),
            cell_width: px(10.0),
            line_height: px(20.0),
            visible_cols: 80,
            scale_factor: 2.0,
        };
        let row = |row: u16, start: u16, end: u16| {
            vec![LinkSpan {
                start,
                end,
                uri: ImageAnchor {
                    key: "k.1".into(),
                    row,
                    col: 2,
                    cols: 4,
                    rows: 3,
                }
                .to_uri(),
            }]
        };
        let mut draws = Vec::new();
        // The image's first row scrolled off: window rows 0 and 1 hold its
        // rows 1 and 2; the last row was partly overwritten.
        append_row_draws(&cache, &row(1, 2, 6), 0, grid, &mut draws);
        append_row_draws(&cache, &row(2, 2, 6), 1, grid, &mut draws);
        append_row_draws(&cache, &row(2, 4, 6), 2, grid, &mut draws);
        assert_eq!(draws.len(), 2);
        // The whole image's box starts one row above the window.
        assert_eq!(draws[0].bounds.origin, point(px(20.0), px(-20.0)));
        // A 40×40 px image in its 40×60 pt box fills the width.
        assert_eq!(draws[0].bounds.size, size(px(40.0), px(40.0)));
        assert_eq!(
            draws[0].clip,
            Bounds::new(point(px(20.0), px(0.0)), size(px(40.0), px(40.0)))
        );
        assert_eq!(draws[1].clip.origin, point(px(40.0), px(40.0)));

        let plain = vec![LinkSpan {
            start: 0,
            end: 3,
            uri: "https://example.com".into(),
        }];
        append_row_draws(&cache, &plain, 3, grid, &mut draws);
        assert_eq!(draws.len(), 2, "ordinary links draw nothing");
    }
}
