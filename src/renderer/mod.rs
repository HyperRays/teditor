//! Batched GPU rendering of cell-aligned text, colors, RGBA bitmaps and antialiased shapes.
//!
//! This layer draws a display list; document storage, shaping, terminal escape
//! sequences, widgets, and input dispatch belong above it. It does not depend on `winit`.
//!
//! Drawing takes whole-cell coordinates, which the renderer converts to physical
//! pixels using the current font metrics. Only complete cells fit in the viewport;
//! leftover pixels along the right and bottom edges keep the frame's clear color.
//!
//! ```
//! use teditor::renderer::{CellRect, Color, Frame};
//!
//! let mut frame = Frame::new(Color::rgb(17, 23, 33));
//! frame.fill(CellRect::new(0, 0, 80, 1), Color::rgb(27, 36, 49));
//! frame.text(2, 0, "hello, grid", Color::rgb(116, 222, 190));
//! // renderer.render(&target_view, [physical_width, physical_height], &frame)?;
//! ```
//!
//! # How frames are drawn
//!
//! Commands keep their order and become instances in one render pass and one draw.
//! Colors are sRGB and blended in linear space, so the target must be an sRGB format.
//! It stores premultiplied alpha; bitmap uploads take straight alpha and are premultiplied
//! before filtering, which avoids colored fringes around transparent edges.
//!
//! - **Glyphs** are rasterized by `ab_glyph` at the physical display scale and cached in
//!   an R8 atlas. A missing glyph draws as `?`.
//! - **Block and box characters** (U+2500–259F) are generated from the cell's dimensions,
//!   because font outlines stop short of the line spacing and could not tile without gaps.
//! - **Emoji** come from the bundled Noto Color Emoji, scaled on first use into their own
//!   atlas. It is emptied between frames once half full; the terminal's retained image
//!   keeps what is already on screen.
//! - **Images** use a separate sRGB atlas. Exhaustion is the explicit error
//!   [`RenderError::AtlasFull`], never a silent eviction.
//!
//! # Unicode
//!
//! Characters the main font lacks come from installed fonts, found on first use. A grapheme
//! cluster reaches the renderer as one [`symbol`], and is shaped as a unit, so combining
//! marks, flags and joined emoji draw as one picture. Text is not shaped across clusters, so
//! Arabic letters show their isolated forms, and right-to-left text is drawn left to right.
//! Device-loss recovery and atlas growth are future work.

mod atlas;
mod cluster;
mod emoji;
mod fallback;
mod frame;
mod graphic;
mod sprite;

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

use ab_glyph::{Font, FontRef, FontVec, GlyphId, ScaleFont, point};
use bytemuck::{Pod, Zeroable};

use atlas::{Atlas, Packer, Region};
pub use cluster::{
    Columns, char_columns, cluster_text, for_each_cluster, is_joinable, is_symbol, symbol,
    text_columns,
};
use frame::Content;
pub use frame::{Bitmap, CellRect, Color, Frame};
pub use graphic::Graphic;

/// The font used when an application supplies none: JetBrains Mono.
pub const DEFAULT_FONT: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");
static NEXT_RENDERER_ID: AtomicU64 = AtomicU64::new(1);
/// Instance kind for color emoji; graphics use small values from 3 upward.
const KIND_EMOJI: u32 = 1000;

/// Why the renderer could not do what it was asked.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// The font data is invalid.
    #[error("the font could not be parsed")]
    InvalidFont,
    /// The font height is not usable.
    #[error("font height must be finite and between 1 and 512 physical pixels")]
    InvalidFontHeight,
    /// The target format is not sRGB.
    #[error("the renderer requires an sRGB color attachment")]
    NonSrgbTarget,
    /// An image is empty or too large.
    #[error("an image must have dimensions between 1 and 2046 pixels")]
    InvalidImageSize,
    /// An image's data has the wrong length.
    #[error("pixel data length does not match the image dimensions")]
    InvalidImageData,
    /// An atlas has no room left.
    #[error("the 2048x2048 texture atlas is full")]
    AtlasFull,
    /// A bitmap came from a different renderer.
    #[error("this bitmap belongs to another renderer")]
    ForeignBitmap,
    /// A bitmap predates the last atlas rebuild.
    #[error("this bitmap was invalidated by rebuilding the bitmap atlas")]
    StaleBitmap,
    /// A frame needs more instances than the device allows.
    #[error("the frame exceeds the device's instance buffer limit")]
    FrameTooLarge,
}

