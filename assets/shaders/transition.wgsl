//! The fullscreen transition: mixes the game image toward black by the
//! engine-driven progress. Mode 1 = rise (progress 0→1), mode 2 = fall
//! (progress 1→0); mode 0 passes the image through untouched.

#import bevy_core_pipeline::fullscreen_vertex_shader::FullscreenVertexOutput

@group(0) @binding(0) var screen_texture: texture_2d<f32>;
@group(0) @binding(1) var texture_sampler: sampler;

struct TransitionPostProcess {
    mode: u32,
    progress: f32,
}

@group(0) @binding(2) var<uniform> settings: TransitionPostProcess;

@fragment
fn fragment(in: FullscreenVertexOutput) -> @location(0) vec4<f32> {
    let color = textureSampleLevel(screen_texture, texture_sampler, in.uv, 0.0).rgb;

    var covered = 0.0;
    var uv = in.uv;
    if (settings.mode == 1u) {
        // The swirl: the frozen picture magnifies about the screen
        // center while the black cover rises over it.
        let zoom = 1.0 - 0.15 * settings.progress;
        uv = (in.uv - vec2<f32>(0.5, 0.5)) * zoom + vec2<f32>(0.5, 0.5);
        covered = settings.progress;
    } else if (settings.mode == 2u) {
        covered = 1.0 - settings.progress;
    }
    let swirled = textureSampleLevel(screen_texture, texture_sampler, uv, 0.0).rgb;
    return vec4(mix(swirled, vec3<f32>(0.0), clamp(covered, 0.0, 1.0)), 1.0);
}
