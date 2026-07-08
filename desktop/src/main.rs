use llm_desk_core::{backend::OllamaBackend, controller};
use llm_desk_ui::{Frontend, UiApp};
use std::sync::Arc;

fn main() -> eframe::Result {
    let handle = controller::spawn(Arc::new(OllamaBackend::new("http://localhost:11434")));

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 600.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "llm-desk",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_pixels_per_point(1.1);
            Ok(Box::new(UiApp::new(Frontend::Local(handle), false)))
        }),
    )
}
