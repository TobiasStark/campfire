//! M0-06: Decode (ffmpeg, d3d11va) + Rendering (iced shader, NV12 -> RGB) als Spike.
//!
//!   video_player <datei.h264> [--sw] [--secs 15] [--matrix 601|709]
//!
//! --sw          Software-Decoder statt d3d11va
//! --frames      Anzeige-Takt über window::frames (Vsync) statt 60-Hz-Timer
//! --matrix      Farbmatrix (Spike-Dateien aus capture_encode sind BT.601 -> Standard 601)
//!
//! Die Datei wird in Endlosschleife mit 60 fps abgespielt. Pro Sekunde werden FPS, Decode-Zeit,
//! CPU-Anteil und RAM ausgegeben. GPU-Last bitte im Task-Manager ablesen.

extern crate ffmpeg_next as ffmpeg;

use std::{
    borrow::Cow,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use ffmpeg::{codec, format, format::Pixel, frame, media, Packet};
use iced::wgpu;
use iced::{
    mouse,
    widget::{
        column,
        shader::{self, Pipeline, Primitive, Viewport},
        text,
    },
    window, Element, Length, Rectangle, Task,
};
use windows::Win32::{
    Foundation::FILETIME,
    System::{
        ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
        },
        Threading::{GetCurrentProcess, GetProcessTimes},
    },
};

// ------------------------------------------------------------------ Decoder

#[derive(Debug)]
struct VFrame {
    id: u64,
    w: u32,
    h: u32,
    y: Vec<u8>,
    uv: Vec<u8>,
}

#[derive(Default, Debug)]
struct Shared {
    latest: Mutex<Option<Arc<VFrame>>>,
    decoded: AtomicU64,
    decode_us_sum: AtomicU64,
    decode_us_max: AtomicU64,
    uploads: AtomicU64,
    using_hw: AtomicBool,
    error: Mutex<Option<String>>,
}

unsafe extern "C" fn get_hw_format(
    _ctx: *mut ffmpeg::ffi::AVCodecContext,
    fmts: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    use ffmpeg::ffi::AVPixelFormat::*;
    let mut p = fmts;
    // SAFETY: ffmpeg übergibt eine mit AV_PIX_FMT_NONE terminierte Liste.
    unsafe {
        while *p != AV_PIX_FMT_NONE {
            if *p == AV_PIX_FMT_D3D11 {
                return *p;
            }
            p = p.add(1);
        }
    }
    AV_PIX_FMT_NONE
}

fn open_decoder(
    stream: &format::stream::Stream,
    hw: bool,
) -> Result<(ffmpeg::decoder::Video, bool), String> {
    let ctx =
        codec::context::Context::from_parameters(stream.parameters()).map_err(|e| e.to_string())?;
    let mut dec = ctx.decoder();
    let mut using_hw = false;
    if hw {
        // SAFETY: Wir setzen vor dem Öffnen den Hardware-Device-Kontext und den get_format-Callback.
        unsafe {
            use ffmpeg::ffi::*;
            let mut dev: *mut AVBufferRef = std::ptr::null_mut();
            let r = av_hwdevice_ctx_create(
                &mut dev,
                AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            );
            if r >= 0 {
                let p = dec.as_mut_ptr();
                (*p).hw_device_ctx = av_buffer_ref(dev);
                (*p).get_format = Some(get_hw_format);
                av_buffer_unref(&mut dev);
                using_hw = true;
            } else {
                return Err(format!("d3d11va-Device nicht verfügbar (Fehler {r})"));
            }
        }
    }
    let video = dec.video().map_err(|e| e.to_string())?;
    Ok((video, using_hw))
}

