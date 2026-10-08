//! Input methods: composed characters reaching the entry.
//!
//! A character an XKB layout has a key for — `é` on a French layout,
//! `ж` on a Russian one — already arrives as the key's text, and needs
//! none of this. What does is a character *composed* from several
//! presses by an input method such as fcitx5 or IBus. Without an input
//! method a password containing one cannot be typed here at all, which
//! is the gap `docs/lock-and-greeter.md` named.
//!
//! iced 0.14 already speaks `text-input-v3` through winit; what it
//! needs is a widget that asks for it. Its own `text_input` would, but it
//! would also draw the text, and the screen is `hyprforge_authui`'s, shared
//! with the lock. So [`Ime`] wraps that screen without drawing anything:
//! it asks for an input method while a question is open, says whether the
//! answer is secret (`Purpose::Secure`, which winit sends as
//! `ContentPurpose::Password` + `SensitiveData`, the hint an input method
//! reads as "do not learn or suggest this"), and turns what the input
//! method commits into a message.
//!
//! **The pre-edit is never drawn and never kept.** What is being composed
//! *is* the password, a few characters early. The request always carries
//! `preedit: None`, so iced draws no over-the-spot overlay, and the
//! pre-edit's text is reduced to "is something being composed" the moment
//! it arrives — the only thing [`Composition`] needs to know. The
//! committed text travels in a [`Committed`], whose `Debug` is a count:
//! CLAUDE.md's rule about anything derived from a keystroke.
//!
//! What this cannot control is the input method's own window: its
//! candidate list is its surface, not ours. Most input methods switch to
//! direct input for a `Password` field and show nothing, which is also why
//! this is worth less than it looks — but whether one shows a composition
//! in clear is up to it.

use iced::{Element, Event, Length, Rectangle, Size, Vector};
use iced_widget::core::input_method::{self, InputMethod, Purpose};
use iced_widget::core::widget::{Operation, Tree};
use iced_widget::core::{layout, mouse, overlay, renderer, Clipboard, Layout, Shell, Widget};

/// Text an input method committed.
///
/// A newtype so `Message`'s derived `Debug` cannot print it: what was
/// committed is part of the password.
#[derive(Clone, PartialEq, Eq)]
pub struct Committed(pub String);

impl std::fmt::Debug for Committed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} chars>", self.0.chars().count())
    }
}

/// What the input method has told us, as far as typing is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Said {
    /// A composition began, or ended without committing anything.
    Composing(bool),
    /// The composition is over and this is what it produced.
    Commit(Committed),
}

/// Whether a composition is under way, so a key cannot count twice.
///
/// While someone is composing, the keys they press belong to the input
/// method: Enter picks a candidate, Backspace edits the composition,
/// letters build it. An input method that holds the keyboard grab
/// receives those keys and the window never sees them, but one that also
/// lets them through would otherwise have each key typed into the entry
/// *and* its result committed — a password with every character doubled,
/// submitted on the Enter meant for the candidate list. So no key reaches
/// the conversation until the composition is committed or abandoned.
#[derive(Debug, Default)]
pub struct Composition {
    composing: bool,
}

impl Composition {
    pub fn hear(&mut self, said: &Said) {
        self.composing = match said {
            Said::Composing(composing) => *composing,
            // A commit ends the composition: iced sends an empty pre-edit
            // first, but nothing here depends on that ordering.
            Said::Commit(_) => false,
        };
    }

    /// Whether a key press should reach the conversation now.
    pub fn lets_keys_through(&self) -> bool {
        !self.composing
    }
}

/// What the wrapped screen should ask the input method for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    /// No question is open: the input method stays off.
    Off,
    /// A question is open. `secret` is the prompt's own flag.
    On { secret: bool },
}

/// `content`, asking for an input method while `wanted` says so.
pub struct Ime<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    wanted: Wanted,
    on_said: fn(Said) -> Message,
}

pub fn ime<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    wanted: Wanted,
    on_said: fn(Said) -> Message,
) -> Ime<'a, Message, Theme, Renderer> {
    Ime { content: content.into(), wanted, on_said }
}

/// The request for one frame.
///
/// The cursor area is where an input method places its candidate list.
/// The screen does not expose where its entry is, so this is a band
/// across the middle of the window, where the card's entry sits; an
/// input method only needs it to avoid covering the entry.
pub fn request(wanted: Wanted, bounds: Rectangle) -> InputMethod<&'static str> {
    match wanted {
        Wanted::Off => InputMethod::Disabled,
        Wanted::On { secret } => InputMethod::Enabled {
            cursor: Rectangle::new(
                iced::Point::new(bounds.x + bounds.width / 2.0, bounds.y + bounds.height / 2.0),
                Size::new(1.0, 24.0),
            ),
            purpose: if secret { Purpose::Secure } else { Purpose::Normal },
            // Never the pre-edit: see the module doc.
            preedit: None,
        },
    }
}

