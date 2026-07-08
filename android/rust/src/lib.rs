//! Android entry point. This is deliberately tiny: it builds the SAME
//! `UiApp` the desktop uses, backed by a `RemoteSession` (TLS WebSocket to
//! the desktop) instead of a local controller.
//!
//! Target device: LG K51 (LM-K500UM), Android 10 / API 29.

#![cfg(target_os = "android")]

use android_activity::AndroidApp;
use eframe::{NativeOptions, Renderer};
use llm_desk_core::remote::client::RemoteSession;
use llm_desk_ui::{Frontend, UiApp};
use std::sync::{Arc, OnceLock};

/// The live session, stashed so the JNI file-picked callback can push an
/// upload into it. Set once at startup.
static SESSION: OnceLock<Arc<RemoteSession>> = OnceLock::new();

#[no_mangle]
fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("llm-desk"),
    );

    // Saved server config (host, port, pinned fingerprint, device token)
    // lives in the app's private storage.
    let config_dir = app
        .internal_data_path()
        .unwrap_or_else(|| std::path::PathBuf::from("/data/local/tmp"));
    let session = RemoteSession::new(config_dir);
    let _ = SESSION.set(session.clone());

    // egui does not summon the IME by itself on Android; pop the soft
    // keyboard whenever a text field gains focus.
    let kb = app.clone();
    let ui = UiApp::new(Frontend::Remote(session), /* mobile = */ true)
        .with_keyboard_hook(Box::new(move |show| {
            if show {
                kb.show_soft_input(true);
            } else {
                kb.hide_soft_input(false);
            }
        }))
        // Tapping "Choose a model file to upload" opens the Android document
        // picker; the chosen file is streamed up to the desktop.
        .with_file_pick_hook(Box::new(launch_file_picker));

    // Scale the UI from the device's reported density (K51: ~295 dpi ⇒ ~1.8).
    let ppp = app
        .config()
        .density()
        .map(|dpi| (dpi as f32 / 160.0).clamp(1.0, 3.0))
        .unwrap_or(2.0);

    let options = NativeOptions {
        android_app: Some(app),
        renderer: Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        "llm-desk",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_pixels_per_point(ppp);
            Ok(Box::new(ui))
        }),
    )
    .expect("eframe failed to start");
}

/// Call `MainActivity.pickModelFile()` on the JVM side to launch the system
/// document picker. Best-effort: any JNI hiccup is logged and swallowed so a
/// picker problem can never crash the app.
fn launch_file_picker() {
    if let Err(e) = try_launch_file_picker() {
        log::warn!("file picker launch failed: {e:?}");
    }
}

fn try_launch_file_picker() -> Result<(), jni::errors::Error> {
    let ctx = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }?;
    let mut env = vm.attach_current_thread()?;
    let activity = unsafe { jni::objects::JObject::from_raw(ctx.context().cast()) };
    env.call_method(&activity, "pickModelFile", "()V", &[])?;
    Ok(())
}

/// Called by `MainActivity` (Kotlin) once it has copied the picked document to
/// a real file path in the app's cache. We hand that path to the session,
/// which streams it up to the desktop for import.
///
/// JNI name: `dev.llmdesk.app.MainActivity.nativeOnFilePicked(String)`.
#[no_mangle]
pub extern "C" fn Java_dev_llmdesk_app_MainActivity_nativeOnFilePicked(
    mut env: jni::JNIEnv,
    _this: jni::objects::JObject,
    path: jni::objects::JString,
) {
    let path: String = match env.get_string(&path) {
        Ok(s) => s.into(),
        Err(_) => return,
    };
    if let Some(session) = SESSION.get() {
        session.upload_model_path(std::path::PathBuf::from(path));
    }
}
