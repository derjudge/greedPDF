//! Affichage wgpu : textures (pages, tuiles, miniatures, texte) dessinées sur
//! des quads. Les pages ont des mipmaps : réduites par le GPU (zoom ≤ S),
//! elles restent nettes, sans scintillement (SPEC.md §6).

use std::num::NonZeroU64;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use winit::window::Window;

const SHADER: &str = r#"
struct Quad {
    rect: vec4<f32>,   // destination, coordonnées normalisées : x0, y0, x1, y1
    uv: vec4<f32>,     // zone de la texture : u0, v0, u1, v1
    tint: vec4<f32>,   // multiplicateur de couleur (prémultiplié)
    params: vec4<f32>, // x : rotation en quarts de tour (sens horaire)
};
@group(0) @binding(0) var<uniform> quad: Quad;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let u = f32(i & 1u);
    let v = f32(i >> 1u);
    var out: VsOut;
    out.pos = vec4<f32>(mix(quad.rect.x, quad.rect.z, u), mix(quad.rect.y, quad.rect.w, v), 0.0, 1.0);
    // Rotation : le coin (u, v) affiché lit la texture en (tu, tv).
    var t = vec2<f32>(u, v);
    let r = u32(quad.params.x + 0.5) % 4u;
    if (r == 1u) { t = vec2<f32>(v, 1.0 - u); }
    if (r == 2u) { t = vec2<f32>(1.0 - u, 1.0 - v); }
    if (r == 3u) { t = vec2<f32>(1.0 - v, u); }
    out.uv = vec2<f32>(mix(quad.uv.x, quad.uv.z, t.x), mix(quad.uv.y, quad.uv.w, t.y));
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv) * quad.tint;
}
"#;

/// Génération des mipmaps : chaque niveau est la moyenne 2×2 du précédent.
const MIP_SHADER: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: VsOut;
    out.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(p.x, 1.0 - p.y);
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSampleLevel(src, samp, in.uv, 0.0);
}
"#;

const QUAD_UNIFORM: u64 = 64;
const MAX_QUADS: u64 = 4096;
const TEX_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Une texture sur le GPU, prête à dessiner.
pub struct Tex {
    pub w: u32,
    pub h: u32,
    bind_group: wgpu::BindGroup,
}

/// Rectangle en pixels de la fenêtre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
}

/// Un élément à dessiner.
pub struct Draw<'a> {
    pub tex: &'a Tex,
    pub dest: Rect,
    /// Couleur multipliée (RGBA prémultiplié, 0..1). Blanc = texture telle quelle.
    pub tint: [f32; 4],
    /// Rotation de l'image en degrés (0, 90, 180, 270), sens horaire.
    pub rot: u32,
}

pub struct Gpu {
    pub window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler_mip: wgpu::Sampler,
    sampler_plain: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    uniform_stride: u64,
    mip_pipeline: wgpu::RenderPipeline,
    mip_layout: wgpu::BindGroupLayout,
    /// Texture blanche 1×1 : rectangles de couleur unie.
    white: Option<Tex>,
    pub background: [f32; 3],
}

