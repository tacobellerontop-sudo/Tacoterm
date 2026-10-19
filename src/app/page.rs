//! The settings page: a list of settings beside a live preview. Every
//! change applies to the whole window at once; Save writes config.toml and
//! Cancel puts things back.

use winit::event::KeyEvent;
use winit::keyboard::{Key, NamedKey};

use tacoterm::config::{self, Config};
use tacoterm::grid::Grid;
use tacoterm::layout::Rect;
use tacoterm::renderer::{Control, SettingsHit, SettingsPage, SettingsRow};
use tacoterm::settings::{preview_text, Kind, Settings};
use tacoterm::vt::Vt;

use super::{show_warning, App};

pub(super) struct Page {
    model: Settings,
    scroll: usize,
    hover: Option<SettingsHit>,
    /// The sample terminal in the preview.
    preview: Grid,
    /// What the preview shows, to rebuild it only when that changes.
    preview_text: String,
    /// A slider being dragged.
    dragging: Option<usize>,
}

fn sample(cols: usize, rows: usize, text: &str) -> Grid {
    let mut grid = Grid::new(cols.max(1), rows.max(1), 0);
    Vt::new().advance(&mut grid, text.as_bytes());
    grid
}

impl App {
    pub(super) fn settings_open(&self) -> bool {
        self.page.is_some()
    }

    pub(super) fn open_settings(&mut self) {
        if self.page.is_some() {
            return;
        }
        let fonts = self
            .renderer
            .as_ref()
            .map(|r| r.monospace_families())
            .unwrap_or_default();
        let shells = config::available_shells()
            .into_iter()
            .map(|s| (s.setting.to_string(), s.label.to_string()))
            .collect();
        self.close_search();
        self.page = Some(Page {
            model: Settings::new(&self.config, fonts, shells),
            scroll: 0,
            hover: None,
            preview: Grid::new(1, 1, 0),
            preview_text: String::new(),
            dragging: None,
        });
        self.fit_preview();
        self.redraw();
    }

    /// Leave the page, keeping the changes (saved) or putting back what was
    /// there before.
    pub(super) fn close_settings(&mut self, save: bool) {
        let Some(mut page) = self.page.take() else {
            return;
        };
        if save {
            if let Err(e) = page.model.save() {
                show_warning(format!("cannot save settings: {e}"));
            }
        } else if page.model.dirty() {
            self.apply_config(page.model.original().clone());
        }
        self.redraw();
    }

    /// Size the sample terminal to the preview box.
    pub(super) fn fit_preview(&mut self) {
        let area = self.content_area();
        let (Some(r), Some(page)) = (&self.renderer, &mut self.page) else {
            return;
        };
        let (cols, rows) = r.pane_grid_size(r.settings_preview_rect(area));
        let text = preview_text(&page.model.draft);
        if (cols as usize, rows as usize) != (page.preview.cols, page.preview.rows)
            || text != page.preview_text
        {
            page.preview = sample(cols as usize, rows as usize, &text);
            page.preview_text = text;
        }
    }

    /// The draft changed: show it everywhere.
    fn settings_changed(&mut self) {
        let Some(page) = &self.page else { return };
        let draft: Config = page.model.draft.clone();
        self.apply_config(draft);
        self.fit_preview();
        self.redraw();
    }

    fn select(&mut self, i: usize) {
        let visible = self.settings_visible();
        let Some(page) = &mut self.page else { return };
        page.model.selected = i.min(page.model.fields.len() - 1);
        let sel = page.model.selected;
        if sel < page.scroll {
            page.scroll = sel;
        } else if sel >= page.scroll + visible {
            page.scroll = sel + 1 - visible;
        }
        self.redraw();
    }

    fn settings_visible(&self) -> usize {
        self.renderer
            .as_ref()
            .map_or(10, |r| r.settings_visible_rows(self.content_area()))
    }

    fn adjust(&mut self, i: usize, delta: i32) {
        if let Some(page) = &mut self.page {
            page.model.adjust(i, delta);
        }
        self.settings_changed();
    }

