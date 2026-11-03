//! La aplicacion de escritorio: una ventana propia con la interfaz dentro.
//!
//! La interfaz es una pagina web que sirve `gui` desde este mismo proceso; aqui
//! se le pone una ventana nativa (WebView2, que ya trae Windows 10/11) para que
//! se use como cualquier programa: doble clic, ventana, cerrar. Si WebView2 no
//! esta disponible se abre en el navegador, que funciona igual.

use std::time::Duration;

use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::{Icon, UserAttentionType, WindowBuilder};
use wry::{NewWindowResponse, WebContext, WebViewBuilder};

use crate::ffmpeg;
use crate::gui::{self, Running};

/// Icono de la ventana: 64x64 en RGBA crudo (el del .exe va incrustado aparte).
const ICON_RGBA: &[u8] = include_bytes!("../assets/icon-64.rgba");

/// Lo que el servidor le pide a la ventana.
#[derive(Debug, Clone, Copy)]
enum Request {
    Focus,
    Attention,
}

/// Donde la ventana abierta deja como llamarla.
fn instance_file() -> std::path::PathBuf {
    ffmpeg::project_root().join("app.instance")
}

/// Pide a la ventana abierta que venga al frente. Si el fichero es de una
/// ejecucion anterior nadie contesta, y se sigue arrancando.
fn focus_running_instance() -> bool {
    let Some(data) = std::fs::read_to_string(instance_file()).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) else {
        return false;
    };
    let (Some(port), Some(token)) = (data["port"].as_u64(), data["token"].as_str()) else { return false };
    ureq::post(&format!("http://127.0.0.1:{port}/api/focus"))
        .set("X-VS-Token", token)
        .timeout(Duration::from_millis(1500))
        .send_json(serde_json::json!({}))
        .is_ok()
}

/// Una direccion que la interfaz puede cargar dentro de la ventana. Todo lo
/// demas (los enlaces a YouTube, a la documentacion...) se abre en el
/// navegador, no dentro de la aplicacion.
pub fn is_internal(url: &str, port: u16) -> bool {
    url.starts_with(&format!("http://127.0.0.1:{port}/"))
        || url == format!("http://127.0.0.1:{port}")
        || url.starts_with("about:")
        || url.starts_with("blob:")
        || url.starts_with("data:")
}

/// Abre la aplicacion. No vuelve hasta que se cierra la ventana.
pub fn run() -> i32 {
    // Ya abierta (otro doble clic, o un clic en un aviso de Windows): se trae al
    // frente en vez de abrir una segunda, que se pelearia por la carpeta de trabajo.
    if focus_running_instance() {
        return 0;
    }
    let running = match gui::launch(gui::DEFAULT_PORT) {
        Ok(running) => running,
        Err(error) => {
            alert(&format!("No se pudo arrancar Viral Shorts.\n\n{error}"));
            return 2;
        }
    };
    let instance = serde_json::json!({"port": running.port, "token": running.app.token});
    let _ = std::fs::write(instance_file(), instance.to_string());

    // `open_window` solo vuelve si no se pudo abrir la ventana; si se abre, el
    // proceso termina al cerrarla.
    let Err(error) = open_window(&running);
    log::warn!("No se pudo abrir la ventana ({error}); se usa el navegador.");
    alert(
        "No se ha podido abrir la ventana de Viral Shorts (falta WebView2 de Microsoft).\n\n\
         Se abrira en el navegador. Mientras no cierres este aviso, el programa sigue funcionando.",
    );
    gui::open_browser(&running.url);
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn open_window(running: &Running) -> Result<std::convert::Infallible, String> {
    let event_loop = EventLoopBuilder::<Request>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(300));
        if gui::take_focus_request() && proxy.send_event(Request::Focus).is_err() {
            return;
        }
        if gui::take_attention_request() && proxy.send_event(Request::Attention).is_err() {
            return;
        }
    });
    let window = WindowBuilder::new()
        .with_title("Viral Shorts")
        .with_inner_size(LogicalSize::new(1200.0, 860.0))
        .with_min_inner_size(LogicalSize::new(900.0, 640.0))
        .with_window_icon(Icon::from_rgba(ICON_RGBA.to_vec(), 64, 64).ok())
        .build(&event_loop)
        .map_err(|e| e.to_string())?;

    let port = running.port;
    // Los datos del navegador integrado (cookies, cache) van a la carpeta de
    // datos del programa y no junto al ejecutable, donde puede no haber permiso.
    let mut context = WebContext::new(Some(ffmpeg::project_root().join("webview")));
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_url(&running.url)
        .with_navigation_handler(move |url| {
            if is_internal(&url, port) {
                return true;
            }
            gui::open_browser(&url);
            false
        })
        .with_new_window_req_handler(move |url, _| {
            if is_internal(&url, port) {
                return NewWindowResponse::Allow;
            }
            gui::open_browser(&url);
            NewWindowResponse::Deny
        })
        .build(&window)
        .map_err(|e| e.to_string())?;

    let job = running.app.job.clone();
    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        let _vivo = &webview; // la vista vive tanto como el bucle
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                job.stop(); // un trabajo a medias no se queda huerfano
                let _ = std::fs::remove_file(instance_file());
                *flow = ControlFlow::Exit;
            }
            Event::UserEvent(Request::Focus) => {
                window.set_minimized(false);
                window.set_focus();
            }
            // Parpadea en la barra de tareas hasta que se vuelve a la ventana.
            Event::UserEvent(Request::Attention) => window.request_user_attention(Some(UserAttentionType::Critical)),
            _ => {}
        }
    })
}

/// Un aviso en ventana: sin consola no hay otro sitio donde decirlo.
fn alert(message: &str) {
    rfd::MessageDialog::new().set_title("Viral Shorts").set_description(message).set_level(rfd::MessageLevel::Warning).show();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solo_la_propia_interfaz_se_abre_dentro_de_la_ventana() {
        assert!(is_internal("http://127.0.0.1:8760/?t=abc", 8760));
        assert!(is_internal("http://127.0.0.1:8760", 8760));
        assert!(is_internal("about:blank", 8760));
        assert!(is_internal("blob:http://127.0.0.1:8760/uuid", 8760));
        for fuera in ["https://www.youtube.com/watch?v=x", "http://127.0.0.1:9999/", "http://127.0.0.1:87601/", "http://localhost:8760/", "http://127.0.0.1.evil.example:8760/", "file:///C:/Windows/win.ini"] {
            assert!(!is_internal(fuera, 8760), "{fuera}");
        }
    }

    #[test]
    fn el_icono_tiene_el_tamano_que_dice() {
        assert_eq!(ICON_RGBA.len(), 64 * 64 * 4);
        assert!(Icon::from_rgba(ICON_RGBA.to_vec(), 64, 64).is_ok());
    }
}
