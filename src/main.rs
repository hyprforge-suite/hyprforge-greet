//! The greeter: the same screen as the lock, in front of a login.
//!
//! It draws nothing of its own. `hyprforge_authui::screen` is the screen
//! and `hyprforge_authui::conversation` is the exchange; this crate
//! supplies a window and greetd. That is the whole reason a greeter and
//! a lock screen can look like one system — not because they were styled
//! to match, but because there is one of them.
//!
//! **It cannot read your home directory.** A greeter runs as its own
//! user and `$HOME` is `drwx------`, so the theme comes from the copy the
//! Settings app exports to `/var/lib/hyprforge/greet`. Nothing here
//! reaches into a user's files.

mod greetd;
mod ime;
mod input_method;

use clap::Parser;
use greetd::GreetdBackend;
use hyprforge_authui::conversation::{self, Conversation, Press, State};
use hyprforge_authui::Theme;
use iced::keyboard::{key::Named, Key};
use iced::{Element, Subscription, Task};

#[derive(Parser)]
#[command(about = "Log in")]
struct Args {
    /// Who to log in.
    ///
    /// A single account for now: this screen shows a username but has no
    /// field for choosing one, so offering a picker would be a promise
    /// the UI does not keep. Multi-user selection is its own piece of
    /// work.
    #[arg(long)]
    user: String,

    /// What to run once the login succeeds, split on spaces.
    ///
    /// greetd starts it when this process exits, so it is scheduled and
    /// then this window goes away.
    #[arg(long, default_value = "Hyprland")]
    command: String,

    /// Where the exported theme lives. The default is the directory the
    /// Settings app writes to.
    #[arg(long)]
    theme_dir: Option<std::path::PathBuf>,

    /// Don't start an input method in this compositor.
    ///
    /// By default fcitx5 is started, with most of it switched off, when
    /// it is installed — see `input_method.rs` for what runs and why.
    #[arg(long)]
    no_input_method: bool,

    /// Type this in once a question is being asked, then submit.
    ///
    /// The only way to prove the whole greetd handshake — including the
    /// `start_session` at the end — without a keyboard. Compiled out of
    /// release builds: a flag that submits a password from argv puts it
    /// in `ps` output, which is not something a login screen should
    /// offer however convenient it is for testing.
    #[cfg(debug_assertions)]
    #[arg(long, value_name = "TEXT")]
    type_in: Option<String>,
}

#[derive(Debug, Clone)]
enum Message {
    /// A key, and the text it actually produced.
    ///
    /// Both, because they answer different questions. `Key` says which
    /// key it was — Enter, Backspace — while the text is what typing it
    /// means with the modifiers applied. Using the key alone loses the
    /// shift: `SHIFT + j` reports `j`, so every capital and symbol in a
    /// password is silently wrong, and PAM rejects a password the user
    /// typed correctly.
    Key(Key, Option<String>),
    /// What an input method said — see the `ime` module. Its pre-edit
    /// has already been reduced to whether something is being composed.
    Ime(ime::Said),
    /// Both the clock and the authenticator are driven from here: one
    /// needs the time, the other needs somebody to collect its answers.
    Tick,
    /// Something the screen's pointer targets asked for. The greeter
    /// offers none of them — no power button, no media — so these are
    /// never produced; the variant exists because the screen is one
    /// element for both hosts.
    Screen,
}

struct Greeter {
    conversation: Conversation<GreetdBackend>,
    /// Whether a frame was ever drawn.
    ///
    /// A greeter that exits without having shown anything has not
    /// "finished"; it has failed, and saying so is the difference
    /// between a one-line diagnosis and an afternoon. This one exited
    /// with status 0 and printed nothing when it lost a startup race
    /// with the compositor, so greetd reported `conversation failed`
    /// for a password nobody had been asked for.
    drew: std::rc::Rc<std::cell::Cell<bool>>,
    #[cfg(debug_assertions)]
    type_in: Option<String>,
    theme: Theme,
    command: Vec<String>,
    /// Set once the session has been scheduled, so it is asked for
    /// exactly once — greetd refuses a second `StartSession`, and the
    /// window is on its way out anyway.
    launched: bool,
    /// When the last failure began, for the shake — shared with the lock
    /// screen so the two cannot animate differently.
    pacing: hyprforge_authui::scene::Pacing,
    /// Whether an input method is mid-composition, which holds keys back
    /// so none is typed twice.
    composition: ime::Composition,
}