fn decode_loop(path: String, hw: bool, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    let res = (|| -> Result<(), String> {
        let t_start = Instant::now();
        let mut n: u64 = 0;
        let mut id: u64 = 0;
        while !stop.load(Ordering::Relaxed) {
            let mut input = format::input(&path).map_err(|e| format!("öffnen: {e}"))?;
            let stream = input
                .streams()
                .best(media::Type::Video)
                .ok_or("kein Videostream")?;
            let idx = stream.index();
            let (mut dec, using_hw) = open_decoder(&stream, hw)?;
            shared.using_hw.store(using_hw, Ordering::Relaxed);
            let mut raw = frame::Video::empty();
            let mut sw = frame::Video::empty();

            for (s, packet) in input.packets() {
                if s.index() != idx {
                    continue;
                }
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                // 60 fps Takt
                let due = t_start + Duration::from_secs_f64(n as f64 / 60.0);
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                }
                let t0 = Instant::now();
                let pkt: Packet = packet;
                dec.send_packet(&pkt)
                    .map_err(|e| format!("send_packet: {e}"))?;
                while dec.receive_frame(&mut raw).is_ok() {
                    let src: &frame::Video = if raw.format() == Pixel::D3D11 {
                        // SAFETY: Beide Frames sind gültig; kopiert vom GPU- in den CPU-Frame.
                        let r = unsafe {
                            ffmpeg::ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), raw.as_ptr(), 0)
                        };
                        if r < 0 {
                            return Err(format!("hwframe_transfer: {r}"));
                        }
                        &sw
                    } else {
                        &raw
                    };
                    let (w, h) = (src.width(), src.height());
                    let mut y = Vec::with_capacity((w * h) as usize);
                    let mut uv = Vec::with_capacity((w * h / 2) as usize);
                    let (ys, uvs) = (src.stride(0), src.stride(1));
                    for row in 0..h as usize {
                        y.extend_from_slice(&src.data(0)[row * ys..row * ys + w as usize]);
                    }
                    if src.format() == Pixel::NV12 {
                        for row in 0..(h / 2) as usize {
                            uv.extend_from_slice(&src.data(1)[row * uvs..row * uvs + w as usize]);
                        }
                    } else {
                        // Software-Decoder: YUV420P (3 Planes) -> UV verschränken
                        let vs = src.stride(2);
                        for row in 0..(h / 2) as usize {
                            let u = &src.data(1)[row * uvs..row * uvs + (w / 2) as usize];
                            let v = &src.data(2)[row * vs..row * vs + (w / 2) as usize];
                            for (a, b) in u.iter().zip(v) {
                                uv.push(*a);
                                uv.push(*b);
                            }
                        }
                    }
                    id += 1;
                    let us = t0.elapsed().as_micros() as u64;
                    shared.decode_us_sum.fetch_add(us, Ordering::Relaxed);
                    shared.decode_us_max.fetch_max(us, Ordering::Relaxed);
                    shared.decoded.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut l) = shared.latest.lock() {
                        *l = Some(Arc::new(VFrame { id, w, h, y, uv }));
                    }
                }
                n += 1;
            }
        }
        Ok(())
    })();
    if let Err(e) = res {
        if let Ok(mut g) = shared.error.lock() {
            *g = Some(e);
        }
    }
}

// ------------------------------------------------------------------ Renderer

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
        let bt709 = if std::env::args().any(|a| a == "709") {
            1.0f32
        } else {
            0.0
        };
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
    frame: Option<Arc<VFrame>>,
    shared: Arc<Shared>,
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
        self.shared.uploads.fetch_add(1, Ordering::Relaxed);
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
    shared: Arc<Shared>,
}

impl shader::Program<Msg> for VideoProgram {
    type State = ();
    type Primitive = VideoPrimitive;

    fn draw(&self, _s: &(), _c: mouse::Cursor, _b: Rectangle) -> VideoPrimitive {
        VideoPrimitive {
            frame: self.shared.latest.lock().ok().and_then(|l| l.clone()),
            shared: self.shared.clone(),
        }
    }
}

// ------------------------------------------------------------------ App

#[derive(Debug, Clone)]
enum Msg {
    Frame,
}

struct Stats {
    wall: Instant,
    cpu_100ns: u64,
    presents: u64,
    decoded: u64,
    uploads: u64,
    dec_us: u64,
}

struct App {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    start: Instant,
    secs: u64,
    info: String,
    presents: u64,
    last: Stats,
    cpu_sum: f64,
    fps_sum: f64,
    n_reports: u32,
    ram_max_mb: f64,
}

fn cpu_time_100ns() -> u64 {
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: Eigener Prozess, gültige Ausgabeparameter.
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
    }
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    t(k) + t(u)
}

fn private_mb() -> f64 {
    let mut m = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: PROCESS_MEMORY_COUNTERS_EX beginnt mit PROCESS_MEMORY_COUNTERS.
    unsafe {
        let _ = GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            m.cb,
        );
    }
    m.PrivateUsage as f64 / 1024.0 / 1024.0
}

impl App {
    fn new(shared: Arc<Shared>, stop: Arc<AtomicBool>, secs: u64) -> Self {
        Self {
            shared,
            stop,
            start: Instant::now(),
            secs,
            info: "starte ...".into(),
            presents: 0,
            last: Stats {
                wall: Instant::now(),
                cpu_100ns: cpu_time_100ns(),
                presents: 0,
                decoded: 0,
                uploads: 0,
                dec_us: 0,
            },
            cpu_sum: 0.0,
            fps_sum: 0.0,
            n_reports: 0,
            ram_max_mb: 0.0,
        }
    }

