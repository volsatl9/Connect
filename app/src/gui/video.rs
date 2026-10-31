//! Shows the remote screen. Decoded frames go to the GPU as their Y, U and V planes and turn into
//! RGB in a shader: a third of the upload of an RGBA image, and no per-pixel work on the CPU,
//! which at 1440p and above otherwise costs more than a whole frame's time at 144 fps.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use connect_client::{ClientHandle, VideoFrame};
use eframe::{
    egui::{self, Color32, ColorImage, Rect, TextureHandle, TextureOptions},
    egui_glow, egui_wgpu, glow, wgpu,
};

/// What the window draws with; set once at startup.
#[derive(Clone, Copy, Debug)]
pub enum Gpu {
    /// Direct3D 12, Vulkan or Metal through wgpu, into a target of this format.
    Wgpu(wgpu::TextureFormat),
    /// OpenGL 3 / GLES 3 or newer.
    Glow,
    /// Nothing the shader runs on: frames are converted to RGBA on the CPU.
    Cpu,
}

static GPU: OnceLock<Gpu> = OnceLock::new();

/// Records what the window renders with; call before any [`VideoView`] is made.
pub fn init(cc: &eframe::CreationContext<'_>) {
    let gpu = if let Some(state) = &cc.wgpu_render_state {
        Gpu::Wgpu(state.target_format)
    } else if let Some(gl) = &cc.gl {
        let version = egui_glow::ShaderVersion::get(gl);
        if version.is_new_shader_interface() { Gpu::Glow } else { Gpu::Cpu }
    } else {
        Gpu::Cpu
    };
    tracing::debug!(?gpu, "video renderer");
    let _ = GPU.set(gpu);
}

fn gpu() -> Gpu {
    *GPU.get().unwrap_or(&Gpu::Cpu)
}

/// Frames passed between the GUI and the renderer, which runs later in the same frame.
#[derive(Default)]
struct Exchange {
    /// The newest frame, not yet uploaded.
    next: Option<VideoFrame>,
    /// Frames uploaded, whose buffers can go back to the decoder.
    done: Vec<VideoFrame>,
}

/// The remote screen as it appears in the session window.
#[derive(Default)]
pub struct VideoView {
    exchange: Arc<Mutex<Exchange>>,
    size: Option<egui::Vec2>,
    /// The CPU fallback's texture and RGBA scratch space.
    cpu: Option<(TextureHandle, Vec<u8>)>,
}

impl VideoView {
    /// The size of the remote screen in pixels, once a frame has arrived.
    pub fn size(&self) -> Option<egui::Vec2> {
        self.size
    }

    /// Shows `frame` from the next repaint on.
    pub fn set_frame(&mut self, ctx: &egui::Context, mut frame: VideoFrame) {
        self.size = Some(egui::vec2(frame.width as f32, frame.height as f32));
        if !matches!(gpu(), Gpu::Cpu) {
            let mut exchange = lock(&self.exchange);
            if let Some(unshown) = exchange.next.replace(frame) {
                exchange.done.push(unshown);
            }
            return;
        }
        let size = [frame.width as usize, frame.height as usize];
        let rgba = match &mut self.cpu {
            Some((_, rgba)) => rgba,
            None => {
                &mut self
                    .cpu
                    .insert((
                        ctx.load_texture("remote-screen", ColorImage::example(), TextureOptions::LINEAR),
                        Vec::new(),
                    ))
                    .1
            }
        };
        frame.write_rgba(rgba);
        // Opaque, so these are already premultiplied.
        let image = ColorImage::from_rgba_premultiplied(size, rgba);
        self.cpu.as_mut().expect("set above").0.set(image, TextureOptions::LINEAR);
        frame.timing.mark_painted();
        lock(&self.exchange).done.push(frame);
    }

    /// Hands the buffers of frames already shown back to the decoder.
    pub fn recycle(&self, client: &ClientHandle) {
        for frame in lock(&self.exchange).done.drain(..) {
            client.recycle(frame);
        }
    }

