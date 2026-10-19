//! Wiring: winit event loop + PTY reader threads + parsers + renderer.
//!
//! The window holds tabs; each tab is a tree of split panes
//! ([`tacoterm::layout`]), and each pane is a shell on its own ConPTY with
//! its own grid and parser. Bytes flow: PTY -read thread→ channel → that
//! pane's parser → grid → redraw. Keystrokes go to the focused pane.
//! Resize flows both ways: window → layout → each pane's grid + ConPTY.

use std::collections::HashMap;
use std::io::Read;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::platform::windows::{BackdropType, WindowAttributesExtWindows, WindowExtWindows};
use winit::window::{
    CursorIcon, Fullscreen, ResizeDirection, Theme as WindowTheme, UserAttentionType, Window,
    WindowId, WindowLevel,
};

use tacoterm::config::{self, Backdrop, Config, ShellKind};
use tacoterm::grid::{Grid, Match, MouseMode, Point};
use tacoterm::input::{encode_key, key_to_bytes, Action, Bindings};
use tacoterm::layout::{Direction, Layout, PaneId, Rect, Split};
use tacoterm::mouse::{self, MouseEventKind, MouseMods};
use tacoterm::renderer::{Fonts, Frame, PaneView, Renderer, SearchBar, TabHit};
use tacoterm::schemes;
use tacoterm::vt::Vt;

mod page;

use crate::menu;
use crate::pty::Pty;
use crate::win;

/// Lines scrolled per mouse-wheel notch.
const WHEEL_LINES: f64 = 3.0;
/// Cursor blink half-period.
const BLINK: Duration = Duration::from_millis(530);
/// How long the visual bell lights the window.
const FLASH: Duration = Duration::from_millis(120);
/// Clicks closer together than this count as a double/triple click.
const MULTI_CLICK: Duration = Duration::from_millis(400);
/// Share of the parent a keyboard pane resize moves the divider by.
const RESIZE_STEP: f32 = 0.05;

