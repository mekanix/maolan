use crate::video_runtime::{
    registry::{RegisteredVideoTextureSource, VideoTextureRegistry},
    types::{VideoFrameMetadata, VideoTextureHandle},
};
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{Tree, Widget};
use iced::{Element, Length, Rectangle, Size, mouse};
use iced_wgpu::primitive::{self, Pipeline as PrimitivePipeline, Renderer as PrimitiveRenderer};
use iced_wgpu::wgpu;
use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{Arc, Mutex},
};

pub struct VideoSurface {
    handle: VideoTextureHandle,
    metadata: VideoFrameMetadata,
    registry: Arc<Mutex<VideoTextureRegistry>>,
    width: Length,
    height: Length,
}

impl VideoSurface {
    pub fn new(
        handle: VideoTextureHandle,
        metadata: VideoFrameMetadata,
        registry: Arc<Mutex<VideoTextureRegistry>>,
    ) -> Self {
        Self {
            handle,
            metadata,
            registry,
            width: Length::Fill,
            height: Length::Fill,
        }
    }

    pub fn width(mut self, width: Length) -> Self {
        self.width = width;
        self
    }

    pub fn height(mut self, height: Length) -> Self {
        self.height = height;
        self
    }
}

#[derive(Debug)]
struct VideoPrimitive {
    handle: VideoTextureHandle,
    registry: Arc<Mutex<VideoTextureRegistry>>,
}

#[derive(Debug)]
struct VideoPrimitivePipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    textures: HashMap<VideoTextureHandle, CachedVideoTexture>,
}

#[derive(Debug)]
struct CachedVideoTexture {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    revision: u64,
}

impl PrimitivePipeline for VideoPrimitivePipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("maolan.video_surface.shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(
                r#"
@group(0) @binding(0)
var video_tex: texture_2d<f32>;

@group(0) @binding(1)
var video_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>( 1.0,  1.0),
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0,  1.0),
        vec2<f32>(-1.0,  1.0),
    );

    var uvs = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 0.0),
    );

    var out: VertexOutput;
    out.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    out.uv = uvs[vertex_index];
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(video_tex, video_sampler, in.uv);
}
"#,
            )),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("maolan.video_surface.bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("maolan.video_surface.pipeline_layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("maolan.video_surface.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..wgpu::PrimitiveState::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("maolan.video_surface.sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        Self {
            pipeline,
            bind_group_layout,
            sampler,
            textures: HashMap::new(),
        }
    }
}

impl primitive::Primitive for VideoPrimitive {
    type Pipeline = VideoPrimitivePipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        _viewport: &iced_wgpu::graphics::Viewport,
    ) {
        let Ok(registry) = self.registry.lock() else {
            return;
        };
        let Some(registered) = registry.get(&self.handle) else {
            return;
        };
        if pipeline
            .textures
            .get(&self.handle)
            .is_some_and(|cached| cached.revision == registered.revision)
        {
            return;
        }
        let (width, height, pixels) = match &registered.source {
            RegisteredVideoTextureSource::Rgba8 {
                width,
                height,
                pixels,
            } => (*width, *height, pixels.clone()),
        };
        let revision = registered.revision;

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("maolan.video_surface.placeholder_texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            texture.as_image_copy(),
            &pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width.saturating_mul(4)),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("maolan.video_surface.bind_group"),
            layout: &pipeline.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&pipeline.sampler),
                },
            ],
        });

        pipeline.textures.insert(
            self.handle,
            CachedVideoTexture {
                texture,
                bind_group,
                revision,
            },
        );
    }

    fn draw(&self, pipeline: &Self::Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        let Some(texture) = pipeline.textures.get(&self.handle) else {
            return false;
        };
        let _ = &texture.texture;

        render_pass.set_pipeline(&pipeline.pipeline);
        render_pass.set_bind_group(0, &texture.bind_group, &[]);
        render_pass.draw(0..6, 0..1);
        true
    }
}

impl<Message, Theme> Widget<Message, Theme, iced::Renderer> for VideoSurface {
    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: self.height,
        }
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = limits.width(self.width).height(self.height).resolve(
            self.width,
            self.height,
            Size::new(self.metadata.width as f32, self.metadata.height as f32),
        );
        layout::Node::new(size)
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        <iced::Renderer as PrimitiveRenderer>::draw_primitive(
            renderer,
            bounds,
            VideoPrimitive {
                handle: self.handle,
                registry: self.registry.clone(),
            },
        );
    }
}

impl<'a, Message, Theme> From<VideoSurface> for Element<'a, Message, Theme, iced::Renderer>
where
    Message: 'a,
    Theme: 'a,
{
    fn from(surface: VideoSurface) -> Self {
        Self::new(surface)
    }
}
