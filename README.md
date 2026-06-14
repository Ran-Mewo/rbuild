# rbuild

Run your normal build command locally, have it compile on a remote machine, and
get the artifacts back as if you'd built them yourself. No manual `ssh`, `scp`,
or `docker` — you just type `cargo build`.

```
cargo build   →   syncs your code to the remote   →   builds in a container
              ←   pulls the binary back to you
```

Your machine stays the source of truth. If the remote is unreachable, builds
fall back to your local toolchain so you're never stuck.

## Install

Linux:
```sh
curl -fsSL https://raw.githubusercontent.com/Ran-Mewo/rbuild/main/install.sh | sh
```

Windows (PowerShell):
```powershell
irm https://raw.githubusercontent.com/Ran-Mewo/rbuild/main/install.ps1 | iex
```

You need an `ssh` client locally, and the remote needs an SSH server plus Docker
(runnable by your SSH user, or via passwordless `sudo` — rbuild figures out
which). Nothing else gets installed on the remote: everything lives in Docker.

## Usage

```sh
rbuild init my-build-host      # your SSH host
rbuild add ~/Code              # everything under here builds remotely
rbuild init-shell bash         # one-time shell hook (bash|zsh|fish|powershell|cmd)
exec $SHELL                    # reload

cd ~/Code/my-project
cargo build                    # builds on the remote, artifact lands locally
```

On first build rbuild pushes its daemon and builds the toolchain image on the
remote automatically. Your edits live-sync in the background, so the remote is
always current.

A Linux client builds for Linux, a Windows client builds for Windows (via Wine —
no VM needed). Force the other way with `rbuild win <cmd>` or
`rbuild linux <cmd>`. Run anything in the build container with `rbuild run <cmd>`
(e.g. `rbuild run pip install -r requirements.txt`).

## Commands

| Command | |
|---------|--|
| `rbuild init <host>` | Set the remote SSH host |
| `rbuild add [path]` | Track a code directory (`--as <name>` to share it across machines) |
| `rbuild download <name> <dir>` | Pull a shared workspace into a new directory |
| `rbuild run <cmd…>` | Run a command in the build container |
| `rbuild win <cmd…>` / `rbuild linux <cmd…>` | Force a build target |
| `rbuild sync [path]` | Sync now (normally automatic) |
| `rbuild roots` | List tracked directories |
| `rbuild uninstall` | Remove rbuild (offers to wipe the remote too) |

## Sharing across machines

A tracked directory's workspace name defaults to its folder name, so two
machines that both `rbuild add ~/Code` (or `D:/Code`) share and merge the same
workspace automatically. Differently-named folders stay separate; use
`--as <name>` to override.

Sync is two-way: concurrent changes from different machines merge in both
directions, deletes propagate everywhere, and if two machines edit the same file
differently nothing is lost — your copy is kept and theirs is saved next to it
as `<file>.rbuild-conflict-<host>`.

## Configuration

One file at `~/.config/rbuild/config.toml` (`%APPDATA%\rbuild` on Windows):

```toml
[[roots]]
path = "/home/you/Code"
name = "Code"            # workspace name; share by matching it on another machine

[remote]
host = "my-build-host"
# identity_file = "~/.ssh/id_ed25519"   # optional; otherwise SSH config decides

[build]
commands = ["cargo", "cmake", "make", "gradle", "mvn", "pip", "npm", "go", "msbuild"]
artifacts = ["target/**"]   # what gets synced back after a build
```

Files ignored by `.gitignore` / `.rbuildignore` aren't synced.

## How it works

Three crates: `rbuild` (the client), `rbuildd` (the daemon, run on the remote in
a container), and `rbuild-proto` (shared protocol). The client talks to the
daemon over your existing SSH connection — no ports opened, no forwarding. On
the remote, code and build caches live in Docker volumes and builds run in
containers, so nothing touches the host filesystem and `rbuild uninstall` leaves
it clean.

## License

MIT — see [LICENSE](LICENSE).