/// An input-method event, reduced to what [`Composition`] and the entry
/// need — the pre-edit's text is dropped here, before it can go anywhere.
pub fn said(event: &input_method::Event) -> Option<Said> {
    match event {
        input_method::Event::Preedit(content, _) => Some(Said::Composing(!content.is_empty())),
        input_method::Event::Commit(text) => Some(Said::Commit(Committed(text.clone()))),
        input_method::Event::Closed => Some(Said::Composing(false)),
        input_method::Event::Opened => None,
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for Ime<'_, Message, Theme, Renderer>
where
    Renderer: renderer::Renderer,
{
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.content.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Event::InputMethod(event) = event {
            if let Some(said) = said(event) {
                shell.publish((self.on_said)(said));
            }
            shell.capture_event();
        }

        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        // Asked on every event rather than only on a redraw: iced reads
        // the request after the redraw pass, and requests merge, so asking
        // more often than that costs nothing and asking less could miss
        // the frame a question opened on.
        shell.request_input_method(&request(self.wanted, layout.bounds()));
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(&mut tree.children[0], layout, renderer, viewport, translation)
    }
}

impl<'a, Message, Theme, Renderer> From<Ime<'a, Message, Theme, Renderer>> for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: renderer::Renderer + 'a,
{
    fn from(ime: Ime<'a, Message, Theme, Renderer>) -> Self {
        Element::new(ime)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> Rectangle {
        Rectangle::new(iced::Point::ORIGIN, Size::new(1920.0, 1080.0))
    }

    #[test]
    fn a_secret_question_asks_for_a_password_field() {
        match request(Wanted::On { secret: true }, bounds()) {
            InputMethod::Enabled { purpose, .. } => assert_eq!(purpose, Purpose::Secure),
            InputMethod::Disabled => panic!("an open question must ask for the input method"),
        }
    }

    #[test]
    fn a_visible_question_asks_for_an_ordinary_field() {
        match request(Wanted::On { secret: false }, bounds()) {
            InputMethod::Enabled { purpose, .. } => assert_eq!(purpose, Purpose::Normal),
            InputMethod::Disabled => panic!("an open question must ask for the input method"),
        }
    }

    #[test]
    fn nothing_is_asked_for_while_no_question_is_open() {
        assert_eq!(request(Wanted::Off, bounds()), InputMethod::Disabled);
    }

    /// The pre-edit is the password before it is committed. iced draws
    /// any pre-edit a request carries as an overlay, so the request must
    /// never carry one.
    #[test]
    fn the_request_never_carries_a_preedit_to_draw() {
        for secret in [true, false] {
            if let InputMethod::Enabled { preedit, .. } = request(Wanted::On { secret }, bounds()) {
                assert!(preedit.is_none());
            }
        }
    }

    #[test]
    fn a_preedit_is_reduced_to_whether_something_is_being_composed() {
        let event = input_method::Event::Preedit("にほ".into(), Some(0..3));
        assert_eq!(said(&event), Some(Said::Composing(true)));
        let cleared = input_method::Event::Preedit(String::new(), None);
        assert_eq!(said(&cleared), Some(Said::Composing(false)));
    }

    #[test]
    fn keys_wait_while_a_composition_is_under_way() {
        let mut composition = Composition::default();
        assert!(composition.lets_keys_through());

        composition.hear(&Said::Composing(true));
        assert!(!composition.lets_keys_through(), "a key during composition is the input method's");

        composition.hear(&Said::Commit(Committed("日本".into())));
        assert!(composition.lets_keys_through(), "a commit ends the composition");
    }

    #[test]
    fn an_input_method_closing_mid_composition_lets_keys_through_again() {
        let mut composition = Composition::default();
        composition.hear(&Said::Composing(true));
        assert_eq!(said(&input_method::Event::Closed), Some(Said::Composing(false)));
        composition.hear(&Said::Composing(false));
        assert!(composition.lets_keys_through());
    }

    #[test]
    fn committed_text_renders_as_a_count() {
        let rendered = format!("{:?}", Said::Commit(Committed("pässwörd".into())));
        assert!(!rendered.contains("pässwörd"));
        assert!(rendered.contains("8 chars"), "{rendered}");
    }
}
