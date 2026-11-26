# 0011: The interface is written in Rust with Leptos, inside Tauri

## Context

The first plan used SolidJS and TypeScript in Tauri's webview, with TypeScript bindings generated
from the Rust types (0002). The owner asked for the interface in Rust as well. The interface has to
look precise, stay light on memory, build for Windows, macOS, Linux, Android and iOS, handle
right-to-left text such as Persian, work with screen readers, and leave room for notifications, a
tray, biometric unlock and automatic updates later.

Native Rust toolkits (Iced, Slint, egui) use less memory than a webview, but their right-to-left
text, accessibility and mobile support are behind the platforms' webviews.

## Decision

The interface is a Leptos application compiled to WebAssembly and shown in Tauri's webview, which
is the operating system's own (WebKitGTK, WKWebView, WebView2), not a bundled browser. Leptos uses
fine-grained signals like SolidJS. The types the app and the interface exchange live in one Rust
crate used by both, so they cannot drift; 0002 is superseded. The SQL editor is written in Rust on
Treasury's own SQL lexer, so highlighting follows the same dialect rules as the guardrails.

## Consequences

One language from the drivers to the buttons. The webview's memory is the operating system's and
is reported separately from the core's. The editor starts with what a database client needs most
(highlighting, running the statement at the cursor, errors marked where the server points, schema
completion) and grows from there.
