struct Viewport {
    size: vec2<f32>,
    padding: vec2<f32>,
};

@group(0) @binding(0) var<uniform> viewport: Viewport;
@group(0) @binding(1) var glyph_atlas: texture_2d<f32>;
@group(0) @binding(2) var bitmap_atlas: texture_2d<f32>;
@group(0) @binding(3) var atlas_sampler: sampler;
@group(0) @binding(4) var emoji_atlas: texture_2d<f32>;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) color: vec4<f32>,
    @location(2) @interpolate(flat) clip: vec4<f32>,
    @location(3) @interpolate(flat) kind: u32,
    @location(4) @interpolate(flat) rect: vec4<f32>,
    @location(5) @interpolate(flat) secondary: vec4<f32>,
    @location(6) @interpolate(flat) cell_size: vec2<f32>,
    @location(7) @interpolate(flat) parameters: vec4<f32>,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vertex: u32,
    @location(0) rect: vec4<f32>,
    @location(1) uv_rect: vec4<f32>,
    @location(2) color: vec4<f32>,
    @location(3) clip: vec4<f32>,
    @location(4) kind: u32,
    @location(5) secondary: vec4<f32>,
    @location(6) cell_size: vec2<f32>,
) -> VertexOutput {
    let corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0),
    );
    let corner = corners[vertex];
    let pixel = rect.xy + corner * rect.zw;
    var output: VertexOutput;
    output.position = vec4(pixel / viewport.size * vec2(2.0, -2.0) + vec2(-1.0, 1.0), 0.0, 1.0);
    output.uv = uv_rect.xy + corner * uv_rect.zw;
    output.color = color;
    output.clip = clip;
    output.kind = kind;
    output.rect = rect;
    output.secondary = secondary;
    output.cell_size = cell_size;
    output.parameters = uv_rect;
    return output;
}

fn rounded_box(p: vec2<f32>, size: vec2<f32>, radius: f32) -> f32 {
    let q = abs(p - size * 0.5) - size * 0.5 + vec2(radius);
    return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - radius;
}

fn segment(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let v = b - a;
    return length(p - a - v * clamp(dot(p - a, v) / max(dot(v, v), 0.001), 0.0, 1.0));
}

fn premultiply(color: vec4<f32>, coverage: f32) -> vec4<f32> {
    let alpha = color.a * coverage;
    return vec4(color.rgb * alpha, alpha);
}

fn graphic(input: VertexOutput) -> vec4<f32> {
    let p = input.position.xy - input.rect.xy;
    let size = input.rect.zw;
    let scale = max(input.cell_size.x / 11.0, 0.5);
    if input.kind == 10u {
        // Vertical geometry matches ui::scroll::thumb in cell units, so dragging
        // keeps the point where the thumb was grabbed even with fractional DPI.
        let inset = input.cell_size.y * 0.1;
        // A nested scroll area can place this track above the framebuffer origin.
        // Restore its original geometry after the CPU clips negative cell bounds.
        let full_height = input.parameters.y * input.cell_size.y;
        let local = p + vec2(0.0, max(full_height - size.y, 0.0));
        let height = max(full_height - 2.0 * inset, 0.0);
        let total = max(input.parameters.z, max(input.parameters.y, 1.0));
        let thumb_height = min(height, max(input.cell_size.y * 0.75, height * input.parameters.y / total));
        let maximum = max(total - input.parameters.y, 1.0);
        let top = inset + (height - thumb_height) * clamp(input.parameters.x / maximum, 0.0, 1.0);
        let track = segment(local, vec2(size.x * 0.5, inset + scale),
                                  vec2(size.x * 0.5, full_height - inset - scale));
        let background = premultiply(input.secondary, 1.0 - smoothstep(0.5 * scale, 1.5 * scale, track));
        let width = min(5.0 * scale, size.x);
        let d = rounded_box(local - vec2((size.x - width) * 0.5, top), vec2(width, thumb_height),
                            min(width, thumb_height) * 0.5);
        let foreground = premultiply(input.color, 1.0 - smoothstep(-0.5, 0.5, d));
        return foreground + background * (1.0 - foreground.a);
    }
    if input.kind == 11u {
        let d = segment(p, vec2(2.0 * scale, size.y * 0.2), vec2(2.0 * scale, size.y * 0.8));
        return premultiply(input.color, 1.0 - smoothstep(0.5 * scale, 1.5 * scale, d));
    }
    let distance = select(size.y - p.y, size.x - p.x, input.kind == 12u);
    return premultiply(input.color, 1.0 - smoothstep(scale - 0.5, scale + 0.5, distance));

}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    if any(input.position.xy < input.clip.xy) || any(input.position.xy >= input.clip.zw) {
        discard;
    }
    if input.kind == 1u {
        let coverage = textureSampleLevel(glyph_atlas, atlas_sampler, input.uv, 0.0).r;
        let alpha = input.color.a * coverage;
        return vec4(input.color.rgb * alpha, alpha);
    }
    if input.kind == 2u {
        return textureSampleLevel(bitmap_atlas, atlas_sampler, input.uv, 0.0) * input.color;
    }
    if input.kind == 1000u {
        return textureSampleLevel(emoji_atlas, atlas_sampler, input.uv, 0.0);
    }
    if input.kind >= 3u {
        return graphic(input);
    }
    return vec4(input.color.rgb * input.color.a, input.color.a);
}