impl Greeter {
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Key(key, text) => {
                if self.composition.lets_keys_through() {
                    self.pacing.key(std::time::Instant::now());
                    self.key(key, text)
                }
            }
            Message::Ime(said) => {
                if apply_ime(&mut self.conversation, &mut self.composition, said) {
                    self.pacing.key(std::time::Instant::now());
                }
            }
            Message::Screen => {}
            // Collect anything greetd has said. Polling rather than
            // waking on the socket because the clock needs a tick
            // regardless, so there is already something arriving
            // regularly to pump on.
            Message::Tick => {
                self.conversation.pump();
            }
        }

        // Only once there is actually a question to answer. Typing at a
        // conversation that is still working would be correctly ignored,
        // which looks exactly like the test not working.
        #[cfg(debug_assertions)]
        if self.conversation.state().accepts_input() {
            if let Some(text) = self.type_in.take() {
                eprintln!("self test: answering with {} character(s)", text.chars().count());
                self.conversation.type_into(text);
                self.conversation.submit();
            }
        }

        self.pacing.observe(self.conversation.state(), std::time::Instant::now());

        if self.conversation.state().is_authenticated() && !self.launched {
            self.launched = true;
            // greetd runs this when the greeter exits, so scheduling it
            // and closing the window are one action.
            self.conversation.backend().launch(self.command.clone(), Vec::new());
            return iced::exit();
        }
        Task::none()
    }

    fn key(&mut self, key: Key, text: Option<String>) {
        apply_key(&mut self.conversation, key, text);
    }

    fn view(&self) -> Element<'_, Message, iced_widget::Theme, iced::Renderer> {
        self.drew.set(true);
        // Caps Lock is passed as false because iced's modifier state does
        // not carry it. The lock screen shows it, and this should too —
        // it is the difference between a stuck key and an apparently
        // forgotten password. Noted rather than quietly dropped.
        //
        // Always the card, never the idle clock: a login screen that
        // makes someone press a key before it shows them who it is
        // asking about is a lock screen's habit, not a greeter's.
        let conversation = &self.conversation;
        let mut scene = hyprforge_authui::scene::Scene::new(
            conversation.state(),
            conversation.username(),
            &self.theme,
            chrono::Local::now(),
        );
        scene.submitted = conversation.submitted();
        scene.rejection = self.pacing.rejection(
            conversation.state(),
            conversation.submitted(),
            conversation.failures(),
            std::time::Instant::now(),
        );
        let wanted = match conversation.state() {
            State::Asking { prompt, .. } => ime::Wanted::On { secret: prompt.secret },
            _ => ime::Wanted::Off,
        };
        ime::ime(hyprforge_authui::screen::view(scene).map(|_| Message::Screen), wanted, Message::Ime).into()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::keyboard::listen().filter_map(|event| match event {
                iced::keyboard::Event::KeyPressed { key, text, .. } => {
                    Some(Message::Key(key, text.map(|t| t.to_string())))
                }
                _ => None,
            }),
            // Fast while the authenticator is working so the screen stays
            // visibly alive through PAM's deliberate pause; slow
            // otherwise, which is all the clock needs.
            // And at frame rate while a rejected password shakes, which
            // is under half a second.
            iced::time::every(if self.pacing.animating(std::time::Instant::now()) {
                std::time::Duration::from_millis(16)
            } else if matches!(self.conversation.state(), State::Working) {
                std::time::Duration::from_millis(100)
            } else {
                std::time::Duration::from_secs(1)
            })
            .map(|_| Message::Tick),
        ])
    }
}

