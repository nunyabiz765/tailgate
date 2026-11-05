//! Everything that doesn't need a window: talking to the CLI, parsing what it
//! says, and working out what to show. `main.rs` and `app.rs` hold the UI.

pub mod cli;
pub mod ipcheck;
pub mod nodes;
pub mod status;
pub mod store;
pub mod summary;