/// Wakes the event loop from other threads.
#[derive(Debug, Clone, Copy)]
enum Wake {
    PtyOutput,
    /// That pane's shell exited.
    ChildExited(PaneId),
    /// config.toml was saved.
    ConfigChanged,
    /// The global summon hotkey was pressed.
    Summon,
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

fn backdrop_type(b: Backdrop) -> BackdropType {
    match b {
        Backdrop::None => BackdropType::None,
        Backdrop::Acrylic => BackdropType::TransientWindow,
        Backdrop::Mica => BackdropType::MainWindow,
        Backdrop::Tabbed => BackdropType::TabbedWindow,
    }
}

/// A tab's name before the shell sets a title: "PowerShell", "cmd"…
fn shell_title(command: &str) -> String {
    match ShellKind::of(command) {
        ShellKind::PowerShell => "PowerShell".to_string(),
        ShellKind::Cmd => "Command Prompt".to_string(),
        ShellKind::Other => {
            let program = command
                .trim()
                .trim_start_matches('"')
                .split(['"', ' '])
                .next()
                .unwrap_or("shell");
            let name = program.rsplit(['\\', '/']).next().unwrap_or(program);
            name.strip_suffix(".exe").unwrap_or(name).to_string()
        }
    }
}

fn toml_string(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// Search case-sensitively only when the query has capitals.
fn smart_case(query: &str) -> bool {
    query.chars().any(char::is_uppercase)
}

/// Dock the window across the top of its monitor, 40% tall.
fn place_quake(w: &Window) {
    let Some(m) = w.current_monitor().or_else(|| w.primary_monitor()) else {
        return;
    };
    let pos = m.position();
    let size = m.size();
    w.set_outer_position(PhysicalPosition::new(pos.x, pos.y));
    let _ = w.request_inner_size(PhysicalSize::new(size.width, size.height * 2 / 5));
}

/// One shell: its console, parser and screen.
struct Pane {
    grid: Grid,
    vt: Vt,
    pty: Pty,
    /// The title the shell set, or the shell's name.
    title: String,
    /// The shell's working directory, when its prompt reports it.
    cwd: Option<String>,
}

struct Tab {
    layout: Layout,
    focused: PaneId,
}

/// The find box.
struct Search {
    query: String,
    matches: Vec<Match>,
    current: Option<usize>,
}

struct Click {
    at: Instant,
    point: Point,
    count: u8,
}

/// A link under the mouse while Ctrl is held: pane, URL, start, end.
type Link = (PaneId, String, Point, Point);

struct App {
    config: Config,
    proxy: EventLoopProxy<Wake>,
    bindings: Bindings,
    warnings: Vec<String>,
    /// Warnings already shown for the current config file, so a save that
    /// doesn't fix them doesn't pop them up again.
    shown_warnings: Vec<String>,
    /// The scheme on screen; next_color_scheme moves it without touching
    /// the file.
    scheme: String,
    /// A fatal error to report once the event loop exits.
    error: Option<String>,
    /// The last pane closed: leave once the loop is back.
    quit: bool,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    panes: HashMap<PaneId, Pane>,
    tabs: Vec<Tab>,
    active: usize,
    next_pane: PaneId,
    /// Every pane's reader thread sends here, tagged with the pane.
    out_tx: Sender<(PaneId, Vec<u8>)>,
    out_rx: Receiver<(PaneId, Vec<u8>)>,
    mods: ModifiersState,
    clipboard: Option<arboard::Clipboard>,
    mouse_pos: (f64, f64),
    /// Where the left button went down; a drag from here selects.
    press: Option<(PaneId, Point)>,
    /// A button held down that the pane's program is being told about.
    reported_button: Option<(PaneId, mouse::MouseButton)>,
    last_click: Option<Click>,
    /// Fractional wheel lines left over from touchpad scrolling.
    wheel_accum: f64,
    search: Option<Search>,
    /// Font size change from zooming, in points.
    zoom: f32,
    focused: bool,
    cursor_on: bool,
    next_blink: Instant,
    flash_until: Option<Instant>,
    link: Option<Link>,
    /// Hidden by the summon hotkey.
    hidden: bool,
    /// The settings page, while it is open.
    page: Option<page::Page>,
    /// We draw the title bar ourselves (in the tab strip), so the whole
    /// window can be see-through. Fixed when the window is made.
    custom_frame: bool,
    /// The last click on bare title bar, to catch double-clicks.
    title_click: Option<Instant>,
    /// The tab-strip control under the mouse.
    tab_hover: Option<TabHit>,
}

impl App {
    fn new(config: Config, proxy: EventLoopProxy<Wake>) -> Self {
        let (bindings, warnings) = Bindings::from_config(&config.keybindings);
        let scheme = config.color_scheme.clone();
        let (out_tx, out_rx) = mpsc::channel();
        App {
            config,
            proxy,
            bindings,
            warnings,
            shown_warnings: Vec::new(),
            scheme,
            error: None,
            quit: false,
            window: None,
            renderer: None,
            panes: HashMap::new(),
            tabs: Vec::new(),
            active: 0,
            next_pane: 1,
            out_tx,
            out_rx,
            mods: ModifiersState::empty(),
            clipboard: None,
            mouse_pos: (0.0, 0.0),
            press: None,
            reported_button: None,
            last_click: None,
            wheel_accum: 0.0,
            search: None,
            zoom: 0.0,
            focused: true,
            cursor_on: true,
            next_blink: Instant::now() + BLINK,
            flash_until: None,
            link: None,
            hidden: false,
            page: None,
            custom_frame: false,
            title_click: None,
            tab_hover: None,
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

    /// The config with zoom applied: what the renderer draws with.
    fn render_config(&self) -> Config {
        Config {
            font_size: (self.config.font_size + self.zoom).max(4.0),
            ..self.config.clone()
        }
    }

    // ---- tabs and panes ---------------------------------------------------

    fn show_tabs(&self) -> bool {
        if self.custom_frame {
            // The strip is the title bar.
            return true;
        }
        match self.config.tab_bar.trim().to_lowercase().as_str() {
            "never" => false,
            "multiple" | "auto" => self.tabs.len() > 1,
            _ => true,
        }
    }

    /// The window area below the tab strip.
    fn content_area(&self) -> Rect {
        let Some(r) = &self.renderer else {
            return Rect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            };
        };
        let (w, h) = r.size();
        let top = if self.show_tabs() {
            r.tab_bar_height() as f32
        } else {
            0.0
        };
        Rect {
            x: 0.0,
            y: top,
            w: w as f32,
            h: (h as f32 - top).max(1.0),
        }
    }

    fn gap(&self) -> f32 {
        self.renderer
            .as_ref()
            .map_or(2.0, |r| r.divider_width() as f32)
    }

    fn tab_rects(&self, tab: &Tab) -> Vec<(PaneId, Rect)> {
        tab.layout.rects(self.content_area(), self.gap())
    }

    fn focused_id(&self) -> Option<PaneId> {
        self.tabs.get(self.active).map(|t| t.focused)
    }

    fn pane(&self) -> Option<&Pane> {
        self.panes.get(&self.focused_id()?)
    }

    fn pane_mut(&mut self) -> Option<&mut Pane> {
        let id = self.focused_id()?;
        self.panes.get_mut(&id)
    }

    fn alloc_id(&mut self) -> PaneId {
        let id = self.next_pane;
        self.next_pane += 1;
        id
    }

    /// Start a shell for pane `id` at `cols`×`rows`. Its output goes to the
    /// shared channel; its exit closes the pane.
    fn spawn_pane(
        &mut self,
        id: PaneId,
        (cols, rows): (u16, u16),
        cwd: Option<String>,
    ) -> Result<(), String> {
        let (shell, env) = self.config.shell_command();
        // The shell inherits our environment (cmd.exe reads PROMPT there).
        std::env::remove_var("PROMPT");
        // How programs (tacoshell's img, chafa, …) tell what we can draw.
        std::env::set_var("TERM_PROGRAM", "tacoterm");
        std::env::set_var("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        std::env::set_var("COLORTERM", "truecolor");
        for (k, v) in env {
            std::env::set_var(k, v);
        }
        let pty = Pty::spawn(&shell, cols, rows, cwd.as_deref())
            .map_err(|e| format!("cannot start {shell}: {e}"))?;
        let mut reader = pty
            .try_clone_reader()
            .map_err(|e| format!("cannot clone pty reader: {e}"))?;
        let waiter = pty
            .exit_waiter()
            .map_err(|e| format!("cannot watch {shell}: {e}"))?;

        let tx = self.out_tx.clone();
        let proxy = self.proxy.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // console closed
                    Ok(n) => {
                        if tx.send((id, buf[..n].to_vec())).is_err() {
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
            let _ = proxy.send_event(Wake::ChildExited(id));
        });
        let mut grid = Grid::new(cols as usize, rows as usize, self.config.scrollback_lines);
        grid.cell_pixels = self.cell_pixels();
        self.panes.insert(
            id,
            Pane {
                grid,
                vt: Vt::new(),
                pty,
                title: shell_title(&shell),
                cwd,
            },
        );
        Ok(())
    }

    fn size_for(&self, rect: Rect) -> (u16, u16) {
        self.renderer
            .as_ref()
            .map_or((80, 24), |r| r.pane_grid_size(rect))
    }

    fn new_tab(&mut self) -> Result<(), String> {
        let cwd = self.pane().and_then(|p| p.cwd.clone());
        let id = self.alloc_id();
        self.tabs.push(Tab {
            layout: Layout::new(id),
            focused: id,
        });
        let size = self.size_for(self.content_area());
        if let Err(e) = self.spawn_pane(id, size, cwd) {
            self.tabs.pop();
            return Err(e);
        }
        self.switch_tab(self.tabs.len() - 1);
        // The tab strip may just have appeared.
        self.fit_all();
        Ok(())
    }

    fn split(&mut self, how: Split) {
        let Some(focused) = self.focused_id() else {
            return;
        };
        let cwd = self.pane().and_then(|p| p.cwd.clone());
        let id = self.alloc_id();
        if !self.tabs[self.active].layout.split(focused, how, id) {
            return;
        }
        let size = self.rect_of(id).map_or((80, 24), |r| self.size_for(r));
        match self.spawn_pane(id, size, cwd) {
            Ok(()) => {
                self.tabs[self.active].focused = id;
                self.close_search();
                self.fit_all();
                self.update_title();
            }
            Err(e) => {
                self.tabs[self.active].layout.remove(id);
                show_warning(e);
            }
        }
    }

    /// Close a pane (its shell exited, or the user closed it). The last
    /// pane of the last tab closes the window.
    fn close_pane(&mut self, id: PaneId) {
        if let Some(mut p) = self.panes.remove(&id) {
            let _ = p.pty.terminate();
        }
        let Some(t) = self
            .tabs
            .iter()
            .position(|t| t.layout.panes().contains(&id))
        else {
            return;
        };
        if self.focused_id() == Some(id) {
            self.close_search();
        }
        if self.link.as_ref().is_some_and(|l| l.0 == id) {
            self.link = None;
        }
        if self.tabs[t].layout.len() == 1 {
            self.tabs.remove(t);
            if self.tabs.is_empty() {
                self.quit = true;
                return;
            }
            if self.active > t || self.active >= self.tabs.len() {
                self.active = self.active.saturating_sub(1);
            }
        } else {
            let area = self.content_area();
            let tab = &mut self.tabs[t];
            // Focus moves to a neighbor that takes the space.
            let next = [
                Direction::Left,
                Direction::Up,
                Direction::Right,
                Direction::Down,
            ]
            .into_iter()
            .find_map(|d| tab.layout.neighbor(area, id, d));
            tab.layout.remove(id);
            if tab.focused == id {
                tab.focused = next
                    .filter(|n| tab.layout.panes().contains(n))
                    .unwrap_or_else(|| tab.layout.panes()[0]);
            }
        }
        self.fit_all();
        self.update_title();
    }

    fn close_tab(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get(index) {
            for id in tab.layout.panes() {
                self.close_pane(id);
            }
        }
    }

    fn switch_tab(&mut self, index: usize) {
        if index < self.tabs.len() && index != self.active {
            self.close_search();
            self.active = index;
            self.link = None;
        }
        self.update_title();
        self.reset_blink();
        self.redraw();
    }

    fn focus_pane(&mut self, id: PaneId) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.focused != id {
            tab.focused = id;
            self.close_search();
            self.update_title();
            self.reset_blink();
            self.redraw();
        }
    }

    /// Fit every pane's grid (and ConPTY) to its rect.
    /// A cell's size in pixels, which pictures are measured against.
    fn cell_pixels(&self) -> (u32, u32) {
        self.renderer.as_ref().map_or((10, 20), |r| r.cell_size())
    }

    fn fit_all(&mut self) {
        let cell = self.cell_pixels();
        for p in self.panes.values_mut() {
            p.grid.cell_pixels = cell;
        }
        let Some(r) = &self.renderer else { return };
        let area = self.content_area();
        let gap = r.divider_width() as f32;
        for tab in &self.tabs {
            for (id, rect) in tab.layout.rects(area, gap) {
                let (cols, rows) = r.pane_grid_size(rect);
                if let Some(p) = self.panes.get_mut(&id) {
                    if (cols as usize, rows as usize) != (p.grid.cols, p.grid.rows) {
                        p.grid.resize(cols as usize, rows as usize);
                        let _ = p.pty.resize(cols, rows);
                    }
                }
            }
        }
        self.refresh_search();
        self.fit_preview();
        self.redraw();
    }

    fn update_title(&self) {
        if let (Some(w), Some(p)) = (&self.window, self.pane()) {
            w.set_title(&p.title);
        }
    }

    /// Replace every running shell with a fresh one (after a shell change):
    /// one pane per tab, as many tabs as before.
    fn restart_shells(&mut self) {
        let count = self.tabs.len().max(1);
        for (_, mut p) in self.panes.drain() {
            let _ = p.pty.terminate();
        }
        self.tabs.clear();
        self.active = 0;
        self.close_search();
        for _ in 0..count {
            if let Err(e) = self.new_tab() {
                show_warning(e);
                break;
            }
        }
        if self.tabs.is_empty() {
            self.quit = true;
        }
        self.switch_tab(0);
    }

    // ---- settings -------------------------------------------------------

    /// The right-click menu.
    fn context_menu(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let shells = config::available_shells();
        let current_shell = match ShellKind::of(&self.config.shell) {
            _ if self.config.shell.trim().is_empty()
                || self.config.shell.trim().eq_ignore_ascii_case("auto") =>
            {
                "auto"
            }
            ShellKind::Cmd => "cmd.exe",
            _ if self.config.shell.to_lowercase().contains("wsl") => "wsl.exe ~",
            _ => "",
        };
        let state = menu::State {
            has_selection: self.pane().is_some_and(|p| p.grid.selection.is_some()),
            scheme: &self.scheme,
            shells: &shells,
            shell: current_shell,
            opacity: self.config.opacity,
            backdrop: self.config.backdrop(),
            fullscreen: window.fullscreen().is_some(),
        };
        let picked = menu::show(&window, &state);
        match picked {
            Some(menu::Choice::NewTab) => self.run_action(Action::NewTab),
            Some(menu::Choice::SplitRight) => self.run_action(Action::SplitRight),
            Some(menu::Choice::SplitDown) => self.run_action(Action::SplitDown),
            Some(menu::Choice::ClosePane) => self.run_action(Action::ClosePane),
            Some(menu::Choice::Find) => self.run_action(Action::Find),
            Some(menu::Choice::Copy) => {
                self.copy_selection();
                if let Some(p) = self.pane_mut() {
                    p.grid.selection = None;
                }
            }
            Some(menu::Choice::Paste) => self.paste(),
            Some(menu::Choice::SelectAll) => self.run_action(Action::SelectAll),
            Some(menu::Choice::Fullscreen) => self.run_action(Action::Fullscreen),
            Some(menu::Choice::Settings) => self.open_settings(),
            Some(menu::Choice::EditFile) => self.edit_settings_file(),
            Some(menu::Choice::Scheme(name)) => {
                // Show it now; saving makes the file watcher agree.
                self.scheme = name.to_string();
                self.config.color_scheme = name.to_string();
                self.apply_theme(&self.config.theme().0);
                self.save_setting("color_scheme", &toml_string(name));
            }
            Some(menu::Choice::Opacity(o)) => {
                self.config.opacity = o;
                self.apply_transparency();
                self.save_setting("opacity", &format!("{o:.2}"));
            }
            Some(menu::Choice::Backdrop(b)) => {
                let name = match b {
                    Backdrop::None => "none",
                    Backdrop::Acrylic => "acrylic",
                    Backdrop::Mica => "mica",
                    Backdrop::Tabbed => "tabbed",
                };
                self.config.backdrop = name.to_string();
                // A material behind a solid background would never show.
                if b != Backdrop::None && self.config.opacity >= 1.0 {
                    self.config.opacity = 0.8;
                    self.save_setting("opacity", "0.80");
                }
                self.apply_transparency();
                self.save_setting("backdrop", &toml_string(name));
            }
            Some(menu::Choice::Shell(setting)) => {
                self.config.shell = setting.to_string();
                self.save_setting("shell", &toml_string(setting));
                self.restart_shells();
            }
            None => {}
        }
        self.redraw();
    }

    /// Open config.toml in Notepad, writing the starter file first if
    /// there isn't one yet.
    fn edit_settings_file(&self) {
        let path = config::config_path();
        if !path.exists() {
            // Writing any key makes the file from the starter text.
            let _ = config::set_top_levels(&[]);
        }
        if let Err(e) = std::process::Command::new("notepad.exe").arg(&path).spawn() {
            show_warning(format!("cannot open {}: {e}", path.display()));
        }
    }

    fn save_setting(&self, key: &str, toml_value: &str) {
        if let Err(e) = config::set_top_level(key, toml_value) {
            show_warning(format!("cannot save {key}: {e}"));
        }
    }

    fn apply_transparency(&mut self) {
        let opacity = self.config.opacity;
        let backdrop = self.config.backdrop();
        if let Some(r) = &mut self.renderer {
            r.set_opacity(opacity);
        }
        if let Some(w) = &self.window {
            w.set_system_backdrop(backdrop_type(backdrop));
            win::extend_frame(w, backdrop != Backdrop::None);
        }
        self.redraw();
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
                "tacoterm — {} (pick it in the right-click menu to keep it)",
                next.name
            ));
        }
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
        let (_, mut warnings) = Bindings::from_config(&config.keybindings);
        warnings.extend(config.warnings());
        self.report(warnings);
        if self.page.is_some() {
            // The page shows its own draft; a hand edit made meanwhile
            // shows once it closes and the file is saved again.
            return;
        }
        self.apply_config(config);
    }

