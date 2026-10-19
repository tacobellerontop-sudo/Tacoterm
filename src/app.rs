//! Wiring: winit event loop + PTY reader thread + parser + renderer.
//!
//! Bytes flow: PTY -read thread→ channel → parser → grid → redraw.
//! Keystrokes flow: winit → keybindings / input map → PTY write. Resize
//! flows both ways: window → renderer → grid + ConPTY.

use std::io::Read;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Theme as WindowTheme, Window, WindowId};

use tacoterm::config::{self, Config};
use tacoterm::grid::{Grid, Point};
use tacoterm::input::{encode_key, key_to_bytes, Action, Bindings};
use tacoterm::renderer::{Fonts, Renderer};
use tacoterm::schemes;
use tacoterm::vt::Vt;

use crate::pty::Pty;

/// Lines scrolled per mouse-wheel notch.
const WHEEL_LINES: f64 = 3.0;

/// Wakes the event loop from the PTY threads.
#[derive(Debug, Clone, Copy)]
enum Wake {
    PtyOutput,
    ChildExited,
    /// config.toml was saved.
    ConfigChanged,
}

pub fn run() -> Result<(), String> {
    let (config, warnings) = config::load();
    let event_loop = EventLoop::<Wake>::with_user_event()
        .build()
        .map_err(|e| e.to_string())?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App::new(config, proxy);
    app.warnings.extend(warnings);
    watch_config(app.proxy.clone());
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    match app.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Show a message box. tacoterm has no console in release builds, so this
/// is the only way an error reaches the user.
pub fn show_error(msg: &str) {
    message_box(msg, windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR);
}

fn message_box(msg: &str, icon: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE) {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_OK};
    eprintln!("tacoterm: {msg}");
    unsafe {
        MessageBoxW(
            HWND::default(),
            &HSTRING::from(msg),
            &HSTRING::from("tacoterm"),
            MB_OK | icon,
        );
    }
}

/// A warning that shouldn't stop startup: shown without blocking the
/// terminal.
fn show_warning(msg: String) {
    thread::spawn(move || {
        message_box(
            &msg,
            windows::Win32::UI::WindowsAndMessaging::MB_ICONWARNING,
        )
    });
}

/// Poll config.toml's modification time and wake the loop when it changes.
/// Polling (twice a second) is simpler than a directory watcher and catches
/// editors that save by replacing the file.
fn watch_config(proxy: EventLoopProxy<Wake>) {
    let path = config::config_path();
    let stamp = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    thread::spawn(move || {
        let mut last = stamp(&path);
        loop {
            thread::sleep(Duration::from_millis(500));
            let now = stamp(&path);
            if now != last {
                last = now;
                if proxy.send_event(Wake::ConfigChanged).is_err() {
                    break;
                }
            }
        }
    });
}

/// Dark title bar for dark backgrounds, light for light ones.
fn title_bar_theme(bg: config::Rgb) -> WindowTheme {
    let luma = 0.299 * bg.0 as f32 + 0.587 * bg.1 as f32 + 0.114 * bg.2 as f32;
    if luma < 128.0 {
        WindowTheme::Dark
    } else {
        WindowTheme::Light
    }
}

struct App {
    config: Config,
    proxy: EventLoopProxy<Wake>,
    bindings: Bindings,
    warnings: Vec<String>,
    /// Warnings already shown for the current config file, so a save that
    /// doesn't fix them doesn't pop them up again.
    shown_warnings: Vec<String>,
    /// The scheme on screen; Ctrl+Shift+T moves it without touching the
    /// file.
    scheme: String,
    /// A fatal error to report once the event loop exits.
    error: Option<String>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    grid: Grid,
    vt: Vt,
    pty: Option<Pty>,
    pty_rx: Option<Receiver<Vec<u8>>>,
    mods: ModifiersState,
    clipboard: Option<arboard::Clipboard>,
    mouse_pos: (f64, f64),
    /// Where the left button went down; a drag from here selects.
    press_point: Option<Point>,
    /// Fractional wheel lines left over from touchpad scrolling.
    wheel_accum: f64,
}

