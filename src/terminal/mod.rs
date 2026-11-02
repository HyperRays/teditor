//! A visual terminal: buffered cells, bitmap assets, keyboard/text input and mouse support.
//!
//! The terminal owns two CPU [`Framebuffer`]s: a committed front and a writable back.
//! Applications edit [`Terminal::back_mut`], or clear and rebuild it completely, without
//! anything showing until [`Terminal::render`] succeeds:
//!
//! 1. Compare cells and overlays against the front buffer, including changed assets.
//! 2. Repaint the damaged cell runs into a retained GPU texture. Each region restores
//!    its background, text and overlapping overlays in order, removing old content.
//! 3. Copy the complete retained image to the supplied surface view and submit once.
//! 4. Swap the buffers and reinitialize back from the new front.
//!
//! The caller presents the surface afterwards. A failed render leaves front unchanged and
//! back editable for a retry. Unchanged frames still copy the retained image, since a fresh
//! swapchain image or expose event cannot rely on earlier contents. There is never a
//! separately presented blank frame, which is what prevents flicker.
//!
//! ```
//! use teditor::renderer::{CellRect, Color};
//! use teditor::terminal::{Cell, Framebuffer, Style};
//!
//! let mut back = Framebuffer::new([80, 24], Color::rgb(17, 23, 33)).unwrap();
//! back.write(2, 1, "hello, grid", Style::default());
//! back.scroll(CellRect::new(0, 0, 80, 24), 0, -1, Cell::default());
//! assert_eq!(back.cell(2, 0).unwrap().character, 'h');
//! ```
//!
//! # Input
//!
//! Pass native events to [`Terminal::handle_window_event`]. One entry point emits three
//! separate event families, described on [`InputEvent`]:
//!
//! - [`input::KeyEvent`] for shortcuts: physical key, layout-dependent logical key, repeat
//!   and modifiers. Use [`is_actionable`](input::KeyEvent::is_actionable) to exclude
//!   releases, cancellations, synthetic presses and keys pressed during composition.
//! - [`input::TextInput`] is the only stream of text to insert. It comes from the native
//!   layout or an IME commit, never by guessing from a key code.
//! - [`input::ImeEvent`] reports composition. Preedit text never inserts anything.
//!
//! Pointer events carry physical and fractional grid positions. A press stays captured
//! until release even outside the window, as long as the host keeps forwarding events.
//!
//! # Bitmaps
//!
//! [`Terminal::upload_bitmap`] returns a [`BitmapId`] that stays valid across atlas
//! repacking. Images are limited to 2046 × 2046 pixels in a 2048 × 2048 atlas, and
//! framebuffers to one million cells.

mod assets;
mod buffer;
/// Normalized keyboard, pointer and IME events.
pub mod input;

use crate::renderer::{CellMetrics, CellRect, Color, DEFAULT_FONT, Frame, RenderError, Renderer};
pub use assets::{AssetError, BitmapId, BitmapInfo, read_png};
pub use buffer::{
    BitmapPlacement, BufferTooLarge, Cell, Framebuffer, GraphicPlacement, Overlay, Style,
};
pub use input::{InputEvent, InputState};
use std::{path::Path, time::Instant};

/// How to create a [`Terminal`].
pub struct TerminalConfig<'a> {
    /// The surface size in physical pixels.
    pub viewport: [u32; 2],
    /// The display's scale factor.
    pub scale_factor: f64,
    /// The font height in logical pixels.
    pub font_height: f32,
    /// The font data.
    pub font: &'a [u8],
    /// The color behind every cell.
    pub background: Color,
}

impl Default for TerminalConfig<'static> {
    fn default() -> Self {
        Self {
            viewport: [1060, 700],
            scale_factor: 1.0,
            font_height: 22.0,
            font: DEFAULT_FONT,
            background: Style::default().background,
        }
    }
}

