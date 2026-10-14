// SPDX-License-Identifier: GPL-3.0-or-later
//! Vulkan-only native-frame upload and integer nearest-neighbor presentation.
//! Destination pixels never pass through a CPU-sized scaling buffer.
use crate::video::{HEIGHT, IndexedFrame, WIDTH};
use std::sync::{Arc, Mutex};
use winit::window::Window;

const SHADER: &str = r#"
@group(0) @binding(0) var image: texture_2d<u32>;
@group(0) @binding(1) var<uniform> destination: vec4<u32>;
@group(0) @binding(2) var palette: texture_2d<f32>;
@vertex fn vs(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let xy = array<vec2<f32>, 3>(vec2(-1., -1.), vec2(3., -1.), vec2(-1., 3.));
    return vec4(xy[index], 0., 1.);
}
@fragment fn fs(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let size = destination.xy;
    let scale = min(size.x / 320u, size.y / 200u);
    var draw: vec2<u32>;
    if scale > 0u {
        draw = vec2(320u, 200u) * scale;
    } else if size.x * 200u <= size.y * 320u {
        draw = vec2(size.x, max(1u, size.x * 200u / 320u));
    } else {
        draw = vec2(max(1u, size.y * 320u / 200u), size.y);
    }
    let offset = (size - draw) / 2u;
    let pixel = vec2<u32>(position.xy);
    if any(pixel < offset) || any(pixel >= offset + draw) {
        return vec4(0., 0., 0., 1.);
    }
    let source = (pixel - offset) * vec2(320u, 200u) / draw;
    let index = textureLoad(image, vec2<i32>(source), 0).r & destination.z;
    return textureLoad(palette, vec2<i32>(i32(index), 0), 0);
}
"#;

fn vulkan_instance() -> wgpu::Instance {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    // Deliberately do not honor WGPU_BACKEND overrides or enable other APIs.
    descriptor.backends = wgpu::Backends::VULKAN;
    wgpu::Instance::new(descriptor)
}

fn hardware_adapter(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'_>>,
) -> Result<wgpu::Adapter, String> {
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: surface,
        apply_limit_buckets: false,
    })).map_err(|e| format!("wgpu Vulkan hardware adapter unavailable: {e}; select --renderer softbuffer explicitly for CPU presentation"))?;
    let info = adapter.get_info();
    if info.backend != wgpu::Backend::Vulkan
        || !matches!(
            info.device_type,
            wgpu::DeviceType::DiscreteGpu
                | wgpu::DeviceType::IntegratedGpu
                | wgpu::DeviceType::VirtualGpu
        )
    {
        return Err(format!(
            "wgpu requires a hardware Vulkan GPU, found {:?} {:?}; select --renderer softbuffer explicitly",
            info.backend, info.device_type
        ));
    }
    Ok(adapter)
}

