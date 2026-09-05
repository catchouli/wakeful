// Bubble fill: a bilinear gradient between the four theme corners.
// The uniforms mirror `GradientMaterial` in src/systems/bubble.rs.

#import bevy_sprite::mesh2d_vertex_output::VertexOutput

@group(2) @binding(0) var<uniform> top_left: vec4<f32>;
@group(2) @binding(1) var<uniform> top_right: vec4<f32>;
@group(2) @binding(2) var<uniform> bottom_right: vec4<f32>;
@group(2) @binding(3) var<uniform> bottom_left: vec4<f32>;

@fragment
fn fragment(
    mesh: VertexOutput,
) -> @location(0) vec4<f32> {
    // Rectangle UVs are y-down: uv.y 0 is the box top, 1 the bottom.
    let top = mix(top_left.rgb, top_right.rgb, mesh.uv.x);
    let bottom = mix(bottom_left.rgb, bottom_right.rgb, mesh.uv.x);
    return vec4<f32>(mix(top, bottom, mesh.uv.y), 1.0);
}
