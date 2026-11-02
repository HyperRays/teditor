use std::{error::Error, sync::Arc, time::Instant};
use teditor::{
    terminal::{Terminal, TerminalConfig},
    ui::Cursor,
};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    window::{CursorIcon, Window, WindowId},
};

type AppResult<T> = Result<T, Box<dyn Error>>;
const FONT_HEIGHT: f32 = 22.0;

struct WindowState {
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    window: Arc<Window>,
    size: PhysicalSize<u32>,
    terminal: Terminal,
    editor: teditor::editor::Editor,
    ime_allowed: bool,
}

impl WindowState {
    async fn new(
        event_loop: &ActiveEventLoop,
        paths: &[std::path::PathBuf],
        proxy: EventLoopProxy<()>,
    ) -> AppResult<Self> {
        let window = Arc::new(
            event_loop.create_window(
                Window::default_attributes()
                    .with_title("teditor")
                    .with_inner_size(LogicalSize::new(1060.0, 700.0))
                    .with_min_inner_size(LogicalSize::new(400.0, 280.0)),
            )?,
        );
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(event_loop.owned_display_handle()),
        ));
        let surface = instance.create_surface(window.clone())?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                power_preference: wgpu::PowerPreference::LowPower,
                ..Default::default()
            })
            .await?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("teditor device"),
                ..Default::default()
            })
            .await?;
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or("the GPU has no compatible surface configuration")?;
        config.format = surface
            .get_capabilities(&adapter)
            .formats
            .into_iter()
            .find(|format| format.is_srgb())
            .ok_or("the surface has no sRGB format")?;
        config.present_mode = wgpu::PresentMode::AutoVsync;
        let terminal = Terminal::new(
            &device,
            &queue,
            config.format,
            TerminalConfig {
                viewport: [size.width, size.height],
                scale_factor: window.scale_factor(),
                font_height: FONT_HEIGHT,
                ..Default::default()
            },
        )?;
        let cwd = std::env::current_dir()?;
        let first = paths.first().map(|path| cwd.join(path));
        let root = first
            .as_ref()
            .map(|path| {
                if path.is_dir() {
                    path.as_path()
                } else {
                    path.parent().unwrap_or(&cwd)
                }
            })
            .unwrap_or(&cwd);
        let mut editor = teditor::editor::Editor::new(root)?;
        let proxy = std::sync::Mutex::new(proxy);
        editor.set_waker(Arc::new(move || {
            if let Ok(proxy) = proxy.lock() {
                let _ = proxy.send_event(());
            }
        }));
        for path in paths {
            if !path.is_dir() {
                editor.request_open(path)?;
            }
        }
        window.set_ime_allowed(false);
        if size.width > 0 && size.height > 0 {
            surface.configure(&device, &config);
        }
        eprintln!(
            "Renderer: {} ({:?})",
            adapter.get_info().name,
            adapter.get_info().backend
        );
        Ok(Self {
            instance,
            surface,
            device,
            queue,
            config,
            window,
            size,
            terminal,
            editor,
            ime_allowed: false,
        })
    }

    fn resize(&mut self, size: PhysicalSize<u32>) -> AppResult<()> {
        let events = self.terminal.resize([size.width, size.height])?;
        for event in events {
            self.editor.handle(event);
        }
        self.size = size;
        if size.width > 0 && size.height > 0 {
            self.config.width = size.width;
            self.config.height = size.height;
            self.surface.configure(&self.device, &self.config);
            self.window.request_redraw();
        }
        Ok(())
    }

    fn draw(&mut self) -> AppResult<bool> {
        if self.size.width == 0 || self.size.height == 0 {
            return Ok(false);
        }
        let (output, reconfigure) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture) => (texture, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(texture) => (texture, true),
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.resize(self.window.inner_size())?;
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self.instance.create_surface(self.window.clone())?;
                self.resize(self.window.inner_size())?;
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Timeout => {
                self.window.request_redraw();
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Occluded => return Ok(false),
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err("surface validation failed".into());
            }
        };
        self.editor.compose(self.terminal.back_mut());
        self.window.set_title(&self.editor.title());
        self.sync_ime();
        self.window.set_cursor(match self.editor.cursor() {
            Cursor::Default => CursorIcon::Default,
            Cursor::Pointer => CursorIcon::Pointer,
            Cursor::Text => CursorIcon::Text,
            Cursor::ResizeColumns => CursorIcon::ColResize,
            Cursor::ResizeRows => CursorIcon::RowResize,
        });
        let view = output.texture.create_view(&Default::default());
        self.terminal.render(&view)?;
        self.window.pre_present_notify();
        self.queue.present(output);
        if reconfigure {
            self.resize(self.window.inner_size())?;
        }
        if self.editor.needs_repaint() {
            self.window.request_redraw();
        }
        Ok(true)
    }

    fn sync_ime(&mut self) {
        if self.editor.take_ime_reset() {
            self.window.set_ime_allowed(false);
            self.ime_allowed = false;
        }
        let allowed = self.editor.text_input_active() && self.terminal.input().focused();
        if allowed != self.ime_allowed {
            self.window.set_ime_allowed(allowed);
            self.ime_allowed = allowed;
        }
        if allowed
            && self.terminal.input().ime_enabled()
            && let Some(area) = self
                .editor
                .ime_cursor_cells()
                .and_then(|cells| self.terminal.ime_cursor_area(cells))
        {
            self.window.set_ime_cursor_area(
                PhysicalPosition::new(area.position[0], area.position[1]),
                PhysicalSize::new(area.size[0], area.size[1]),
            );
        }
    }
}

