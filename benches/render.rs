//! GPU benchmarks: diffing a framebuffer and submitting it through the renderer.
//!
//! Run with `cargo bench --bench render`. Prints a notice and measures nothing
//! when the machine has no usable GPU adapter.
use criterion::{Criterion, criterion_group, criterion_main};
use teditor::{
    renderer::Color,
    terminal::{Cell, Style, Terminal, TerminalConfig},
};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const VIEWPORT: [u32; 2] = [1760, 1300];

fn gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).ok()?;
    pollster::block_on(adapter.request_device(&Default::default())).ok()
}

fn target(device: &wgpu::Device) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: VIEWPORT[0],
                height: VIEWPORT[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
        .create_view(&Default::default())
}

/// A full screen of colored text, as a syntax-highlighted file produces.
fn fill(terminal: &mut Terminal, shift: u8) {
    let [columns, rows] = terminal.back().size();
    let line = "let value = compute(input, 42); // comment ".repeat(8);
    for row in 0..rows {
        let color = Color::rgb(120 + (row % 100) as u8, 200, 180u8.wrapping_add(shift));
        let style = Style {
            foreground: color,
            ..Default::default()
        };
        let text: String = line
            .chars()
            .skip(row as usize % 40)
            .take(columns as usize)
            .collect();
        terminal.back_mut().write(0, row, &text, style);
    }
}

fn render(c: &mut Criterion) {
    let Some((device, queue)) = gpu() else {
        eprintln!("No GPU adapter; skipping render benchmarks.");
        return;
    };
    let view = target(&device);
    let config = TerminalConfig {
        viewport: VIEWPORT,
        ..Default::default()
    };
    let mut terminal = Terminal::new(&device, &queue, FORMAT, config).unwrap();
    fill(&mut terminal, 0);
    terminal.render(&view).unwrap();
    let finish = |device: &wgpu::Device| {
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(std::time::Duration::from_secs(10)),
            })
            .unwrap();
    };
    c.bench_function("render/unchanged_frame", |b| {
        b.iter(|| {
            terminal.render(&view).unwrap();
            finish(&device);
        })
    });
    c.bench_function("render/one_cell_changed", |b| {
        let mut character = 'a';
        b.iter(|| {
            character = if character == 'a' { 'b' } else { 'a' };
            terminal
                .back_mut()
                .set(10, 10, Cell::new(character, Style::default()));
            terminal.render(&view).unwrap();
            finish(&device);
        })
    });
    c.bench_function("render/whole_screen_changed", |b| {
        let mut shift = 0u8;
        b.iter(|| {
            shift = shift.wrapping_add(1);
            fill(&mut terminal, shift);
            terminal.render(&view).unwrap();
            finish(&device);
        })
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(40);
    targets = render
}
criterion_main!(benches);