    /// Put `config` on screen: everything that can change without a restart.
    fn apply_config(&mut self, config: Config) {
        let (bindings, _) = Bindings::from_config(&config.keybindings);
        self.bindings = bindings;
        for p in self.panes.values_mut() {
            p.grid.scrollback_limit = config.scrollback_lines;
        }
        self.scheme = config.color_scheme.clone();
        self.config = config;
        let render_config = self.render_config();
        if let Some(r) = &mut self.renderer {
            r.set_config(&render_config);
        }
        self.apply_theme(&self.config.theme().0);
        self.apply_transparency();
        // Fonts, padding or the tab strip may have changed.
        self.fit_all();
    }

    /// Show warnings that weren't already shown for this config.
    fn report(&mut self, warnings: Vec<String>) {
        if warnings != self.shown_warnings && !warnings.is_empty() {
            show_warning(warnings.join("\n\n"));
        }
        self.shown_warnings = warnings;
    }

    /// Zoom by `delta` points; 0 resets.
    fn zoom_by(&mut self, delta: f32) {
        self.zoom = if delta == 0.0 {
            0.0
        } else {
            (self.zoom + delta).clamp(4.0 - self.config.font_size, 72.0)
        };
        let config = self.render_config();
        if let Some(r) = &mut self.renderer {
            r.set_config(&config);
        }
        self.fit_all();
    }

