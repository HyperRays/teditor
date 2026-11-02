//! Shelf-packed GPU texture atlases.

use super::{Color, RenderError};

pub(super) const ATLAS_SIZE: u32 = 2048;

#[derive(Clone, Copy, Debug)]
pub(super) struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    pub fn uv(self) -> [f32; 4] {
        let size = ATLAS_SIZE as f32;
        [
            self.x as f32 / size,
            self.y as f32 / size,
            self.width as f32 / size,
            self.height as f32 / size,
        ]
    }
}

/// Shelf packing with a one-pixel gutter around every allocation.
#[derive(Default)]
pub(super) struct Packer {
    x: u32,
    y: u32,
    row_height: u32,
}

impl Packer {
    /// Height consumed so far, including the current shelf.
    pub fn used_height(&self) -> u32 {
        self.y + self.row_height
    }
    pub fn allocate(&mut self, width: u32, height: u32) -> Result<Region, RenderError> {
        if width == 0 || height == 0 || width > ATLAS_SIZE - 2 || height > ATLAS_SIZE - 2 {
            return Err(RenderError::InvalidImageSize);
        }
        let (w, h) = (width + 2, height + 2);
        let (x, y, row_height) = if self.x + w > ATLAS_SIZE {
            (0, self.y + self.row_height, 0)
        } else {
            (self.x, self.y, self.row_height)
        };
        if y + h > ATLAS_SIZE {
            return Err(RenderError::AtlasFull);
        }
        self.x = x + w;
        self.y = y;
        self.row_height = row_height.max(h);
        Ok(Region {
            x: x + 1,
            y: y + 1,
            width,
            height,
        })
    }
}

pub(super) struct Atlas {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub packer: Packer,
    channels: u32,
}

impl Atlas {
    pub fn new(
        device: &wgpu::Device,
        label: &str,
        format: wgpu::TextureFormat,
        channels: u32,
    ) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Self {
            texture,
            view,
            packer: Packer::default(),
            channels,
        }
    }

    pub fn insert(
        &mut self,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Result<Region, RenderError> {
        self.validate(width, height, data)?;
        let region = self.packer.allocate(width, height)?;
        self.write(queue, region, data);
        Ok(region)
    }

    pub fn validate(&self, width: u32, height: u32, data: &[u8]) -> Result<(), RenderError> {
        if width == 0 || height == 0 || width > ATLAS_SIZE - 2 || height > ATLAS_SIZE - 2 {
            return Err(RenderError::InvalidImageSize);
        }
        if data.len() != (width * height * self.channels) as usize {
            return Err(RenderError::InvalidImageData);
        }
        Ok(())
    }

    pub fn write(&self, queue: &wgpu::Queue, region: Region, data: &[u8]) {
        let c = self.channels as usize;
        let stride = (region.width as usize + 2) * c;
        let mut padded = vec![0; stride * (region.height as usize + 2)];
        for y in 0..region.height as usize {
            let source = &data[y * region.width as usize * c..(y + 1) * region.width as usize * c];
            let start = (y + 1) * stride + c;
            padded[start..start + source.len()].copy_from_slice(source);
            if c == 4 {
                // Premultiply in linear space BEFORE filtering, so the RGB of
                // transparent texels cannot produce colored fringes on images.
                for pixel in padded[start..start + source.len()].chunks_exact_mut(4) {
                    let linear = Color(pixel[0], pixel[1], pixel[2], pixel[3]).linear();
                    for channel in 0..3 {
                        pixel[channel] = encode_srgb(linear[channel] * linear[3]);
                    }
                }
                // Extrude the premultiplied edge pixels into the filtering gutter.
                padded.copy_within(start..start + c, start - c);
                let end = start + source.len();
                padded.copy_within(end - c..end, end);
            }
        }
        if c == 4 {
            padded.copy_within(stride..2 * stride, 0);
            let last = region.height as usize * stride;
            padded.copy_within(last..last + stride, last + stride);
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: region.x - 1,
                    y: region.y - 1,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &padded,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride as u32),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: region.width + 2,
                height: region.height + 2,
                depth_or_array_layers: 1,
            },
        );
    }
}

pub(super) fn encode_srgb(linear: f32) -> u8 {
    let encoded = if linear <= 0.0031308 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round() as u8
}
