# MiSTer Cloud Saves

A utility to sync MiSTer FPGA save files with a cloud server.

## Features

- Sync save files to/from a cloud server
- Support for multiple MiSTer devices syncing to the same server
- Syncs saves, save states and arcade NVRAM files
- Uses inotify and file hashing to detect changes
- Compression to reduce bandwidth usage
- Core agnostic - works with any MiSTer core that uses save files
- Conflict resolution for multiple devices

## Installation

Grab the [`cloud_saves.sh`](https://github.com/bleach86/mister_cloud_saves/blob/main/scripts/cloud_saves.sh) file from the `scripts` directory in this repository and place it in the `Scripts` folder of your MiSTer FPGA's SD card.

Make sure to make a backup of your saves and savestates directories before proceeding!

Boot up your MiSTer. From the MiSTer menu:

1. Press the **back** action (Esc key on a keyboard).
2. Select the **Scripts** option.
3. Choose **Yes** to allow running scripts.
4. Select the `cloud_saves` script to run it.

This will automatically begin the installation and initial sync process.

- **Using the provided cloud server:** Simply press **Enter** when prompted for the server URL.
- **Hosting your own server:** Enter the full URL to your server when prompted.

Examples:
`https://example.com`
`http://192.168.1.45:8000`

> Your server must be running and reachable from the MiSTer for the sync to work.

The script will now begin syncing with the server. Once complete, the MiSTer will reboot automatically.

- **Single MiSTer device:** You’re done! The `mister_save_client` will automatically run in the background on boot and sync your saves.
- **Multiple MiSTer devices:** There are a couple of additional steps to complete (see below).

### Multiple MiSTer Devices

During the initial setup on the first MiSTer, an `.ini` file named `cloud_saves.ini` is created in the root of the SD card.  
Copy this file to the root of the SD card on each additional MiSTer device you want to sync with the same cloud server.

Each device will also need the  
[`cloud_saves.sh`](https://github.com/bleach86/mister_cloud_saves/blob/main/scripts/cloud_saves.sh) script placed in the `Scripts` folder.

Once copied, run the **cloud_saves** script from the MiSTer menu on each additional device. The script will detect the existing `cloud_saves.ini` file and automatically use the same server URL and user ID as the first device.

During the initial sync from the second device onward, if a save file already exists on the server but has a different hash than the local file, a conflict will be detected. You will be prompted to choose which version of the file to keep.

You can choose to:

- Keep the **local** version
- Keep the **server** version

If you choose to keep the local version, it will be uploaded to the server and overwrite the existing server file. This updated version will then be synced to all other devices during their next sync.

You will also have the option to apply the same choice to all remaining conflicts.

> ⚠️ **Important:** If you play the same game on multiple MiSTer devices at the same time, save data could be **overwritten**. Always make sure only one device is actively playing or saving a game at a time to avoid conflicts.

## Usage

After the initial setup, the `mister_save_client` will automatically run in the background on MiSTer at boot and sync your save files with the cloud server.

## Logs and Troubleshooting

The client writes a log of everything it does to:

```
/media/fat/cloud_saves/cloud_saves.log
```

The log rotates to `cloud_saves.log.1` at 4 MiB, so it uses at most 8 MiB of
SD card space.

Every decision is recorded with the `xxh3` content hash of the file before and
after the change, along with the reason the change was made. For example, a
machine picking up a save made on another machine logs:

```
INFO  client: GameSave GBA/zelda.sav: content differs - local hash 889ba0dad4e3b6ec (modified_index 1) vs server hash 498b9240a4677916 (modified_index 2); server copy is newer
INFO  client: downloading GBA/zelda.sav: replacing /media/fat/saves/GBA/zelda.sav [on disk: 31 bytes, mtime 2026-10-05T05:05:06Z, hash 889ba0dad4e3b6ec] with server copy [expected hash 498b9240a4677916, modified_index 2]; reason: server modified_index 2 >= local 1
INFO  client: GBA/zelda.sav: wrote /media/fat/saves/GBA/zelda.sav - hash 889ba0dad4e3b6ec -> 498b9240a4677916 (41 bytes, mtime 2026-10-05T05:05:06Z), now at modified_index 2
```

Lines worth searching for when a save does not propagate as expected:

| Search for | Meaning |
| --- | --- |
| `content changed while the client was not watching` | A file changed on disk between runs, so this machine's `modified_index` was bumped and it now claims the newest copy. |
| `no content change` | The file was written but the bytes are identical, so nothing claims to be newer. |
| `DIVERGENT` | Both copies changed independently and sit at the same `modified_index`. The index cannot break the tie, the server copy wins, and the local changes are lost. |
| `does not match the save map's hash` | Something changed a save without the client noticing, and those changes are about to be overwritten. |
| `verification FAILED` | A file did not contain what was just written to it. |
| `REJECTED` (server) / `not a continuation` (client) | A write was refused because it wasn't based on the server's current data for that save - see below. |

The server logs the same information for every request, including what each
upload replaces.

### Conflict protection for a machine that never synced

If a machine writes a save without ever having pulled down what the server
currently holds for it - most commonly because an earlier sync failed (see
Network resilience below) and the user then played anyway, not realizing the
save they loaded wasn't the latest one - that write is **not** a continuation
of the server's data, even if it happens to be the only local history that
machine knows about. Uploading it anyway would silently destroy whatever the
other machine had already synced.

The server refuses this: an upload is only accepted if its `modified_index`
is strictly newer than what the server already has, whenever the content
differs. A machine with no local record of a save computes its own index
starting from zero, same as the very first machine to ever upload it, so a
`REJECTED` response is a clear signal that this content is an unrelated
branch, not stale data to be overwritten with.

When the client's live file watcher gets this rejection, it does not retry
or force the write - the server's existing save is left alone. It re-fetches
the server's current state and aligns its own bookkeeping to it (keeping the
file that's actually on disk, but matching the server's version number), so
the two copies show up as a proper, honest conflict at the next sync instead
of one silently overwriting the other. In the normal, non-interactive daemon
mode that next sync then prefers the server's continuity automatically; the
same conflict surfaces an explicit local/remote choice when the client is
run interactively (see Multiple MiSTer Devices above) - and that interactive
choice to keep local, when made deliberately, is the one case where
overwriting the server's history is intended.

This only protects against a machine that never saw the current save. It
does not and cannot resolve two machines actively writing to the same save
at the same time - as noted above, running the same game on two devices at
once isn't something the tool can safely referee.

Logging is controlled by environment variables on both the client and server:

| Variable | Default | Purpose |
| --- | --- | --- |
| `MISTER_SAVE_LOG_LEVEL` | `info` | `error`, `warn`, `info` or `debug`. `debug` adds every raw filesystem event. |
| `MISTER_SAVE_LOG_FILE` | client: `/media/fat/cloud_saves/cloud_saves.log`, server: unset | Log file path. Set it to an empty string to log only to stdout. |
| `MISTER_SAVE_LOG_MAX_BYTES` | `4194304` | Rotation threshold in bytes. `0` disables rotation. |

The server logs to stdout by default, so under Docker use `docker logs
mister-saves-container`.

### Boot and supervision

MiSTer has no systemd (or any other service manager), so the client is
launched by its own small supervisor, started once from
`/media/fat/linux/user-startup.sh` at boot. The supervisor:

- Waits for each save directory and for `/tmp/CORENAME` to exist before
  watching it, since a core may not have loaded yet in the first seconds
  after boot. If a watch is lost later for any reason, it's retried with
  capped exponential backoff instead of staying down for the rest of the
  session - this is logged to `cloud_saves.log` as `watch target ... does
  not exist yet` / `... appeared after ...`.
- Restarts the client itself if it ever exits unexpectedly (crash, unhandled
  error), with the same kind of backoff. Supervisor activity - starts,
  crashes, restarts - is logged separately to:

  ```
  /media/fat/cloud_saves/launcher.log
  ```

  since the client's own log only covers the client's lifetime, not whether
  something had to restart it.

Stopping the client (via the `cloud_saves` script's update/uninstall/change
server flows) stops the supervisor first, so it doesn't simply relaunch the
client that was just asked to stop.

### Network resilience

MiSTer's wifi commonly takes the better part of a minute to associate after
boot, and can drop briefly at any point. The client handles this at a few
levels:

- At startup, it waits for the server's `/health` endpoint to respond before
  doing anything else - this loop is unbounded (logged every 30s) since
  there's no good alternative to waiting for the network to actually be up.
- The sync that follows (both at startup and on each return to MENU) retries
  on failure with capped exponential backoff - about a minute's worth of
  attempts at startup, since the network can still be settling down for a
  moment even after that first health check succeeds; a shorter window on
  return to MENU, since the next visit or next local save will try again
  regardless.
- Every HTTP request has a bounded timeout (10s to connect, 60s overall), so
  a connection that neither completes nor fails outright - common with a
  flaky link - fails loudly and gets retried instead of hanging the
  responsible watcher indefinitely.

One thing this deliberately does **not** do is retry a failed sync while a
core is actively running. A full sync can download and overwrite local save
files, and doing that automatically while a game is mid-session is exactly
the kind of surprise overwrite this tool tries hard to avoid elsewhere (see
the conflict and hash logging above) - so if the startup sync exhausts its
retries, or a session never returns to MENU, newly-downloaded saves from
another machine won't appear until the next MENU visit. That trade-off is
intentional.

## Updating

Mister Cloud Saves is updated using the `update` or `update_all` script from the MiSTer Scripts menu.

An update can also be preformed by running the `cloud_saves` script again from the MiSTer menu. When prompted, choose the update option. The script will download and install the latest version of the client and perform a sync.

## Uninstallation

To uninstall the `mister_save_client`, run the `cloud_saves` script from the MiSTer menu and choose the uninstall option. This will remove the client and all associated files from your MiSTer SD card.

# Running Your Own Server

## Precompiled Binaries

Precompiled binaries for the server can be found in the [Releases](https://github.com/bleach86/mister_cloud_saves/releases) section of this repository.

## Docker

You can run the server using Docker. A Docker image is available on GitHub Container Registry.

To run the server using Docker, use the following command:

```bash
docker run -d --name mister-saves-container \
           -p 8000:8000 \
           -v mister_saves:/app/user_saves \
           -v mister_sled:/app/user_saves_sled \
           ghcr.io/bleach86/mister-cloud-saves:latest
```

This command will:

- Map port `8000` on the host to port `8000` in the container
- Create and mount a Docker volume named `mister_saves` to persist user save files
- Create and mount a Docker volume named `mister_sled` to persist the Sled database

To specify a different port, change the `-p` option. For example, to use port `8080` on the host:

```bash
-p 8080:8000
```

To specify custom directories on the host for saves and the Sled database, replace the `-v` options with paths to your desired directories. For example:

```bash
-v /path/to/your/saves:/app/user_saves \
-v /path/to/your/sled_db:/app/user_saves_sled \
```

To stop and remove the container, use the following commands:

```bash
docker stop mister-saves-container
docker rm mister-saves-container
```

To stay updated with the latest image, you can pull the latest version from GitHub Container Registry:

```bash
docker pull ghcr.io/bleach86/mister-cloud-saves:latest
```

Or automate the update process using a tool like [Watchtower](https://containrrr.dev/watchtower/).

```bash
docker run -d \
  --name watchtower \
  -v /var/run/docker.sock:/var/run/docker.sock \
  containrrr/watchtower \
  --cleanup \
  mister-saves-container
```

## Compiling the Server

This project requires Rust and Cargo to build. You can find installation instructions for Rust [here](https://www.rust-lang.org/tools/install).
This project also requires C toolchain for building some dependencies. Make sure you have a C compiler installed (e.g., `gcc` or `clang`).

```bash
sudo apt install build-essential pkg-config libssl-dev perl make libipc-run-perl # For Debian/Ubuntu
sudo dnf install @development-tools pkgconfig openssl-devel perl-core perl-ExtUtils-MakeMaker perl-IPC-Cmd # For Fedora
```

1. Clone the repository:

   ```bash
   git clone https://github.com/bleach86/mister_cloud_saves.git
   cd mister_cloud_saves
   ```

2. Build the server:

   ```bash
   cargo build --release --bin=mister_save_server --features=server
   ```

3. Run the server:

   ```bash
    ./target/release/mister_save_server
   ```

   or

   ```bash
    cargo run --release --bin=mister_save_server --features=server
   ```

## Compiling the Client

This project requires Rust and Cargo to build. You can find installation instructions for Rust [here](https://www.rust-lang.org/tools/install).

The client is intended to run on the MiSTer FPGA platform, which uses an ARMv7 architecture. To compile the client for MiSTer, you will need to set up a cross-compilation environment.

It is recommended to use the `cross` tool for cross-compiling Rust projects. You can find installation instructions for `cross` [here](https://github.com/cross-rs/cross).

1. Clone the repository:

   ```bash
   git clone https://github.com/bleach86/mister_cloud_saves.git
   cd mister_cloud_saves
   ```

2. Build the client for ARMv7 architecture:

   ```bash
   cross build --target=armv7-unknown-linux-gnueabihf --release --bin=mister_save_client
   ```

3. The compiled binary will be located at:

   ```bash
   ./target/armv7-unknown-linux-gnueabihf/release/mister_save_client
   ```

## License

This project is licensed under the GPLv3 License. See the [LICENSE](LICENSE) file for details.
