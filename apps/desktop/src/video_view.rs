//! Anzeige eines dekodierten Streams (M3-06): NV12-Texturen, Umrechnung nach RGB im Shader.
//! Das Bild wird mit richtigem Seitenverhaeltnis in den verfuegbaren Platz gesetzt.

use app_video::frame::{FrameSlot, YuvFrame};
use iced::wgpu;
use iced::widget::shader::{Pipeline, Primitive, Viewport};
use iced::widget::{container, responsive, shader};
use iced::{mouse, Element, Length, Rectangle};
use std::borrow::Cow;
use std::sync::Arc;

const WGSL: &str = r#"
struct U { flags: vec4<f32> }; // x: Ziel ist sRGB, y: 1 = BT.709, sonst BT.601
@group(0) @binding(0) var ty: texture_2d<f32>;
@group(0) @binding(1) var tuv: texture_2d<f32>;
@group(0) @binding(2) var smp: sampler;
@group(0) @binding(3) var<uniform> u: U;
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs(@builtin(vertex_index) i: u32) -> VOut {
    var p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var o: VOut;
    o.pos = vec4<f32>(p[i], 0.0, 1.0);
    o.uv = vec2<f32>((p[i].x + 1.0) * 0.5, (1.0 - p[i].y) * 0.5);
    return o;
}
@fragment fn fs(in: VOut) -> @location(0) vec4<f32> {
    let y = textureSample(ty, smp, in.uv).r;
    let c = textureSample(tuv, smp, in.uv).rg;
    let yy = (y - 16.0 / 255.0) * (255.0 / 219.0);
    let cb = (c.x - 128.0 / 255.0) * (255.0 / 224.0);
    let cr = (c.y - 128.0 / 255.0) * (255.0 / 224.0);
    var rgb: vec3<f32>;
    if (u.flags.y > 0.5) {
        rgb = vec3<f32>(yy + 1.5748 * cr, yy - 0.1873 * cb - 0.4681 * cr, yy + 1.8556 * cb);
    } else {
        rgb = vec3<f32>(yy + 1.402 * cr, yy - 0.344136 * cb - 0.714136 * cr, yy + 1.772 * cb);
    }
    rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    if (u.flags.x > 0.5) {
        rgb = pow(rgb, vec3<f32>(2.2));
    }
    return vec4<f32>(rgb, 1.0);
}
"#;

struct Tex {
    w: u32,
    h: u32,
    y: wgpu::Texture,
    uv: wgpu::Texture,
    bind: wgpu::BindGroup,
}

struct VideoPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    tex: Option<Tex>,
    last_id: u64,
}

impl Pipeline for VideoPipeline {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(WGSL)),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
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
            label: Some("nv12 layout"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nv12 pipeline"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("flags"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // Der Encoder (swscale) kodiert BGRA nach BT.601.
        let bt709 = 0.0f32;
        let srgb = if format.is_srgb() { 1.0f32 } else { 0.0 };
        let mut bytes = [0u8; 16];
        bytes[..4].copy_from_slice(&srgb.to_le_bytes());
        bytes[4..8].copy_from_slice(&bt709.to_le_bytes());
        queue.write_buffer(&uniform, 0, &bytes);
        Self {
            pipeline,
            layout,
            sampler,
            uniform,
            tex: None,
            last_id: 0,
        }
    }
}

#[derive(Debug)]
struct VideoPrimitive {
    frame: Option<Arc<YuvFrame>>,
}

impl Primitive for VideoPrimitive {
    type Pipeline = VideoPipeline;

    fn prepare(
        &self,
        p: &mut VideoPipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        _viewport: &Viewport,
    ) {
        let Some(f) = &self.frame else { return };
        let need_new = p.tex.as_ref().is_none_or(|t| t.w != f.w || t.h != f.h);
        if need_new {
            let mk = |label, w, h, format| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let y = mk("y", f.w, f.h, wgpu::TextureFormat::R8Unorm);
            let uv = mk("uv", f.w / 2, f.h / 2, wgpu::TextureFormat::Rg8Unorm);
            let yv = y.create_view(&Default::default());
            let uvv = uv.create_view(&Default::default());
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("nv12 bind"),
                layout: &p.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&yv),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&uvv),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&p.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: p.uniform.as_entire_binding(),
                    },
                ],
            });
            p.tex = Some(Tex {
                w: f.w,
                h: f.h,
                y,
                uv,
                bind,
            });
            p.last_id = 0;
        }
        if p.last_id == f.id {
            return;
        }
        let Some(t) = &p.tex else { return };
        let write = |tex: &wgpu::Texture, data: &[u8], w: u32, h: u32, bpp: u32| {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * bpp),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        };
        write(&t.y, &f.y, f.w, f.h, 1);
        write(&t.uv, &f.uv, f.w / 2, f.h / 2, 2);
        p.last_id = f.id;
    }

    fn draw(&self, p: &VideoPipeline, pass: &mut wgpu::RenderPass<'_>) -> bool {
        let Some(t) = &p.tex else { return true };
        pass.set_pipeline(&p.pipeline);
        pass.set_bind_group(0, &t.bind, &[]);
        pass.draw(0..3, 0..1);
        true
    }
}

struct VideoProgram {
    slot: Arc<FrameSlot>,
}

impl<M> shader::Program<M> for VideoProgram {
    type State = ();
    type Primitive = VideoPrimitive;

    fn draw(&self, _s: &(), _c: mouse::Cursor, _b: Rectangle) -> VideoPrimitive {
        VideoPrimitive {
            frame: self.slot.get(),
        }
    }
}

/// Das Videobild, eingepasst in den verfuegbaren Platz (schwarze Balken statt Verzerrung).
pub fn view<'a, M: 'a>(slot: Arc<FrameSlot>) -> Element<'a, M> {
    container(responsive(move |size| {
        let (w, h) = slot.get().map_or((16.0, 9.0), |f| (f.w as f32, f.h as f32));
        let scale = (size.width / w).min(size.height / h).max(0.0);
        let video = shader(VideoProgram { slot: slot.clone() })
            .width(Length::Fixed((w * scale).floor().max(1.0)))
            .height(Length::Fixed((h * scale).floor().max(1.0)));
        container(video)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }))
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_| container::Style {
        background: Some(crate::ui::current().media_bg.into()),
        ..Default::default()
    })
    .into()
}