    /// Draws the remote screen into `rect`.
    pub fn paint(&self, painter: &egui::Painter, rect: Rect) {
        if self.size.is_none() {
            return;
        }
        let exchange = self.exchange.clone();
        match gpu() {
            Gpu::Wgpu(format) => {
                painter.add(egui_wgpu::Callback::new_paint_callback(rect, WgpuVideo { exchange, format }));
            }
            Gpu::Glow => {
                let callback = egui_glow::CallbackFn::new(move |_, painter| {
                    // SAFETY: called by egui_glow while painting, with its context current.
                    unsafe { paint_gl(painter.gl(), &exchange) };
                });
                painter.add(egui::PaintCallback { rect, callback: Arc::new(callback) });
            }
            Gpu::Cpu => {
                if let Some((texture, _)) = &self.cpu {
                    let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    painter.image(texture.id(), rect, uv, Color32::WHITE);
                }
            }
        }
    }
}

/// The planes to upload, as (width, height, bytes) each.
fn planes(frame: &VideoFrame) -> [(u32, u32, &[u8]); 3] {
    let (y, u, v) = frame.planes();
    let (cw, ch) = frame.chroma_size();
    [(frame.width, frame.height, y), (cw as u32, ch as u32, u), (cw as u32, ch as u32, v)]
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- wgpu ---------------------------------------------------------------------------------

/// BT.601 limited range, as the host encodes; output in sRGB-encoded values, or linear for a
/// target that encodes to sRGB itself.
const WGSL: &str = r#"
struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// One triangle over the whole viewport, which egui sets to the video's rectangle.
@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: VsOut;
    out.position = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@group(0) @binding(0) var y_plane: texture_2d<f32>;
@group(0) @binding(1) var u_plane: texture_2d<f32>;
@group(0) @binding(2) var v_plane: texture_2d<f32>;
@group(0) @binding(3) var planes: sampler;

fn convert(luma: f32, cb: f32, cr: f32) -> vec3<f32> {
    let y = (luma - 16.0 / 255.0) * 1.164383;
    let u = cb - 128.0 / 255.0;
    let v = cr - 128.0 / 255.0;
    let rgb = vec3<f32>(y + 1.596027 * v, y - 0.391762 * u - 0.812968 * v, y + 2.017232 * u);
    return clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
}

// Three uploaded planes.
fn i420(uv: vec2<f32>) -> vec3<f32> {
    let y = textureSample(y_plane, planes, uv).r;
    return convert(y, textureSample(u_plane, planes, uv).r, textureSample(v_plane, planes, uv).r);
}

// A decoded NV12 texture: its luma plane, and its chroma plane (bound twice) with U in red
// and V in green.
fn nv12(uv: vec2<f32>) -> vec3<f32> {
    let chroma = textureSample(u_plane, planes, uv).rg;
    return convert(textureSample(y_plane, planes, uv).r, chroma.r, chroma.g);
}

fn to_linear(c: vec3<f32>) -> vec4<f32> {
    return vec4<f32>(select(c / 12.92, pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c > vec3<f32>(0.04045)), 1.0);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(i420(in.uv), 1.0);
}

@fragment
fn fs_linear(in: VsOut) -> @location(0) vec4<f32> {
    return to_linear(i420(in.uv));
}

@fragment
fn fs_nv12(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(nv12(in.uv), 1.0);
}

@fragment
fn fs_nv12_linear(in: VsOut) -> @location(0) vec4<f32> {
    return to_linear(nv12(in.uv));
}
"#;

struct WgpuVideo {
    exchange: Arc<Mutex<Exchange>>,
    format: wgpu::TextureFormat,
}

/// Lives in egui's renderer for the life of the window.
struct WgpuResources {
    pipeline: wgpu::RenderPipeline,
    /// For decoded textures, on a window sharing the decoder's graphics card.
    nv12_pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    planes: Option<WgpuPlanes>,
    /// The picture to draw.
    showing: Option<Showing>,
}

enum Showing {
    /// The uploaded planes in `planes`.
    Planes,
    /// A decoded texture, kept alive while it's drawn.
    Nv12 { bind_group: wgpu::BindGroup, _texture: wgpu::Texture },
}

struct WgpuPlanes {
    size: [u32; 2],
    textures: [wgpu::Texture; 3],
    bind_group: wgpu::BindGroup,
}

impl WgpuResources {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("remote screen"),
            source: wgpu::ShaderSource::Wgsl(WGSL.into()),
        });
        let texture = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("remote screen"),
            entries: &[
                texture(0),
                texture(1),
                texture(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("remote screen"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |fragment: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("remote screen"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fragment),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let srgb = format.is_srgb();
        let nv12_pipeline = pipeline(if srgb { "fs_nv12_linear" } else { "fs_nv12" });
        let pipeline = pipeline(if srgb { "fs_linear" } else { "fs_main" });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("remote screen"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self { pipeline, nv12_pipeline, layout, sampler, planes: None, showing: None }
    }

    /// Binds a decoded NV12 texture for drawing.
    fn show_texture(&mut self, device: &wgpu::Device, texture: wgpu::Texture) {
        let plane = |aspect, format| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                label: Some("remote screen plane"),
                format: Some(format),
                aspect,
                ..Default::default()
            })
        };
        let luma = plane(wgpu::TextureAspect::Plane0, wgpu::TextureFormat::R8Unorm);
        let chroma = plane(wgpu::TextureAspect::Plane1, wgpu::TextureFormat::Rg8Unorm);
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("remote screen"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&luma) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&chroma) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&chroma) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        self.showing = Some(Showing::Nv12 { bind_group, _texture: texture });
    }

    fn planes_for(&mut self, device: &wgpu::Device, frame: &VideoFrame) -> &WgpuPlanes {
        let size = [frame.width, frame.height];
        if self.planes.as_ref().is_none_or(|p| p.size != size) {
            let textures = planes(frame).map(|(width, height, _)| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("remote screen plane"),
                    size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            });
            let views = textures.each_ref().map(|t| t.create_view(&Default::default()));
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("remote screen"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&views[0]) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&views[1]) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&views[2]) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            });
            self.planes = Some(WgpuPlanes { size, textures, bind_group });
        }
        self.planes.as_ref().expect("set above")
    }
}

