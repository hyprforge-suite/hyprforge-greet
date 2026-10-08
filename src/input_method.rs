//! The input method the login screen's compositor runs — and how little
//! of it is allowed to run.
//!
//! The greeter asks for an input method while a question is open
//! (`ime.rs`), but nothing answers that unless one is running in the
//! compositor the greeter lives in, as the greeter's own user. This
//! starts one: fcitx5 if it is installed, and nothing otherwise. A
//! machine with no input method installed gets exactly the login screen
//! it had before.
//!
//! **This is the greeter's no-keybinds rule, arriving through another
//! door.** Whoever is at a login screen is unauthenticated, and anything
//! running in its compositor that reacts to the keyboard is theirs. An
//! input method is a program that reacts to the keyboard by design, and
//! a full fcitx5 or IBus carries several ways to start another program.
//! Read from fcitx5's source (5.1.x, `src/`), every one of them:
//!
//! - **`Instance::configure()`** starts `fcitx5-configtool`. It is
//!   reached from the X11 tray menu (`ui/classic/xcbtraywindow.cpp`),
//!   the StatusNotifierItem menu (`modules/notificationitem/dbusmenu.cpp`),
//!   kimpanel (`ui/kimpanel/kimpanel.cpp`) and the D-Bus controller
//!   (`modules/dbus/dbusmodule.cpp`) — and from nothing else.
//! - **Restart** re-executes fcitx5 with `-r` and *none of the flags
//!   below* (`server/main.cpp`), so a restart would come back with every
//!   addon on. It is reached from the same menus and the same D-Bus
//!   controller.
//! - **`xcb`** runs `xmodmap` (`modules/xcb/xcbkeyboard.cpp`), and the
//!   **ibus frontend** execs a program of its own.
//! - **classicui's Plasma theme watchdog** runs
//!   `fcitx5-plasma-theme-generator`, only when the theme is set to
//!   `plasma`; the config written here never sets it.
//! - **fcitx5-anthy** binds F11 and F12 by default to start `kasumi`, a
//!   dictionary editor (`src/config.h`, `DictAdminKey`/`AddWordKey`).
//!   A default key that starts a GUI program is exactly what this rule
//!   exists to keep away from a login screen, so anthy is not allowed.
//! - **fcitx5-rime**'s "open user data dir" is `xdg-open`, but it is an
//!   `ExternalOption` — a button in the config tool, unreachable without
//!   it.
//! - **fcitx5-lua** runs Lua with `os.execute`; it is not allowed.
//!
//! So fcitx5 starts with `--disable=all` and `--enable=` exactly
//! [`ALLOWED_ADDONS`]: the Wayland input-method frontend, the candidate
//! window, and the engines whose source was read for this. Every menu
//! above lives in an addon that list leaves out, and an addon that is not
//! enabled is not loaded (`AddonManager::load`, where an `--enable` name
//! overrides `--disable=all` and nothing else does). An engine left off
//! the list is not broken, only not offered here until someone has read
//! it the same way.
//!
//! **IBus is not started, deliberately.** Its Wayland input method is
//! `ibus-ui-gtk3 --enable-wayland-im` — the same GTK panel process that
//! carries the menu with Preferences (`ibus-setup`), Restart and the
//! engines' own setup tools (`tools/main.vala`, `start_daemon_in_wayland`).
//! The part that talks to the compositor cannot be had without the part
//! that starts programs, so there is no restricted way to run it here.
//!
//! **A password goes past the engine, on purpose.** fcitx5 switches a
//! password field to plain keyboard input unless
//! `AllowInputMethodForPassword` is set (`isInputMethodDisabled` in
//! `instance.cpp`), and the config written here pins that off: composing
//! a password puts it in the candidate window, in clear, on the login
//! screen. What an input method adds here is the questions greetd asks
//! that are not secret.
//!
//! **Nothing it learns outlives the boot.** Its configuration and data
//! directories are made fresh under `$XDG_RUNTIME_DIR` every start, so a
//! pinyin history is never written to a disk, and nothing a previous
//! boot left there — an addon file in the data directory, a hand-edited
//! config — is read. The one thing carried in is which input methods to
//! offer: a `fcitx5-profile` the Settings app may export beside the
//! theme, read and copied, never executed.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The fcitx5 addons the login screen runs, and only these.
///
/// The infrastructure is three: `wayland` and `waylandim` are the
/// connection and the `input-method-v2` frontend the compositor talks
/// to, and `classicui` draws the candidate window (its menus are X11
/// only, in `xcb`, which is not here). `keyboard` is plain layouts.
/// The engines are the ones read for this: `pinyin` (needs
/// `punctuation`), `table` (needs `punctuation` and `pinyinhelper`,
/// which needs `quickphrase` and `clipboard` — none of the four start
/// anything), `rime` and `hangul`.
pub const ALLOWED_ADDONS: &[&str] = &[
    "wayland",
    "waylandim",
    "classicui",
    "keyboard",
    "pinyin",
    "punctuation",
    "table",
    "pinyinhelper",
    "quickphrase",
    "clipboard",
    "rime",
    "hangul",
];