    fn update(&mut self, msg: Msg) -> Task<Msg> {
        let Msg::Frame = msg;
        self.presents += 1;
        let dt = self.last.wall.elapsed().as_secs_f64();
        if dt >= 1.0 {
            let cpu = cpu_time_100ns();
            let cpu_pct = (cpu - self.last.cpu_100ns) as f64 / 1e7 / dt * 100.0;
            let decoded = self.shared.decoded.load(Ordering::Relaxed);
            let uploads = self.shared.uploads.load(Ordering::Relaxed);
            let dec_us = self.shared.decode_us_sum.load(Ordering::Relaxed);
            let d_dec = decoded - self.last.decoded;
            let fps_shown = (self.presents - self.last.presents) as f64 / dt;
            let fps_new = (uploads - self.last.uploads) as f64 / dt;
            let dec_ms = if d_dec > 0 {
                (dec_us - self.last.dec_us) as f64 / d_dec as f64 / 1000.0
            } else {
                0.0
            };
            let ram = private_mb();
            self.info = format!(
                "{} | angezeigt {:.0} fps | neue Frames {:.0}/s | decode+copy {:.2} ms (max {:.1}) | CPU {:.1}% eines Kerns-Summe | RAM {:.0} MB",
                if self.shared.using_hw.load(Ordering::Relaxed) { "d3d11va" } else { "Software" },
                fps_shown, fps_new, dec_ms,
                self.shared.decode_us_max.swap(0, Ordering::Relaxed) as f64 / 1000.0,
                cpu_pct, ram
            );
            println!("{}", self.info);
            if self.start.elapsed().as_secs() >= 2 {
                self.cpu_sum += cpu_pct;
                self.fps_sum += fps_shown;
                self.n_reports += 1;
                self.ram_max_mb = self.ram_max_mb.max(ram);
            }
            self.last = Stats {
                wall: Instant::now(),
                cpu_100ns: cpu,
                presents: self.presents,
                decoded,
                uploads,
                dec_us,
            };
        }
        if let Some(e) = self.shared.error.lock().ok().and_then(|g| g.clone()) {
            println!("DECODER-FEHLER: {e}");
            self.stop.store(true, Ordering::Relaxed);
            return iced::exit();
        }
        if self.start.elapsed().as_secs() >= self.secs {
            self.stop.store(true, Ordering::Relaxed);
            let n = self.n_reports.max(1) as f64;
            println!(
                "\n=== ZUSAMMENFASSUNG ===\nØ angezeigt {:.1} fps, Ø CPU {:.1}% (100% = ein Kern), RAM max {:.0} MB",
                self.fps_sum / n,
                self.cpu_sum / n,
                self.ram_max_mb
            );
            return iced::exit();
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Msg> {
        column![
            text(self.info.clone()).size(14),
            iced::widget::shader(VideoProgram {
                shared: self.shared.clone()
            })
            .width(Length::Fill)
            .height(Length::Fill)
        ]
        .into()
    }
}

fn main() -> iced::Result {
    ffmpeg::init().expect("ffmpeg init");
    ffmpeg::log::set_level(ffmpeg::log::Level::Error);
    let args: Vec<String> = std::env::args().collect();
    let Some(path) = args.get(1).filter(|a| !a.starts_with("--")).cloned() else {
        println!("Aufruf: video_player <datei.h264> [--sw] [--secs 15] [--matrix 601|709]");
        return Ok(());
    };
    let hw = !args.iter().any(|a| a == "--sw");
    let secs: u64 = args
        .iter()
        .position(|a| a == "--secs")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);

    let use_frames = args.iter().any(|a| a == "--frames");
    let shared = Arc::new(Shared::default());
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (s, st) = (shared.clone(), stop.clone());
        std::thread::spawn(move || decode_loop(path, hw, s, st));
    }
    iced::application(
        move || (App::new(shared.clone(), stop.clone(), secs), Task::none()),
        App::update,
        App::view,
    )
    .subscription(move |_| {
        if use_frames {
            window::frames().map(|_| Msg::Frame)
        } else {
            iced::time::every(Duration::from_micros(16_667)).map(|_| Msg::Frame)
        }
    })
    .window_size((1280.0, 720.0))
    .run()
}