impl App {
    fn new(config: Config, proxy: EventLoopProxy<Wake>) -> Self {
        let (bindings, warnings) = Bindings::from_config(&config.keybindings);
        let scheme = config.color_scheme.clone();
        let grid = Grid::new(80, 24, config.scrollback_lines);
        App {
            config,
            proxy,
            bindings,
            warnings,
            shown_warnings: Vec::new(),
            scheme,
            error: None,
            window: None,
            renderer: None,
            grid,
            vt: Vt::new(),
            pty: None,
            pty_rx: None,
            mods: ModifiersState::empty(),
            clipboard: None,
            mouse_pos: (0.0, 0.0),
            press_point: None,
            wheel_accum: 0.0,
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, msg: String) {
        self.error = Some(msg);
        event_loop.exit();
    }

    fn redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Spawn the shell, then hand the read end to a drain thread and the
    /// process handle to an exit watcher. The UI thread keeps the Pty for
    /// writes and resizes.
    fn spawn_pty(&mut self) -> Result<(), String> {
        let shell = self.config.shell.clone();
        if is_cmd(&shell) && !self.config.prompt.is_empty() {
            // cmd.exe reads its prompt from the environment it inherits.
            std::env::set_var("PROMPT", &self.config.prompt);
        }
        let (cols, rows) = (self.grid.cols as u16, self.grid.rows as u16);
        let pty =
            Pty::spawn(&shell, cols, rows).map_err(|e| format!("cannot start {shell}: {e}"))?;
        let mut reader = pty
            .try_clone_reader()
            .map_err(|e| format!("cannot clone pty reader: {e}"))?;
        let waiter = pty
            .exit_waiter()
            .map_err(|e| format!("cannot watch {shell}: {e}"))?;

        let (tx, rx) = mpsc::channel();
        self.pty_rx = Some(rx);
        let proxy = self.proxy.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // console closed
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                        let _ = proxy.send_event(Wake::PtyOutput);
                    }
                }
            }
        });
        let proxy = self.proxy.clone();
        thread::spawn(move || {
            waiter.wait();
            let _ = proxy.send_event(Wake::ChildExited);
        });
        self.pty = Some(pty);
        Ok(())
    }

    /// Parse everything the drain thread delivered, answer any queries, then
    /// redraw once.
    fn drain_pty(&mut self) {
        let mut dirty = false;
        if let Some(rx) = &self.pty_rx {
            while let Ok(bytes) = rx.try_recv() {
                self.vt.advance(&mut self.grid, &bytes);
                dirty = true;
            }
        }
        if !self.vt.responses.is_empty() {
            let responses = std::mem::take(&mut self.vt.responses);
            self.write_pty(&responses);
        }
        if let (Some(title), Some(w)) = (self.vt.title.take(), &self.window) {
            w.set_title(&title);
        }
        if dirty {
            self.redraw();
        }
    }

    fn write_pty(&mut self, bytes: &[u8]) {
        if let Some(p) = &mut self.pty {
            let _ = p.write_input(bytes);
        }
    }

    /// Fit the grid (and ConPTY) to the window at the current cell size.
    fn fit_grid(&mut self) {
        let Some(r) = &self.renderer else { return };
        let (cols, rows) = r.grid_size();
        if (cols as usize, rows as usize) != (self.grid.cols, self.grid.rows) {
            self.grid.resize(cols as usize, rows as usize);
            if let Some(p) = &mut self.pty {
                let _ = p.resize(cols, rows);
            }
        }
        self.redraw();
    }

    fn clipboard(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut()
    }

    fn copy_selection(&mut self) -> bool {
        let Some(text) = self.grid.selection_text() else {
            return false;
        };
        let text = text.replace('\n', "\r\n");
        if let Some(cb) = self.clipboard() {
            let _ = cb.set_text(text);
        }
        true
    }

    fn paste(&mut self) {
        let Some(text) = self.clipboard().and_then(|cb| cb.get_text().ok()) else {
            return;
        };
        // Terminals send Enter as CR; so does a pasted newline.
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bytes = if self.grid.modes.bracketed_paste {
            // Strip ESC so pasted text can't end the bracket early.
            let clean: String = text.chars().filter(|&c| c != '\x1b').collect();
            format!("\x1b[200~{clean}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        };
        self.grid.scroll_view_to_bottom();
        self.write_pty(&bytes);
        self.redraw();
    }

    fn run_action(&mut self, action: Action) {
        let page = self.grid.rows.saturating_sub(1).max(1) as isize;
        match action {
            Action::Copy => {
                self.copy_selection();
            }
            Action::Paste => self.paste(),
            Action::ScrollPageUp => self.grid.scroll_view(page),
            Action::ScrollPageDown => self.grid.scroll_view(-page),
            Action::ScrollToTop => self.grid.scroll_view(isize::MAX / 2),
            Action::ScrollToBottom => self.grid.scroll_view_to_bottom(),
            Action::NextColorScheme => self.next_scheme(),
            Action::OpenSettings => {
                let path = config::config_path();
                if let Err(e) = std::process::Command::new("notepad.exe").arg(&path).spawn() {
                    show_warning(format!("cannot open {}: {e}", path.display()));
                }
            }
        }
        self.redraw();
    }

    /// Show the next built-in scheme (a preview: the file is unchanged).
    fn next_scheme(&mut self) {
        let next = schemes::next(&self.scheme);
        self.scheme = next.name.to_string();
        let preview = Config {
            color_scheme: next.name.to_string(),
            theme: Default::default(),
            ..self.config.clone()
        };
        self.apply_theme(&preview.theme().0);
        if let Some(w) = &self.window {
            w.set_title(&format!(
                "tacoterm — {} (set color_scheme = \"{}\" to keep it)",
                next.name, next.name
            ));
        }
    }

    fn apply_theme(&mut self, theme: &config::Theme) {
        if let Some(r) = &mut self.renderer {
            r.set_theme(theme);
        }
        if let Some(w) = &self.window {
            w.set_theme(Some(title_bar_theme(theme.background)));
        }
        self.redraw();
    }

    /// config.toml changed on disk: apply what can change live.
    fn reload_config(&mut self) {
        let path = config::config_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let config = match config::parse(&text) {
            Ok(c) => c,
            Err(e) => {
                self.report(vec![format!(
                    "bad config {}: {}",
                    path.display(),
                    e.trim_end()
                )]);
                return;
            }
        };
        let (bindings, mut warnings) = Bindings::from_config(&config.keybindings);
        warnings.extend(config.warnings());
        self.report(warnings);
        self.bindings = bindings;
        self.grid.scrollback_limit = config.scrollback_lines;
        self.scheme = config.color_scheme.clone();
        let refit = match &mut self.renderer {
            Some(r) => r.set_config(&config),
            None => false,
        };
        self.apply_theme(&config.theme().0);
        self.config = config;
        if refit {
            self.fit_grid();
        }
    }

    /// Show warnings that weren't already shown for this config.
    fn report(&mut self, warnings: Vec<String>) {
        if warnings != self.shown_warnings && !warnings.is_empty() {
            show_warning(warnings.join("\n\n"));
        }
        self.shown_warnings = warnings;
    }

    fn point_under_mouse(&self) -> Option<Point> {
        let r = self.renderer.as_ref()?;
        let (col, row) = r.cell_at(self.mouse_pos.0, self.mouse_pos.1, &self.grid);
        Some(self.grid.viewport_point(col, row))
    }

    fn wheel(&mut self, lines: f64) {
        self.wheel_accum += lines;
        let whole = self.wheel_accum.trunc();
        self.wheel_accum -= whole;
        let n = whole as isize;
        if n == 0 {
            return;
        }
        if self.grid.is_alt_screen() {
            // Full-screen apps (less, vim…) have no scrollback of ours to
            // show: send arrow keys instead, like other terminals do.
            let key = Key::Named(if n > 0 {
                NamedKey::ArrowUp
            } else {
                NamedKey::ArrowDown
            });
            if let Some(seq) = encode_key(
                &key,
                None,
                ModifiersState::empty(),
                self.grid.modes.app_cursor,
            ) {
                let bytes = seq.repeat(n.unsigned_abs());
                self.write_pty(&bytes);
            }
        } else {
            self.grid.scroll_view(n);
            self.redraw();
        }
    }
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Size the window for 80×24 cells before it appears.
        let scale = event_loop
            .primary_monitor()
            .map(|m| m.scale_factor())
            .unwrap_or(1.0);
        let fonts = Fonts::new(&self.config, scale);
        let pad = (self.config.padding as f64 * scale).round() as u32;
        let size = PhysicalSize::new(
            80 * fonts.cell_width + 2 * pad,
            24 * fonts.cell_height + 2 * pad,
        );
        let attrs = Window::default_attributes()
            .with_title("tacoterm")
            .with_theme(Some(title_bar_theme(self.config.theme().0.background)))
            .with_inner_size(size);
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => return self.fail(event_loop, format!("cannot create window: {e}")),
        };
        let fonts = if window.scale_factor() != scale {
            Fonts::new(&self.config, window.scale_factor())
        } else {
            fonts
        };
        let renderer = match Renderer::new(window.clone(), fonts, &self.config) {
            Ok(r) => r,
            Err(e) => return self.fail(event_loop, e),
        };
        let (cols, rows) = renderer.grid_size();
        self.grid.resize(cols as usize, rows as usize);
        self.window = Some(window);
        self.renderer = Some(renderer);
        if let Err(e) = self.spawn_pty() {
            return self.fail(event_loop, e);
        }
        let warnings = std::mem::take(&mut self.warnings);
        self.report(warnings);
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
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
                self.fit_grid();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(r) = &mut self.renderer {
                    r.set_scale(scale_factor);
                }
                self.fit_grid();
            }
            WindowEvent::Focused(focused) => {
                if let Some(r) = &mut self.renderer {
                    r.focused = focused;
                }
                self.redraw();
            }
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    let bare = event.key_without_modifiers();
                    if let Some(action) = self.bindings.lookup(&bare, self.mods) {
                        return self.run_action(action);
                    }
                }
                if let Some(bytes) = key_to_bytes(&event, self.mods, self.grid.modes.app_cursor) {
                    if self.grid.display_offset != 0 {
                        self.grid.scroll_view_to_bottom();
                        self.redraw();
                    }
                    self.write_pty(&bytes);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x, position.y);
                if let (Some(anchor), Some(p)) = (self.press_point, self.point_under_mouse()) {
                    if self.grid.selection.is_none() && p != anchor {
                        self.grid.start_selection(anchor);
                    }
                    if self.grid.selection.is_some() {
                        self.grid.extend_selection(p);
                        self.redraw();
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => match (button, state) {
                (MouseButton::Left, ElementState::Pressed) => {
                    self.press_point = self.point_under_mouse();
                    if self.grid.selection.take().is_some() {
                        self.redraw();
                    }
                }
                (MouseButton::Left, ElementState::Released) => {
                    self.press_point = None;
                }
                // Right-click copies a selection, or pastes — like conhost.
                (MouseButton::Right, ElementState::Pressed) => {
                    if self.copy_selection() {
                        self.grid.selection = None;
                        self.redraw();
                    } else {
                        self.paste();
                    }
                }
                _ => {}
            },
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64 * WHEEL_LINES,
                    MouseScrollDelta::PixelDelta(p) => {
                        let ch = self.renderer.as_ref().map_or(16, |r| r.cell_size().1);
                        p.y / ch as f64
                    }
                };
                self.wheel(lines);
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = &mut self.renderer {
                    if !r.render(&self.grid) {
                        self.redraw();
                    }
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Wake) {
        if let Wake::ConfigChanged = event {
            return self.reload_config();
        }
        self.drain_pty();
        if let Wake::ChildExited = event {
            event_loop.exit();
        }
    }
}

/// Whether a shell command line runs cmd.exe.
fn is_cmd(shell: &str) -> bool {
    let shell = shell.trim();
    let program = match shell.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(""),
        None => shell.split(' ').next().unwrap_or(""),
    };
    let name = program.rsplit(['\\', '/']).next().unwrap_or("");
    name.eq_ignore_ascii_case("cmd.exe") || name.eq_ignore_ascii_case("cmd")
}