    // ---- terminal I/O ---------------------------------------------------

    /// Parse everything the reader threads delivered, answer queries, and
    /// pick up titles, directories, bells and clipboard writes.
    fn drain_pty(&mut self) {
        let mut touched = Vec::new();
        while let Ok((id, bytes)) = self.out_rx.try_recv() {
            if let Some(p) = self.panes.get_mut(&id) {
                p.vt.advance(&mut p.grid, &bytes);
                if !touched.contains(&id) {
                    touched.push(id);
                }
            }
        }
        if touched.is_empty() {
            return;
        }
        let focused = self.focused_id();
        let mut bell = false;
        let mut clipboard = None;
        let mut title_changed = false;
        for id in &touched {
            let Some(p) = self.panes.get_mut(id) else {
                continue;
            };
            if !p.vt.responses.is_empty() {
                let responses = std::mem::take(&mut p.vt.responses);
                let _ = p.pty.write_input(&responses);
            }
            if let Some(title) = p.vt.title.take() {
                if !title.trim().is_empty() {
                    p.title = title;
                    title_changed |= Some(*id) == focused;
                }
            }
            if let Some(cwd) = p.vt.cwd.take() {
                p.cwd = Some(cwd);
            }
            bell |= std::mem::take(&mut p.vt.bell);
            if let Some(text) = p.vt.clipboard.take() {
                clipboard = Some(text);
            }
        }
        if let Some(text) = clipboard {
            if let Some(cb) = self.clipboard() {
                let _ = cb.set_text(text);
            }
        }
        if title_changed {
            self.update_title();
        }
        if bell {
            self.ring();
        }
        if focused.is_some_and(|f| touched.contains(&f)) {
            self.refresh_search();
            self.reset_blink();
        }
        // Tab titles show for every tab, so any pane's output may matter.
        self.redraw();
    }

    fn write_pty(&mut self, bytes: &[u8]) {
        if let Some(p) = self.pane_mut() {
            let _ = p.pty.write_input(bytes);
        }
    }

    fn ring(&mut self) {
        let bell = self.config.bell();
        if bell.sound {
            win::beep();
        }
        if bell.flash {
            self.flash_until = Some(Instant::now() + FLASH);
            self.redraw();
        }
        if !self.focused && (bell.flash || bell.sound) {
            if let Some(w) = &self.window {
                w.request_user_attention(Some(UserAttentionType::Informational));
            }
        }
    }

    fn clipboard(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut()
    }