/// Integer physical-pixel dimensions of one cell at the current font scale.
#[derive(Clone, Copy, Debug)]
pub struct CellMetrics {
    /// Cell width in pixels.
    pub width: u32,
    /// Cell height in pixels.
    pub height: u32,
    /// Distance from a cell's top to the text baseline, in pixels.
    pub baseline: u32,
}

impl CellMetrics {
    /// How many whole `[columns, rows]` fit in a viewport.
    pub fn grid_size(self, viewport: [u32; 2]) -> [u32; 2] {
        [viewport[0] / self.width, viewport[1] / self.height]
    }

    /// Convert a physical pointer position to a cell. Rejects leftover edge pixels.
    pub fn cell_at(self, viewport: [u32; 2], position: [f64; 2]) -> Option<[u32; 2]> {
        if position.iter().any(|p| !p.is_finite() || *p < 0.0) {
            return None;
        }
        let grid = self.grid_size(viewport);
        if position[0] >= f64::from(grid[0]) * f64::from(self.width)
            || position[1] >= f64::from(grid[1]) * f64::from(self.height)
        {
            return None;
        }
        Some([
            (position[0] / f64::from(self.width)) as u32,
            (position[1] / f64::from(self.height)) as u32,
        ])
    }

    fn pixels(self, rect: CellRect) -> [f32; 4] {
        [
            rect.column as f32 * self.width as f32,
            rect.row as f32 * self.height as f32,
            rect.columns as f32 * self.width as f32,
            rect.rows as f32 * self.height as f32,
        ]
    }
}

#[derive(Clone, Copy)]
struct CachedGlyph {
    region: Region,
    offset: [f32; 2],
    advance: f32,
}

#[derive(Clone, Copy)]
struct CachedEmoji {
    region: Region,
    display: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Instance {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 4],
    clip: [f32; 4],
    kind: u32,
    secondary: [f32; 4],
    cell_size: [f32; 2],
}

/// A renderer for a single font and sRGB target format, independent of windowing.
///
/// Glyphs are rasterized once per character at the requested physical pixel height.
/// Changing that height resets only the glyph atlas, preserving uploaded bitmaps.
/// Both atlases are bounded; exhaustion returns `AtlasFull` instead of evicting
/// resources referenced by a frame. Bitmap updates reuse their original storage.
pub struct Renderer {
    id: u64,
    bitmap_generation: u64,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bindings: wgpu::BindGroup,
    viewport_buffer: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    instances: Vec<Instance>,
    glyph_atlas: Atlas,
    bitmap_atlas: Atlas,
    font: FontVec,
    shaping_font: harfrust::Font,
    font_height: f32,
    metrics: CellMetrics,
    /// Keyed by display width too, as a font's glyph may need shrinking to fit its cells.
    glyphs: HashMap<(char, u32), Option<CachedGlyph>>,
    emoji_font: FontRef<'static>,
    emoji_atlas: Atlas,
    /// Keyed by display width too, as a character can be drawn in one or two cells.
    emoji: HashMap<(char, u32), Option<CachedEmoji>>,
}

