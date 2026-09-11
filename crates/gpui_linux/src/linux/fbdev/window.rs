use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use gpui::{
    Bounds, Capslock, DevicePixels, DispatchEventResult, DisplayId, GpuSpecs, Modifiers, Pixels,
    PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow, Point,
    PromptButton, PromptLevel, RequestFrameOptions, Scene, Size, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, px,
};
use gpui_wgpu::{WgpuContext, WgpuRenderer, WgpuSurfaceConfig, wgpu};
use uuid::Uuid;

use super::egl::{self, EglWindow};

#[derive(Debug)]
pub(crate) struct FbdevDisplay {
    bounds: Bounds<Pixels>,
}

impl FbdevDisplay {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self {
            bounds: Bounds::from_corners(
                Point::default(),
                Point::new(px(width as f32), px(height as f32)),
            ),
        }
    }
}

impl PlatformDisplay for FbdevDisplay {
    fn id(&self) -> DisplayId {
        DisplayId::new(0)
    }

    fn uuid(&self) -> anyhow::Result<Uuid> {
        Ok(Uuid::nil())
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }
}

const PRESENT_SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let position = positions[vertex_index];
    var out: VertexOutput;
    out.position = vec4<f32>(position, 0.0, 1.0);
    // Drawing into the GL default framebuffer skips the vertical flip wgpu's own GL surface
    // presentation applies, so the frame is flipped here instead.
    out.uv = position * 0.5 + 0.5;
    return out;
}

@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, in.uv);
}
"#;

/// Copies the renderer's offscreen frame into the EGL default framebuffer, which wgpu can
/// only reach as an imported renderbuffer.
struct Presenter {
    framebuffer_view: wgpu::TextureView,
    _framebuffer: wgpu::Texture,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl Presenter {
    fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        const GL_RGBA: u32 = 0x1908;
        const GL_RGBA8: u32 = 0x8058;
        const GL_UNSIGNED_BYTE: u32 = 0x1401;

        let format = wgpu::TextureFormat::Rgba8Unorm;
        let hal_framebuffer = wgpu::hal::gles::Texture {
            inner: wgpu::hal::gles::TextureInner::DefaultRenderbuffer,
            mip_level_count: 1,
            array_layer_count: 1,
            format,
            format_desc: wgpu::hal::gles::TextureFormatDesc {
                internal: GL_RGBA8,
                external: GL_RGBA,
                data_type: GL_UNSIGNED_BYTE,
            },
            copy_size: wgpu::hal::CopyExtent {
                width,
                height,
                depth: 1,
            },
            drop_guard: None,
        };
        // Safety: the default framebuffer belongs to the EGL surface, which the window keeps
        // alive until every wgpu object has been dropped.
        let framebuffer = unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Gles>(
                hal_framebuffer,
                &wgpu::TextureDescriptor {
                    label: Some("egl_default_framebuffer"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                },
            )
        };
        let framebuffer_view = framebuffer.create_view(&wgpu::TextureViewDescriptor::default());

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fbdev_present_layout"),
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
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fbdev_present"),
            source: wgpu::ShaderSource::Wgsl(PRESENT_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fbdev_present"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fbdev_present"),
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
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("fbdev_present"),
            ..Default::default()
        });

        Self {
            framebuffer_view,
            _framebuffer: framebuffer,
            pipeline,
            bind_group_layout,
            sampler,
        }
    }

    fn present(&self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &wgpu::TextureView) {
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fbdev_present"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(frame),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("fbdev_present"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fbdev_present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.framebuffer_view,
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
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        queue.submit(std::iter::once(encoder.finish()));
    }
}

#[derive(Default)]
struct Callbacks {
    request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    input: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
}

struct FbdevWindowState {
    display: Rc<dyn PlatformDisplay>,
    input_handler: Option<PlatformInputHandler>,
    title: Option<String>,
    // Fields drop in declaration order: wgpu objects must be released while the GL context
    // they were created on still exists, and that context before the EGL surface.
    renderer: WgpuRenderer,
    presenter: Presenter,
    gpu: WgpuContext,
    egl: EglWindow,
}

struct FbdevWindowInner {
    state: RefCell<FbdevWindowState>,
    callbacks: RefCell<Callbacks>,
}

#[derive(Clone)]
pub(crate) struct FbdevWindow(Rc<FbdevWindowInner>);

