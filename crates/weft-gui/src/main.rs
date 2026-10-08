#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::BTreeMap;

use serde::Serialize;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State, WindowEvent};
use tauri_plugin_opener::OpenerExt;
use weft_i18n::{Language, Localizer};
use weft_ipc::{Request, Response};

mod watch;

use watch::{Settings, SettingsStore};

const RELEASES: &str = "https://github.com/qateralong/weft/releases/";

#[derive(Serialize)]
struct Catalog {
    language: &'static str,
    version: &'static str,
    messages: BTreeMap<String, String>,
}

#[tauri::command]
fn catalog(l: State<'_, Localizer>) -> Catalog {
    let language = match l.language() {
        Language::English => "en",
        Language::Russian => "ru",
    };
    Catalog { language, version: env!("CARGO_PKG_VERSION"), messages: l.catalog() }
}

#[tauri::command]
fn settings(store: State<'_, SettingsStore>) -> Settings {
    store.get()
}

#[tauri::command]
fn set_settings(store: State<'_, SettingsStore>, settings: Settings) -> Result<(), String> {
    store.set(settings)
}

#[tauri::command]
fn open_release(app: AppHandle, url: String) -> Result<(), String> {
    if !url.starts_with(RELEASES) {
        return Err("not a Weft release page".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(|error| error.to_string())
}

#[tauri::command]
async fn request(l: State<'_, Localizer>, request: Request) -> Result<Response, String> {
    send(&l, request).await
}

#[tauri::command]
async fn diagnostics(l: State<'_, Localizer>) -> Result<String, String> {
    report(&l, false).await
}

#[tauri::command]
async fn save_report(app: AppHandle, l: State<'_, Localizer>) -> Result<String, String> {
    let text = report(&l, true).await?;
    let dir = app.path().download_dir().or_else(|_| app.path().home_dir()).map_err(|error| error.to_string())?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let path = dir.join(format!("weft-report-{stamp}.txt"));
    std::fs::write(&path, text).map_err(|error| error.to_string())?;
    Ok(path.display().to_string())
}

async fn report(l: &Localizer, logs: bool) -> Result<String, String> {
    match send(l, Request::Diagnose { logs }).await? {
        Response::Diagnostics(diagnostics) => {
            Ok(weft_ipc::report::format(&diagnostics, &|id, args| l.tr_args(id, args)))
        }
        _ => Err(l.tr("error-internal")),
    }
}

pub(crate) async fn send(l: &Localizer, request: Request) -> Result<Response, String> {
    let path = weft_ipc::socket_path();
    let response = weft_ipc::request(&path, &request).await.map_err(|error| {
        let path = path.display().to_string();
        match error.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                l.tr_args("error-daemon-missing", &[("path", &path)])
            }
            std::io::ErrorKind::PermissionDenied => l.tr_args("gui-error-permission", &[("path", &path)]),
            _ => l.tr_args("error-daemon", &[("path", &path), ("reason", &error.to_string())]),
        }
    })?;
    match response {
        Response::Error(failure) => Err(l.tr(failure.message_id())),
        response => Ok(response),
    }
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn toggle_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        } else {
            show_window(app);
        }
    }
}

fn tray(app: &AppHandle) -> tauri::Result<()> {
    let l = app.state::<Localizer>();
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, "show", l.tr("gui-tray-open"), true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "up", l.tr("gui-connect"), true, None::<&str>)?,
            &MenuItem::with_id(app, "down", l.tr("gui-disconnect"), true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "quit", l.tr("gui-tray-quit"), true, None::<&str>)?,
        ],
    )?;
    TrayIconBuilder::with_id("main")
        .icon(Image::from_bytes(include_bytes!("../icons/tray.png"))?)
        .tooltip("Weft")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_window(app),
            "quit" => app.exit(0),
            id @ ("up" | "down") => {
                let request = if id == "up" { Request::Up { link: None, nickname: None } } else { Request::Down };
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = send(&app.state::<Localizer>(), request).await;
                });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                toggle_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    let hidden = std::env::args().any(|arg| arg == "--hidden");
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show_window(app)))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Localizer::from_env())
        .invoke_handler(tauri::generate_handler![
            catalog,
            request,
            diagnostics,
            save_report,
            settings,
            set_settings,
            open_release
        ])
        .setup(move |app| {
            app.manage(SettingsStore::load(app.handle()));
            tray(app.handle())?;
            tauri::async_runtime::spawn(watch::run(app.handle().clone()));
            if !hidden {
                show_window(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("cannot start the Weft app");
}