impl Renderer {
    /// `font_height` is the ascent-to-descent height in physical pixels. A window
    /// application should multiply its logical font height by the display scale.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        font_bytes: &[u8],
        font_height: f32,
    ) -> Result<Self, RenderError> {
        if !format.is_srgb() {
            return Err(RenderError::NonSrgbTarget);
        }
        let font =
            FontVec::try_from_vec(font_bytes.to_vec()).map_err(|_| RenderError::InvalidFont)?;
        let shaping_font =
            harfrust::Font::new(font_bytes.to_vec(), 0).ok_or(RenderError::InvalidFont)?;
        let metrics = font_metrics(&font, font_height)?;
        let glyph_atlas = Atlas::new(device, "glyph atlas", wgpu::TextureFormat::R8Unorm, 1);
        let bitmap_atlas = Atlas::new(
            device,
            "bitmap atlas",
            wgpu::TextureFormat::Rgba8UnormSrgb,
            4,
        );
        let emoji_font =
            FontRef::try_from_slice(emoji::EMOJI_FONT).map_err(|_| RenderError::InvalidFont)?;
        let emoji_atlas = Atlas::new(
            device,
            "emoji atlas",
            wgpu::TextureFormat::Rgba8UnormSrgb,
            4,
        );
        let viewport_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewport"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("grid bindings"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                texture_binding(1),
                texture_binding(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                texture_binding(4),
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let bindings = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("grid resources"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: viewport_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&glyph_atlas.view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&bitmap_atlas.view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&emoji_atlas.view),
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("grid pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("grid shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("grid.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cell renderer"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4,
                        2 => Float32x4, 3 => Float32x4, 4 => Uint32,
                        5 => Float32x4, 6 => Float32x2],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let instance_buffer = instance_buffer(device, size_of::<Instance>() as u64);
        Ok(Self {
            id: NEXT_RENDERER_ID.fetch_add(1, Ordering::Relaxed),
            bitmap_generation: 0,
            device: device.clone(),
            queue: queue.clone(),
            pipeline,
            bindings,
            viewport_buffer,
            instance_buffer,
            instances: Vec::new(),
            glyph_atlas,
            bitmap_atlas,
            font,
            shaping_font,
            font_height,
            metrics,
            glyphs: HashMap::new(),
            emoji_font,
            emoji_atlas,
            emoji: HashMap::new(),
        })
    }

    /// The size of one cell at the current font height.
    pub fn cell_metrics(&self) -> CellMetrics {
        self.metrics
    }

    pub(crate) fn metrics_at_height(&self, height: f32) -> Result<CellMetrics, RenderError> {
        font_metrics(&self.font, height)
    }

    /// Change the font height in physical pixels. Glyphs are redrawn; bitmaps survive.
    pub fn set_font_height(&mut self, font_height: f32) -> Result<(), RenderError> {
        let metrics = font_metrics(&self.font, font_height)?;
        if self.font_height != font_height {
            self.font_height = font_height;
            self.metrics = metrics;
            self.glyphs.clear();
            self.glyph_atlas.packer = Packer::default();
            self.clear_emoji();
        }
        Ok(())
    }

    /// Upload tightly packed, row-major sRGB RGBA8 pixels with straight alpha.
    pub fn upload_bitmap(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<Bitmap, RenderError> {
        let region = self.bitmap_atlas.insert(&self.queue, width, height, rgba)?;
        Ok(Bitmap {
            owner: self.id,
            generation: self.bitmap_generation,
            region,
        })
    }

    /// Replace all bitmap allocations, reclaiming unused atlas space. Planning is
    /// transactional: an error leaves existing handles valid. Success invalidates
    /// every old handle and returns new ones in input order.
    pub fn rebuild_bitmaps(
        &mut self,
        images: &[(u32, u32, &[u8])],
    ) -> Result<Vec<Bitmap>, RenderError> {
        let mut packer = Packer::default();
        let mut regions = Vec::with_capacity(images.len());
        for &(width, height, pixels) in images {
            self.bitmap_atlas.validate(width, height, pixels)?;
            regions.push(packer.allocate(width, height)?);
        }
        self.bitmap_generation += 1;
        for (&region, &(_, _, pixels)) in regions.iter().zip(images) {
            self.bitmap_atlas.write(&self.queue, region, pixels);
        }
        self.bitmap_atlas.packer = packer;
        Ok(regions
            .into_iter()
            .map(|region| Bitmap {
                owner: self.id,
                generation: self.bitmap_generation,
                region,
            })
            .collect())
    }

    /// Replace bitmap pixels without allocating atlas space. Dimensions stay fixed.
    pub fn update_bitmap(&self, bitmap: Bitmap, rgba: &[u8]) -> Result<(), RenderError> {
        self.check_bitmap(bitmap)?;
        self.bitmap_atlas
            .validate(bitmap.region.width, bitmap.region.height, rgba)?;
        self.bitmap_atlas.write(&self.queue, bitmap.region, rgba);
        Ok(())
    }

    /// Render and submit a complete frame in one pass and one instanced draw.
    /// The caller presents the surface only after this succeeds. Zero-sized targets
    /// are skipped. `viewport` must be the physical size of the supplied target.
    /// The target stores premultiplied alpha, including for transparent clear colors.
    pub fn render(
        &mut self,
        target: &wgpu::TextureView,
        viewport: [u32; 2],
        frame: &Frame,
    ) -> Result<(), RenderError> {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("grid frame"),
            });
        self.encode(target, viewport, frame, true, &mut encoder)?;
        self.queue.submit([encoder.finish()]);
        Ok(())
    }

    // Encode once per submission: queue writes populate the shared instance buffer.
    pub(crate) fn encode(
        &mut self,
        target: &wgpu::TextureView,
        viewport: [u32; 2],
        frame: &Frame,
        clear_target: bool,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<(), RenderError> {
        if viewport.contains(&0) {
            return Ok(());
        }
        self.prepare(viewport, frame)?;
        let bytes = bytemuck::cast_slice(&self.instances);
        let limit = self.device.limits().max_buffer_size;
        if bytes.len() as u64 > limit || self.instances.len() > u32::MAX as usize {
            return Err(RenderError::FrameTooLarge);
        }
        if bytes.len() as u64 > self.instance_buffer.size() {
            self.instance_buffer = instance_buffer(
                &self.device,
                (bytes.len() as u64).next_power_of_two().min(limit),
            );
        }
        if !bytes.is_empty() {
            self.queue.write_buffer(&self.instance_buffer, 0, bytes);
        }
        self.queue.write_buffer(
            &self.viewport_buffer,
            0,
            bytemuck::cast_slice(&[viewport[0] as f32, viewport[1] as f32, 0.0, 0.0]),
        );
        let clear = frame.clear.linear();
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("grid pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: if clear_target {
                            wgpu::LoadOp::Clear(wgpu::Color {
                                r: (clear[0] * clear[3]) as f64,
                                g: (clear[1] * clear[3]) as f64,
                                b: (clear[2] * clear[3]) as f64,
                                a: clear[3] as f64,
                            })
                        } else {
                            wgpu::LoadOp::Load
                        },
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if !self.instances.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bindings, &[]);
                pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
                pass.draw(0..6, 0..self.instances.len() as u32);
            }
        }
        Ok(())
    }

    fn clear_emoji(&mut self) {
        self.emoji.clear();
        self.emoji_atlas.packer = Packer::default();
    }

    fn prepare(&mut self, viewport: [u32; 2], frame: &Frame) -> Result<(), RenderError> {
        self.instances.clear();
        // Evict between frames, never during one, while room remains for this frame.
        if self.emoji_atlas.packer.used_height() > atlas::ATLAS_SIZE / 2 {
            self.clear_emoji();
        }
        let [columns, rows] = self.metrics.grid_size(viewport);
        let screen = CellRect::new(0, 0, columns, rows);
        for command in &frame.commands {
            let clip = command
                .rect
                .intersection(command.clip.unwrap_or(screen))
                .intersection(screen);
            if clip.columns == 0 || clip.rows == 0 {
                continue;
            }
            let [x, y, width, height] = self.metrics.pixels(clip);
            let mut instance = Instance {
                rect: self.metrics.pixels(command.rect),
                uv: [0.0; 4],
                color: [1.0; 4],
                clip: [x, y, x + width, y + height],
                kind: 0,
                secondary: [0.0; 4],
                cell_size: [self.metrics.width as f32, self.metrics.height as f32],
            };
            match command.content {
                Content::Fill(color) | Content::FillToEdge(color) => {
                    // Avoid very large offscreen geometry for clipped fills.
                    instance.rect = [x, y, width, height];
                    instance.color = color.linear();
                }
                Content::Glyph(character, color) => {
                    if let Some(emoji) = self.emoji_glyph(character, command.rect.columns)? {
                        let [x, y, width, height] = instance.rect;
                        let [w, h] = emoji.display.map(|size| size as f32);
                        instance.rect = [
                            x + ((width - w) * 0.5).floor(),
                            y + ((height - h) * 0.5).floor(),
                            w,
                            h,
                        ];
                        instance.uv = emoji.region.uv();
                        instance.kind = KIND_EMOJI;
                        self.instances.push(instance);
                        continue;
                    }
                    let Some(glyph) = self.glyph(character, command.rect.columns)? else {
                        continue;
                    };
                    instance.rect = [
                        instance.rect[0]
                            + glyph.offset[0]
                            + ((instance.rect[2] - glyph.advance) * 0.5).floor(),
                        instance.rect[1] + glyph.offset[1],
                        glyph.region.width as f32,
                        glyph.region.height as f32,
                    ];
                    instance.uv = glyph.region.uv();
                    instance.color = color.linear();
                    instance.kind = 1;
                }
                Content::Bitmap(bitmap, source) => {
                    self.check_bitmap(bitmap)?;
                    let uv = bitmap.region.uv();
                    instance.uv = [
                        uv[0] + source[0] * uv[2],
                        uv[1] + source[1] * uv[3],
                        source[2] * uv[2],
                        source[3] * uv[3],
                    ];
                    instance.kind = 2;
                }
                Content::Graphic(graphic) => {
                    instance.uv = graphic.parameters();
                    let (kind, color, secondary) = graphic.instance();
                    instance.kind = kind;
                    instance.color = color.linear();
                    instance.secondary = secondary.linear();
                }
            }
            // Shared pane edges continue through partial terminal cells. Damage
            // restoration uses the same extension so moving a divider erases its
            // previous pixels in the padding as well as in complete cells.
            if matches!(
                command.content,
                Content::FillToEdge(_)
                    | Content::Graphic(Graphic::Divider {
                        vertical: false,
                        ..
                    })
            ) && clip.column.saturating_add(clip.columns) == columns
            {
                instance.rect[2] += (viewport[0] - columns * self.metrics.width) as f32;
                instance.clip[2] = viewport[0] as f32;
            }
            if matches!(
                command.content,
                Content::FillToEdge(_) | Content::Graphic(Graphic::Divider { vertical: true, .. })
            ) && clip.row.saturating_add(clip.rows) == rows
            {
                instance.rect[3] += (viewport[1] - rows * self.metrics.height) as f32;
                instance.clip[3] = viewport[1] as f32;
            }
            self.instances.push(instance);
        }
        Ok(())
    }

    /// The color image for a character, if it should be drawn as one: when it is wide
    /// or asked to be, when it is a cluster, or when the main font does not have it.
    fn emoji_glyph(
        &mut self,
        character: char,
        columns: u32,
    ) -> Result<Option<CachedEmoji>, RenderError> {
        if character.is_ascii() {
            return Ok(None);
        }
        let wide = columns >= 2 || char_columns(character) == 2 || is_symbol(character);
        if !wide && self.font.glyph_id(character).0 != 0 {
            return Ok(None);
        }
        let key = (character, columns);
        if let Some(cached) = self.emoji.get(&key) {
            return Ok(*cached);
        }
        // Slightly short of the full row, so tall emoji keep the font's line spacing.
        let box_width = self.metrics.width * columns;
        let box_height = (self.font_height.ceil() as u32).min(self.metrics.height);
        let id = match cluster_text(character) {
            // A sequence is one picture only if the font's ligatures make it so.
            Some(text) => {
                let shaped =
                    harfrust::Font::new(emoji::EMOJI_FONT, 0).and_then(|font| shape(&font, &text));
                shaped
                    .and_then(|glyphs| glyphs.first().map(|glyph| glyph.0))
                    .unwrap_or(GlyphId(0))
            }
            None => self.emoji_font.glyph_id(character),
        };
        let cached = match emoji::rasterize(&self.emoji_font, id, box_width, box_height) {
            Some(image) => {
                match self
                    .emoji_atlas
                    .insert(&self.queue, image.width, image.height, &image.rgba)
                {
                    Ok(region) => Some(CachedEmoji {
                        region,
                        display: image.display,
                    }),
                    // Only possible with an enormous frame; draw what fits, then evict.
                    Err(RenderError::AtlasFull) => return Ok(None),
                    Err(error) => return Err(error),
                }
            }
            None => None,
        };
        self.emoji.insert(key, cached);
        Ok(cached)
    }

    fn glyph(&mut self, character: char, columns: u32) -> Result<Option<CachedGlyph>, RenderError> {
        if let Some(glyph) = self.glyphs.get(&(character, columns)) {
            return Ok(*glyph);
        }
        // Block and box characters must tile, so they come from the cell, not the font.
        let cell = [self.metrics.width, self.metrics.height];
        if let Some(pixels) = sprite::sprite(character, cell[0], cell[1]) {
            let region = self
                .glyph_atlas
                .insert(&self.queue, cell[0], cell[1], &pixels)?;
            let cached = Some(CachedGlyph {
                region,
                offset: [0.0, 0.0],
                advance: cell[0] as f32,
            });
            self.glyphs.insert((character, columns), cached);
            return Ok(cached);
        }
        let cluster = cluster_text(character);
        let mut scalar = [0; 4];
        let text: &str = match &cluster {
            Some(text) => text,
            None => character.encode_utf8(&mut scalar),
        };
        let image = self.rasterize_text(text, columns);
        let cached = match image {
            Some(image) => {
                let region = self.glyph_atlas.insert(
                    &self.queue,
                    image.width,
                    image.height,
                    &image.pixels,
                )?;
                Some(CachedGlyph {
                    region,
                    offset: image.offset,
                    advance: image.advance,
                })
            }
            None => None,
        };
        self.glyphs.insert((character, columns), cached);
        Ok(cached)
    }

    /// Draw text with the first font that has it: the main font, then installed fonts.
    /// When none does, a single `?` from the main font marks what is missing.
    fn rasterize_text(&self, text: &str, columns: u32) -> Option<TextImage> {
        let width = (self.metrics.width * columns.max(1)) as f32;
        let baseline = self.metrics.baseline as f32;
        if let Some(glyphs) = glyphs_of(&self.font, &self.shaping_font, text) {
            return draw_glyphs(&self.font, &glyphs, self.font_height, baseline, width);
        }
        if let Some(face) = fallback::find(text)
            && let Ok(font) = FontRef::try_from_slice_and_index(face.data, face.index)
            && let Some(shaping) = harfrust::Font::new(face.data, face.index)
            && let Some(glyphs) = glyphs_of(&font, &shaping, text)
        {
            let image = draw_glyphs(&font, &glyphs, self.font_height, baseline, width);
            // An icon has no baseline to share with text, so it goes in the middle of its cells.
            return image.map(|mut image| {
                if face.is_icons() {
                    image.offset = [
                        (width - image.width as f32) / 2.0,
                        (self.metrics.height as f32 - image.height as f32) / 2.0,
                    ];
                }
                image
            });
        }
        let id = self.font.glyph_id('?');
        let question = [(id, 0.0, 0.0, self.font.h_advance_unscaled(id))];
        draw_glyphs(&self.font, &question, self.font_height, baseline, width)
    }

    fn check_bitmap(&self, bitmap: Bitmap) -> Result<(), RenderError> {
        if bitmap.owner != self.id {
            return Err(RenderError::ForeignBitmap);
        }
        if bitmap.generation != self.bitmap_generation {
            return Err(RenderError::StaleBitmap);
        }
        Ok(())
    }
}