/// One iced key press, in the terms `hyprforge-authui` understands.
///
/// The grammar itself — what Enter means in each state, that Escape and
/// Backspace dismiss a failed attempt, which text may reach the entry —
/// is `conversation::apply_press`, shared with the lock screen. This
/// function is only the translation, and it is the whole of what is
/// host-specific.
///
/// It had been the grammar too, and the two copies drifted: the lock
/// screen learned that Escape and Backspace must dismiss a failure and
/// this one did not, so on the login screen both keys did nothing at
/// all after "Incorrect password".
///
/// `text` rather than the key for characters: CLAUDE.md's rule, and the
/// expensive one — reading the key turns `SHIFT + j` into `j`, so a
/// password loses every capital and PAM rejects one typed correctly,
/// spending a `pam_faillock` attempt each time.
fn apply_key<B: hyprforge_authui::conversation::Backend>(
    conversation: &mut Conversation<B>,
    key: Key,
    text: Option<String>,
) {
    let press = match key {
        Key::Named(Named::Enter) => Press::Enter,
        Key::Named(Named::Escape) => Press::Escape,
        Key::Named(Named::Backspace) => Press::Backspace,
        _ => match text {
            Some(text) => Press::Text(text),
            // A key that produced nothing typable — a modifier, a
            // function key — is not a press this prompt has a meaning
            // for.
            None => return,
        },
    };
    conversation::apply_press(conversation, press);
}

/// What an input method said, applied to the conversation. True when
/// it typed something.
///
/// A commit goes through the same door as a key's text, so it obeys the
/// same grammar: it leaves a failed attempt, it is buffered while the
/// backend works, and control characters are dropped.
fn apply_ime<B: hyprforge_authui::conversation::Backend>(
    conversation: &mut Conversation<B>,
    composition: &mut ime::Composition,
    said: ime::Said,
) -> bool {
    composition.hear(&said);
    match said {
        ime::Said::Commit(ime::Committed(text)) => {
            conversation::apply_press(conversation, Press::Text(text));
            true
        }
        ime::Said::Composing(_) => false,
    }
}

