use crate::maybe_watch;

use super::Options;
use shared::ShaderConstants;
use winit::{
    event::{ElementState, Event, KeyboardInput, MouseButton, VirtualKeyCode, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopBuilder},
    window::Window,
};

#[cfg(not(any(target_os = "android", target_arch = "wasm32")))]
mod shaders {
    // The usual usecase of code generation is always building in build.rs, and so the codegen
    // always happens. However, we want to both test code generation (on android) and runtime
    // compilation (on desktop), so manually fill in what would have been codegenned for desktop.
    #[allow(non_upper_case_globals)]
    pub const main_fs: &str = "main_fs";
    #[allow(non_upper_case_globals)]
    pub const main_vs: &str = "main_vs";
}
#[cfg(any(target_os = "android", target_arch = "wasm32"))]
mod shaders {
    include!(concat!(env!("OUT_DIR"), "/entry_points.rs"));
}

fn mouse_button_index(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::Other(i) => 3 + (i as usize),
    }
}

async fn run(
    event_loop: EventLoop<wgpu::ShaderModuleDescriptor<'static>>,
    window: Window,
    shader_binary: wgpu::ShaderModuleDescriptor<'static>,
) {
    let backends = wgpu::util::backend_bits_from_env()
        .unwrap_or(wgpu::Backends::VULKAN | wgpu::Backends::METAL);
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends,
        dx12_shader_compiler: wgpu::util::dx12_shader_compiler_from_env().unwrap_or_default(),
    });

    // HACK(eddyb) marker error type for lazily-created surfaces (e.g. on Android).
    struct SurfaceCreationPending {
        preferred_format: wgpu::TextureFormat,
    }

    // Wait for Resumed event on Android; the surface is only otherwise needed
    // early to find an adapter that can render to this surface.
    let initial_surface = if cfg!(target_os = "android") {
        Err(SurfaceCreationPending {
            preferred_format: wgpu::TextureFormat::Rgba8UnormSrgb,
        })
    } else {
        Ok(unsafe { instance.create_surface(&window) }
            .expect("Failed to create surface from window"))
    };

    let adapter = wgpu::util::initialize_adapter_from_env_or_default(
        &instance,
        backends,
        // Request an adapter which can render to our surface
        initial_surface.as_ref().ok(),
    )
    .await
    .expect("Failed to find an appropriate adapter");

    let features = wgpu::Features::PUSH_CONSTANTS;
    let limits = wgpu::Limits {
        max_push_constant_size: 128,
        ..Default::default()
    };

    // Create the logical device and command queue
    let (device, queue) = adapter
        .request_device(
            &wgpu::DeviceDescriptor {
                label: None,
                features,
                limits,
            },
            None,
        )
        .await
        .expect("Failed to create device");

    let auto_configure_surface =
        |adapter: &_, device: &_, surface: wgpu::Surface, size: winit::dpi::PhysicalSize<_>| {
            let mut surface_config = surface
                .get_default_config(adapter, size.width, size.height)
                .unwrap_or_else(|| {
                    panic!(
                        "Missing formats/present modes in surface capabilities: {:#?}",
                        surface.get_capabilities(adapter)
                    )
                });

            // FIXME(eddyb) should this be toggled by a CLI arg?
            // NOTE(eddyb) VSync was disabled in the past, but without VSync,
            // especially for simpler shaders, you can easily hit thousands
            // of frames per second, stressing GPUs for no reason.
            surface_config.present_mode = wgpu::PresentMode::AutoVsync;

            surface.configure(device, &surface_config);

            (surface, surface_config)
        };
    let mut surface_with_config = initial_surface
        .map(|surface| auto_configure_surface(&adapter, &device, surface, window.inner_size()));

    // Load the shaders from disk

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[],
        push_constant_ranges: &[wgpu::PushConstantRange {
            stages: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
            range: 0..std::mem::size_of::<ShaderConstants>() as u32,
        }],
    });

    let mut render_pipeline = create_pipeline(
        &device,
        &pipeline_layout,
        surface_with_config.as_ref().map_or_else(
            |pending| pending.preferred_format,
            |(_, surface_config)| surface_config.format,
        ),
        shader_binary,
    );

    let start = std::time::Instant::now();

    let (mut cursor_x, mut cursor_y) = (0.0, 0.0);
    let (mut drag_start_x, mut drag_start_y) = (0.0, 0.0);
    let (mut drag_end_x, mut drag_end_y) = (0.0, 0.0);
    let mut mouse_button_pressed = 0;
    let mut mouse_button_press_since_last_frame = 0;
    let mut mouse_button_press_time = [f32::NEG_INFINITY; 3];

    event_loop.run(move |event, _, control_flow| {
        // Have the closure take ownership of the resources.
        // `event_loop.run` never returns, therefore we must do this to ensure
        // the resources are properly cleaned up.
        let _ = (&instance, &adapter, &pipeline_layout);
        let render_pipeline = &mut render_pipeline;

        *control_flow = ControlFlow::Wait;
        match event {
            Event::MainEventsCleared => {
                window.request_redraw();
            }
            Event::Resumed => {
                let new_surface = unsafe { instance.create_surface(&window) }
                    .expect("Failed to create surface from window (after resume)");
                surface_with_config = Ok(auto_configure_surface(
                    &adapter,
                    &device,
                    new_surface,
                    window.inner_size(),
                ));
            }
            Event::Suspended => {
                if let Ok((_, surface_config)) = &surface_with_config {
                    surface_with_config = Err(SurfaceCreationPending {
                        preferred_format: surface_config.format,
                    });
                }
            }
            Event::WindowEvent {
                event: WindowEvent::Resized(size),
                ..
            } => {
                if size.width != 0 && size.height != 0 {
                    // Recreate the swap chain with the new size
                    if let Ok((surface, surface_config)) = &mut surface_with_config {
                        surface_config.width = size.width;
                        surface_config.height = size.height;
                        surface.configure(&device, surface_config);
                    }
                }
            }
            Event::RedrawRequested(_) => {
                // FIXME(eddyb) only the mouse shader *really* needs this, could
                // avoid doing wasteful rendering by special-casing each shader?
                // (with VSync enabled this can't be *too* bad, thankfully)
                // FIXME(eddyb) is this the best way to do continuous redraws in
                // `winit`? (or should we stop using `ControlFlow::Wait`? etc.)
                window.request_redraw();

                if let Ok((surface, surface_config)) = &mut surface_with_config {
                    let output = match surface.get_current_texture() {
                        Ok(surface) => surface,
                        Err(err) => {
                            eprintln!("get_current_texture error: {err:?}");
                            match err {
                                wgpu::SurfaceError::Lost => {
                                    surface.configure(&device, surface_config);
                                }
                                wgpu::SurfaceError::OutOfMemory => {
                                    *control_flow = ControlFlow::Exit;
                                }
                                _ => (),
                            }
                            return;
                        }
                    };
                    let output_view = output
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default());
                    let mut encoder = device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                    {
                        let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: None,
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &output_view,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(wgpu::Color::GREEN),
                                    store: true,
                                },
                            })],
                            depth_stencil_attachment: None,
                        });

                        let time = start.elapsed().as_secs_f32();
                        for (i, press_time) in mouse_button_press_time.iter_mut().enumerate() {
                            if (mouse_button_press_since_last_frame & (1 << i)) != 0 {
                                *press_time = time;
                            }
                        }
                        mouse_button_press_since_last_frame = 0;

                        let push_constants = ShaderConstants {
                            width: window.inner_size().width,
                            height: window.inner_size().height,
                            time,
                            cursor_x,
                            cursor_y,
                            drag_start_x,
                            drag_start_y,
                            drag_end_x,
                            drag_end_y,
                            mouse_button_pressed,
                            mouse_button_press_time,
                        };

                        rpass.set_pipeline(render_pipeline);
                        rpass.set_push_constants(
                            wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                            0,
                            bytemuck::bytes_of(&push_constants),
                        );
                        rpass.draw(0..3, 0..1);
                    }

                    queue.submit(Some(encoder.finish()));
                    output.present();
                }
            }
            Event::WindowEvent {
                event:
                    WindowEvent::CloseRequested
                    | WindowEvent::KeyboardInput {
                        input:
                            KeyboardInput {
                                virtual_keycode: Some(VirtualKeyCode::Escape),
                                ..
                            },
                        ..
                    },
                ..
            } => *control_flow = ControlFlow::Exit,
            Event::WindowEvent {
                event: WindowEvent::MouseInput { state, button, .. },
                ..
            } => {
                let mask = 1 << mouse_button_index(button);
                match state {
                    ElementState::Pressed => {
                        mouse_button_pressed |= mask;
                        mouse_button_press_since_last_frame |= mask;

                        if button == MouseButton::Left {
                            drag_start_x = cursor_x;
                            drag_start_y = cursor_y;
                            drag_end_x = cursor_x;
                            drag_end_y = cursor_y;
                        }
                    }
                    ElementState::Released => mouse_button_pressed &= !mask,
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CursorMoved { position, .. },
                ..
            } => {
                cursor_x = position.x as f32;
                cursor_y = position.y as f32;
                if (mouse_button_pressed & (1 << mouse_button_index(MouseButton::Left))) != 0 {
                    drag_end_x = cursor_x;
                    drag_end_y = cursor_y;
                }
            }
            Event::UserEvent(new_module) => {
                *render_pipeline = create_pipeline(
                    &device,
                    &pipeline_layout,
                    surface_with_config.as_ref().map_or_else(
                        |pending| pending.preferred_format,
                        |(_, surface_config)| surface_config.format,
                    ),
                    new_module,
                );
                window.request_redraw();
                *control_flow = ControlFlow::Poll;
            }
            _ => {}
        }
    });
}

