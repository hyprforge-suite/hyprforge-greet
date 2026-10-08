-- The compositor the greeter runs under. Install as
-- /etc/greetd/hyprland-greeter.lua.
--
-- The important thing about this file is what is NOT in it.
--
-- Whoever is standing at the keyboard in front of a login screen is
-- unauthenticated, and every keybind this compositor has is theirs. A
-- greeter compositor with the usual `SUPER + Q -> terminal` gets that
-- person a shell as the greeter user without logging in — which is the
-- classic way greeters are broken into, and nothing the greeter program
-- itself can prevent. Verified against the test compositor used while
-- developing this: with the login screen on screen, its binds were
-- still live.
--
-- So: no binds. Not one. Not a "harmless" volume key, because the point
-- is the habit, and the next person to add one will add it next to the
-- others.
--
-- No `exec_cmd` either, beyond the greeter itself. The greeter starts
-- one program of its own: fcitx5, when it is installed, with every addon
-- off except an allow-list that has no menu, settings tool or key that
-- starts another program — see `src/input_method.rs` for the list and
-- why each is safe. It is started from the greeter, after the wait
-- below, rather than with an `exec_cmd` here, so it gets the same
-- display the greeter found and the choice stays in code a test can
-- read. Nothing else runs.

-- Displays, in the layout the session uses.
--
-- "Let Hyprland pick" is not enough, because the mode is not the part
-- that goes wrong. A preferred mode is almost always the right mode;
-- the *scale* is a choice, it lives in the user's own config, and a
-- greeter that hardcodes 1 on a display the session runs at 1.6 looks
-- like the wrong resolution even though the pixels are identical.
--
-- So the real layout is exported alongside the theme, for the same
-- reason the theme is: the greeter runs as another user and cannot
-- traverse into a home directory to read the original. This is a plain
-- `dofile` rather than a `require` because the export is not on the
-- greeter's Lua path and should not be added to it.
--
-- The fallback is not styling, it is a guarantee: a greeter must come
-- up on a display that has never been configured, on a machine where
-- nothing has been exported yet, and on the first boot after a new
-- monitor is plugged in.
local exported = "/var/lib/hyprforge/greet/monitors.lua"
local loaded = loadfile(exported)
if loaded then
    local ok, err = pcall(loaded)
    if not ok then
        -- Never fatal. A malformed export must not cost someone the
        -- ability to log in and fix it.
        print("hyprforge-greet: ignoring " .. exported .. ": " .. tostring(err))
        loaded = nil
    end
end
if not loaded then
    hl.monitor({ output = [[]], mode = [[preferred]], position = [[auto]], scale = 1 })
end

hl.config({
    misc = {
        disable_hyprland_logo = true,
        disable_splash_rendering = true,
        force_default_wallpaper = 0,
        -- Nothing to save and nobody to ask: the greeter is the only
        -- client and it is expected to exit.
        close_special_on_empty = true,
    },
    general = {
        -- No gaps or borders. The greeter is fullscreen and alone, and
        -- a border around it would only advertise that it is a window.
        gaps_in = 0,
        gaps_out = 0,
        border_size = 0,
    },
    decoration = {
        rounding = 0,
    },
    animations = {
        -- A login screen that animates in is a login screen that is
        -- slower to appear.
        enabled = false,
    },
})

-- The greeter, and then out. The compositor exits when the greeter does,
-- because greetd waits for the whole session command to finish before
-- starting the real session — a compositor that outlived its greeter
-- would leave the machine at a blank screen.
--
-- **It waits for the compositor, and works out its own display.** Both
-- halves are necessary and both were learned the hard way.
--
-- `hl.exec_cmd` runs while the config is being parsed, before Hyprland
-- accepts clients, so without the wait the greeter loses a race it did
-- not know it was in and exits. greetd then reports
-- `conversation failed` for a password nobody was asked for.
--
-- And at that moment `WAYLAND_DISPLAY` is **empty** — measured, not
-- assumed — while `HYPRLAND_INSTANCE_SIGNATURE` is already set. So the
-- wait is on `hyprctl` answering, which needs only the signature, and
-- the display is then found by looking. A first attempt waited on
-- `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`, which with an empty variable is
-- just the directory: it never matched, spun for ten seconds, and
-- launched the greeter with nothing to connect to. That produced the
-- worst symptom of the lot — no error and no login screen.
--
-- Taking the last `wayland-N` socket is safe here specifically: the
-- greeter user gets a fresh runtime directory with exactly one
-- compositor in it.
--
-- `--user` must name the account to log in. There is no user picker
-- yet, so this is where the choice is made.
--
-- `--command` must be exactly how the session is started by hand. On a
-- machine that uses uwsm that is `uwsm start hyprland.desktop` — the
-- `.desktop` matters, `uwsm start hyprland` is not the same thing and
-- fails at the point where there is nothing left to look at.
-- Everything this session says goes to /tmp/hyprforge-greet.log.
--
-- Not instrumentation for its own sake: greetd starts this compositor
-- with the VT as its stdout, so Hyprland's log, the greeter's warnings
-- and the monitor layout it resolved all scroll past on tty1 and are
-- gone by the time anyone can read them. Nothing about a login screen
-- that came up wrong is diagnosable afterwards, which is how a wrong
-- display scale survived two reboots.
--
-- /tmp because the greeter runs as its own user and can write nowhere
-- else; truncated each start so it describes this boot and not every
-- boot. It holds no secrets — the greeter never writes anything derived
-- from a keystroke.
hl.exec_cmd([[sh -c '
    exec >/tmp/hyprforge-greet.log 2>&1
    echo "--- greeter starting $(date -Is) ---"
    n=0
    until hyprctl monitors >/dev/null 2>&1 || [ $n -ge 100 ]; do n=$((n+1)); sleep 0.1; done
    echo "waited ${n} tenths for the compositor"
    echo "--- monitors as the greeter sees them ---"
    hyprctl monitors 2>&1
    echo "--- exported layout ---"
    cat /var/lib/hyprforge/greet/monitors.lua 2>&1 || echo "(no exported layout)"
    WAYLAND_DISPLAY=$(cd "$XDG_RUNTIME_DIR" && ls -1 wayland-[0-9]* 2>/dev/null | grep -v "\\.lock$" | tail -1)
    export WAYLAND_DISPLAY
    echo "--- greeter output ---"
    hyprforge-greet --user CHANGE_ME --command "uwsm start hyprland.desktop"
    echo "greeter exited $?"
    hyprctl dispatch exit
']])
