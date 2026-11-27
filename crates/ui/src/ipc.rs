//! Calls into the app: Tauri's `invoke`, with arguments and results as serde values.

use serde::Serialize;
use serde::de::DeserializeOwned;
use treasury_core::Error;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch, js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    async fn tauri_invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch, js_namespace = ["window", "__TAURI__", "event"], js_name = listen)]
    async fn tauri_listen(
        event: &str,
        handler: &Closure<dyn FnMut(JsValue)>,
    ) -> Result<JsValue, JsValue>;
}

/// Calls command `cmd` with `args` (an object whose keys are the command's parameters in
/// camelCase) and decodes its result.
pub async fn call<A: Serialize, R: DeserializeOwned>(cmd: &str, args: &A) -> Result<R, Error> {
    let ser = serde_wasm_bindgen::Serializer::json_compatible();
    let args = args
        .serialize(&ser)
        .map_err(|e| Error::internal(format!("arguments for {cmd}: {e}")))?;
    match tauri_invoke(cmd, args).await {
        Ok(v) => serde_wasm_bindgen::from_value(v)
            .map_err(|e| Error::internal(format!("the reply to {cmd}: {e}"))),
        Err(e) => Err(
            serde_wasm_bindgen::from_value::<Error>(e.clone()).unwrap_or_else(|_| {
                Error::internal(e.as_string().unwrap_or_else(|| format!("{cmd} failed")))
            }),
        ),
    }
}

/// Calls a command with no arguments.
pub async fn call0<R: DeserializeOwned>(cmd: &str) -> Result<R, Error> {
    call(cmd, &serde_json::json!({})).await
}

/// Runs `handler` for every `event` the app emits. The listener lives as long as the page.
pub fn listen(event: &'static str, handler: impl FnMut(JsValue) + 'static) {
    let closure = Closure::<dyn FnMut(JsValue)>::new(handler);
    wasm_bindgen_futures::spawn_local(async move {
        let _ = tauri_listen(event, &closure).await;
        // The closure must outlive the listener, which lasts as long as the window.
        closure.forget();
    });
}

/// Asks for a file to open, through the dialog plugin. `None` when the user cancels.
pub async fn pick_file(title: &str) -> Option<String> {
    let args =
        serde_json::json!({ "options": { "title": title, "multiple": false, "directory": false } });
    call::<_, Option<String>>("plugin:dialog|open", &args)
        .await
        .ok()
        .flatten()
}

/// Asks where to save a file.
pub async fn save_file(title: &str, name: &str) -> Option<String> {
    let args = serde_json::json!({ "options": { "title": title, "defaultPath": name } });
    call::<_, Option<String>>("plugin:dialog|save", &args)
        .await
        .ok()
        .flatten()
}

/// Puts `text` on the system clipboard; `false` when the webview refused.
pub async fn to_clipboard(text: &str) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let promise = window.navigator().clipboard().write_text(text);
    wasm_bindgen_futures::JsFuture::from(promise).await.is_ok()
}