    fn copy_selection(&mut self) -> bool {
        let Some(text) = self.pane().and_then(|p| p.grid.selection_text()) else {
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
        if let Some(s) = &mut self.search {
            s.query.push_str(text.lines().next().unwrap_or(""));
            return self.refresh_search_jump();
        }
        // Terminals send Enter as CR; so does a pasted newline.
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let Some(p) = self.pane_mut() else { return };
        let bytes = if p.grid.modes.bracketed_paste {
            // Strip ESC so pasted text can't end the bracket early.
            let clean: String = text.chars().filter(|&c| c != '\x1b').collect();
            format!("\x1b[200~{clean}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        };
        p.grid.scroll_view_to_bottom();
        self.write_pty(&bytes);
        self.redraw();
    }

    fn run_action(&mut self, action: Action) {
        let page = self
            .pane()
            .map_or(1, |p| p.grid.rows.saturating_sub(1).max(1)) as isize;
        match action {
            Action::Copy => {
                self.copy_selection();
            }
            Action::Paste => self.paste(),
            Action::SelectAll => {
                if let Some(p) = self.pane_mut() {
                    p.grid.select_all();
                }
            }
            Action::Find => {
                if self.search.is_some() {
                    self.close_search();
                } else {
                    // Start from the selection, like most editors.
                    let query = self
                        .pane()
                        .and_then(|p| p.grid.selection_text())
                        .filter(|t| !t.contains('\n'))
                        .unwrap_or_default();
                    self.search = Some(Search {
                        query,
                        matches: Vec::new(),
                        current: None,
                    });
                    self.refresh_search_jump();
                }
            }
            Action::ScrollPageUp => self.scroll_focused(page),
            Action::ScrollPageDown => self.scroll_focused(-page),
            Action::ScrollToTop => self.scroll_focused(isize::MAX / 2),
            Action::ScrollToBottom => {
                if let Some(p) = self.pane_mut() {
                    p.grid.scroll_view_to_bottom();
                }
            }
            Action::NewTab => {
                if let Err(e) = self.new_tab() {
                    show_warning(e);
                }
            }
            Action::ClosePane => {
                if let Some(id) = self.focused_id() {
                    self.close_pane(id);
                }
            }
            Action::NextTab => {
                let n = self.tabs.len().max(1);
                self.switch_tab((self.active + 1) % n);
            }
            Action::PreviousTab => {
                let n = self.tabs.len().max(1);
                self.switch_tab((self.active + n - 1) % n);
            }
            Action::SplitRight => self.split(Split::Right),
            Action::SplitDown => self.split(Split::Down),
            Action::Focus(dir) => {
                let area = self.content_area();
                let next = self
                    .tabs
                    .get(self.active)
                    .and_then(|t| t.layout.neighbor(area, t.focused, dir));
                if let Some(n) = next {
                    self.focus_pane(n);
                }
            }
            Action::Resize(dir) => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    if tab.layout.resize(tab.focused, dir, RESIZE_STEP) {
                        self.fit_all();
                    }
                }
            }
            Action::ZoomIn => self.zoom_by(1.0),
            Action::ZoomOut => self.zoom_by(-1.0),
            Action::ZoomReset => self.zoom_by(0.0),
            Action::Fullscreen => {
                if let Some(w) = &self.window {
                    let full = w.fullscreen().is_some();
                    w.set_fullscreen((!full).then_some(Fullscreen::Borderless(None)));
                }
            }
            Action::NextColorScheme => self.next_scheme(),
            Action::OpenSettings if self.settings_open() => self.close_settings(false),
            Action::OpenSettings => self.open_settings(),
        }
        self.redraw();
    }

    fn scroll_focused(&mut self, lines: isize) {
        if let Some(p) = self.pane_mut() {
            p.grid.scroll_view(lines);
        }
    }

    // ---- find -------------------------------------------------------------

    fn close_search(&mut self) {
        if self.search.take().is_some() {
            self.redraw();
        }
    }

    /// Re-run the search after output or a resize, keeping the current hit
    /// when it's still there, else the newest.
    fn refresh_search(&mut self) {
        let Some(id) = self.focused_id() else { return };
        let (Some(s), Some(p)) = (&mut self.search, self.panes.get(&id)) else {
            return;
        };
        let keep = s.current.and_then(|i| s.matches.get(i)).map(|m| m.start);
        s.matches = p.grid.search(&s.query, smart_case(&s.query));
        s.current = keep
            .and_then(|k| s.matches.iter().position(|m| m.start == k))
            .or_else(|| s.matches.len().checked_sub(1));
    }

    /// The query changed: search and jump to the newest hit.
    fn refresh_search_jump(&mut self) {
        if let Some(s) = &mut self.search {
            s.current = None;
            s.matches.clear();
        }
        self.refresh_search();
        self.jump_to_match();
    }

    /// Move to the next older (up) or newer hit, wrapping around.
    fn step_search(&mut self, older: bool) {
        let Some(s) = &mut self.search else { return };
        let n = s.matches.len();
        if n == 0 {
            return;
        }
        s.current = Some(match s.current {
            Some(i) if older => (i + n - 1) % n,
            Some(i) => (i + 1) % n,
            None => n - 1,
        });
        self.jump_to_match();
    }

    fn jump_to_match(&mut self) {
        let Some(id) = self.focused_id() else { return };
        let target = self
            .search
            .as_ref()
            .and_then(|s| s.matches.get(s.current?))
            .map(|m| m.start);
        if let (Some(t), Some(p)) = (target, self.panes.get_mut(&id)) {
            p.grid.scroll_to_point(t);
        }
        self.redraw();
    }

    /// Typing while the find box is open. Returns whether the key was used.
    fn search_key(&mut self, event: &KeyEvent) -> bool {
        let Some(s) = &mut self.search else {
            return false;
        };
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.close_search(),
            Key::Named(NamedKey::Enter) => {
                let older = !self.mods.shift_key();
                self.step_search(older);
            }
            Key::Named(NamedKey::Backspace) => {
                s.query.pop();
                self.refresh_search_jump();
            }
            Key::Named(NamedKey::ArrowUp) => self.step_search(true),
            Key::Named(NamedKey::ArrowDown) => self.step_search(false),
            _ => {
                let text = event
                    .text
                    .as_deref()
                    .filter(|t| !t.chars().any(char::is_control));
                match text {
                    Some(t) if !self.mods.control_key() => {
                        s.query.push_str(t);
                        self.refresh_search_jump();
                    }
                    _ => return false,
                }
            }
        }
        self.redraw();
        true
    }

    /// A key that isn't a binding or for the find box: send it to the
    /// focused pane's program.
    fn type_key(&mut self, event: &KeyEvent) {
        let mods = self.mods;
        let Some(p) = self.pane_mut() else { return };
        let Some(bytes) = key_to_bytes(event, mods, p.grid.modes.app_cursor) else {
            return;
        };
        p.grid.scroll_view_to_bottom();
        let _ = p.pty.write_input(&bytes);
        self.reset_blink();
        self.redraw();
    }

    // ---- mouse ------------------------------------------------------------

    /// The pane under the mouse.
    fn pane_under_mouse(&self) -> Option<PaneId> {
        let tab = self.tabs.get(self.active)?;
        let (x, y) = (self.mouse_pos.0 as f32, self.mouse_pos.1 as f32);
        tab.layout.pane_at(self.content_area(), self.gap(), x, y)
    }

    fn rect_of(&self, id: PaneId) -> Option<Rect> {
        let tab = self.tabs.get(self.active)?;
        self.tab_rects(tab)
            .into_iter()
            .find(|(p, _)| *p == id)
            .map(|(_, r)| r)
    }

    /// The viewport cell of pane `id` under the mouse.
    fn cell_under_mouse(&self, id: PaneId) -> Option<(usize, usize)> {
        let r = self.renderer.as_ref()?;
        let rect = self.rect_of(id)?;
        let p = self.panes.get(&id)?;
        Some(r.cell_in(rect, self.mouse_pos.0, self.mouse_pos.1, &p.grid))
    }

    fn point_under_mouse(&self, id: PaneId) -> Option<Point> {
        let (col, row) = self.cell_under_mouse(id)?;
        Some(self.panes.get(&id)?.grid.viewport_point(col, row))
    }

    /// Whether mouse events go to the program in pane `id` (Shift keeps
    /// them for selecting).
    fn reporting(&self, id: PaneId) -> bool {
        !self.mods.shift_key()
            && self
                .panes
                .get(&id)
                .is_some_and(|p| p.grid.modes.mouse != MouseMode::Off)
    }

    fn report_mouse(&mut self, id: PaneId, button: mouse::MouseButton, kind: MouseEventKind) {
        let Some((col, row)) = self.cell_under_mouse(id) else {
            return;
        };
        let mods = MouseMods {
            shift: self.mods.shift_key(),
            alt: self.mods.alt_key(),
            ctrl: self.mods.control_key(),
        };
        let Some(p) = self.panes.get_mut(&id) else {
            return;
        };
        if let Some(bytes) = mouse::encode(
            p.grid.modes.mouse,
            p.grid.modes.mouse_sgr,
            button,
            kind,
            col,
            row,
            mods,
        ) {
            let _ = p.pty.write_input(&bytes);
        }
    }