impl egui_wgpu::CallbackTrait for WgpuVideo {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if !resources.contains::<WgpuResources>() {
            resources.insert(WgpuResources::new(device, self.format));
        }
        let mut exchange = lock(&self.exchange);
        let Some(mut frame) = exchange.next.take() else { return Vec::new() };
        let gpu = resources.get_mut::<WgpuResources>().expect("inserted above");
        if let Some(texture) = frame.texture.take() {
            // Decoded on this graphics card: nothing to upload.
            gpu.show_texture(device, texture);
            frame.timing.mark_painted();
            exchange.done.push(frame);
            return Vec::new();
        }
        gpu.showing = Some(Showing::Planes);
        let textures = &gpu.planes_for(device, &frame).textures;
        for (texture, (width, height, bytes)) in textures.iter().zip(planes(&frame)) {
            queue.write_texture(
                texture.as_image_copy(),
                bytes,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(width), rows_per_image: Some(height) },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
        }
        frame.timing.mark_painted();
        exchange.done.push(frame);
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(gpu) = resources.get::<WgpuResources>() else { return };
        match &gpu.showing {
            Some(Showing::Planes) => {
                let Some(planes) = &gpu.planes else { return };
                render_pass.set_pipeline(&gpu.pipeline);
                render_pass.set_bind_group(0, &planes.bind_group, &[]);
            }
            Some(Showing::Nv12 { bind_group, .. }) => {
                render_pass.set_pipeline(&gpu.nv12_pipeline);
                render_pass.set_bind_group(0, bind_group, &[]);
            }
            None => return,
        }
        render_pass.draw(0..3, 0..1);
    }
}

// ---- OpenGL -------------------------------------------------------------------------------

const GL_VERTEX: &str = r#"
out vec2 uv;
void main() {
    uv = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
}
"#;