fn create_pipeline(
    device: &wgpu::Device,
    pipeline_layout: &wgpu::PipelineLayout,
    surface_format: wgpu::TextureFormat,
    shader_binary: wgpu::ShaderModuleDescriptor<'_>,
) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(shader_binary);
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: None,
        layout: Some(pipeline_layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: shaders::main_vs,
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState {
            count: 1,
            mask: !0,
            alpha_to_coverage_enabled: false,
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: shaders::main_fs,
            targets: &[Some(wgpu::ColorTargetState {
                format: surface_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview: None,
    })
}

#[allow(clippy::match_wild_err_arm)]
pub fn start(
    #[cfg(target_os = "android")] android_app: winit::platform::android::activity::AndroidApp,
    options: &Options,
) {
    let mut event_loop_builder = EventLoopBuilder::with_user_event();
    cfg_if::cfg_if! {
        if #[cfg(target_os = "android")] {
            android_logger::init_once(
                android_logger::Config::default()
                    .with_min_level("info".parse().unwrap()),
            );

            use winit::platform::android::EventLoopBuilderExtAndroid;
            event_loop_builder.with_android_app(android_app);
        } else if #[cfg(target_arch = "wasm32")] {
            std::panic::set_hook(Box::new(console_error_panic_hook::hook));
            console_log::init().expect("could not initialize logger");
        } else {
            env_logger::init();
        }
    }
    let event_loop = event_loop_builder.build();

    // Build the shader before we pop open a window, since it might take a while.
    let initial_shader = maybe_watch(
        options.shader,
        #[cfg(not(any(target_os = "android", target_arch = "wasm32")))]
        {
            let proxy = event_loop.create_proxy();
            Some(Box::new(move |res| match proxy.send_event(res) {
                Ok(it) => it,
                // ShaderModuleDescriptor is not `Debug`, so can't use unwrap/expect
                Err(_err) => panic!("Event loop dead"),
            }))
        },
    );

    let window = winit::window::WindowBuilder::new()
        .with_title("Rust GPU - wgpu")
        .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0))
        .build(&event_loop)
        .unwrap();

    cfg_if::cfg_if! {
        if #[cfg(target_arch = "wasm32")] {
            use winit::platform::web::WindowExtWebSys;
            // On wasm, append the canvas to the document body
            web_sys::window()
                .and_then(|win| win.document())
                .and_then(|doc| doc.body())
                .and_then(|body| {
                    body.append_child(&web_sys::Element::from(window.canvas()))
                        .ok()
                })
                .expect("couldn't append canvas to document body");
            wasm_bindgen_futures::spawn_local(run(
                event_loop,
                window,
                initial_shader,
            ));
        } else {
            futures::executor::block_on(run(
                event_loop,
                window,
                initial_shader,

            ));
        }
    }
}