    /// Track the link under the mouse while Ctrl is held, to underline it.
    fn update_hover(&mut self) {
        let hover = if self.mods.control_key() {
            self.pane_under_mouse().and_then(|id| {
                let p = self.point_under_mouse(id)?;
                let (url, a, b) = self.panes.get(&id)?.grid.url_at(p)?;
                Some((id, url, a, b))
            })
        } else {
            None
        };
        let same = match (&hover, &self.link) {
            (Some(a), Some(b)) => a.0 == b.0 && a.2 == b.2 && a.3 == b.3,
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.link = hover;
            self.redraw();
        }
    }

    /// A click in the tab strip. Returns whether it landed there.
    fn tab_click(&mut self, button: MouseButton) -> bool {
        if !self.show_tabs() {
            return false;
        }
        let Some(r) = &self.renderer else {
            return false;
        };
        if self.mouse_pos.1 >= r.tab_bar_height() as f64 {
            return false;
        }
        let hit = r.tab_hit(self.mouse_pos.0, self.mouse_pos.1, self.tabs.len());
        if matches!(hit, Some(TabHit::Tab(_) | TabHit::Close(_) | TabHit::New)) {
            // Going to a tab leaves the page, putting things back.
            self.close_settings(false);
        }
        match (button, hit) {
            (MouseButton::Left, Some(TabHit::Tab(i))) => self.switch_tab(i),
            (MouseButton::Left, Some(TabHit::Close(i)))
            | (MouseButton::Middle, Some(TabHit::Tab(i))) => self.close_tab(i),
            (MouseButton::Left, Some(TabHit::New)) => self.run_action(Action::NewTab),
            (MouseButton::Left, Some(TabHit::Settings)) => self.run_action(Action::OpenSettings),
            (MouseButton::Left, Some(TabHit::Minimize)) => {
                if let Some(w) = &self.window {
                    w.set_minimized(true);
                }
            }
            (MouseButton::Left, Some(TabHit::Maximize)) => self.toggle_maximized(),
            (MouseButton::Left, Some(TabHit::CloseWindow)) => self.close_window(),
            (MouseButton::Left, Some(TabHit::Empty)) if self.custom_frame => {
                let now = Instant::now();
                if self.title_click.is_some_and(|t| now - t < MULTI_CLICK) {
                    self.title_click = None;
                    self.toggle_maximized();
                } else {
                    self.title_click = Some(now);
                    if let Some(w) = &self.window {
                        let _ = w.drag_window();
                    }
                }
            }
            _ => {}
        }
        true
    }

    fn toggle_maximized(&self) {
        if let Some(w) = &self.window {
            w.set_maximized(!w.is_maximized());
        }
    }

    fn close_window(&mut self) {
        for p in self.panes.values_mut() {
            let _ = p.pty.terminate();
        }
        self.quit = true;
    }

    /// Which window edge the mouse is on, for resizing a window we draw
    /// the frame of.
    fn resize_edge(&self) -> Option<ResizeDirection> {
        let w = self.window.as_ref()?;
        if !self.custom_frame || w.is_maximized() || w.fullscreen().is_some() {
            return None;
        }
        let size = w.inner_size();
        let border = (6.0 * w.scale_factor()).max(4.0);
        let (x, y) = self.mouse_pos;
        let (left, right) = (x < border, x >= size.width as f64 - border);
        let (top, bottom) = (y < border, y >= size.height as f64 - border);
        Some(match (left, right, top, bottom) {
            (true, _, true, _) => ResizeDirection::NorthWest,
            (_, true, true, _) => ResizeDirection::NorthEast,
            (true, _, _, true) => ResizeDirection::SouthWest,
            (_, true, _, true) => ResizeDirection::SouthEast,
            (true, ..) => ResizeDirection::West,
            (_, true, ..) => ResizeDirection::East,
            (_, _, true, _) => ResizeDirection::North,
            (.., true) => ResizeDirection::South,
            _ => return None,
        })
    }

    fn left_press(&mut self) {
        if let Some(dir) = self.resize_edge() {
            if let Some(w) = &self.window {
                let _ = w.drag_resize_window(dir);
            }
            return;
        }
        if self.tab_click(MouseButton::Left) {
            return;
        }
        if self.settings_open() {
            return self.settings_press();
        }
        let Some(id) = self.pane_under_mouse() else {
            return;
        };
        self.focus_pane(id);
        if self.mods.control_key() {
            if let Some((_, url, ..)) = &self.link {
                win::open_url(url);
                return;
            }
        }
        if self.reporting(id) {
            self.report_mouse(id, mouse::MouseButton::Left, MouseEventKind::Press);
            self.reported_button = Some((id, mouse::MouseButton::Left));
            return;
        }
        let Some(point) = self.point_under_mouse(id) else {
            return;
        };
        let now = Instant::now();
        let count = match &self.last_click {
            Some(c) if now - c.at < MULTI_CLICK && c.point.line == point.line => c.count % 3 + 1,
            _ => 1,
        };
        self.last_click = Some(Click {
            at: now,
            point,
            count,
        });
        let shift = self.mods.shift_key();
        let Some(p) = self.panes.get_mut(&id) else {
            return;
        };
        match count {
            _ if shift && p.grid.selection.is_some() => p.grid.extend_selection(point),
            1 => p.grid.selection = None,
            2 => p.grid.select_word(point),
            _ => p.grid.select_line(point),
        }
        self.press = Some((id, point));
        self.redraw();
    }

    fn left_release(&mut self) {
        if self.settings_open() {
            return self.settings_release();
        }
        if let Some((id, button)) = self.reported_button.take() {
            self.report_mouse(id, button, MouseEventKind::Release);
            return;
        }
        self.press = None;
        if self.config.copy_on_select {
            self.copy_selection();
        }
    }

    /// Keep the tab-strip hover and the resize cursor up to date.
    fn chrome_hover(&mut self) {
        let edge = self.resize_edge();
        if let Some(w) = &self.window {
            w.set_cursor(match edge {
                Some(ResizeDirection::East | ResizeDirection::West) => CursorIcon::EwResize,
                Some(ResizeDirection::North | ResizeDirection::South) => CursorIcon::NsResize,
                Some(ResizeDirection::NorthWest | ResizeDirection::SouthEast) => {
                    CursorIcon::NwseResize
                }
                Some(ResizeDirection::NorthEast | ResizeDirection::SouthWest) => {
                    CursorIcon::NeswResize
                }
                None => CursorIcon::Default,
            });
        }
        let hover = match (&self.renderer, edge) {
            (Some(r), None) if self.show_tabs() => {
                r.tab_hit(self.mouse_pos.0, self.mouse_pos.1, self.tabs.len())
            }
            _ => None,
        };
        if hover != self.tab_hover {
            self.tab_hover = hover;
            if let Some(r) = &mut self.renderer {
                r.hover = hover;
            }
            self.redraw();
        }
    }