/// A glyph, then its x offset, y offset and advance in font units.
type Placed = (GlyphId, f32, f32, f32);

/// Arrange a cluster with a font's own rules for joining and placing its parts.
/// `None` means the font lacks part of it.
fn shape(font: &harfrust::Font, text: &str) -> Option<Vec<Placed>> {
    let shaper = harfrust::ShaperFont::new(font);
    let mut buffer = harfrust::Buffer::new();
    buffer.push_str(text);
    buffer.guess_segment_properties();
    harfrust::shape(&shaper, &mut buffer, Default::default()).ok()?;
    let placed: Vec<_> = buffer
        .glyph_infos()
        .iter()
        .zip(buffer.glyph_positions())
        .map(|(info, position)| {
            (
                GlyphId(info.glyph_id as u16),
                position.x_offset as f32,
                position.y_offset as f32,
                position.x_advance as f32,
            )
        })
        .collect();
    placed.iter().all(|glyph| glyph.0.0 != 0).then_some(placed)
}

/// The glyphs for `text` in one font, or `None` if it lacks any of them.
fn glyphs_of(font: &impl Font, shaping: &harfrust::Font, text: &str) -> Option<Vec<Placed>> {
    let mut scalars = text.chars();
    if let (Some(character), None) = (scalars.next(), scalars.next()) {
        let id = font.glyph_id(character);
        return (id.0 != 0).then(|| vec![(id, 0.0, 0.0, font.h_advance_unscaled(id))]);
    }
    shape(shaping, text)
}