impl Gpu {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance.create_surface(window.clone())?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .context("aucun adaptateur GPU compatible")?;
        let al = adapter.limits();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("greedpdf"),
            required_limits: wgpu::Limits {
                max_texture_dimension_2d: al.max_texture_dimension_2d,
                ..wgpu::Limits::default()
            },
            ..Default::default()
        }))?;

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("surface non supportée")?;
        let caps = surface.get_capabilities(&adapter);
        if let Some(f) = caps.formats.iter().find(|f| f.is_srgb()) {
            config.format = *f;
        }
        // Mailbox : synchronisé à l'écran mais jamais bloquant (la dernière
        // image gagne). Avec FIFO, `present` peut bloquer jusqu'à 1 s quand la
        // fenêtre n'est pas affichée (constaté avec le pilote NVIDIA), ce qui
        // gèle la boucle d'événements.
        config.present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::AutoVsync
        };
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);

        // Pipeline principal.
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("quad"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
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
        let sampler_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: NonZeroU64::new(QUAD_UNIFORM),
                    },
                    count: None,
                },
                tex_entry(1),
                sampler_entry(2),
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quad"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quad"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });

        // Pipeline des mipmaps.
        let mip_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mip"),
            source: wgpu::ShaderSource::Wgsl(MIP_SHADER.into()),
        });
        let mip_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("mip"),
            entries: &[tex_entry(0), sampler_entry(1)],
        });
        let mpl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mip"),
            bind_group_layouts: &[Some(&mip_layout)],
            ..Default::default()
        });
        let mip_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mip"),
            layout: Some(&mpl),
            vertex: wgpu::VertexState {
                module: &mip_shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &mip_shader,
                entry_point: Some("fs"),
                targets: &[Some(TEX_FORMAT.into())],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler_mip = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("trilinéaire"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let sampler_plain = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linéaire"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let align = device.limits().min_uniform_buffer_offset_alignment as u64;
        let uniform_stride = QUAD_UNIFORM.div_ceil(align) * align;
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quads"),
            size: uniform_stride * MAX_QUADS,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let mut me = Self {
            window,
            surface,
            device,
            queue,
            config,
            pipeline,
            layout,
            sampler_mip,
            sampler_plain,
            uniforms,
            uniform_stride,
            mip_pipeline,
            mip_layout,
            white: None,
            background: [0.02, 0.02, 0.025],
        };
        me.white = Some(me.upload(1, 1, &[255, 255, 255, 255], false));
        Ok(me)
    }

    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if w > 0 && h > 0 && (w, h) != self.size() {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(&self.device, &self.config);
        }
    }

    pub fn white(&self) -> &Tex {
        self.white.as_ref().expect("créée dans Gpu::new")
    }

    /// Envoie une image RGBA8 (sRGB, alpha prémultiplié) au GPU, avec ou sans mipmaps.
    pub fn upload(&self, w: u32, h: u32, rgba: &[u8], mipmaps: bool) -> Tex {
        let levels = if mipmaps { 32 - w.max(h).max(1).leading_zeros() } else { 1 };
        let extent = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
        let mut usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
        if levels > 1 {
            usage |= wgpu::TextureUsages::RENDER_ATTACHMENT;
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image"),
            size: extent,
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TEX_FORMAT,
            usage,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            extent,
        );
        if levels > 1 {
            self.generate_mips(&texture, levels);
        }
        let view = texture.create_view(&Default::default());
        let sampler = if levels > 1 { &self.sampler_mip } else { &self.sampler_plain };
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &self.uniforms,
                        offset: 0,
                        size: NonZeroU64::new(QUAD_UNIFORM),
                    }),
                },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(sampler) },
            ],
        });
        Tex { w, h, bind_group }
    }

    fn generate_mips(&self, texture: &wgpu::Texture, levels: u32) {
        let mut enc = self.device.create_command_encoder(&Default::default());
        let view = |level| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                base_mip_level: level,
                mip_level_count: Some(1),
                ..Default::default()
            })
        };
        for level in 1..levels {
            let src = view(level - 1);
            let dst = view(level);
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("mip"),
                layout: &self.mip_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&src) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler_plain) },
                ],
            });
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("mip"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dst,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::WHITE), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.mip_pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([enc.finish()]);
    }

    /// Dessine une frame : fond, puis les éléments dans l'ordre.
    pub fn draw(&self, items: &[Draw]) {
        let t0 = std::time::Instant::now();
        let acquired = self.surface.get_current_texture();
        if std::env::var_os("GREEDPDF_DEBUG_GPU").is_some() {
            let kind = match &acquired {
                wgpu::CurrentSurfaceTexture::Success(_) => "ok",
                wgpu::CurrentSurfaceTexture::Suboptimal(_) => "sous-optimal",
                wgpu::CurrentSurfaceTexture::Timeout => "délai dépassé",
                wgpu::CurrentSurfaceTexture::Occluded => "masquée",
                wgpu::CurrentSurfaceTexture::Outdated => "périmée",
                wgpu::CurrentSurfaceTexture::Lost => "perdue",
                _ => "autre",
            };
            eprintln!("[gpu] acquisition {kind} en {:.1} ms ({} éléments)", t0.elapsed().as_secs_f64() * 1e3, items.len());
        }
        let frame = match acquired {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return;
            }
            _ => return,
        };
        let (ww, wh) = (self.config.width as f32, self.config.height as f32);
        let items = &items[..items.len().min(MAX_QUADS as usize)];
        let mut data = vec![0u8; (self.uniform_stride * items.len().max(1) as u64) as usize];
        for (i, it) in items.iter().enumerate() {
            let d = it.dest;
            let v: [f32; 16] = [
                d.x / ww * 2.0 - 1.0,
                1.0 - d.y / wh * 2.0,
                (d.x + d.w) / ww * 2.0 - 1.0,
                1.0 - (d.y + d.h) / wh * 2.0,
                0.0,
                0.0,
                1.0,
                1.0,
                it.tint[0],
                it.tint[1],
                it.tint[2],
                it.tint[3],
                (it.rot / 90 % 4) as f32,
                0.0,
                0.0,
                0.0,
            ];
            let off = i * self.uniform_stride as usize;
            for (j, f) in v.iter().enumerate() {
                data[off + j * 4..off + j * 4 + 4].copy_from_slice(&f.to_le_bytes());
            }
        }
        self.queue.write_buffer(&self.uniforms, 0, &data);
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let [r, g, b] = self.background;
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: r as f64, g: g as f64, b: b as f64, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            for (i, it) in items.iter().enumerate() {
                pass.set_bind_group(0, &it.tex.bind_group, &[(i as u64 * self.uniform_stride) as u32]);
                pass.draw(0..4, 0..1);
            }
        }
        self.queue.submit([enc.finish()]);
        self.window.pre_present_notify();
        let t = std::time::Instant::now();
        self.queue.present(frame);
        if std::env::var_os("GREEDPDF_DEBUG_GPU").is_some() {
            eprintln!("[gpu] présentation en {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
        }
    }
}
