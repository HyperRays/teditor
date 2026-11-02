use super::*;

#[test]
fn hit_testing_rejects_padding_and_invalid_coordinates() {
    let metrics = CellMetrics {
        width: 9,
        height: 20,
        baseline: 16,
    };
    let viewport = [100, 65];
    assert_eq!(metrics.grid_size(viewport), [11, 3]);
    assert_eq!(metrics.cell_at(viewport, [8.99, 19.99]), Some([0, 0]));
    assert_eq!(metrics.cell_at(viewport, [9.0, 20.0]), Some([1, 1]));
    for position in [
        [99.0, 0.0],
        [0.0, 60.0],
        [-0.1, 0.0],
        [f64::NAN, 1.0],
        [f64::INFINITY, 0.0],
    ] {
        assert_eq!(metrics.cell_at(viewport, position), None);
    }
    assert_eq!(metrics.cell_at([0, 0], [0.0, 0.0]), None);
}

#[test]
fn clipping_handles_disjoint_and_overflowing_rectangles() {
    let screen = CellRect::new(0, 0, 80, 24);
    assert_eq!(
        screen.intersection(CellRect::new(78, 22, 10, 10)),
        CellRect::new(78, 22, 2, 2)
    );
    assert_eq!(screen.intersection(CellRect::new(90, 30, 4, 4)).columns, 0);
    assert_eq!(
        CellRect::new(1, 1, u32::MAX, u32::MAX).intersection(screen),
        CellRect::new(1, 1, 79, 23)
    );
}

#[test]
fn atlas_exhaustion_does_not_corrupt_existing_packing() {
    let mut packer = Packer::default();
    let first = packer.allocate(2046, 100).unwrap();
    assert_eq!((first.x, first.y), (1, 1));
    assert!(matches!(
        packer.allocate(1, 2046),
        Err(RenderError::AtlasFull)
    ));
    let next = packer.allocate(10, 10).unwrap();
    assert_eq!((next.x, next.y), (1, 103));
    assert!(matches!(
        packer.allocate(u32::MAX, 1),
        Err(RenderError::InvalidImageSize)
    ));
}

#[test]
fn character_widths_are_two_only_for_wide_characters() {
    for narrow in ['a', ' ', 'é', '→', '─', '█', '\t', '\n', '❤'] {
        assert_eq!(char_columns(narrow), 1, "{narrow:?}");
    }
    for modifier in ['\u{301}', '\u{FE0F}', '\u{200D}', '\u{1F3FD}'] {
        assert_eq!(char_columns(modifier), 0, "{modifier:?}");
    }
    for wide in ['\u{1F389}', '\u{1F600}', '\u{4E2D}', '\u{3042}'] {
        assert_eq!(char_columns(wide), 2, "{wide:?}");
    }
}

#[test]
fn cell_width_matches_the_font_advance_without_rounding_noise() {
    let font = FontVec::try_from_vec(DEFAULT_FONT.to_vec()).unwrap();
    // JetBrains Mono's advance is 5/11 of its height, so these are exact widths...
    for (height, width) in [(22.0, 10), (33.0, 15), (44.0, 20), (66.0, 30)] {
        assert_eq!(font_metrics(&font, height).unwrap().width, width);
    }
    // ...while a genuinely fractional advance still needs a whole cell.
    assert_eq!(font_metrics(&font, 27.5).unwrap().width, 13);
}

#[test]
fn font_metrics_have_integer_cells_and_reject_bad_scales() {
    let font = FontVec::try_from_vec(DEFAULT_FONT.to_vec()).unwrap();
    let normal = font_metrics(&font, 22.0).unwrap();
    let retina = font_metrics(&font, 44.0).unwrap();
    assert!(retina.width >= normal.width * 2 - 1);
    assert!(normal.baseline < normal.height);
    for height in [0.0, -1.0, f32::NAN, f32::INFINITY, 513.0] {
        assert!(matches!(
            font_metrics(&font, height),
            Err(RenderError::InvalidFontHeight)
        ));
    }
}