/// A rasterized run of glyphs, ready for the atlas.
struct TextImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    offset: [f32; 2],
    advance: f32,
}

/// Rasterize glyphs along a baseline at `height`, shrinking any wider than `box_width`.
fn draw_glyphs(
    font: &impl Font,
    glyphs: &[Placed],
    height: f32,
    baseline: f32,
    box_width: f32,
) -> Option<TextImage> {
    let unit = height / font.height_unscaled();
    let natural = snapped(glyphs.iter().map(|glyph| glyph.3 * unit).sum());
    let shrink = if natural > box_width {
        box_width / natural
    } else {
        1.0
    };
    let scale = height * shrink;
    let unit = unit * shrink;
    let mut pen = 0.0;
    let mut outlines = Vec::with_capacity(glyphs.len());
    for &(id, x, y, advance) in glyphs {
        let position = point(pen + x * unit, baseline - y * unit);
        if let Some(outline) = font.outline_glyph(id.with_scale_and_position(scale, position)) {
            outlines.push(outline);
        }
        pen += advance * unit;
    }
    let bounds = outlines
        .iter()
        .map(|outline| outline.px_bounds())
        .reduce(|a, b| ab_glyph::Rect {
            min: point(a.min.x.min(b.min.x), a.min.y.min(b.min.y)),
            max: point(a.max.x.max(b.max.x), a.max.y.max(b.max.y)),
        })?;
    let (width, image_height) = (bounds.width() as u32, bounds.height() as u32);
    let mut pixels = vec![0u8; (width * image_height) as usize];
    for outline in &outlines {
        let own = outline.px_bounds();
        let (left, top) = (
            (own.min.x - bounds.min.x) as u32,
            (own.min.y - bounds.min.y) as u32,
        );
        outline.draw(|x, y, coverage| {
            let pixel = &mut pixels[((y + top) * width + x + left) as usize];
            *pixel = (*pixel).max((coverage * 255.0).round() as u8);
        });
    }
    Some(TextImage {
        width,
        height: image_height,
        pixels,
        offset: [bounds.min.x, bounds.min.y],
        advance: natural * shrink,
    })
}

/// Drop floating-point noise from an advance. Scaling yields values like 20.0000019,
/// which would otherwise round a cell up a whole pixel and shift centered glyphs left.
fn snapped(advance: f32) -> f32 {
    (advance * 1024.0).round() / 1024.0
}

fn font_metrics(font: &FontVec, height: f32) -> Result<CellMetrics, RenderError> {
    if !height.is_finite() || !(1.0..=512.0).contains(&height) {
        return Err(RenderError::InvalidFontHeight);
    }
    let scaled = font.as_scaled(height);
    let row_height = (height + scaled.line_gap()).max(height).ceil() + (height * 0.15).ceil();
    Ok(CellMetrics {
        width: snapped(scaled.h_advance(scaled.glyph_id('M')))
            .ceil()
            .max(1.0) as u32,
        height: row_height as u32,
        baseline: (scaled.ascent() + (row_height - height) * 0.5).round() as u32,
    })
}

fn texture_binding(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn instance_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("cell instances"),
        size,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

#[cfg(test)]
mod tests;
