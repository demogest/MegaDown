# MegaDown

MegaDown is a Tauri 2 desktop downloader for MEGA public links. It pairs a Rust download engine with a React/Vite interface for managing file and folder downloads from a local desktop app.

## Highlights

- Download MEGA public file links, folder links, and password-protected public links.
- Preview a link before downloading, including item type, name, file count, and total size.
- Manage the queue with pause, resume, cancel, retry, redownload, delete, and clear-finished actions.
- Tune transfer behavior with performance presets, connection count, chunk size, retry mode, overwrite mode, and optional integrity verification.
- Resume interrupted file downloads from `.megadown.part` and `.megadown.json` artifacts when possible.
- Open completed files or their containing folders after validating that the saved path still exists.

## Tech Stack

- Tauri 2 desktop shell
- Rust backend with async download, decryption, retry, resume, and queue control
- React 19 frontend
- Vite 7 build tooling

## Requirements

- Node.js and npm
- Rust 1.77 or newer
- Platform dependencies required by Tauri 2

See the official Tauri setup guide for OS-specific prerequisites: https://tauri.app/start/prerequisites/

## Getting Started

Install JavaScript dependencies:

```bash
npm install
```

Run the desktop app in development mode:

```bash
npm run dev
```

Run only the frontend dev server:

```bash
npm run web:dev
```

Build the frontend:

```bash
npm run web:build
```

Build the desktop installer:

```bash
npm run build
```

## Usage Notes

1. Paste one or more MEGA public links into the download page.
2. Choose an output directory or use the default downloads folder.
3. Adjust performance, retry, and file-handling options if needed.
4. Start the download and manage active or finished tasks from the queue.

MegaDown is intended for public links you are allowed to access and download. Respect the rights of content owners and the terms of the services you use.

## Project Structure

```text
.
|-- src/                  React frontend
|-- src-tauri/            Rust/Tauri backend
|-- src-tauri/icons/      Application icons
|-- src-tauri/capabilities/
|-- package.json          npm scripts and frontend dependencies
|-- vite.config.js        Vite configuration
`-- README.md
```

Generated directories such as `dist/`, `node_modules/`, and `src-tauri/target/` are intentionally ignored.