    /// A key while the page is up.
    pub(super) fn settings_key(&mut self, event: &KeyEvent) {
        let Some(page) = &self.page else { return };
        let sel = page.model.selected;
        let n = page.model.fields.len();
        let ctrl = self.mods.control_key();
        if page.model.is_text(sel) && !ctrl {
            match &event.logical_key {
                Key::Named(NamedKey::Backspace) => {
                    if let Some(page) = &mut self.page {
                        page.model.backspace(sel);
                    }
                    return self.settings_changed();
                }
                Key::Named(NamedKey::Delete) => {
                    if let Some(page) = &mut self.page {
                        while !page.model.value_text(sel).is_empty() {
                            page.model.backspace(sel);
                        }
                    }
                    return self.settings_changed();
                }
                Key::Character(_) | Key::Named(NamedKey::Space) => {
                    if let Some(text) = &event.text {
                        if let Some(page) = &mut self.page {
                            page.model.type_text(sel, text);
                        }
                        return self.settings_changed();
                    }
                }
                _ => {}
            }
        }
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.close_settings(false),
            Key::Named(NamedKey::ArrowUp) => self.select(sel.saturating_sub(1)),
            Key::Named(NamedKey::ArrowDown) => self.select((sel + 1).min(n - 1)),
            Key::Named(NamedKey::Home) => self.select(0),
            Key::Named(NamedKey::End) => self.select(n - 1),
            Key::Named(NamedKey::Tab) if self.mods.shift_key() => {
                self.select(sel.saturating_sub(1))
            }
            Key::Named(NamedKey::Tab) => self.select((sel + 1).min(n - 1)),
            Key::Named(NamedKey::ArrowLeft) => self.adjust(sel, -1),
            Key::Named(NamedKey::ArrowRight) => self.adjust(sel, 1),
            Key::Named(NamedKey::Enter) if ctrl => self.close_settings(true),
            Key::Named(NamedKey::Enter | NamedKey::Space) => self.adjust(sel, 1),
            Key::Character(c) if ctrl && c.eq_ignore_ascii_case("s") => self.close_settings(true),
            _ => {}
        }
    }

    fn hit(&self) -> Option<SettingsHit> {
        let page = self.page.as_ref()?;
        let r = self.renderer.as_ref()?;
        r.settings_hit(
            self.content_area(),
            &rows(&page.model),
            page.scroll,
            self.mouse_pos.0,
            self.mouse_pos.1,
        )
    }

    pub(super) fn settings_press(&mut self) {
        match self.hit() {
            Some(SettingsHit::Row(i)) => self.select(i),
            Some(SettingsHit::Prev(i)) => {
                self.select(i);
                self.adjust(i, -1);
            }
            Some(SettingsHit::Next(i)) => {
                self.select(i);
                self.adjust(i, 1);
            }
            Some(SettingsHit::Slider(i, t)) => {
                self.select(i);
                if let Some(page) = &mut self.page {
                    page.model.set_fraction(i, t);
                    page.dragging = Some(i);
                }
                self.settings_changed();
            }
            Some(SettingsHit::Save) => self.close_settings(true),
            Some(SettingsHit::Cancel) => self.close_settings(false),
            Some(SettingsHit::EditFile) => {
                self.close_settings(false);
                self.edit_settings_file();
            }
            None => {}
        }
    }

    pub(super) fn settings_release(&mut self) {
        if let Some(page) = &mut self.page {
            page.dragging = None;
        }
    }

    pub(super) fn settings_motion(&mut self) {
        let dragging = self.page.as_ref().and_then(|p| p.dragging);
        if let Some(i) = dragging {
            // Slide with the mouse even when it leaves the row.
            let pos = self.mouse_pos;
            let area = self.content_area();
            let hit = self.renderer.as_ref().and_then(|r| {
                let page = self.page.as_ref()?;
                r.settings_slider_at(area, &rows(&page.model), page.scroll, i, pos.0)
            });
            if let (Some(t), Some(page)) = (hit, &mut self.page) {
                page.model.set_fraction(i, t);
                self.settings_changed();
            }
            return;
        }
        let hover = self.hit();
        if let Some(page) = &mut self.page {
            if page.hover != hover {
                page.hover = hover;
                self.redraw();
            }
        }
    }

    pub(super) fn settings_wheel(&mut self, lines: isize) {
        let visible = self.settings_visible();
        if let Some(page) = &mut self.page {
            let max = page.model.fields.len().saturating_sub(visible);
            page.scroll = (page.scroll as isize - lines).clamp(0, max as isize) as usize;
            self.redraw();
        }
    }
}

impl Page {
    /// What the renderer draws for the page, filling `area`.
    pub(super) fn view(&self, area: Rect) -> SettingsPage<'_> {
        let m = &self.model;
        SettingsPage {
            area,
            rows: rows(m),
            selected: m.selected,
            scroll: self.scroll,
            help: m.fields[m.selected].help,
            preview: &self.preview,
            dirty: m.dirty(),
            hover: self.hover,
        }
    }
}

fn rows(m: &Settings) -> Vec<SettingsRow> {
    m.fields
        .iter()
        .enumerate()
        .map(|(i, f)| SettingsRow {
            label: f.label.to_string(),
            value: m.value_text(i),
            control: match f.kind {
                Kind::Choice(_) => Control::Choice,
                Kind::Number { .. } => Control::Slider(m.fraction(i).unwrap_or(0.0)),
                Kind::Toggle => Control::Toggle(m.toggle_state(i)),
                Kind::Text { .. } => Control::Text {
                    presets: m.has_presets(i),
                },
            },
        })
        .collect()
}
