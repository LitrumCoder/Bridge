//! The window shows what `bridged` reports and sends its commands; nothing is decided
//! here (ADR-0010). Closing the window hides it to the tray.

use bridge_ipc::{IpcClient, Request, Response, default_endpoint};
use qrcode::QrCode;
use qrcode::render::svg;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State, WindowEvent};
use tokio::sync::Mutex;

#[derive(Default)]
struct Daemon(Mutex<Option<IpcClient>>);

impl Daemon {
    async fn request(&self, request: Request) -> Result<Response, String> {
        let mut slot = self.0.lock().await;
        if slot.is_none() {
            *slot = Some(
                IpcClient::connect(&endpoint())
                    .await
                    .map_err(|e| format!("daemon is not running: {e}"))?,
            );
        }
        let client = slot.as_mut().expect("connected above");
        match client.request(request).await {
            Ok(r) => Ok(r),
            Err(e) => {
                *slot = None;
                Err(e.to_string())
            }
        }
    }
}

fn endpoint() -> String {
    std::env::var("BRIDGE_IPC").unwrap_or_else(|_| default_endpoint())
}

#[tauri::command]
async fn ipc(daemon: State<'_, Daemon>, request: serde_json::Value) -> Result<Response, String> {
    let request: Request = serde_json::from_value(request).map_err(|e| e.to_string())?;
    daemon.request(request).await
}

#[tauri::command]
fn qr_svg(text: String) -> Result<String, String> {
    let code = QrCode::new(text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(code
        .render::<svg::Color<'_>>()
        .min_dimensions(160, 160)
        .quiet_zone(true)
        .build())
}

fn show(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn send(app: &AppHandle, request: Request) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = app.state::<Daemon>().request(request).await;
    });
}

fn tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Bridge", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "Pause", true, None::<&str>)?;
    let resume = MenuItem::with_id(app, "resume", "Resume", true, None::<&str>)?;
    let leave = MenuItem::with_id(app, "leave", "Disconnect", true, None::<&str>)?;
    let rejoin = MenuItem::with_id(app, "rejoin", "Rejoin", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit window", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[&open, &sep, &pause, &resume, &leave, &rejoin, &sep2, &quit],
    )?;
    TrayIconBuilder::with_id("bridge")
        .icon(app.default_window_icon().cloned().expect("bundle icon"))
        .tooltip("Bridge")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, e| match e.id.as_ref() {
            "open" => show(app),
            "pause" => send(app, Request::Pause),
            "resume" => send(app, Request::Resume),
            "leave" => send(app, Request::Leave),
            "rejoin" => send(app, Request::Rejoin),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                show(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show(app)))
        .plugin(tauri_plugin_dialog::init())
        .manage(Daemon::default())
        .invoke_handler(tauri::generate_handler![ipc, qr_svg])
        .setup(|app| {
            tray(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("tauri runtime");
}
