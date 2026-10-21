// Instanced quads: solid rectangles, alpha-mask glyphs, color glyphs and
// curly underlines. Output is premultiplied alpha.

struct Globals {
    screen: vec2<f32>,
    // 1 if the surface is sRGB: colors arrive sRGB-encoded and must be linearized.
    linear: u32,
    _pad: u32,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var mask_tex: texture_2d<f32>;
@group(0) @binding(2) var color_tex: texture_2d<f32>;
@group(0) @binding(3) var nearest: sampler;
@group(0) @binding(4) var linear_sampler: sampler;

struct VsIn {
    @builtin(vertex_index) vi: u32,
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv: vec4<f32>,
    @location(3) color: vec4<f32>,
    @location(4) kind: u32,
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
    @location(3) local: vec2<f32>,
    @location(4) @interpolate(flat) params: vec4<f32>,
};

@vertex
fn vs(in: VsIn) -> VsOut {
    let corner = vec2<f32>(f32(in.vi & 1u), f32((in.vi >> 1u) & 1u));
    let p = in.pos + corner * in.size;
    var out: VsOut;
    out.pos = vec4<f32>(p.x / g.screen.x * 2.0 - 1.0, 1.0 - p.y / g.screen.y * 2.0, 0.0, 1.0);
    out.uv = mix(in.uv.xy, in.uv.zw, corner);
    out.color = in.color;
    out.kind = in.kind;
    out.local = corner * in.size;
    out.params = in.uv;
    return out;
}

fn to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    var c = in.color;
    if g.linear == 1u {
        c = vec4<f32>(to_linear(c.rgb), c.a);
    }
    switch in.kind {
        case 1u: {
            let a = textureSampleLevel(mask_tex, nearest, in.uv, 0.0).r * c.a;
            return vec4<f32>(c.rgb * a, a);
        }
        case 2u: {
            var t = textureSampleLevel(color_tex, linear_sampler, in.uv, 0.0);
            if g.linear == 1u {
                t = vec4<f32>(to_linear(t.rgb), t.a);
            }
            return vec4<f32>(t.rgb * t.a, t.a);
        }
        case 3u: {
            // Curly underline: params.x = stroke thickness, params.y = period.
            let h = in.local.y;
            let amp = max(0.0, (in.params.w - in.params.x) * 0.5);
            let center = in.params.w * 0.5 + amp * sin(in.local.x * 6.2831853 / in.params.y);
            let a = clamp(in.params.x * 0.5 + 0.5 - abs(h - center), 0.0, 1.0) * c.a;
            return vec4<f32>(c.rgb * a, a);
        }
        default: {
            return vec4<f32>(c.rgb * c.a, c.a);
        }
    }
}
