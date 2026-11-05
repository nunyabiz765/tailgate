# Tailgate

A native Linux desktop app for [Tailscale](https://tailscale.com) and its Mullvad exit nodes: see where your traffic is going, pick an exit node from a searchable list, and turn things on and off without the command line.

Free and open source. Written in Rust with [Slint](https://slint.dev). It wraps the `tailscale` CLI you already have.

> **Early release (0.1).** Status, devices and serving have been read from a real daemon. Changing anything on a real daemon, including switching exit nodes, had not been exercised when this was released. See [Known limitations](#known-limitations). Start with `tailgate --read-only`.

![Tailgate showing the exit node list](docs/screenshots/exit-nodes.png)

| | |
|---|---|
| ![Dark theme](docs/screenshots/dark.png) | ![Turn off dialog listing what goes down](docs/screenshots/turn-off-dialog.png) |
| ![Serving view](docs/screenshots/serving.png) | ![Devices view](docs/screenshots/devices.png) |

The screenshots use the bundled fake CLI and made-up data, which is why the banner at the top names a test binary. You won't see that banner in normal use.

## What it does

- **Status:** up or down, machine name, tailnet IP, and the current exit node with city and country. Polls `tailscale status --json` and `debug prefs` every 4 seconds on a worker thread, so a slow CLI can't freeze the window.
- **Exit nodes:** one row per city by default, highest priority first, or every server with a toggle. Search by city, country or hostname, accents optional. One click to switch. Favorites, plus a section for non-Mullvad exit nodes on your tailnet.
- **Mullvad off and on:** off warns that sites will see your own IP, then runs `tailscale set --exit-node=`. On restores the last-used node.
- **Allow LAN access:** `tailscale set --exit-node-allow-lan-access=true|false`.
- **Tailscale up and down:** down asks first, and lists what it would cut from the live `tailscale serve status`.
- **Devices** and a read-only **Serving** view.
- **Check my IP:** asks `https://am.i.mullvad.net/json` through curl, only when you click it. That's the only network request Tailgate makes itself.
- **Light and dark** themes, and a layout that adapts to narrow windows.

## Requirements

- Linux on x86_64 with Wayland or X11, for the prebuilt binary
- Tailscale installed and running, with the `tailscale` command on your `PATH`
- Mullvad's add-on on your tailnet, for Mullvad exit nodes. Other exit nodes work without it
- `pkexec` (polkit) for the permission prompt, `curl` for Check my IP, `xdg-open` for links
- fontconfig and freetype, which most desktops already have

## Install

Download the tarball from the [latest release](https://github.com/nunyabiz765/tailgate/releases/latest), check it, and run the installer:

```sh
sha256sum -c tailgate-0.1.0-x86_64-linux.tar.gz.sha256
tar xf tailgate-0.1.0-x86_64-linux.tar.gz
cd tailgate-0.1.0-x86_64-linux
./install.sh
tailgate --read-only
```

Or build from source, which needs a current Rust toolchain plus the fontconfig and freetype development packages:

```sh
git clone https://github.com/nunyabiz765/tailgate
cd tailgate
./install.sh
```

The installer puts the binary in `~/.local/bin/tailgate`, with a desktop entry and icons under `~/.local/share`. It touches nothing outside your home directory.

To uninstall:

```sh
rm ~/.local/bin/tailgate ~/.local/share/applications/tailgate.desktop
rm ~/.local/share/icons/hicolor/*/apps/tailgate.*
rm -r ~/.config/tailgate
```

## Read-only mode

```sh
tailgate --read-only
```

This shows live status and refuses every change before anything is run.

## Permissions

Without an operator set, Tailscale refuses changes from a normal user ("Access denied"). The first time that happens, Tailgate offers two choices:

- **Let Tailgate control Tailscale** runs `pkexec tailscale set --operator=<your username>` once, with your password, then retries what you asked for. The username comes from the system's password database for the account running the app. From then on your account can change Tailscale settings without a password, from Tailgate or from the plain `tailscale` command.
- **Just this once** runs only that one change through pkexec. Nothing is remembered.

Tailgate never stores a password and never uses sudo.

**To undo the operator setting:**

```sh
sudo tailscale set --operator=
```

## Settings

Tailgate keeps favorites, the last-used node and view settings in `~/.config/tailgate/state.json`. There are no secrets in it.

Served ports show as "Port 8443". To give them names, create `~/.config/tailgate/labels.json`. Tailgate only ever reads this file. Ports with the same name are grouped together in the turn-off dialog.

```json
{ "ports": { "8443": "Dashboard", "465": "Mail", "993": "Mail" } }
```

## Known limitations

As of 0.1.0:

- **No change has been made through Tailgate against a real Tailscale daemon.** That covers switching exit nodes, Mullvad on and off, LAN access, up and down, the operator grant, and the real pkexec prompt.
- What has been tested:
  - The automated test suite.
  - A fake `tailscale` CLI driving the real window, with every control clicked and the exact arguments recorded.
  - Read-only use against a real daemon (Tailscale v1.102.5): status, devices, serving, and Check my IP.
- The only platform tested is x86_64 Arch Linux on KDE Plasma (Wayland). The prebuilt binary is compiled on Debian 12 to reach more systems, but other distributions and desktops haven't been tried.
- The commands were chosen by reading the Tailscale CLI source and help (see below), which isn't the same as seeing them work. Please try `--read-only` first, and open an issue if something misbehaves.

## How the commands were chosen

Checked against the Tailscale v1.102.5 CLI source and `--help`:

- **Bare `tailscale up`** takes no flags on purpose. Its help says it brings the network online "without changing any settings", and the accidental-revert check only applies when flags are passed.
- **`tailscale set`** only changes the flags you pass (`setFlagSet.Visit`), so nothing else gets reset.
- **`--exit-node=<IP>`** uses the node's Tailscale IP, which the CLI resolves and validates against the netmap. That avoids any hostname matching.
- **`--exit-node-allow-lan-access`** has no dependency on `--exit-node` under `set`. That restriction is only in `up`.
- **`tailscale down`** can ask for confirmation when run over Tailscale SSH. Tailgate has no stdin to answer with, so in that case it shows the CLI's error rather than going ahead.

## Testing without touching the daemon

All process plumbing goes through `src/cli.rs`. Reads are limited to a fixed list, writes to a fixed set of actions, and read-only mode refuses a change before anything is spawned.

`tests/fake-tailscale` stands in for the CLI: it records each argv as JSON, keeps fake state, and can misbehave. `tests/fake-pkexec` does the same for pkexec. Its data in `tests/fixtures/status.json` is synthetic, apart from Mullvad's public server names and locations.

```sh
cargo test
```

Run the app against the fake, and read what it was asked to do:

```sh
export FAKE_TS_LOG=/tmp/ts.log
export FAKE_PKEXEC_LOG=/tmp/pkexec.log
export TAILGATE_TAILSCALE_BIN=$PWD/tests/fake-tailscale
export TAILGATE_PKEXEC_BIN=$PWD/tests/fake-pkexec
export TAILGATE_CONFIG_DIR=/tmp/tailgate-test
FAKE_TS_MODE=denied-until-operator cargo run
grep -vE 'status|prefs' /tmp/ts.log
```

`FAKE_TS_MODE` can be `ok`, `denied`, `denied-until-operator`, `slow`, `gone`, `malformed`, `fail`, `login` or `stopped`. While a test binary is configured the window says so in a banner, and Tailgate refuses to run it through the real pkexec.

Other test overrides: `TAILGATE_OPEN_BIN`, `TAILGATE_IPCHECK_URL`, `TAILGATE_READ_TIMEOUT_MS`.

`tools/drive-fake.sh` and `tools/drive-states.sh` run the real window on a private Xvfb display, click through every control with XTEST, and save screenshots. `tools/public-shots.sh` retakes the screenshots above. Their coordinates match the default window size, so they need updating if the layout moves.

## Layout

- `src/cli.rs`: running commands, timeouts, error classification, the read-only gate
- `src/status.rs`: parsing `status --json`, `debug prefs`, `serve status --json`
- `src/nodes.rs`: picker rows and search
- `src/summary.rs`: the headline, and what `down` cuts
- `src/store.rs`, `src/ipcheck.rs`: saved settings and labels, Check my IP
- `src/app.rs`: connects the window to all of the above
- `ui/`: Slint markup

## License

Tailgate's own code is licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

It's built with Slint, used under the [Slint Royalty-free Desktop, Mobile, and Web Applications License 2.0](LICENSES/LicenseRef-Slint-Royalty-free-2.0.md). Slint isn't covered by the MIT and Apache terms. The app shows the required attribution with the `AboutSlint` widget in its About dialog (the ⓘ button at the top). If you distribute a modified build, keep that widget or follow the license's other attribution option. See [NOTICE](NOTICE). Release tarballs include the licenses of the other Rust crates in `THIRD-PARTY-LICENSES.txt`.

<a href="https://slint.dev"><img alt="Made with Slint" src="https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png" width="180"></a>

Tailgate is not affiliated with or endorsed by Tailscale Inc. or Mullvad VPN AB. Tailscale and Mullvad are their trademarks.

By [Center Point Labs](https://centerpointlabs.dev/services/tailgate/).
