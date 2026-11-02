//! CPU copies and GPU handles for application bitmaps, behind stable [`BitmapId`]s.

use crate::renderer::{Bitmap, RenderError, Renderer};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::BufReader,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_ASSET: AtomicU64 = AtomicU64::new(1);

/// Stable application handle. Handles are never reused, including after removal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BitmapId(u64);

/// The size of an uploaded bitmap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitmapInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes of pixel data.
    pub bytes: usize,
}

/// Why a bitmap operation failed.
#[derive(Debug, thiserror::Error)]
pub enum AssetError {
    /// The handle is unknown or the bitmap was removed.
    #[error("unknown or removed bitmap asset")]
    UnknownBitmap,
    /// The renderer rejected the image.
    #[error(transparent)]
    Render(#[from] RenderError),
    /// The file could not be read.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The file is not a valid PNG.
    #[error(transparent)]
    Png(#[from] png::DecodingError),
}

struct Asset {
    size: [u32; 2],
    pixels: Vec<u8>,
    gpu: Option<Bitmap>,
}

#[derive(Default)]
pub(super) struct Assets {
    entries: BTreeMap<BitmapId, Asset>,
    pub changed: BTreeSet<BitmapId>,
}

impl Assets {
    pub fn insert(
        &mut self,
        renderer: &mut Renderer,
        size: [u32; 2],
        pixels: &[u8],
    ) -> Result<BitmapId, AssetError> {
        validate(size, pixels)?;
        let id = BitmapId(NEXT_ASSET.fetch_add(1, Ordering::Relaxed));
        let asset = Asset {
            size,
            pixels: pixels.to_vec(),
            gpu: None,
        };
        let handles = self.repack(renderer, Some((id, &asset)), None)?;
        self.entries.insert(id, asset);
        self.install(handles);
        self.changed.insert(id);
        Ok(id)
    }

    pub fn update(
        &mut self,
        renderer: &mut Renderer,
        id: BitmapId,
        size: [u32; 2],
        pixels: &[u8],
    ) -> Result<(), AssetError> {
        validate(size, pixels)?;
        let old = self.entries.get(&id).ok_or(AssetError::UnknownBitmap)?;
        if old.size == size && old.pixels == pixels {
            return Ok(());
        }
        if old.size == size {
            renderer.update_bitmap(old.gpu.expect("installed asset"), pixels)?;
            self.entries
                .get_mut(&id)
                .unwrap()
                .pixels
                .copy_from_slice(pixels);
        } else {
            let asset = Asset {
                size,
                pixels: pixels.to_vec(),
                gpu: None,
            };
            let handles = self.repack(renderer, Some((id, &asset)), Some(id))?;
            self.entries.insert(id, asset);
            self.install(handles);
        }
        self.changed.insert(id);
        Ok(())
    }

    pub fn remove(&mut self, renderer: &mut Renderer, id: BitmapId) -> Result<(), AssetError> {
        if !self.entries.contains_key(&id) {
            return Err(AssetError::UnknownBitmap);
        }
        let handles = self.repack(renderer, None, Some(id))?;
        self.entries.remove(&id);
        self.install(handles);
        self.changed.insert(id);
        Ok(())
    }

    pub fn bitmap(&self, id: BitmapId) -> Result<Bitmap, AssetError> {
        self.entries
            .get(&id)
            .and_then(|asset| asset.gpu)
            .ok_or(AssetError::UnknownBitmap)
    }

    pub fn info(&self, id: BitmapId) -> Option<BitmapInfo> {
        self.entries.get(&id).map(|asset| BitmapInfo {
            width: asset.size[0],
            height: asset.size[1],
            bytes: asset.pixels.len(),
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn repack(
        &self,
        renderer: &mut Renderer,
        extra: Option<(BitmapId, &Asset)>,
        exclude: Option<BitmapId>,
    ) -> Result<Vec<(BitmapId, Bitmap)>, AssetError> {
        let mut entries: Vec<_> = self
            .entries
            .iter()
            .filter(|(id, _)| Some(**id) != exclude)
            .map(|(&id, asset)| (id, asset))
            .chain(extra)
            .collect();
        entries.sort_by_key(|(id, asset)| {
            (
                std::cmp::Reverse(asset.size[1]),
                std::cmp::Reverse(asset.size[0]),
                *id,
            )
        });
        let images: Vec<_> = entries
            .iter()
            .map(|(_, asset)| (asset.size[0], asset.size[1], asset.pixels.as_slice()))
            .collect();
        let handles = renderer.rebuild_bitmaps(&images)?;
        Ok(entries.iter().map(|(id, _)| *id).zip(handles).collect())
    }

    fn install(&mut self, handles: Vec<(BitmapId, Bitmap)>) {
        for (id, gpu) in handles {
            self.entries.get_mut(&id).unwrap().gpu = Some(gpu);
        }
    }
}

fn validate([width, height]: [u32; 2], pixels: &[u8]) -> Result<(), RenderError> {
    if width == 0 || height == 0 || width > 2046 || height > 2046 {
        return Err(RenderError::InvalidImageSize);
    }
    if pixels.len() != (width * height * 4) as usize {
        return Err(RenderError::InvalidImageData);
    }
    Ok(())
}

/// Decode PNG data into tightly packed straight-alpha RGBA8, with bounded dimensions.
pub fn read_png(path: impl AsRef<Path>) -> Result<([u32; 2], Vec<u8>), AssetError> {
    let mut decoder = png::Decoder::new(BufReader::new(File::open(path)?));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    decoder.set_limits(png::Limits {
        bytes: 32 * 1024 * 1024,
    });
    let mut reader = decoder.read_info()?;
    let size = [reader.info().width, reader.info().height];
    if size.contains(&0) || size.iter().any(|&dimension| dimension > 2046) {
        return Err(RenderError::InvalidImageSize.into());
    }
    let mut bytes = vec![
        0;
        reader
            .output_buffer_size()
            .ok_or(RenderError::InvalidImageSize)?
    ];
    let output = reader.next_frame(&mut bytes)?;
    let bytes = &bytes[..output.buffer_size()];
    let mut rgba = Vec::with_capacity((size[0] * size[1] * 4) as usize);
    match output.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(bytes),
        png::ColorType::Rgb => {
            for p in bytes.chunks_exact(3) {
                rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for &p in bytes {
                rgba.extend_from_slice(&[p, p, p, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for p in bytes.chunks_exact(2) {
                rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
            }
        }
        png::ColorType::Indexed => unreachable!("EXPAND converts indexed PNGs"),
    }
    Ok((size, rgba))
}
