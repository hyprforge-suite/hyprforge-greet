# Installing the greeter

The greeter replaces whatever logs you in today. Get that wrong and the
machine boots to something that cannot accept a password, so the order
below is deliberate: every step is reversible until the last one, and
the last one is a single file.

Keep a second VT available throughout. `ctrl+alt+F3` reaches a text
console, and that console is how you undo any of this.

## What the pieces are

| Piece | Runs as | Job |
|---|---|---|
| `greetd` | root | Owns the VT; starts the greeter, then your session |
| `start-hyprland` | `greeter` | A compositor with **no keybinds** |
| `hyprforge-greet` | `greeter` | Draws the screen, talks to greetd |
| `/var/lib/hyprforge/greet` | — | The look *and the display layout*, exported out of your home directory |

The greeter does not authenticate anything itself. It hands what you
type to greetd, which runs the PAM exchange as root and starts the
session. That is the point of using greetd rather than writing the
privileged half ourselves.

## 1. The export directory

Do this first. Without it the greeter works but always looks like a
default install, and the failure is quiet from the outside — the
Settings app saves successfully and only the login screen, which you see
once per boot, keeps the old appearance.

Two things land here, and both are here for the same reason: the greeter
runs as another user and `$HOME` is `drwx------`, so it cannot traverse
into a home directory to read the originals.

- `theme.toml` (+ `wallpaper.*`) — written when Appearance applies
- `monitors.lua` — written by `hyprforge-displayd` when displays settle

The layout matters more than it sounds. The mode is rarely wrong; the
*scale* is, because it is a choice that lives in your config. A greeter
defaulting to scale 1 on a display the session runs at 1.6 reads as "the
login screen picked the wrong resolution" even though the pixel count is
identical.

```sh
sudo install -Dm644 crates/hyprforge-greet/config/hyprforge-greet.sysusers \
    /usr/lib/sysusers.d/hyprforge-greet.conf
sudo install -Dm644 crates/hyprforge-greet/config/hyprforge-greet.tmpfiles \
    /usr/lib/tmpfiles.d/hyprforge-greet.conf
sudo systemd-sysusers
sudo systemd-tmpfiles --create
sudo usermod -aG hyprforge "$USER"
```

`usermod` does not affect processes that are already running, including
your shell and your session. The group arrives at your next login, which
is also when you will first be testing the greeter — so this resolves
itself, but `id -nG` will not show it until then.

Check:

```sh
ls -ld /var/lib/hyprforge/greet     # expect drwxrwsr-x root hyprforge
```

The `s` matters. It is what keeps a second user's export readable by the
greeter instead of landing group-root.

## 2. The binary and the greeter's compositor

```sh
cargo build --release -p hyprforge-greet
sudo install -Dm755 target/release/hyprforge-greet /usr/local/bin/hyprforge-greet
sudo install -Dm644 crates/hyprforge-greet/config/hyprland-greeter.lua \
    /etc/greetd/hyprland-greeter.lua
```

Then edit `/etc/greetd/hyprland-greeter.lua` and replace `CHANGE_ME`
with your username. It is a template, not a drop-in.

From the Arch package (`hyprforge-greet`), the binary, the sysusers and
tmpfiles entries of step 1 are already installed, and the two templates
are in `/usr/share/hyprforge-greet/`:

```sh
sudo install -Dm644 /usr/share/hyprforge-greet/hyprland-greeter.lua \
    /etc/greetd/hyprland-greeter.lua
```

Read the rest of that file before installing it. **It has no keybinds,
and that is the security property, not an oversight.** Every bind the
greeter's compositor has belongs to whoever is standing at the keyboard,
authenticated or not — a terminal bind is a shell as the `greeter` user
without logging in. No amount of care inside `hyprforge-greet` can
compensate for one added here.

## 3. Test it on a spare VT

`config/greetd.toml` is set up for the *installed* configuration. For
testing, change two lines so it cannot take the screen away from you:

```toml
[terminal]
vt = 2
switch = false
```

Install it as `/etc/greetd/config.toml`, then start greetd **without
enabling it**:

```sh
sudo systemctl start greetd
```

Nothing visible should happen. Switch to it deliberately with
`ctrl+alt+F2`, log in, and confirm you land in a session. If it fails,
`ctrl+alt+F3` still gets you a console and `sudo systemctl stop greetd`
undoes it entirely — nothing has been enabled, so a reboot also clears
it.

Useful while debugging:

```sh
journalctl -u greetd -b --no-pager
```

## 4. Commit to it

Only after step 3 has worked at least twice. Restore `vt = 1` and
`switch = true` in `/etc/greetd/config.toml`, then:

```sh
sudo systemctl enable greetd
```

And remove whatever used to log you in — most likely a getty autologin
override:

```sh
sudo rm -rf /etc/systemd/system/getty@tty1.service.d/
sudo systemctl daemon-reload
```

If you were launching a lock screen at session start as a stand-in for
authentication, that line can go now. The greeter *is* the password at
boot; leaving both asks for the same door twice.

## What it looks like at boot

Between the greeter accepting your password and your desktop appearing
there is a brief console: the greeter's compositor has released the
display and yours has not claimed it yet, so Hyprland's startup banner
prints to a bare VT for about a second. This is not a fault in any of
the above — it is what handing a VT from one compositor to another looks
like — and hiding it is a boot-splash concern, not a greeter one.

## Backing all of it out

```sh
sudo systemctl disable --now greetd
```

Then restore whatever logged you in before. Nothing in steps 1 and 2
does anything on its own; the only file that changes how the machine
boots is `/etc/greetd/config.toml` plus that `enable`.