fn main() -> iced::Result {
    // Draw with tiny-skia, the renderer the lock screen uses, and never
    // wgpu. Turning `web-colors` off made the two agree on *opaque*
    // colours, but wgpu still blends in linear light while tiny-skia
    // blends the sRGB values as written, so everything translucent came
    // out different: the card's 55% background over a bright wallpaper
    // came out close to clear here and nearly opaque on the lock screen,
    // and the wallpaper dim did the same. The only way both can draw the
    // same pixels is the same renderer. A login screen has no business
    // needing a GPU driver in a good mood anyway — see authui's
    // `screen` module.
    //
    // The environment variable is iced 0.14's only way to choose, and it
    // is overridden unconditionally rather than defaulted: whatever greetd
    // inherited must not bring the mismatch back. Set before anything
    // here starts a thread.
    std::env::set_var("ICED_BACKEND", "tiny-skia");

    // Defaulting to ERROR would silence every `warn!` here, and a greeter
    // is started by greetd with no RUST_LOG and no terminal — the journal
    // is the only place anyone can see what it did. See the same note in
    // hyprforge-settings.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let args = Args::parse();

    let backend = match GreetdBackend::connect() {
        Ok(backend) => backend,
        Err(e) => {
            // Not a panic: this is the program standing between a person
            // and their machine, and a stack trace on a login screen
            // helps nobody.
            eprintln!("can't reach the login service: {e}");
            std::process::exit(1);
        }
    };

    let dir = args
        .theme_dir
        .unwrap_or_else(hyprforge_look::theme::export_dir);
    // `load_exported_from` is the one that degrades a missing or
    // unparseable export to the default theme rather than an error — see
    // its doc for why. `--theme-dir` is why this calls that with a
    // variable directory instead of the fixed-path `load_exported()`.
    let theme = hyprforge_authui::screen::renderable(Theme::load_exported_from(&dir));

    // Started before the window, so it is usually there by the time the
    // first question asks for it; held for the life of the greeter and
    // never waited on — it ends with the compositor, when its display
    // goes. Absent or failing, the login screen is the one it was.
    let _input_method = if args.no_input_method { None } else { input_method::start(&dir) };

    let command: Vec<String> = args.command.split_whitespace().map(str::to_string).collect();
    let font = hyprforge_authui::screen::font(&theme);

    // `boot` is an `Fn`, and a greetd connection cannot be cloned into
    // it, so the state is built once and handed over on the first call.
    // A single-window application boots exactly once; if that ever stops
    // being true, this will say so loudly rather than silently open a
    // second login screen with no way to authenticate.
    let drew = std::rc::Rc::new(std::cell::Cell::new(false));
    let once = std::cell::RefCell::new(Some(Greeter {
        conversation: Conversation::new(backend, args.user.clone()),
        drew: std::rc::Rc::clone(&drew),
        #[cfg(debug_assertions)]
        type_in: args.type_in.clone(),
        theme,
        command,
        launched: false,
        pacing: Default::default(),
        composition: Default::default(),
    }));

    iced::application(
        move || {
            (
                once.borrow_mut().take().expect("the greeter boots once"),
                Task::none(),
            )
        },
        Greeter::update,
        Greeter::view,
    )
    .subscription(Greeter::subscription)
    .default_font(font)
    .window(iced::window::Settings {
        // A login screen owns the display. Fullscreen rather than
        // maximised so there is no chrome to look behind.
        fullscreen: true,
        decorations: false,
        ..iced::window::Settings::default()
    })
    .run()?;

    // Ending without ever having drawn is a failure, whatever the event
    // loop thought. Reporting it as success is what made a lost startup
    // race look like a rejected password.
    if !drew.get() {
        eprintln!(
            "the greeter exited without ever drawing — it could not open a window on \
             WAYLAND_DISPLAY={}. If it was started alongside the compositor, it has to \
             wait for the compositor's socket first; see config/hyprland-greeter.lua.",
            std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "(unset)".into())
        );
        std::process::exit(1);
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use hyprforge_authui::conversation::{Backend, Prompt, Response};

    /// A backend that just asks for a password, so key handling can be
    /// exercised without a socket.
    struct Asking(Option<Response>);
    impl Backend for Asking {
        fn start(&mut self, _username: &str) {
            self.0 = Some(Response::Ask(Prompt::secret("Password:")));
        }
        fn answer(&mut self, _answer: &str) {}
        fn proceed(&mut self) {}
        fn poll(&mut self) -> Option<Response> {
            self.0.take()
        }
    }

    fn typing() -> Conversation<Asking> {
        Conversation::new(Asking(None), "apost")
    }

    /// A backend that fails whatever it is told, so the state after a
    /// wrong password can be reached.
    struct Rejecting(Option<Response>);
    impl Backend for Rejecting {
        fn start(&mut self, _username: &str) {
            self.0 = Some(Response::Ask(Prompt::secret("Password:")));
        }
        fn answer(&mut self, _answer: &str) {
            self.0 = Some(Response::Failure { reason: "Incorrect password".into() });
        }
        fn proceed(&mut self) {}
        fn poll(&mut self) -> Option<Response> {
            self.0.take()
        }
    }

    /// Escape and Backspace must get a person out of a failed attempt.
    ///
    /// They did not: `clear` and `type_into` are both no-ops in
    /// `Failed`, so on the login screen — the one screen where a frozen
    /// response is frightening — the two keys anyone reaches for after
    /// "Incorrect password" did nothing whatsoever. The lock screen had
    /// already fixed this; the greeter's copy of the grammar had not,
    /// which is why there is no longer a copy.
    #[test]
    fn escape_and_backspace_get_out_of_a_failed_attempt() {
        for key in [Key::Named(Named::Escape), Key::Named(Named::Backspace)] {
            let mut c = Conversation::new(Rejecting(None), "apost");
            apply_key(&mut c, Key::Named(Named::Enter), None);
            assert!(matches!(c.state(), State::Failed { .. }), "the fixture must fail first");

            apply_key(&mut c, key.clone(), None);
            assert!(
                !matches!(c.state(), State::Failed { .. }),
                "{key:?} left the error on screen with nothing a person could do"
            );
        }
    }

    /// Shifted characters must survive.
    ///
    /// iced reports three things for a key press: the unmodified key,
    /// the modified key, and the text produced. Reading the first one
    /// turns `SHIFT + j` into `j`, so every capital and symbol in a
    /// password is silently wrong and PAM rejects a password that was
    /// typed correctly — while each attempt spends a faillock slot.
    #[test]
    fn what_reaches_the_password_is_the_text_that_was_typed() {
        let mut c = typing();
        // What iced sends for SHIFT+j: the key is still lowercase and
        // the text carries the capital.
        apply_key(&mut c, Key::Character("j".into()), Some("J".into()));
        apply_key(&mut c, Key::Character("1".into()), Some("!".into()));
        apply_key(&mut c, Key::Character("a".into()), Some("a".into()));
        assert_eq!(c.entered(), "J!a", "the shift was lost somewhere");
    }

    /// Enter and Backspace also produce text — "\r" and "\u{8}" — and
    /// appending those would put invisible characters in the password.
    #[test]
    fn keys_that_are_not_characters_never_reach_the_password() {
        let mut c = typing();
        apply_key(&mut c, Key::Character("a".into()), Some("a".into()));
        apply_key(&mut c, Key::Named(Named::Backspace), Some("\u{8}".into()));
        apply_key(&mut c, Key::Character("b".into()), Some("b".into()));
        assert_eq!(c.entered(), "b", "backspace should delete, not type");

        // Escape clears rather than typing an escape character.
        apply_key(&mut c, Key::Character("x".into()), Some("x".into()));
        apply_key(&mut c, Key::Named(Named::Escape), Some("\u{1b}".into()));
        assert_eq!(c.entered(), "");
    }

    /// One key press as the window delivers it, held back while an input
    /// method is composing — `update`'s rule, without a greetd socket.
    fn press<B: Backend>(c: &mut Conversation<B>, composition: &ime::Composition, key: Key, text: Option<&str>) {
        if composition.lets_keys_through() {
            apply_key(c, key, text.map(str::to_string));
        }
    }

    /// The keys that build a composition belong to the input method. If
    /// it lets them through as well as committing their result, each
    /// must still count once — and the Enter that picks a candidate must
    /// not submit the password.
    #[test]
    fn a_composed_character_is_typed_once_and_its_enter_submits_nothing() {
        let mut c = typing();
        c.pump();
        let mut composition = ime::Composition::default();

        press(&mut c, &composition, Key::Character("a".into()), Some("a"));
        apply_ime(&mut c, &mut composition, ime::Said::Composing(true));
        press(&mut c, &composition, Key::Character("n".into()), Some("n"));
        press(&mut c, &composition, Key::Named(Named::Enter), Some("\r"));
        assert!(c.state().accepts_input(), "the candidate's Enter must not submit");
        apply_ime(&mut c, &mut composition, ime::Said::Composing(false));
        apply_ime(&mut c, &mut composition, ime::Said::Commit(ime::Committed("日".into())));
        press(&mut c, &composition, Key::Character("b".into()), Some("b"));

        assert_eq!(c.typed(), "a日b");
    }

    /// A commit after "Incorrect password" starts a new attempt and is
    /// kept, exactly as a typed key is.
    #[test]
    fn a_commit_after_a_failure_starts_over_and_is_kept() {
        let mut c = Conversation::new(Rejecting(None), "apost");
        c.pump();
        apply_key(&mut c, Key::Named(Named::Enter), None);
        c.pump();
        assert!(matches!(c.state(), State::Failed { .. }), "the fixture must fail first");

        let mut composition = ime::Composition::default();
        assert!(apply_ime(&mut c, &mut composition, ime::Said::Commit(ime::Committed("日".into()))));
        c.pump();
        assert_eq!(c.typed(), "日");
    }
}
