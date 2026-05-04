# MegaDown

MegaDown is a Rust + Slint desktop downloader for MEGA public links. It keeps the Rust download engine in-process and uses a native Slint interface instead of a Tauri WebView.

## Highlights

- Download MEGA public file links, folder links, and password-protected public links.
- Preview a link before downloading, including item type, name, file count, and total size.
- Manage the queue with pause, resume, cancel, retry, delete, clear-finished, and open-file/folder actions.
- Tune transfer behavior with performance presets, connection count, chunk size, retry mode, overwrite mode, and optional integrity verification.
- Resume interrupted file downloads from `.megadown.part` and `.megadown.json` artifacts when possible.

## Tech Stack

- Rust download engine
- Slint native desktop UI
- Tokio async runtime

## Requirements

- Rust 1.77 or newer
- Platform toolchain required by Slint and `rfd` native dialogs

## Getting Started

Run the desktop app:

```bash
cargo run
```

Run tests:

```bash
cargo test
```

Build a release binary:

```bash
cargo build --release
```

## Project Structure

```text
.
|-- src/              Rust app entry point and download engine
|-- ui/               Slint UI markup
|-- assets/icons/     Application icon assets
|-- Cargo.toml        Rust package manifest
|-- build.rs          Slint UI compilation
`-- README.md
```

MegaDown is intended for public links you are allowed to access and download. Respect the rights of content owners and the terms of the services you use.