/// Why the terminal could not do what it was asked.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    /// The renderer failed.
    #[error(transparent)]
    Render(#[from] RenderError),
    /// A bitmap operation failed.
    #[error(transparent)]
    Asset(#[from] AssetError),
    /// The grid would be too large.
    #[error(transparent)]
    Buffer(#[from] BufferTooLarge),
    /// The viewport is larger than the device allows.
    #[error("the viewport exceeds the device's maximum texture dimension")]
    ViewportTooLarge,
    /// The scale factor is unusable.
    #[error("the display scale must be finite and greater than zero")]
    InvalidScale,
    /// The back buffer's size does not match the grid.
    #[error("the writable framebuffer dimensions must match the terminal grid")]
    BufferSizeMismatch,
}

/// What a [`Terminal::render`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    /// Cells repainted.
    pub dirty_cells: usize,
    /// Rectangles repainted.
    pub dirty_regions: usize,
    /// Whether the whole image was repainted.
    pub full_repaint: bool,
    /// True for a zero-sized viewport. No buffers were committed or presented.
    pub skipped: bool,
}

/// A grid of cells on a GPU surface. See the [module documentation](self).
pub struct Terminal {
    device: wgpu::Device,
    queue: wgpu::Queue,
    format: wgpu::TextureFormat,
    renderer: Renderer,
    front: Framebuffer,
    back: Framebuffer,
    retained: Option<wgpu::TextureView>,
    blitter: wgpu::util::TextureBlitter,
    viewport: [u32; 2],
    scale_factor: f64,
    font_height: f32,
    full_repaint: bool,
    assets: assets::Assets,
    input: InputState,
}

impl Terminal {
    /// Create a terminal for a surface of this format.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        config: TerminalConfig<'_>,
    ) -> Result<Self, TerminalError> {
        validate_geometry(device, config.viewport, config.scale_factor)?;
        let renderer = Renderer::new(
            device,
            queue,
            format,
            config.font,
            config.font_height * config.scale_factor as f32,
        )?;
        let metrics = renderer.cell_metrics();
        let front = Framebuffer::new(metrics.grid_size(config.viewport), config.background)?;
        let back = front.clone();
        let retained = retained_view(device, format, config.viewport);
        Ok(Self {
            device: device.clone(),
            queue: queue.clone(),
            format,
            renderer,
            front,
            back,
            retained,
            blitter: wgpu::util::TextureBlitter::new(device, format),
            viewport: config.viewport,
            scale_factor: config.scale_factor,
            font_height: config.font_height,
            full_repaint: true,
            assets: assets::Assets::default(),
            input: InputState::new(metrics, config.viewport, config.scale_factor),
        })
    }

    /// The committed screen.
    pub fn front(&self) -> &Framebuffer {
        &self.front
    }
    /// The buffer the next frame is built in.
    pub fn back(&self) -> &Framebuffer {
        &self.back
    }
    /// Edit the next frame.
    pub fn back_mut(&mut self) -> &mut Framebuffer {
        &mut self.back
    }
    /// The size of one cell in physical pixels.
    pub fn cell_metrics(&self) -> CellMetrics {
        self.renderer.cell_metrics()
    }
    /// The surface size in physical pixels.
    pub fn viewport(&self) -> [u32; 2] {
        self.viewport
    }
    /// Keyboard, pointer and IME state.
    pub fn input(&self) -> &InputState {
        &self.input
    }
    /// Number of uploaded bitmaps.
    pub fn bitmap_count(&self) -> usize {
        self.assets.len()
    }
    /// The size of an uploaded bitmap.
    pub fn bitmap_info(&self, bitmap: BitmapId) -> Option<BitmapInfo> {
        self.assets.info(bitmap)
    }

    /// Request a full repaint on the next render, for example after changing a theme.
    pub fn invalidate(&mut self) {
        self.full_repaint = true;
    }

    /// Upload straight-alpha RGBA pixels, returning a stable handle.
    pub fn upload_bitmap(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<BitmapId, AssetError> {
        self.assets
            .insert(&mut self.renderer, [width, height], rgba)
    }

    /// Updates retain their application handle, including when dimensions change.
    pub fn update_bitmap(
        &mut self,
        id: BitmapId,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), AssetError> {
        self.assets
            .update(&mut self.renderer, id, [width, height], rgba)
    }

    /// Reclaim CPU/GPU storage and remove all placements from the writable frame.
    /// The old front placements remain until rendering repairs their regions.
    pub fn remove_bitmap(&mut self, id: BitmapId) -> Result<(), AssetError> {
        self.assets.remove(&mut self.renderer, id)?;
        self.back.overlays.retain(
            |overlay| !matches!(overlay, Overlay::Bitmap(placement) if placement.bitmap == id),
        );
        Ok(())
    }

    /// Decode a PNG file and upload it.
    pub fn load_png(&mut self, path: impl AsRef<Path>) -> Result<BitmapId, AssetError> {
        let (size, pixels) = read_png(path)?;
        self.upload_bitmap(size[0], size[1], &pixels)
    }

    /// Convert a native window event into [`InputEvent`]s.
    pub fn handle_window_event(
        &mut self,
        event: &winit::event::WindowEvent,
        now: Instant,
    ) -> Vec<InputEvent> {
        self.input.handle_window_event(event, now)
    }

    /// Cancel all held keys and gestures, such as on suspension.
    pub fn cancel_input(&mut self) -> Vec<InputEvent> {
        self.input.cancel()
    }

    /// Normalized keyboard injection for alternate hosts and deterministic tests.
    pub fn handle_key_input(&mut self, key: input::KeyInput) -> Vec<InputEvent> {
        self.input.handle_key_input(key)
    }

    /// Clip a cell-aligned caret/editing region to the viewport and convert to
    /// physical pixels for Window::set_ime_cursor_area. None means it is offscreen.
    pub fn ime_cursor_area(&self, cells: CellRect) -> Option<input::ImeCursorArea> {
        if self.viewport.contains(&0) {
            return None;
        }
        let metrics = self.cell_metrics();
        let [columns, rows] = metrics.grid_size(self.viewport);
        let cells = cells.intersection(CellRect::new(0, 0, columns, rows));
        if cells.columns == 0 || cells.rows == 0 {
            return None;
        }
        Some(input::ImeCursorArea {
            position: [cells.column * metrics.width, cells.row * metrics.height],
            size: [cells.columns * metrics.width, cells.rows * metrics.height],
        })
    }

    /// Preserve overlapping cells and invalidate the retained image. Returns any
    /// pointer cancellations caused by changing the coordinate system.
    pub fn resize(&mut self, viewport: [u32; 2]) -> Result<Vec<InputEvent>, TerminalError> {
        self.set_geometry(viewport, self.scale_factor)
    }

    /// Change the display scale, keeping the window size.
    pub fn set_scale_factor(
        &mut self,
        scale_factor: f64,
    ) -> Result<Vec<InputEvent>, TerminalError> {
        self.set_geometry(self.viewport, scale_factor)
    }

    fn set_geometry(
        &mut self,
        viewport: [u32; 2],
        scale_factor: f64,
    ) -> Result<Vec<InputEvent>, TerminalError> {
        validate_geometry(&self.device, viewport, scale_factor)?;
        if viewport == self.viewport && scale_factor == self.scale_factor {
            return Ok(Vec::new());
        }
        let height = self.font_height * scale_factor as f32;
        let metrics = self.renderer.metrics_at_height(height)?;
        if viewport.contains(&0) {
            // Minimizing must not discard the committed screen or pending edits.
            self.renderer.set_font_height(height)?;
            self.retained = None;
            self.viewport = viewport;
            self.scale_factor = scale_factor;
            self.full_repaint = true;
            return Ok(self.input.set_geometry(metrics, viewport, scale_factor));
        }
        let size = metrics.grid_size(viewport);
        // Prepare fallible CPU allocations before changing any live state.
        let front = self.front.resized(size)?;
        let back = self.back.resized(size)?;
        self.renderer.set_font_height(height)?;
        if viewport != self.viewport {
            self.retained = retained_view(&self.device, self.format, viewport);
        }
        self.front = front;
        self.back = back;
        self.viewport = viewport;
        self.scale_factor = scale_factor;
        self.full_repaint = true;
        Ok(self.input.set_geometry(metrics, viewport, scale_factor))
    }

    /// Diff, repaint damaged cells offscreen, and submit a complete image to `target`.
    /// The caller presents its surface after success. A failed frame keeps front
    /// unchanged and leaves back editable for retry. Unchanged frames still blit
    /// the retained image, which is necessary for native expose/redraw events.
    pub fn render(&mut self, target: &wgpu::TextureView) -> Result<FrameStats, TerminalError> {
        let Some(retained) = &self.retained else {
            return Ok(FrameStats {
                skipped: true,
                ..Default::default()
            });
        };
        if self.back.size() != self.cell_metrics().grid_size(self.viewport) {
            return Err(TerminalError::BufferSizeMismatch);
        }
        let full = self.full_repaint || self.front.backdrop() != self.back.backdrop();
        let damage = self.back.damage(&self.front, &self.assets.changed, full);
        let frame = self.paint(&damage)?;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("terminal commit"),
            });
        if full || !damage.is_empty() {
            self.renderer
                .encode(retained, self.viewport, &frame, full, &mut encoder)?;
        }
        self.blitter
            .copy(&self.device, &mut encoder, retained, target);
        self.queue.submit([encoder.finish()]);
        let stats = FrameStats {
            dirty_cells: damage
                .iter()
                .map(|rect| (rect.columns * rect.rows) as usize)
                .sum(),
            dirty_regions: damage.len(),
            full_repaint: full,
            skipped: false,
        };
        std::mem::swap(&mut self.front, &mut self.back);
        // Keep back initialized to the committed state so incremental writes and
        // full-frame composition are both supported; clone_from reuses allocations.
        self.back.clone_from(&self.front);
        self.assets.changed.clear();
        self.full_repaint = false;
        Ok(stats)
    }

    fn paint(&self, damage: &[CellRect]) -> Result<Frame, AssetError> {
        // Fail before anything is drawn, even when the damage misses the placement.
        for placement in self.back.bitmaps() {
            self.assets.bitmap(placement.bitmap)?;
        }
        let mut frame = Frame::new(self.back.backdrop());
        for &region in damage {
            frame.set_clip(Some(region));
            // Erase old glyphs and transparent bitmap pixels before recompositing.
            // This clear happens only in the retained image, never as a presented frame.
            frame.fill_to_edge(region, self.back.backdrop());
            let columns = region.column as usize..(region.column + region.columns) as usize;
            for y in region.row..region.row + region.rows {
                let row = self.back.row(y);
                let mut x = region.column;
                for run in
                    row[columns.clone()].chunk_by(|a, b| a.style.background == b.style.background)
                {
                    let width = run.len() as u32;
                    frame.fill(CellRect::new(x, y, width, 1), run[0].style.background);
                    x += width;
                }
                for x in columns.clone() {
                    let cell = row[x];
                    if cell.character != ' ' && !cell.character.is_control() {
                        let width = 1 + row[x + 1..]
                            .iter()
                            .take_while(|next| next.character == Cell::CONTINUATION)
                            .count() as u32;
                        frame.glyph(x as u32, y, cell.character, width, cell.style.foreground);
                    }
                }
            }
            for overlay in self.back.overlays() {
                let overlap = overlay.clip().intersection(region);
                if overlap.columns == 0 || overlap.rows == 0 {
                    continue;
                }
                frame.set_clip(Some(overlap));
                match overlay {
                    Overlay::Graphic(placement) => frame.graphic(placement.rect, placement.graphic),
                    Overlay::Bitmap(placement) => {
                        let source = placement.source;
                        let [width, height] = placement.source_size.map(|size| size as f32);
                        frame.bitmap_region(
                            placement.rect,
                            self.assets.bitmap(placement.bitmap)?,
                            [
                                source.column as f32 / width,
                                source.row as f32 / height,
                                source.columns as f32 / width,
                                source.rows as f32 / height,
                            ],
                        );
                    }
                }
            }
        }
        Ok(frame)
    }
}

fn validate_geometry(
    device: &wgpu::Device,
    viewport: [u32; 2],
    scale: f64,
) -> Result<(), TerminalError> {
    if !scale.is_finite() || scale <= 0.0 {
        return Err(TerminalError::InvalidScale);
    }
    if viewport
        .iter()
        .any(|&side| side > device.limits().max_texture_dimension_2d)
    {
        return Err(TerminalError::ViewportTooLarge);
    }
    Ok(())
}

fn retained_view(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    [width, height]: [u32; 2],
) -> Option<wgpu::TextureView> {
    if width == 0 || height == 0 {
        return None;
    }
    Some(
        device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("retained terminal framebuffer"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&Default::default()),
    )
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod keyboard_tests;