#[derive(Default)]
struct App {
    state: Option<WindowState>,
    error: Option<Box<dyn Error>>,
    smoke_test: bool,
    paths: Vec<std::path::PathBuf>,
    next_io_poll: Option<Instant>,
    proxy: Option<EventLoopProxy<()>>,
}

impl App {
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: Box<dyn Error>) {
        self.error = Some(error);
        event_loop.exit();
    }
}

impl ApplicationHandler for App {
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(state) = &self.state
            && state.editor.has_pending_background_work()
        {
            let now = Instant::now();
            let next = self.next_io_poll.get_or_insert(now);
            if now >= *next {
                state.window.request_redraw();
                *next = now
                    + std::time::Duration::from_millis(if state.editor.has_pending_io() {
                        50
                    } else {
                        16
                    });
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(*next));
        } else {
            self.next_io_poll = None;
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
    /// Terminal output arrived on a background thread.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        if let Some(state) = &self.state {
            state.window.request_redraw();
        }
    }
    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = &mut self.state {
            for event in state.terminal.cancel_input() {
                state.editor.handle(event);
            }
            state.window.set_ime_allowed(false);
            state.ime_allowed = false;
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(state) = &mut self.state {
            state.sync_ime();
            state.window.request_redraw();
            return;
        }
        let proxy = self.proxy.clone().expect("main sets the proxy");
        match pollster::block_on(WindowState::new(event_loop, &self.paths, proxy)) {
            Ok(state) => {
                state.window.request_redraw();
                self.state = Some(state);
            }
            Err(error) => self.fail(event_loop, error),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut().filter(|state| state.window.id() == id) else {
            return;
        };
        let input = state.terminal.handle_window_event(&event, Instant::now());
        if !input.is_empty() {
            for event in input {
                state.editor.handle(event);
            }
            state.sync_ime();
            state.window.request_redraw();
        }
        match event {
            WindowEvent::CloseRequested => {
                state.editor.compose(state.terminal.back_mut());
                state.editor.request_quit();
                if state.editor.quit_requested() {
                    event_loop.exit();
                } else {
                    state.window.request_redraw();
                }
            }
            WindowEvent::Resized(size) => {
                if let Err(error) = state.resize(size) {
                    self.fail(event_loop, error);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                match state.terminal.set_scale_factor(scale_factor) {
                    Ok(events) => {
                        for event in events {
                            state.editor.handle(event);
                        }
                    }
                    Err(error) => {
                        self.fail(event_loop, error.into());
                        return;
                    }
                }
                if let Err(error) = state.resize(state.window.inner_size()) {
                    self.fail(event_loop, error);
                }
            }
            WindowEvent::Occluded(false) => state.window.request_redraw(),
            WindowEvent::RedrawRequested => match state.draw() {
                Ok(true) if self.smoke_test && !state.editor.has_pending_background_work() => {
                    eprintln!("Presented editor successfully.");
                    event_loop.exit();
                }
                Ok(_) if state.editor.quit_requested() => event_loop.exit(),
                Ok(_) => {}
                Err(error) => self.fail(event_loop, error),
            },
            _ => {}
        }
    }
}

fn main() -> AppResult<()> {
    let event_loop = EventLoop::new()?;
    let mut app = App {
        proxy: Some(event_loop.create_proxy()),
        smoke_test: std::env::args_os().any(|arg| arg == "--smoke-test"),
        paths: std::env::args_os()
            .skip(1)
            .filter(|arg| arg != "--smoke-test")
            .map(std::path::PathBuf::from)
            .collect(),
        ..Default::default()
    };
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut app)?;
    app.error.map_or(Ok(()), Err)
}