type Failure = Arc<Mutex<Option<String>>>;
struct Raster {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    image: wgpu::Texture,
    palette: wgpu::Texture,
    destination: wgpu::Buffer,
    failure: Failure,
}
impl Raster {
    fn new(adapter: &wgpu::Adapter, format: wgpu::TextureFormat) -> Result<Self, String> {
        if !matches!(
            format,
            wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Bgra8Unorm
        ) {
            return Err(
                "wgpu needs an 8-bit non-sRGB surface to preserve DAC bytes exactly".into(),
            );
        }
        let limits = wgpu::Limits {
            max_texture_dimension_2d: adapter.limits().max_texture_dimension_2d.min(8192),
            ..wgpu::Limits::downlevel_defaults()
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("PAL Vulkan renderer"),
            required_features: wgpu::Features::empty(),
            required_limits: limits,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: wgpu::Trace::Off,
        }))
        .map_err(|e| format!("wgpu device initialization failed: {e}"))?;
        let failure: Failure = Arc::new(Mutex::new(None));
        let errors = failure.clone();
        device.on_uncaptured_error(Arc::new(move |error: wgpu::Error| {
            *errors.lock().unwrap() = Some(format!(
                "wgpu fatal GPU error (including out-of-memory): {error}"
            ));
        }));
        let lost = failure.clone();
        device.set_device_lost_callback(move |reason, message| {
            *lost.lock().unwrap() = Some(format!("wgpu device lost: {reason:?}: {message}"));
        });
        let image = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("PAL 320x200 index upload"),
            size: wgpu::Extent3d {
                width: WIDTH as u32,
                height: HEIGHT as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let palette = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("PAL 256-entry exact RGBA palette"),
            size: wgpu::Extent3d {
                width: 256,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let destination = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("PAL destination dimensions and pixel mask"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("PAL exact nearest letterbox"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("PAL raster pipeline"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(format.into())],
            }),
            multiview_mask: None,
            cache: None,
        });
        let view = image.create_view(&Default::default());
        let palette_view = palette.create_view(&Default::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("PAL native frame and geometry"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: destination.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&palette_view),
                },
            ],
        });
        let raster = Self {
            device,
            queue,
            pipeline,
            bind_group,
            image,
            palette,
            destination,
            failure,
        };
        raster.check()?;
        Ok(raster)
    }
    fn check(&self) -> Result<(), String> {
        self.device
            .poll(wgpu::PollType::Poll)
            .map_err(|e| format!("wgpu polling failed: {e}"))?;
        match &*self.failure.lock().unwrap() {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }
    fn validate_size(&self, width: u32, height: u32) -> Result<(), String> {
        let max = self.device.limits().max_texture_dimension_2d;
        if width == 0
            || height == 0
            || width > max
            || height > max
            || u64::from(width) * u64::from(height) > 64 * 1024 * 1024
        {
            return Err(format!(
                "wgpu destination dimensions unsupported: {width}x{height} (limit {max})"
            ));
        }
        Ok(())
    }
    fn upload(&mut self, source: &IndexedFrame, width: u32, height: u32) -> Result<(), String> {
        self.check()?;
        self.validate_size(width, height)?;
        source
            .validate()
            .map_err(|e| format!("wgpu indexed frame invalid: {e}"))?;
        #[cfg(feature = "profiling")]
        let _upload_span = crate::host_profile::Span::new("gpu_cpu_upload");
        // Queue::write_texture does not require 256-byte row alignment.
        // Upload native indices directly; never expand the frame on the CPU.
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.image,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &source.indices,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(WIDTH as u32),
                rows_per_image: Some(HEIGHT as u32),
            },
            wgpu::Extent3d {
                width: WIDTH as u32,
                height: HEIGHT as u32,
                depth_or_array_layers: 1,
            },
        );
        let mut palette_bytes = [0u8; 256 * 4];
        for (bytes, color) in palette_bytes.chunks_exact_mut(4).zip(&source.palette) {
            bytes.copy_from_slice(color);
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.palette,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &palette_bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256 * 4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 256,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let mut uniform = [0u8; 16];
        for (bytes, value) in
            uniform
                .chunks_exact_mut(4)
                .zip([width, height, u32::from(source.pixel_mask), 0])
        {
            bytes.copy_from_slice(&value.to_le_bytes());
        }
        self.queue.write_buffer(&self.destination, 0, &uniform);
        Ok(())
    }
    fn encode(&self, encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("PAL nearest letterbox draw"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

pub(crate) struct GpuDisplay {
    // Surface owns an Arc of Window and is dropped before instance/window.
    surface: wgpu::Surface<'static>,
    raster: Raster,
    instance: wgpu::Instance,
    window: Arc<Window>,
    config: wgpu::SurfaceConfiguration,
    configured: bool,
    #[cfg(feature = "profiling")]
    presentation_clock: crate::host_profile::PresentationClock,
}
impl GpuDisplay {
    pub(crate) fn new(window: Arc<Window>) -> Result<Self, String> {
        let instance = vulkan_instance();
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("wgpu Wayland surface failed: {e}"))?;
        let adapter = hardware_adapter(&instance, Some(&surface))?;
        let caps = surface.get_capabilities(&adapter);
        let format = [wgpu::TextureFormat::Bgra8Unorm, wgpu::TextureFormat::Rgba8Unorm].into_iter().find(|f| caps.formats.contains(f)).ok_or("wgpu Wayland surface has no exact 8-bit non-sRGB format; use --renderer softbuffer explicitly")?;
        let raster = Raster::new(&adapter, format)?;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: 1,
            height: 1,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
        };
        if !caps.alpha_modes.contains(&config.alpha_mode) {
            return Err("wgpu Wayland surface lacks opaque presentation".into());
        }
        let mut display = Self {
            surface,
            raster,
            instance,
            window,
            config,
            configured: false,
            #[cfg(feature = "profiling")]
            presentation_clock: Default::default(),
        };
        let size = display.window.inner_size();
        display.resize(size.width, size.height)?;
        eprintln!(
            "renderer=wgpu backend=Vulkan adapter={} format={format:?}",
            adapter.get_info().name
        );
        Ok(display)
    }
    pub(crate) fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        self.raster.validate_size(width, height)?;
        if !self.configured || self.config.width != width || self.config.height != height {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.raster.device, &self.config);
            self.configured = true;
        }
        self.raster.check()
    }
    pub(crate) fn draw(&mut self, source: &IndexedFrame) -> Result<(), String> {
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        self.resize(size.width, size.height)?;
        self.raster.check()?;
        // At most two acquisitions; never spin on lost/outdated/occluded surfaces.
        for attempt in 0..2 {
            let acquired = {
                #[cfg(feature = "profiling")]
                let _acquire_span = crate::host_profile::Span::new("swapchain_acquire");
                self.surface.get_current_texture()
            };
            let (frame, suboptimal) = match acquired {
                wgpu::CurrentSurfaceTexture::Success(frame) => (frame, false),
                wgpu::CurrentSurfaceTexture::Suboptimal(frame) => (frame, true),
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    return Ok(());
                }
                wgpu::CurrentSurfaceTexture::Outdated => {
                    self.configured = false;
                    self.resize(size.width, size.height)?;
                    if attempt == 0 {
                        continue;
                    } else {
                        return Ok(());
                    }
                }
                wgpu::CurrentSurfaceTexture::Lost => {
                    self.surface = self
                        .instance
                        .create_surface(self.window.clone())
                        .map_err(|e| format!("wgpu lost surface recreation failed: {e}"))?;
                    self.configured = false;
                    self.resize(size.width, size.height)?;
                    if attempt == 0 {
                        continue;
                    } else {
                        return Ok(());
                    }
                }
                wgpu::CurrentSurfaceTexture::Validation => {
                    return Err("wgpu surface acquisition validation failed".into());
                }
            };
            self.raster.upload(source, size.width, size.height)?;
            let view = frame.texture.create_view(&Default::default());
            let mut encoder = self
                .raster
                .device
                .create_command_encoder(&Default::default());
            self.raster.encode(&mut encoder, &view);
            let commands = encoder.finish();
            {
                #[cfg(feature = "profiling")]
                let _submit_span = crate::host_profile::Span::new("queue_submit");
                self.raster.queue.submit([commands]);
            }
            self.window.pre_present_notify();
            {
                #[cfg(feature = "profiling")]
                let _present_span = crate::host_profile::Span::new("present_cpu");
                self.raster.queue.present(frame);
            }
            if suboptimal {
                self.configured = false;
            }
            self.raster.check()?;
            #[cfg(feature = "profiling")]
            self.presentation_clock.observe(true);
            return Ok(());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::FRAME_BYTES;
    use std::time::{Duration, Instant};
    #[test]
    #[ignore = "bounded real GPU timing diagnostic; not compositor/physical latency"]
    fn real_gpu_warm_dispatch_timing() {
        let instance = vulkan_instance();
        let adapter = hardware_adapter(&instance, None).unwrap();
        eprintln!("GPU timing adapter: {:?}", adapter.get_info());
        let mut raster = Raster::new(&adapter, wgpu::TextureFormat::Rgba8Unorm).unwrap();
        let source = IndexedFrame {
            indices: vec![1; FRAME_BYTES],
            palette: [[0x12, 0x83, 0xfa, 255]; 256],
            pixel_mask: 0xff,
        };
        // Match the old solid-color workload and CPU scale-only comparison.
        // Frame snapshot/decode is intentionally outside both timed paths.
        let rgb = source.rgb_pixels().unwrap();
        let target = raster.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("bounded 4K timing target"),
            size: wgpu::Extent3d {
                width: 3840,
                height: 2160,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let mut dispatch = Vec::new();
        let mut completed = Vec::new();
        let mut cpu = Vec::new();
        for index in 0..16 {
            let before = Instant::now();
            raster.upload(&source, 3840, 2160).unwrap();
            let mut encoder = raster.device.create_command_encoder(&Default::default());
            raster.encode(&mut encoder, &view);
            let submission = raster.queue.submit([encoder.finish()]);
            let submitted = before.elapsed();
            raster
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(submission),
                    timeout: Some(Duration::from_secs(5)),
                })
                .unwrap();
            let complete = before.elapsed();
            if index >= 4 {
                dispatch.push(submitted);
                completed.push(complete);
            }
            let before = Instant::now();
            std::hint::black_box(
                crate::video::scale_frame(std::hint::black_box(&rgb), 3840, 2160).unwrap(),
            );
            if index >= 4 {
                cpu.push(before.elapsed());
            }
        }
        dispatch.sort();
        completed.sort();
        cpu.sort();
        eprintln!(
            "4K warm diagnostic samples=12 GPU_host_submit_median={:?} GPU_completed_median={:?} CPU_scale_allocate_median={:?}; excludes frame decode, compositor, readback, guest and audio",
            dispatch[6], completed[6], cpu[6]
        );
        raster.check().unwrap();
    }

    #[test]
    #[ignore = "opt-in real Vulkan GPU upload/render/readback; no desktop or audio"]
    fn real_gpu_pixels_match_cpu_oracle() {
        let instance = vulkan_instance();
        let adapter = hardware_adapter(&instance, None).unwrap();
        eprintln!("GPU oracle adapter: {:?}", adapter.get_info());
        let mut source = IndexedFrame {
            // Includes every index, including high bits masked by the shader.
            indices: (0..FRAME_BYTES)
                .map(|i| (i + i / WIDTH * 13) as u8)
                .collect(),
            palette: [[0; 4]; 256],
            pixel_mask: 0xff,
        };
        for format in [
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Bgra8Unorm,
        ] {
            let mut raster = Raster::new(&adapter, format).unwrap();
            assert_eq!(raster.image.format(), wgpu::TextureFormat::R8Uint);
            assert_eq!(raster.image.width(), WIDTH as u32);
            assert_eq!(raster.image.height(), HEIGHT as u32);
            assert_eq!(raster.palette.format(), wgpu::TextureFormat::Rgba8Unorm);
            for (pass, mask) in [0xff, 0x00, 0x0f, 0xa5, 0xff].into_iter().enumerate() {
                // Authored six-bit DAC values, including 0 and 63. Change only
                // palette/mask between passes on the same raster and indices.
                source.pixel_mask = mask;
                source.palette = std::array::from_fn(|i| {
                    let raw = [
                        (i + pass * 5) & 63,
                        (i * 7 + (i >> 6) * 17 + pass * 11) & 63,
                        (i * 13 + (i >> 6) * 29 + pass * 3) & 63,
                    ];
                    let [r, g, b] = raw.map(|v| ((v << 2) | (v >> 4)) as u8);
                    [r, g, b, 255]
                });
                // Independent scalar lookup; do not use rgb_pixels(), which
                // would let a shared CPU conversion bug define the oracle.
                let rgb: Vec<u32> = source
                    .indices
                    .iter()
                    .map(|&index| {
                        let [r, g, b, _] = source.palette[usize::from(index & mask)];
                        (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
                    })
                    .collect();
                for (width, height) in [
                    (1, 1),
                    (1, 200),
                    (320, 1),
                    (159, 99),
                    (319, 199),
                    (320, 200),
                    (321, 201),
                    (641, 401),
                    (800, 600),
                    (1920, 1080),
                    (3840, 2160),
                ] {
                    // Keep the existing geometry matrix once. Palette/mask
                    // changes need only one odd-size representative, not the
                    // Cartesian product of every color state and destination.
                    if pass != 0 && (width, height) != (641, 401) {
                        continue;
                    }
                    let before = Instant::now();
                    raster.upload(&source, width, height).unwrap();
                    let extent = wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    };
                    let target = raster.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("pixel oracle target"),
                        size: extent,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                            | wgpu::TextureUsages::COPY_SRC,
                        view_formats: &[],
                    });
                    let stride = (width * 4).div_ceil(256) * 256;
                    let buffer = raster.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("pixel oracle readback"),
                        size: u64::from(stride) * u64::from(height),
                        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                        mapped_at_creation: false,
                    });
                    let view = target.create_view(&Default::default());
                    let mut encoder = raster.device.create_command_encoder(&Default::default());
                    raster.encode(&mut encoder, &view);
                    encoder.copy_texture_to_buffer(
                        wgpu::TexelCopyTextureInfo {
                            texture: &target,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        wgpu::TexelCopyBufferInfo {
                            buffer: &buffer,
                            layout: wgpu::TexelCopyBufferLayout {
                                offset: 0,
                                bytes_per_row: Some(stride),
                                rows_per_image: Some(height),
                            },
                        },
                        extent,
                    );
                    let index = raster.queue.submit([encoder.finish()]);
                    let (send, recv) = std::sync::mpsc::channel();
                    buffer
                        .slice(..)
                        .map_async(wgpu::MapMode::Read, move |result| {
                            send.send(result).unwrap();
                        });
                    raster
                        .device
                        .poll(wgpu::PollType::Wait {
                            submission_index: Some(index),
                            timeout: Some(Duration::from_secs(10)),
                        })
                        .unwrap();
                    recv.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
                    let mapped = buffer.slice(..).get_mapped_range().unwrap();
                    let oracle =
                        crate::video::scale_frame(&rgb, width as usize, height as usize).unwrap();
                    for (i, &expected) in oracle.iter().enumerate() {
                        let at = i / width as usize * stride as usize + i % width as usize * 4;
                        let bytes = &mapped[at..at + 4];
                        let actual = if format == wgpu::TextureFormat::Bgra8Unorm {
                            u32::from(bytes[2]) << 16
                                | u32::from(bytes[1]) << 8
                                | u32::from(bytes[0])
                        } else {
                            u32::from(bytes[0]) << 16
                                | u32::from(bytes[1]) << 8
                                | u32::from(bytes[2])
                        };
                        assert_eq!(
                            actual, expected,
                            "{format:?} pass={pass} mask={mask:02x} {width}x{height} pixel {i}"
                        );
                        assert_eq!(bytes[3], 255);
                    }
                    drop(mapped);
                    buffer.unmap();
                    raster.check().unwrap();
                    eprintln!(
                        "pixel-equivalent {format:?} pass={pass} mask={mask:02x} {width}x{height} elapsed={:?}",
                        before.elapsed()
                    );
                }
            }
            let mut invalid = source.clone();
            invalid.indices.truncate(1);
            let error = raster.upload(&invalid, 320, 200).unwrap_err();
            assert!(error.starts_with("wgpu indexed frame invalid:"), "{error}");
            assert!(raster.validate_size(0, 200).is_err());
            assert!(raster.validate_size(u32::MAX, 200).is_err());
        }
    }
}