const GL_FRAGMENT: &str = r#"
in vec2 uv;
out vec4 color;
uniform sampler2D y_plane;
uniform sampler2D u_plane;
uniform sampler2D v_plane;
void main() {
    float y = (texture(y_plane, uv).r - 16.0 / 255.0) * 1.164383;
    float u = texture(u_plane, uv).r - 128.0 / 255.0;
    float v = texture(v_plane, uv).r - 128.0 / 255.0;
    vec3 rgb = vec3(y + 1.596027 * v, y - 0.391762 * u - 0.812968 * v, y + 2.017232 * u);
    color = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
"#;

/// The window's OpenGL objects for the video, made on first use. There's only the one context.
struct GlResources {
    program: glow::Program,
    vertex_array: glow::VertexArray,
    textures: [glow::Texture; 3],
    size: Option<[u32; 2]>,
}

static GL: Mutex<Option<Result<GlResources, String>>> = Mutex::new(None);

unsafe fn paint_gl(gl: &glow::Context, exchange: &Mutex<Exchange>) {
    use glow::HasContext as _;
    let mut slot = lock(&GL);
    let resources = match slot.get_or_insert_with(|| unsafe { GlResources::new(gl) }) {
        Ok(resources) => resources,
        Err(_) => return,
    };
    unsafe {
        let mut exchange = lock(exchange);
        if let Some(mut frame) = exchange.next.take() {
            let size = [frame.width, frame.height];
            let resize = resources.size != Some(size);
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            for (texture, (width, height, bytes)) in resources.textures.iter().zip(planes(&frame)) {
                gl.bind_texture(glow::TEXTURE_2D, Some(*texture));
                let pixels = glow::PixelUnpackData::Slice(Some(bytes));
                if resize {
                    let (w, h) = (width as i32, height as i32);
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::R8 as i32,
                        w,
                        h,
                        0,
                        glow::RED,
                        glow::UNSIGNED_BYTE,
                        pixels,
                    );
                } else {
                    let (w, h) = (width as i32, height as i32);
                    gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, w, h, glow::RED, glow::UNSIGNED_BYTE, pixels);
                }
            }
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            resources.size = Some(size);
            frame.timing.mark_painted();
            exchange.done.push(frame);
        }
        if resources.size.is_none() {
            return;
        }
        gl.use_program(Some(resources.program));
        gl.bind_vertex_array(Some(resources.vertex_array));
        gl.disable(glow::BLEND);
        for (unit, texture) in resources.textures.iter().enumerate() {
            gl.active_texture(glow::TEXTURE0 + unit as u32);
            gl.bind_texture(glow::TEXTURE_2D, Some(*texture));
        }
        gl.draw_arrays(glow::TRIANGLES, 0, 3);
        gl.active_texture(glow::TEXTURE0);
    }
}

impl GlResources {
    unsafe fn new(gl: &glow::Context) -> Result<Self, String> {
        use glow::HasContext as _;
        let version = egui_glow::ShaderVersion::get(gl);
        let header = format!(
            "{}{}",
            version.version_declaration(),
            if version.is_embedded() { "precision mediump float;\n" } else { "" }
        );
        unsafe {
            let program = gl.create_program()?;
            let mut shaders = Vec::new();
            for (kind, source) in [(glow::VERTEX_SHADER, GL_VERTEX), (glow::FRAGMENT_SHADER, GL_FRAGMENT)] {
                let shader = gl.create_shader(kind)?;
                gl.shader_source(shader, &format!("{header}{source}"));
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    let log = gl.get_shader_info_log(shader);
                    tracing::warn!("video shader didn't compile: {log}");
                    return Err(log);
                }
                gl.attach_shader(program, shader);
                shaders.push(shader);
            }
            gl.link_program(program);
            for shader in shaders {
                gl.detach_shader(program, shader);
                gl.delete_shader(shader);
            }
            if !gl.get_program_link_status(program) {
                let log = gl.get_program_info_log(program);
                tracing::warn!("video shader didn't link: {log}");
                return Err(log);
            }
            gl.use_program(Some(program));
            for (unit, name) in ["y_plane", "u_plane", "v_plane"].into_iter().enumerate() {
                gl.uniform_1_i32(gl.get_uniform_location(program, name).as_ref(), unit as i32);
            }
            let textures = [gl.create_texture()?, gl.create_texture()?, gl.create_texture()?];
            for texture in textures {
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                for (parameter, value) in [
                    (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                    (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                    (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                    (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
                ] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, value as i32);
                }
            }
            Ok(Self { program, vertex_array: gl.create_vertex_array()?, textures, size: None })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wgsl_is_valid() {
        let module = naga::front::wgsl::parse_str(WGSL).expect("WGSL parses");
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
            .validate(&module)
            .expect("WGSL validates");
    }
}