/// The file in the export directory naming which input methods to offer
/// — a copy of the user's `~/.config/fcitx5/profile`, when the Settings
/// app has exported one.
pub const EXPORTED_PROFILE: &str = "fcitx5-profile";

/// Bigger than any real profile, and small enough that a corrupt export
/// cannot make the login screen read a disk's worth of it.
const PROFILE_LIMIT: u64 = 64 * 1024;

/// The configuration written fresh at every start.
///
/// `AllowInputMethodForPassword` is fcitx5's own default; it is spelled
/// out because it is the decision this module rests on, and a default
/// can change under it.
const CONFIG: &str = "[Behavior]\nAllowInputMethodForPassword=False\nShowPreeditForPassword=False\n";

/// classicui with its stock theme: `plasma` is the one theme that runs a
/// program.
const CLASSICUI: &str = "Theme=default\nDarkTheme=default-dark\nUseDarkTheme=False\n";

/// How to start the input method, worked out before anything is touched.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Where its configuration and data live for this boot.
    pub root: PathBuf,
}

impl Plan {
    /// The environment it runs with: its own directories, and the
    /// system's data directories spelled out rather than inherited, so
    /// addon files are found only where packages put them.
    pub fn env(&self) -> Vec<(&'static str, OsString)> {
        vec![
            ("XDG_CONFIG_HOME", self.root.join("config").into()),
            ("XDG_DATA_HOME", self.root.join("data").into()),
            ("XDG_CACHE_HOME", self.root.join("cache").into()),
            ("XDG_DATA_DIRS", "/usr/local/share:/usr/share".into()),
        ]
    }
}

/// Whether there is an input method to start, and how.
///
/// `find` looks a program up on `$PATH`. No `fcitx5` — or no runtime
/// directory to keep its state in — is no plan, which is not an error:
/// it is the login screen without an input method, as before.
pub fn plan(find: impl Fn(&str) -> Option<PathBuf>, runtime_dir: Option<&Path>) -> Option<Plan> {
    let program = find("fcitx5")?;
    let root = runtime_dir?.join("hyprforge-greet-ime");
    Some(Plan {
        program,
        args: vec![
            // Every addon off, then back on by name. An allow-list rather
            // than a list of what to turn off, because a newly installed
            // addon is then off until someone has read it.
            "--disable=all".into(),
            format!("--enable={}", ALLOWED_ADDONS.join(",")),
            // Errors only. Key events are logged by `key_trace` at debug
            // level, and this greeter's log is a file in /tmp.
            "--verbose=*=2".into(),
            // In the foreground of the process started here, not forked
            // away from it: it ends when the compositor does, because it
            // exits when its display goes.
            "-D".into(),
        ],
        root,
    })
}

