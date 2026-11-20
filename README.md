# Switchboard

A Windows desktop library for isolated Microsoft Edge profiles, built with Rust and Slint. Each profile has its own cookies, extensions, sign-ins, and browser settings. This separates Edge data; it is **not** a Windows security sandbox.

## Run on Windows

Use Windows 10 or 11 with Microsoft Edge (stable). To build from source, install stable Rust with the MSVC toolchain and the Visual Studio **Desktop development with C++** workload.

```powershell
cargo build --release --locked
.\target\release\switchboard.exe
```

The UI and icon artwork are embedded. You can copy `switchboard.exe` to a permanent folder before creating shortcuts; the shortcuts point to that executable.

## Use the library

- **New profile:** enter a name, optionally enter a collection (for example, *Customers*), and select a symbol and color. The searchable catalog includes 100 symbols in Pine, Iris, Clay, Slate, Teal, and Olive. **Edit** changes an existing profile without losing its browser data.
- **Find and open:** search by profile or collection, sort by recent use or name, or filter to running profiles. Enter in the search box opens the first match. Pin favorites for quick access; the library shows the first three pinned profiles in the current sort order.
- **Create shortcuts:** select profiles using their checkboxes, Ctrl-click, or Shift-click. Choose **Create shortcut…**, then **Browse…** to any writable folder (including a new folder on your desktop). The exported `.lnk` files use the profile's icon and stable ID. A duplicate filename gets a numbered suffix; existing files are never replaced. Windows may cache old icon artwork until Explorer refreshes.
- **Trash:** hides a closed profile without deleting its data and removes the shortcuts Switchboard created for it (only links that still open that profile; moved links are not tracked and offer to restore the profile when opened). **Restore** keeps the data and recreates those shortcuts at their original paths when the folder still exists and the name is free. Use **Delete forever** for one trashed profile or **Empty Trash…** for all trashed profiles and leftover temporary sessions; both permanently erase the browser data, icon, and tracked shortcuts.
- **Start temporary:** launches a fresh disposable profile. You can close the library while browsing. A separate watcher removes only that session's folder once all its Edge processes have exited. Live temporary sessions appear under **Running** (Switch to, or **End session** to close and erase them); failed cleanup appears in **Trash** for retry.
- **End session:** requests a graceful close, then enforces 5-second limits for graceful exit, process-group termination, and releasing file locks. If necessary, Restart Manager identifies processes holding files in that session's data folder; Switchboard terminates only verified `msedge.exe` processes matching those lock records, never other applications or Edge processes that do not hold those files. Deletion retries with backoff for up to 30 seconds. A failed operation stays visible on its row with **Retry** and **Details**; unfinished cleanup is retained in a marked folder so startup recovery can try again.
- **Progress and errors:** catalog and browser actions run on a serialized background worker. Rows show their current cleanup phase and disable the action button while it runs; failures remain in a red banner until dismissed. Open **Activity** for the last 30 events in this run, newest first in local time. Search action names or details and combine the search with **Progress**, **Completed**, or **Failed**. New matching events appear while the panel is open. The JSON-lines log at `%LOCALAPPDATA%\EdgeProfileSwitcher\switchboard.log` persists across restarts.

## Local data and lifecycle

Switchboard stores its catalog in `%LOCALAPPDATA%\EdgeProfileSwitcher\library.json`. Persistent browser data lives in `Profiles\<id>`, shortcut artwork in `Icons\<id>.ico`, and temporary sessions in `Temporary\<id>`. A failed temporary cleanup is preserved under a marked `.cleanup-<id>` folder and retried at startup. A rename never moves a browser data directory. Switchboard doesn't import or alter your normal Edge profile.

Edge launches with a dedicated `--user-data-dir` and a Windows Job Object assigned at process creation. Temporary sessions use kill-on-job-close so an unexpectedly terminated watcher doesn't leave a browser writing into an untracked temporary directory. On startup Switchboard retries cleanup of marked, inactive temporary sessions. Changes to the catalog are serialized across processes and saved by atomic replacement.

**Command line:** `switchboard.exe --open <profile-id>` opens or switches to a saved profile; `switchboard.exe --temporary` opens a disposable session. Neither requires the library window to be open.

## Development

`cargo nextest run --locked --all-targets` runs the Windows test suite with per-test hang timeouts; GitHub Actions runs the same command on `windows-latest`. The tests cover library persistence, icon rendering, native shortcut files, job tracking, fake-clock cleanup deadlines, UI callback responsiveness, and an isolated helper process that holds a file lock (no Edge required). `cargo test --locked real_edge_session -- --ignored` additionally opens a disposable Edge instance and checks cleanup. It does not target any existing browser.

The original symbol outlines live in `assets\symbols.tsv` (one name and SVG path per row). Keep the order stable: saved profiles reference the catalog index. The UI preview and multi-resolution Windows `.ico` renderer share these outlines and the same palette.