    fn mouse_moved(&mut self) {
        self.chrome_hover();
        if self.settings_open() {
            return self.settings_motion();
        }
        if let Some((id, button)) = self.reported_button {
            self.report_mouse(id, button, MouseEventKind::Motion);
            return;
        }
        if let Some((id, anchor)) = self.press {
            let point = self.point_under_mouse(id);
            if let (Some(p), Some(pane)) = (point, self.panes.get_mut(&id)) {
                let grid = &mut pane.grid;
                if grid.selection.is_none() && p != anchor {
                    grid.start_selection(anchor);
                }
                if grid.selection.is_some() {
                    grid.extend_selection(p);
                    self.redraw();
                }
            }
            return;
        }
        if let Some(id) = self.pane_under_mouse() {
            let motion = self
                .panes
                .get(&id)
                .is_some_and(|p| p.grid.modes.mouse == MouseMode::Motion);
            if motion && self.reporting(id) {
                self.report_mouse(id, mouse::MouseButton::None, MouseEventKind::Motion);
            }
        }
        self.update_hover();
    }

    fn other_button(&mut self, button: MouseButton, state: ElementState) {
        if state == ElementState::Pressed && self.tab_click(button) {
            return;
        }
        if self.settings_open() {
            return;
        }
        let Some(id) = self.pane_under_mouse() else {
            return;
        };
        let code = match button {
            MouseButton::Right => mouse::MouseButton::Right,
            MouseButton::Middle => mouse::MouseButton::Middle,
            _ => return,
        };
        if self.reporting(id) {
            let kind = if state == ElementState::Pressed {
                MouseEventKind::Press
            } else {
                MouseEventKind::Release
            };
            return self.report_mouse(id, code, kind);
        }
        if button != MouseButton::Right {
            return;
        }
        self.focus_pane(id);
        match state {
            // Menus open on release, like the rest of Windows: opening on
            // press lets the release pick whatever lands under it.
            ElementState::Released if self.config.right_click_menu() => self.context_menu(),
            ElementState::Pressed if !self.config.right_click_menu() => {
                // Right-click copies a selection, or pastes — like conhost.
                if self.copy_selection() {
                    if let Some(p) = self.pane_mut() {
                        p.grid.selection = None;
                    }
                    self.redraw();
                } else {
                    self.paste();
                }
            }
            _ => {}
        }
    }

    fn wheel(&mut self, lines: f64) {
        if self.mods.control_key() {
            // Ctrl+wheel zooms, a step per notch.
            if lines.abs() >= 1.0 {
                self.zoom_by(lines.signum() as f32);
            }
            return;
        }
        self.wheel_accum += lines;
        let whole = self.wheel_accum.trunc();
        self.wheel_accum -= whole;
        let n = whole as isize;
        if n == 0 {
            return;
        }
        if self.settings_open() {
            return self.settings_wheel(n);
        }
        let Some(id) = self.pane_under_mouse() else {
            return;
        };
        if self.reporting(id) {
            let button = if n > 0 {
                mouse::MouseButton::WheelUp
            } else {
                mouse::MouseButton::WheelDown
            };
            for _ in 0..n.unsigned_abs() {
                self.report_mouse(id, button, MouseEventKind::Press);
            }
            return;
        }
        let Some(p) = self.panes.get_mut(&id) else {
            return;
        };
        if p.grid.is_alt_screen() {
            // Full-screen apps (less, vim…) have no scrollback of ours to
            // show: send arrow keys instead, like other terminals do.
            let key = Key::Named(if n > 0 {
                NamedKey::ArrowUp
            } else {
                NamedKey::ArrowDown
            });
            if let Some(seq) =
                encode_key(&key, None, ModifiersState::empty(), p.grid.modes.app_cursor)
            {
                let _ = p.pty.write_input(&seq.repeat(n.unsigned_abs()));
            }
        } else {
            p.grid.scroll_view(n);
            self.redraw();
        }
    }

    // ---- cursor blink, summon -------------------------------------------

    fn blinking(&self) -> bool {
        if !self.focused || self.hidden {
            return false;
        }
        self.pane().is_some_and(|p| {
            p.grid.display_offset == 0
                && p.grid.modes.cursor_visible
                && p.grid
                    .modes
                    .cursor_shape
                    .map_or(self.config.cursor_blink, |(_, blink)| blink)
        })
    }

    /// Show the cursor and restart the blink (after typing or output).
    fn reset_blink(&mut self) {
        if !self.cursor_on {
            self.redraw();
        }
        self.cursor_on = true;
        self.next_blink = Instant::now() + BLINK;
    }

    /// The summon hotkey: hide when in front, else bring tacoterm up.
    fn summon(&mut self) {
        let Some(w) = self.window.clone() else { return };
        if !self.hidden && self.focused {
            if self.config.quake_mode {
                w.set_visible(false);
            } else {
                w.set_minimized(true);
            }
            self.hidden = true;
        } else {
            if self.config.quake_mode {
                place_quake(&w);
            }
            w.set_visible(true);
            w.set_minimized(false);
            w.focus_window();
            self.hidden = false;
            self.redraw();
        }
    }

    // ---- drawing ----------------------------------------------------------

