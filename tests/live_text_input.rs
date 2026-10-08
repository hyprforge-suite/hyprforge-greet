//! Does the compositor offer what the greeter's input-method support
//! asks for?
//!
//! The greeter asks iced for an input method, iced asks winit, and winit
//! binds `zwp_text_input_manager_v3`. If the compositor does not
//! advertise that global, every request goes nowhere and a composed
//! character still cannot be typed — with nothing in this crate's unit
//! tests able to notice, because they stop at the request.
//!
//! **Read-only:** it lists the registry and binds nothing.
//!
//! Run with `cargo test -p hyprforge-greet --test live_text_input -- --ignored`.

use std::sync::{Arc, Mutex};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};

/// The suite's convention: libtest has no skipped state, so a check
/// that cannot run says so rather than reporting `ok` for nothing.
const SKIP_MARKER: &str = "HYPRFORGE-SKIP:";

#[derive(Default)]
struct Globals(Arc<Mutex<Vec<String>>>);

impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { interface, .. } = event {
            state.0.lock().unwrap().push(interface);
        }
    }
}

#[test]
#[ignore = "needs a running Wayland compositor"]
fn the_compositor_offers_text_input_v3() {
    let connection = match Connection::connect_to_env() {
        Ok(connection) => connection,
        Err(e) => {
            eprintln!("{SKIP_MARKER} no Wayland compositor to connect to ({e})");
            return;
        }
    };
    let mut queue = connection.new_event_queue();
    let _registry = connection.display().get_registry(&queue.handle(), ());
    let mut globals = Globals::default();
    queue.roundtrip(&mut globals).expect("the registry roundtrip");

    let seen = globals.0.lock().unwrap();
    assert!(
        seen.iter().any(|g| g == "zwp_text_input_manager_v3"),
        "the compositor does not advertise zwp_text_input_manager_v3, so no input method can reach the greeter; it offers: {seen:?}"
    );
}
