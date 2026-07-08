//! llm-desk-core: everything except the (egui) user interface, so the same
//! logic runs headless behind the desktop window and the Android app.

pub mod agent;
pub mod autotune;
pub mod backend;
pub mod controller;
pub mod ingest;
pub mod protocol;
pub mod remote;
pub mod tools;
