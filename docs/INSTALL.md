# Installing Gather

Gather is one download. The installer contains the app, the Gather daemon, and a private
PostgreSQL database with pgvector, so there is nothing else to install and nothing to
configure. Everything runs on your machine; nothing is sent anywhere.

- [Download](#download)
- [System requirements](#system-requirements)
- [First launch](#first-launch) (Windows, macOS, Linux)
- [Using Gather](#using-gather)
- [Checking your download](#checking-your-download)
- [What runs, and where your data lives](#what-runs-and-where-your-data-lives)
- [Updates](#updates)
- [Troubleshooting](#troubleshooting)
- [Uninstalling](#uninstalling)

## Download

Get the installer for your system from the
[releases page](https://github.com/joeydd032995-pixel/Gather/releases):

| System | File |
|---|---|
| Windows 10/11 (64-bit) | `Gather_<version>_x64-setup.exe` (or the `.msi`) |
| macOS 11+ on Apple silicon (M1 and later) | `Gather_<version>_aarch64.dmg` |
| Linux (x86-64) | `Gather_<version>_amd64.AppImage`, `.deb` or `.rpm` |

Optional, for richer extraction: [Ollama](https://ollama.com) for local AI models, and
[Tesseract](https://tesseract-ocr.github.io) for text in images. Both run locally too; see
[CONFIGURATION.md](CONFIGURATION.md).

## System requirements

| | Minimum | Recommended |
|---|---|---|
| Memory (RAM) | 4 GB | 8 GB or more |
| Processor | 2 cores, 64-bit | 4 cores |
| Disk | 1 GB for Gather, plus room for your data | SSD |

On a computer with less than 6 GB of RAM, Gather turns on **low-memory mode** by itself.
Settings shows which mode is in use. In low-memory mode:

- The database and the daemon use smaller buffers and fewer connections. While
  Gather is busy with a large file, the two together peak at about 300 MB. Measured on
  Linux, the daemon's peak is about 90 MB and the database's about 195 MB (it idles near
  100 MB). The app window adds what the system web view uses, typically 150–250 MB.
- Each file can be up to 32 MB. Storing a file briefly takes several times its size in
  memory, so larger files are refused with a clear message rather than slowing the
  computer down.
- If you use Ollama, Gather uses only its small embedding model (for search and matching
  names), one request at a time, and lets Ollama unload it a minute after the work is done.
  Chat models need well over 1 GB of memory, which a 4 GB computer doesn't have to spare
  next to the operating system. On 4 GB, also start Ollama with `OLLAMA_MAX_LOADED_MODELS=1`
  and `OLLAMA_NUM_PARALLEL=1`.

Everything else works the same. To choose the mode yourself, start Gather with
`GATHER_MEMORY_PROFILE` set to `low` or `standard` ([CONFIGURATION.md](CONFIGURATION.md)).

## First launch

The installers are not yet code-signed. That's why Windows and macOS show a warning the first
time you open Gather. It isn't a sign that anything is wrong: code signing is a paid
certificate that proves who published a program, and the project hasn't bought one yet. To
confirm your download is the published one, [check its checksum](#checking-your-download)
first.

### Windows

1. Run `Gather_<version>_x64-setup.exe`.
2. If **"Windows protected your PC"** (SmartScreen) appears, click **More info**, then
   **Run anyway**.
3. Open Gather from the Start menu.

### macOS

1. Open the `.dmg` and drag **Gather** to **Applications**.
2. Open Gather. macOS says it **"cannot be opened because Apple cannot check it for malicious
   software"** (or that it "is damaged", on some versions). Click **Done** / **Cancel**.
3. Open **System Settings → Privacy & Security**, scroll to the message about Gather, and click
   **Open Anyway**. Confirm with your password.

If there's no **Open Anyway** button, run this once in Terminal, then open Gather normally:

```bash
xattr -dr com.apple.quarantine /Applications/Gather.app
```

### Linux

- **AppImage:** `chmod +x Gather_*.AppImage` and run it.
- **Debian/Ubuntu:** `sudo apt install ./Gather_*_amd64.deb`
- **Fedora/openSUSE:** `sudo dnf install ./Gather-*.x86_64.rpm`

Gather keeps its API token in your desktop keyring (GNOME Keyring or KWallet). On a desktop
without one, start Gather with `GATHER_AUTH_MODE=env GATHER_API_TOKEN=<a long random string>`;
see [CONFIGURATION.md](CONFIGURATION.md).

### What the first launch does

The first start takes a little longer. Gather creates your private database (you'll see
"Setting up your private database"), then starts. Later launches take a few seconds.

## Using Gather

1. **Upload** files on the Upload tab: PDFs, Markdown, text files, photos or screenshots.
   Gather stores each one and reads it in the background.
2. **Library** lists everything you've added. Pick a file to see what Gather found in it
   (facts, decisions, preferences, events) and to read its text. The search box searches
   all of it.
3. **Graph** shows how things connect: people, places, tools and ideas as dots, joined when a
   statement links them, and each file joined to what it mentions. Click a dot for details;
   drag, scroll and "Find in graph" to explore.

The other tabs fill in only when Gather has something for you to decide:

| Tab | Shows | Checked about every |
|---|---|---|
| Review | Things Gather wasn't sure about and would like you to confirm | 10 minutes |
| Contradictions | Two statements that disagree, e.g. two different amounts for the same bill | 10 minutes |
| Entities | Two names that may be the same thing | 15 minutes |
| Groups | Related items gathered into topics | 15 minutes |
| Photos | Duplicate photos, and albums from photos taken close together | 10 minutes |

On its own, Gather picks up clear statements such as "I prefer…", "We decided to use…",
"I work at…", "Our rent is $1,200 per month" or "On 2026-03-01, …". Everything else in a file
is still stored and searchable. A local AI chat model (Ollama, see
[CONFIGURATION.md](CONFIGURATION.md)) makes it find far more, but needs more memory than a
4 GB computer has to spare.

## Checking your download

Every release publishes `SHA256SUMS` with a checksum for each file. Compare yours:

```bash
# macOS / Linux
shasum -a 256 Gather_*        # or: sha256sum Gather_*
```

```powershell
# Windows (PowerShell)
Get-FileHash .\Gather_*-setup.exe -Algorithm SHA256
```

The value must match the line for that file in `SHA256SUMS`.

## What runs, and where your data lives

While Gather is open it runs two background processes, and stops both when you quit:

- **PostgreSQL 16 with pgvector**: your database. It listens on `127.0.0.1:7603` only (no
  network access, no local socket) and requires a password generated on first launch.
- **gather-daemon**: extraction, search and organizing. It listens on `127.0.0.1:7601`
  (REST) and `127.0.0.1:7602` (gRPC), loopback only.

Your data is in Gather's app-data folder:

| System | Folder |
|---|---|
| Windows | `%APPDATA%\dev.gather.desktop\` |
| macOS | `~/Library/Application Support/dev.gather.desktop/` |
| Linux | `~/.local/share/dev.gather.desktop/` |

Inside: `pgdata/` (the database), `logs/` (`postgres.log`, `daemon.log`) and `db-password`
(readable only by you). To back up, use the export (`GET /api/v1/export`) or the scheduled
backups in [BACKUP-RUNBOOK.md](BACKUP-RUNBOOK.md).

## Updates

Gather never checks for updates on its own. In **Settings** you can:

- press **Check now**, or
- turn on **Check for updates when Gather starts**.

A check is one request to this project's GitHub release page. It sends nothing about you or
your data, but like any web request it reveals your IP address to GitHub. Builds signed with the
project's update key can install the update for you: it's downloaded, verified against a key
built into the app, installed, and Gather restarts. Other builds point you to the releases page.

Your data stays in place across updates. If a future version bundles a newer major version of
PostgreSQL, Gather refuses to start on the old data rather than risk it, and the release notes
explain the upgrade step.

## Troubleshooting

- **"did not become ready in time" or "stopped during start-up":** look at `logs/daemon.log`
  in the app-data folder above.
- **"starting the database failed" / port in use:** something else is using port 7603. Quit
  it, or start Gather with `GATHER_PG_PORT=<free port>`.
- **"The database folder … is incomplete or damaged":** Gather never repairs or deletes an
  existing database on its own. Restore the folder from a backup, or quit, rename `pgdata`
  to keep it, and relaunch to start with an empty database.
- **You already run your own Gather daemon** (e.g. `docker compose up`): the app notices a
  daemon on `127.0.0.1:7601` and uses it instead of starting its own.
- **Reset everything:** quit Gather and delete the app-data folder. This deletes your data;
  export first.

## Uninstalling

- **Windows:** Settings → Apps → Gather → Uninstall.
- **macOS:** drag Gather from Applications to the Trash.
- **Linux:** delete the AppImage, or `sudo apt remove gather` / `sudo dnf remove gather`.

Uninstalling leaves your data in place. Delete the app-data folder to remove it too.
