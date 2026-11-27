//! The Treasury interface. It shows what the app's runtime decides and sends the user's actions
//! back; every rule (guardrails, formatting, parsing) runs in Rust, most of it on the app side.

#![forbid(unsafe_code)]

mod app;
mod complete;
mod dialogs;
mod editor;
mod grid;
mod history;
mod home;
mod ipc;
mod sidebar;
mod state;
mod tabs;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(app::App);
}
