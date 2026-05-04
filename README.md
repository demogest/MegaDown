# MegaDown

MegaDown is a Tauri 2 desktop downloader for MEGA public links. It uses a Rust backend for download work and a React/Vite frontend for the management interface.

## Features

- Add one or more MEGA links from the download page.
- Manage tasks with pause, resume, cancel, retry, redownload, and task cleanup actions.
- Open completed files or their containing folders after verifying the saved path still exists.
- Configure performance mode, connections, chunk size, integrity checks, and retry behavior.

## Development

Install dependencies:

```bash
npm install
```

Run the frontend build:

```bash
npm run web:build
```

Build the desktop app:

```bash
npm run build
```

## Project Structure

- `src/` - React frontend.
- `src-tauri/` - Rust/Tauri backend.
- `dist/` - generated frontend build output.