/// `$PATH` lookup: the first entry with an executable file of that name.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// Lay out a fresh configuration for `plan`: whatever was there is
/// removed, the two files above are written, and an exported profile is
/// copied in when there is one.
pub fn prepare(plan: &Plan, export_dir: &Path) -> std::io::Result<()> {
    use std::io::Read;
    use std::os::unix::fs::DirBuilderExt;

    match std::fs::remove_dir_all(&plan.root) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let config = plan.root.join("config").join("fcitx5");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(config.join("conf"))?;
    for dir in ["data", "cache"] {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(plan.root.join(dir))?;
    }
    std::fs::write(config.join("config"), CONFIG)?;
    std::fs::write(config.join("conf").join("classicui.conf"), CLASSICUI)?;

    // Missing is the usual case — Settings has not exported one, or the
    // user has never set fcitx5 up — and fcitx5 then offers what the
    // locale suggests. Unreadable is reported and treated the same way:
    // a login screen does not stop over which input methods to list.
    let exported = export_dir.join(EXPORTED_PROFILE);
    match std::fs::File::open(&exported) {
        Ok(file) => {
            let mut profile = String::new();
            file.take(PROFILE_LIMIT).read_to_string(&mut profile)?;
            std::fs::write(config.join("profile"), profile)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("not reading {}: {e}", exported.display()),
    }
    Ok(())
}

/// Start the input method, if there is one. Never fails the greeter:
/// every way this can go wrong leaves the login screen as it would be
/// without one.
pub fn start(export_dir: &Path) -> Option<std::process::Child> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let Some(plan) = plan(find_on_path, runtime.as_deref()) else {
        tracing::info!("no input method to start (fcitx5 is not installed)");
        return None;
    };
    if let Err(e) = prepare(&plan, export_dir) {
        tracing::warn!("not starting fcitx5: couldn't prepare {}: {e}", plan.root.display());
        return None;
    }
    let mut command = std::process::Command::new(&plan.program);
    command.args(&plan.args).stdin(std::process::Stdio::null());
    for (key, value) in plan.env() {
        command.env(key, value);
    }
    match command.spawn() {
        Ok(child) => {
            tracing::info!("started fcitx5 (pid {})", child.id());
            Some(child)
        }
        Err(e) => {
            tracing::warn!("couldn't start fcitx5: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(names: &'static [&'static str]) -> impl Fn(&str) -> Option<PathBuf> {
        move |name| names.contains(&name).then(|| PathBuf::from("/usr/bin").join(name))
    }

    fn runtime() -> Option<&'static Path> {
        Some(Path::new("/run/user/970"))
    }

    #[test]
    fn with_no_input_method_installed_nothing_is_started() {
        assert_eq!(plan(installed(&[]), runtime()), None);
    }

    #[test]
    fn ibus_alone_is_never_started_because_its_input_method_is_its_menu_panel() {
        assert_eq!(plan(installed(&["ibus-daemon", "ibus-ui-gtk3"]), runtime()), None);
    }

    #[test]
    fn with_no_runtime_directory_nothing_is_started_rather_than_writing_elsewhere() {
        assert_eq!(plan(installed(&["fcitx5"]), None), None);
    }

    #[test]
    fn fcitx5_starts_with_every_addon_off_but_the_allowed_ones() {
        let plan = plan(installed(&["fcitx5"]), runtime()).expect("fcitx5 is installed");
        assert_eq!(plan.program, Path::new("/usr/bin/fcitx5"));
        assert_eq!(plan.args[0], "--disable=all");
        assert_eq!(plan.args[1], format!("--enable={}", ALLOWED_ADDONS.join(",")));
    }

    /// Every addon that can start a program, by name, from the audit in
    /// the module doc. Adding one of these to the list is the change this
    /// test exists to stop.
    #[test]
    fn no_addon_that_can_start_a_program_is_allowed() {
        for name in [
            "dbus",
            "notificationitem",
            "kimpanel",
            "xcb",
            "ibusfrontend",
            "fcitx4frontend",
            "anthy",
            "lua",
            "imeapi",
            "cloudpinyin",
            "virtualkeyboard",
        ] {
            assert!(!ALLOWED_ADDONS.contains(&name), "{name} must not run before login");
        }
    }

    #[test]
    fn the_input_method_never_logs_below_errors() {
        let plan = plan(installed(&["fcitx5"]), runtime()).expect("fcitx5 is installed");
        assert!(plan.args.contains(&"--verbose=*=2".to_string()));
    }

    #[test]
    fn its_state_lives_under_the_runtime_directory_and_data_dirs_are_the_systems() {
        let plan = plan(installed(&["fcitx5"]), runtime()).expect("fcitx5 is installed");
        assert!(plan.root.starts_with("/run/user/970"));
        for (key, value) in plan.env() {
            if key == "XDG_DATA_DIRS" {
                assert_eq!(value, "/usr/local/share:/usr/share");
            } else {
                assert!(Path::new(&value).starts_with(&plan.root), "{key} escapes the runtime dir");
            }
        }
    }

    #[test]
    fn a_password_is_never_composed_by_the_input_method() {
        assert!(CONFIG.contains("AllowInputMethodForPassword=False"));
        assert!(!CLASSICUI.contains("plasma"));
    }

    #[test]
    fn preparing_replaces_what_a_previous_boot_left_and_copies_only_the_profile() {
        let runtime = tempdir();
        let export = tempdir();
        let plan = plan(installed(&["fcitx5"]), Some(runtime.as_path())).expect("fcitx5");
        // Something a previous start left: an addon file fcitx5 would
        // otherwise load from its data directory.
        let stale = plan.root.join("data/fcitx5/addon/evil.conf");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, "[Addon]\nName=evil\n").unwrap();
        std::fs::write(export.join(EXPORTED_PROFILE), "[Groups/0]\nName=Default\n").unwrap();
        std::fs::write(export.join("fcitx5-config"), "[Hotkey]\n").unwrap();

        prepare(&plan, &export).unwrap();

        assert!(!stale.exists(), "a previous boot's files must be gone");
        let config = plan.root.join("config/fcitx5");
        assert_eq!(std::fs::read_to_string(config.join("profile")).unwrap(), "[Groups/0]\nName=Default\n");
        assert_eq!(std::fs::read_to_string(config.join("config")).unwrap(), CONFIG);
        let _ = std::fs::remove_dir_all(&runtime);
        let _ = std::fs::remove_dir_all(&export);
    }

    #[test]
    fn with_no_exported_profile_there_is_no_profile_and_no_failure() {
        let runtime = tempdir();
        let export = tempdir();
        let plan = plan(installed(&["fcitx5"]), Some(runtime.as_path())).expect("fcitx5");
        prepare(&plan, &export).unwrap();
        assert!(!plan.root.join("config/fcitx5/profile").exists());
        let _ = std::fs::remove_dir_all(&runtime);
        let _ = std::fs::remove_dir_all(&export);
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hyprforge-greet-ime-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
