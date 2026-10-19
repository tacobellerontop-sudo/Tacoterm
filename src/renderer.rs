//! GPU text renderer.
//!
//! Plan: a wgpu surface plus a glyph atlas fed by cosmic-text. Each frame
//! walks the grid and repaints only cells whose (char, fg, bg, attrs) changed
//! since the last frame — damage tracking keeps idle frames free.
//!
//! This is the next build. Everything else is wired and waiting on it:
//! `App` holds an `Option<Renderer>` and calls these once implemented.

use winit::window::Window;

use crate::grid::Grid;

pub struct Renderer {
    // TODO: wgpu Instance/Adapter/Device/Queue/Surface, texture atlas,
    // cosmic-text FontSystem + SwashCache, per-cell last-painted cache.
}

impl Renderer {
    pub fn new(_window: &Window) -> Self {
        todo!("renderer: create wgpu surface, load fonts via cosmic-text, build glyph atlas")
    }

    pub fn resize(&mut self, _width: u32, _height: u32) {
        todo!("renderer: reconfigure surface")
    }

    /// (cols, rows) that fit the surface at the current cell metrics.
    /// The app uses this to resize the grid and ConPTY together.
    pub fn grid_size(&self) -> (u16, u16) {
        todo!("renderer: surface size / cell size")
    }

    pub fn render(&mut self, _grid: &Grid) {
        todo!("renderer: paint damage rects, submit frame")
    }
}
