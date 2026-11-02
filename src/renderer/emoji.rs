//! Rasterizes color emoji from the bundled font's embedded images.

use super::{Color, atlas};
use ab_glyph::{Font, FontRef, GlyphId};

pub(super) const EMOJI_FONT: &[u8] = include_bytes!("../../assets/fonts/NotoColorEmoji.ttf");
/// The font's only strike, in pixels per em.
const STRIKE: u16 = 109;

/// Straight-alpha sRGB pixels, and the size to draw them at.
pub(super) struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub display: [u32; 2],
}

/// Rasterize a glyph to fit a `box_width` × `box_height` pixel box, or None if the font
/// has no image for it.
pub(super) fn rasterize(
    font: &FontRef<'static>,
    id: GlyphId,
    box_width: u32,
    box_height: u32,
) -> Option<Image> {
    if id.0 == 0 {
        return None;
    }
    let image = font.glyph_raster_image2(id, STRIKE)?;
    let (width, height, rgba) = decode(image.data)?;
    let scale =
        (f64::from(box_width) / f64::from(width)).min(f64::from(box_height) / f64::from(height));
    let display = [
        ((f64::from(width) * scale).round() as u32).max(1),
        ((f64::from(height) * scale).round() as u32).max(1),
    ];
    // Shrink here, where every source pixel counts. The GPU's bilinear filter only
    // samples four and would alias; it does handle any enlargement beyond the strike.
    if display[0] < width {
        Some(Image {
            width: display[0],
            height: display[1],
            rgba: shrink(&rgba, [width, height], display),
            display,
        })
    } else {
        Some(Image {
            width,
            height,
            rgba,
            display,
        })
    }
}

fn decode(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut bytes = vec![0; reader.output_buffer_size()?];
    let output = reader.next_frame(&mut bytes).ok()?;
    let bytes = &bytes[..output.buffer_size()];
    let rgba = match output.color_type {
        png::ColorType::Rgba => bytes.to_vec(),
        png::ColorType::Rgb => bytes
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        _ => return None,
    };
    Some((output.width, output.height, rgba))
}

/// Area-average in premultiplied linear light, so edges keep their true color.
fn shrink(rgba: &[u8], [w, h]: [u32; 2], [tw, th]: [u32; 2]) -> Vec<u8> {
    let mut out = Vec::with_capacity((tw * th * 4) as usize);
    for ty in 0..th {
        let (y0, y1) = span(ty, th, h);
        for tx in 0..tw {
            let (x0, x1) = span(tx, tw, w);
            let mut sum = [0.0f32; 4];
            let mut count = 0.0;
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = &rgba[((y * w + x) * 4) as usize..];
                    let linear = Color(p[0], p[1], p[2], p[3]).linear();
                    for channel in 0..3 {
                        sum[channel] += linear[channel] * linear[3];
                    }
                    sum[3] += linear[3];
                    count += 1.0;
                }
            }
            let alpha = sum[3] / count;
            for value in &sum[..3] {
                let straight = if sum[3] > 0.0 { value / sum[3] } else { 0.0 };
                out.push(atlas::encode_srgb(straight));
            }
            out.push((alpha * 255.0).round() as u8);
        }
    }
    out
}

/// The source pixels covered by one target pixel; never empty.
fn span(target: u32, targets: u32, sources: u32) -> (u32, u32) {
    let start = target * sources / targets;
    let end = ((target + 1) * sources).div_ceil(targets).min(sources);
    (start, end.max(start + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font() -> FontRef<'static> {
        FontRef::try_from_slice(EMOJI_FONT).unwrap()
    }

    #[test]
    fn rasterizes_color_emoji_to_fit_the_requested_box() {
        let image = rasterize(&font(), font().glyph_id('\u{1F389}'), 40, 51).unwrap();
        assert_eq!(image.display[0], 40);
        assert!(image.display[1] <= 51);
        assert_eq!((image.width, image.height), (40, image.display[1]));
        assert_eq!(image.rgba.len() as u32, image.width * image.height * 4);
        let pixels = || image.rgba.as_chunks::<4>().0.iter();
        assert!(pixels().any(|p| p[3] == 255), "has opaque pixels");
        assert!(pixels().any(|p| p[3] == 0), "keeps transparent corners");
        assert!(
            pixels().any(|p| p[3] == 255 && (p[0] != p[1] || p[1] != p[2])),
            "is actually in color"
        );
    }

    #[test]
    fn keeps_the_strike_when_the_box_is_larger_than_it() {
        let image = rasterize(&font(), font().glyph_id('\u{1F600}'), 400, 400).unwrap();
        assert_eq!(image.display[0], 400);
        assert!(image.width < 400, "the GPU enlarges it");
    }

    #[test]
    fn covers_emoji_but_not_ordinary_text() {
        for emoji in ['\u{1F389}', '\u{1F600}', '\u{2764}', '\u{1F680}'] {
            assert!(
                rasterize(&font(), font().glyph_id(emoji), 20, 20).is_some(),
                "{emoji}"
            );
        }
        for text in ['\u{4E2D}', 'é'] {
            assert!(
                rasterize(&font(), font().glyph_id(text), 20, 20).is_none(),
                "{text}"
            );
        }
    }

    #[test]
    fn shrinking_a_uniform_image_preserves_its_color() {
        let rgba = [200, 100, 50, 255].repeat(100);
        let out = shrink(&rgba, [10, 10], [3, 3]);
        for pixel in out.as_chunks::<4>().0 {
            assert_eq!(*pixel, [200, 100, 50, 255]);
        }
    }

    #[test]
    fn spans_cover_every_source_pixel_without_gaps() {
        for (targets, sources) in [(3, 10), (40, 136), (38, 128), (1, 5), (5, 5)] {
            let mut next = 0;
            for target in 0..targets {
                let (start, end) = span(target, targets, sources);
                assert!(start <= next && end > start && end <= sources);
                next = end;
            }
            assert_eq!(next, sources);
        }
    }
}
