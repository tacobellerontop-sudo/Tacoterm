//! Wiring: winit event loop + PTY reader thread + parser + renderer.
//!
//! Bytes flow: PTY -read thread→ channel → parser → grid → redraw.
//! Keystrokes flow: winit → input map → PTY write. Resize flows both ways:
//! window → grid + ConPTY.

use std::io::Read;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::config::{self, Config};
use crate::grid::Grid;
use crate::input::key_to_bytes;
use crate::pty::Pty;
use crate::renderer::Renderer;
use crate::vt::Vt;

/// Wakes the event loop when the PTY has output to parse.
#[derive(Debug, Clone, Copy)]
enum Wake {
    PtyOutput,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::load();
    let event_loop = EventLoop::<Wake>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App::new(config, proxy);
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct App {
    config: Config,
    proxy: EventLoopProxy<Wake>,
    window: Option<Window>,
    renderer: Option<Renderer>,
    grid: Grid,
    vt: Vt,
    pty: Option<Pty>,
    pty_rx: Option<Receiver<Vec<u8>>>,
    mods: ModifiersState,
}

impl App {
    fn new(config: Config, proxy: EventLoopProxy<Wake>) -> Self {
        let grid = Grid::new(80, 24, config.scrollback_lines);
        App {
            config,
            proxy,
            window: None,
            renderer: None,
            grid,
            vt: Vt::new(),
            pty: None,
            pty_rx: None,
            mods: ModifiersState::empty(),
        }
    }

    /// Spawn the shell on the UI thread, then hand the read end to a drain
    /// thread. The UI thread keeps the Pty for writes and resizes.
    fn spawn_pty(&mut self) {
        let shell = self.config.shell.clone();
        let (cols, rows) = (self.grid.cols as u16, self.grid.rows as u16);
        let mut pty = match Pty::spawn(&shell, cols, rows) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("tacoterm: cannot spawn {shell}: {e}");
                std::process::exit(1);
            }
        };
        let reader = match pty.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("tacoterm: cannot clone pty reader: {e}");
                std::process::exit(1);
            }
        };
        let (tx, rx) = mpsc::channel();
        self.pty_rx = Some(rx);
        let proxy = self.proxy.clone();
        thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // child exited, console gone
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                        let _ = proxy.send_event(Wake::PtyOutput);
                    }
                    Err(e) => {
                        eprintln!("tacoterm: pty read error: {e}");
                        break;
                    }
                }
            }
        });
        self.pty = Some(pty);
    }

    /// Parse everything the drain thread delivered, then redraw once.
    fn drain_pty(&mut self) {
        let mut dirty = false;
        if let Some(rx) = &self.pty_rx {
            while let Ok(bytes) = rx.try_recv() {
                // ConPTY speaks UTF-8 on the pipe; replace anything odd.
                let text = String::from_utf8_lossy(&bytes);
                self.vt.advance(&mut self.grid, text.as_bytes());
                dirty = true;
            }
        }
        if dirty {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title("tacoterm");
        match event_loop.create_window(attrs) {
            Ok(w) => {
                // Renderer::new(&w) goes here once src/renderer.rs is real.
                self.window = Some(w);
                self.spawn_pty();
            }
            Err(e) => {
                eprintln!("tacoterm: cannot create window: {e}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                if let Some(p) = &mut self.pty {
                    let _ = p.terminate();
                }
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                // TODO: take real cell metrics from the renderer; 9x18 is a
                // placeholder so resize plumbing works end to end.
                let cols = (size.width / 9).max(20) as u16;
                let rows = (size.height / 18).max(10) as u16;
                self.grid.resize(cols as usize, rows as usize);
                if let Some(p) = &mut self.pty {
                    let _ = p.resize(cols, rows);
                }
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(bytes) = key_to_bytes(&event, &self.mods) {
                    if let Some(p) = &mut self.pty {
                        let _ = p.write_input(&bytes);
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = &mut self.renderer {
                    r.render(&self.grid);
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: Wake) {
        self.drain_pty();
    }
}