    fn draw(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let area = self.content_area();
        let gap = self.gap();
        let rects = tab.layout.rects(area, gap);
        let dividers = if rects.len() > 1 {
            tab.layout.dividers(area, gap)
        } else {
            Vec::new()
        };
        let search = self.search.as_ref();
        let settings = self.page.as_ref().map(|p| p.view(area));
        // The page takes the place of the panes.
        let rects = if settings.is_some() {
            Vec::new()
        } else {
            rects
        };
        let dividers = if settings.is_some() {
            Vec::new()
        } else {
            dividers
        };
        let panes = rects
            .iter()
            .filter_map(|(id, rect)| {
                let pane = self.panes.get(id)?;
                let focused = *id == tab.focused;
                let (matches, current_match) = match search {
                    Some(s) if focused => (&s.matches[..], s.current),
                    _ => (&[][..], None),
                };
                Some(PaneView {
                    grid: &pane.grid,
                    rect: *rect,
                    focused,
                    matches,
                    current_match,
                    link: self
                        .link
                        .as_ref()
                        .filter(|l| l.0 == *id)
                        .map(|l| (l.2, l.3)),
                })
            })
            .collect();
        let tabs = if self.show_tabs() {
            self.tabs
                .iter()
                .map(|t| {
                    self.panes
                        .get(&t.focused)
                        .map_or_else(String::new, |p| p.title.clone())
                })
                .collect()
        } else {
            Vec::new()
        };
        let frame = Frame {
            panes,
            dividers,
            tabs,
            active_tab: self.active,
            search: search.map(|s| SearchBar {
                query: &s.query,
                current: s.current,
                total: s.matches.len(),
            }),
            cursor_on: self.cursor_on,
            flash: self.flash_until.is_some_and(|t| Instant::now() < t),
            settings,
        };
        let Some(r) = &mut self.renderer else { return };
        if !r.render(&frame) {
            self.redraw();
        }
    }
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Size the window for 80×24 cells (plus the tab strip) before it
        // appears.
        let scale = event_loop
            .primary_monitor()
            .map(|m| m.scale_factor())
            .unwrap_or(1.0);
        let fonts = Fonts::new(&self.config, scale);
        let pad = (self.config.padding as f64 * scale).round() as u32;
        let quake = self.config.quake_mode;
        let custom = self.config.custom_title_bar() && !quake;
        let bar = if custom || self.config.tab_bar.trim().eq_ignore_ascii_case("always") {
            fonts.tab_bar_height()
        } else {
            0
        };
        let size = PhysicalSize::new(
            80 * fonts.cell_width + 2 * pad,
            24 * fonts.cell_height + 2 * pad + bar,
        );
        let mut attrs = Window::default_attributes()
            .with_title("tacoterm")
            .with_theme(Some(title_bar_theme(self.config.theme().0.background)))
            .with_inner_size(size)
            .with_system_backdrop(backdrop_type(self.config.backdrop()));
        if quake {
            attrs = attrs
                .with_window_level(WindowLevel::AlwaysOnTop)
                .with_skip_taskbar(true);
        }
        if quake || custom {
            attrs = attrs.with_decorations(false);
        }
        let config = self.render_config();
        // See-through windows: the GPU image goes straight to the screen
        // through DirectComposition. Windows' own copy of the window (the
        // redirection surface) would sit under it and show through as a
        // gray slab, so leave it out. If the GPU can't present that way,
        // make the window again the ordinary way.
        let mut fonts = Some(fonts);
        let mut made = None;
        for composited in [true, false] {
            let attrs = if composited {
                attrs.clone().with_no_redirection_bitmap(true)
            } else {
                attrs.clone().with_transparent(true)
            }
            // Shown once we know it is the one we keep.
            .with_visible(false);
            let window = match event_loop.create_window(attrs) {
                Ok(w) => Arc::new(w),
                Err(e) => return self.fail(event_loop, format!("cannot create window: {e}")),
            };
            let fonts = match fonts.take() {
                Some(f) if window.scale_factor() == scale => f,
                _ => Fonts::new(&config, window.scale_factor()),
            };
            let renderer = match Renderer::new(window.clone(), fonts, &config) {
                Ok(r) => r,
                Err(e) => return self.fail(event_loop, e),
            };
            if composited && !renderer.uses_composition() {
                drop(renderer);
                drop(window);
                continue;
            }
            made = Some((window, renderer));
            break;
        }
        let Some((window, mut renderer)) = made else {
            return self.fail(event_loop, "cannot create window".to_string());
        };
        if quake {
            place_quake(&window);
        }
        if custom {
            window.set_undecorated_shadow(true);
            renderer.window_buttons = true;
            renderer.maximized = window.is_maximized();
        }
        self.custom_frame = custom;
        window.set_visible(true);
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.apply_transparency();
        if let Err(e) = self.new_tab() {
            return self.fail(event_loop, e);
        }
        let mut warnings = std::mem::take(&mut self.warnings);
        let hotkey = self.config.summon_hotkey.trim().to_string();
        if !hotkey.is_empty() {
            let proxy = self.proxy.clone();
            let registered = win::register_hotkey(&hotkey, move || {
                let _ = proxy.send_event(Wake::Summon);
            });
            // The default combo being taken (say, by Windows Terminal's
            // own quake mode) isn't worth a popup; a chosen one is.
            if let Err(e) = registered {
                if hotkey != Config::default().summon_hotkey || quake {
                    warnings.push(e);
                }
            }
        }
        self.report(warnings);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                for p in self.panes.values_mut() {
                    let _ = p.pty.terminate();
                }
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                let maximized = self.window.as_ref().is_some_and(|w| w.is_maximized());
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                    r.maximized = maximized;
                }
                self.fit_all();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(r) = &mut self.renderer {
                    r.set_scale(scale_factor);
                }
                self.fit_all();
            }
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                if let Some(r) = &mut self.renderer {
                    r.focused = focused;
                }
                if let Some(p) = self.pane_mut() {
                    if p.grid.modes.focus_events {
                        let _ = p.pty.write_input(mouse::focus_report(focused));
                    }
                }
                if focused {
                    self.hidden = false;
                }
                self.reset_blink();
                self.redraw();
            }
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
                self.update_hover();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    let bare = event.key_without_modifiers();
                    let action = self.bindings.lookup(&bare, self.mods);
                    if self.settings_open() {
                        // Only the binding that toggles the page gets past
                        // it; zooming and full screen are fine too.
                        match action {
                            Some(
                                a @ (Action::OpenSettings
                                | Action::Fullscreen
                                | Action::ZoomIn
                                | Action::ZoomOut
                                | Action::ZoomReset),
                            ) => self.run_action(a),
                            _ => self.settings_key(&event),
                        }
                    } else if let Some(action) = action {
                        self.run_action(action);
                    } else if !self.search_key(&event) {
                        self.type_key(&event);
                    }
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.tab_hover = None;
                if let Some(r) = &mut self.renderer {
                    r.hover = None;
                }
                self.redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x, position.y);
                self.mouse_moved();
            }
            WindowEvent::MouseInput { state, button, .. } => match (button, state) {
                (MouseButton::Left, ElementState::Pressed) => self.left_press(),
                (MouseButton::Left, ElementState::Released) => self.left_release(),
                (b, s) => self.other_button(b, s),
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
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
        if self.quit {
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Wake) {
        match event {
            Wake::ConfigChanged => self.reload_config(),
            Wake::Summon => self.summon(),
            Wake::PtyOutput => self.drain_pty(),
            Wake::ChildExited(id) => {
                self.drain_pty();
                self.close_pane(id);
            }
        }
        if self.quit {
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let mut wake = None;
        if self.blinking() {
            if now >= self.next_blink {
                self.cursor_on = !self.cursor_on;
                self.next_blink = now + BLINK;
                self.redraw();
            }
            wake = Some(self.next_blink);
        } else if !self.cursor_on {
            self.cursor_on = true;
            self.redraw();
        }
        if let Some(t) = self.flash_until {
            if now >= t {
                self.flash_until = None;
                self.redraw();
            } else {
                wake = Some(wake.map_or(t, |w: Instant| w.min(t)));
            }
        }
        event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}