impl FbdevWindow {
    pub(crate) fn new(egl: EglWindow, display: Rc<dyn PlatformDisplay>) -> Result<Self> {
        let (width, height) = egl.size();
        // Safety: `EglWindow::new` made its context current on this thread, and the window
        // keeps it alive for as long as the wgpu objects created from it.
        let gpu = unsafe { WgpuContext::from_external_gl(egl::get_proc_address) }?;
        let renderer = WgpuRenderer::new_offscreen(
            &gpu,
            WgpuSurfaceConfig {
                size: Size {
                    width: DevicePixels(width as i32),
                    height: DevicePixels(height as i32),
                },
                transparent: false,
                preferred_present_mode: None,
            },
        )?;
        let presenter = Presenter::new(&gpu.device, width, height);

        Ok(Self(Rc::new(FbdevWindowInner {
            state: RefCell::new(FbdevWindowState {
                display,
                input_handler: None,
                title: None,
                renderer,
                presenter,
                gpu,
                egl,
            }),
            callbacks: RefCell::new(Callbacks::default()),
        })))
    }

    pub(crate) fn refresh(&self, options: RequestFrameOptions) {
        let callback = self.0.callbacks.borrow_mut().request_frame.take();
        if let Some(mut callback) = callback {
            callback(options);
            self.0.callbacks.borrow_mut().request_frame = Some(callback);
        }
    }

    pub(crate) fn handle_input(&self, input: PlatformInput) {
        let callback = self.0.callbacks.borrow_mut().input.take();
        if let Some(mut callback) = callback {
            let result = callback(input.clone());
            self.0.callbacks.borrow_mut().input = Some(callback);
            if !result.propagate {
                return;
            }
        }
        // Key presses nothing handled type their character into the focused text input.
        if let PlatformInput::KeyDown(event) = input
            && event.keystroke.modifiers.is_subset_of(&Modifiers::shift())
            && let Some(key_char) = event.keystroke.key_char
        {
            let input_handler = self.0.state.borrow_mut().input_handler.take();
            if let Some(mut input_handler) = input_handler {
                input_handler.replace_text_in_range(None, &key_char);
                self.0.state.borrow_mut().input_handler = Some(input_handler);
            }
        }
    }
}

impl raw_window_handle::HasWindowHandle for FbdevWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

impl raw_window_handle::HasDisplayHandle for FbdevWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

impl PlatformWindow for FbdevWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.0.state.borrow().display.bounds()
    }

    fn is_maximized(&self) -> bool {
        false
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds().size
    }

    fn resize(&mut self, _size: Size<Pixels>) {}

    fn scale_factor(&self) -> f32 {
        1.0
    }

    fn appearance(&self) -> WindowAppearance {
        WindowAppearance::Dark
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.0.state.borrow().display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        Point::default()
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers::default()
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.0.state.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.state.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {}

    fn is_active(&self) -> bool {
        true
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn set_title(&mut self, title: &str) {
        self.0.state.borrow_mut().title = Some(title.to_owned());
    }

    fn get_title(&self) -> String {
        self.0.state.borrow().title.clone().unwrap_or_default()
    }

    fn set_background_appearance(&self, _background: WindowBackgroundAppearance) {}

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.callbacks.borrow_mut().request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.0.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, _callback: Box<dyn FnMut(bool)>) {}

    fn on_hover_status_change(&self, _callback: Box<dyn FnMut(bool)>) {}

    fn on_resize(&self, _callback: Box<dyn FnMut(Size<Pixels>, f32)>) {}

    fn on_moved(&self, _callback: Box<dyn FnMut()>) {}

    fn on_should_close(&self, _callback: Box<dyn FnMut() -> bool>) {}

    fn on_close(&self, _callback: Box<dyn FnOnce()>) {}

    fn on_hit_test_window_control(&self, _callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
    }

    fn on_appearance_changed(&self, _callback: Box<dyn FnMut()>) {}

    fn draw(&self, scene: &Scene) {
        let mut state = self.0.state.borrow_mut();
        let state = &mut *state;
        if !state.renderer.draw(scene) {
            return;
        }
        if let Some(frame) = state.renderer.offscreen_view() {
            state
                .presenter
                .present(&state.gpu.device, &state.gpu.queue, frame);
            state.egl.swap_buffers();
        }
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.0.state.borrow().renderer.sprite_atlas().clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        Some(self.0.state.borrow().renderer.gpu_specs())
    }
}
