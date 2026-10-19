//! tacoterm's terminal core: everything except the Windows plumbing.
//!
//! Bytes flow PTY -> [`vt`] parser -> [`grid`] -> [`renderer`], and
//! keystrokes flow back through [`input`]. None of this depends on ConPTY,
//! so it builds and tests on any platform; `src/main.rs` wires it to a
//! window and a pseudo-console on Windows.

pub mod boxdraw;
pub mod config;
pub mod graphics;
pub mod grid;
pub mod input;
pub mod layout;
pub mod mouse;
pub mod renderer;
pub mod schemes;
pub mod settings;
pub mod vt;
