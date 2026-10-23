//! Authenticating through greetd.
//!
//! greetd is a proxy for a PAM conversation: it asks the questions PAM
//! asks and carries the answers back. That is why this crate and the
//! lock screen can share one screen — the shape of the exchange is the
//! same, and [`hyprforge_authui::conversation`] is that shape.
//!
//! The daemon's own documentation makes the point better than this could:
//! an auth message "can consist of anything ... it is therefore important
//! that no assumptions are made about the questions that will be asked,
//! and attempts to automatically answer these questions should not be
//! made." A greeter that modelled a password box would be wrong the first
//! time someone enabled a hardware token.
//!
//! Like PAM, greetd is answered on its own thread. The reason is the
//! same: PAM sits behind greetd, so a wrong password still costs the
//! deliberate `pam_unix` delay, and a login screen that stops repainting
//! for two seconds looks broken.
//!
//! # What is trusted, and what is not
//!
//! greetd's socket is created by greetd, owned by root, and reachable
//! only by the greeter user. Whatever comes down it is therefore taken
//! at its word, and that includes a `Success` in reply to
//! `CreateSession` with nothing asked in between — which is how
//! autologin works, and must keep working. Requiring a question first
//! would look like hardening and would only break `pam_permit`.
//!
//! Everything an *attacker* can reach is treated as hostile, and was
//! tested that way: malformed frames, an unknown message type, a length
//! prefix claiming four gigabytes, and a greetd that answers nothing.
//! None of them start a session and none of them bring the greeter
//! down.
//!
//! The real way into a greeter is not through this socket at all. It is
//! the compositor the greeter runs under: every keybind that compositor
//! has belongs to whoever is standing at the keyboard, authenticated or
//! not. See `config/hyprland-greeter.lua`, which has none.

use greetd_ipc::codec::SyncCodec;
use greetd_ipc::{AuthMessageType, ErrorType, Request, Response as Greetd};
use hyprforge_authui::conversation::{Backend, Prompt, Response};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long to wait for greetd to answer one request.
///
/// Bounded because nothing may wait on another process forever, and
/// because the consequence here is specific: a greetd that stops
/// answering leaves the screen saying "Checking…" with no way to retry
/// and no way in. That is a denial of login, not an inconvenience.
///
/// Generous, though. greetd is relaying PAM, and PAM can legitimately
/// take a while — `pam_unix` sleeps about two seconds after a wrong
/// password, and a directory-backed stack can take far longer. Cutting
/// off a slow-but-working login would be worse than the hang.
const REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long the greeter waits to hear that greetd took `StartSession`.
///
/// Far shorter than [`REPLY_TIMEOUT`], because no PAM sits behind this
/// one: greetd only records the command it will run once the greeter
/// exits, and answers at once. A greetd that cannot do that in ten
/// seconds is not going to.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Where greetd told us to talk to it.
///
/// Set by greetd for the process it launches, so its absence means this
/// is not running as a greeter — which is worth saying plainly rather
/// than failing to connect to a path nobody mentioned.
pub const SOCKET_ENV: &str = "GREETD_SOCK";

#[derive(Debug, thiserror::Error)]
pub enum GreetdError {
    #[error("{SOCKET_ENV} is not set — this is only meaningful when greetd starts it")]
    NotAGreeter,
    #[error("couldn't reach greetd on {path}: {source}")]
    Connect {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// What the UI thread asks the greetd thread to do.
///
/// No `Ask::Cancel`, because cancelling is not something the UI ever asks
/// for on its own: [`run`] sends `CancelSession` itself immediately
/// before every `CreateSession`, so a retry cannot inherit a
/// half-configured session from the attempt before it. See the comment
/// there for what believing greetd's documentation on this cost.
enum Ask {
    Start(String),
    Answer(Option<Wiped>),
    /// Not part of the conversation: the session to launch once this
    /// process exits. Answered on its own channel, so the reply can
    /// never be read as a step of the conversation.
    Launch { command: Vec<String>, environment: Vec<String>, reply: Sender<Result<(), String>> },
}

/// An answer on its way to greetd, erased when it is dropped.
///
/// The conversation hands this backend a `&str` that its own `Secret`
/// erases; the copy that crosses to the greetd thread is this crate's,
/// and a bare `String` would be freed with the password still in it.
/// What `greetd_ipc` does with it while serialising is out of reach.
struct Wiped(String);

impl Wiped {
    fn new(text: &str) -> Wiped {
        Wiped(text.to_string())
    }
}

impl Drop for Wiped {
    fn drop(&mut self) {
        wipe(std::mem::take(&mut self.0).into_bytes());
    }
}

/// Zeroes a buffer in a way the optimiser may not remove — a plain
/// `fill(0)` on memory about to be freed is a dead store, and it is
/// allowed to vanish.
fn wipe(mut bytes: Vec<u8>) {
    for byte in bytes.iter_mut() {
        // SAFETY: a valid, aligned, exclusively borrowed `u8`.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}

/// Where a requested launch stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// Asked; greetd has not answered yet.
    Waiting,
    /// greetd will start the session when this process exits.
    Started,
    /// greetd said no. The login has to be done again.
    Refused(String),
}

/// greetd, answered on its own thread.
pub struct GreetdBackend {
    to_greetd: Sender<Ask>,
    from_greetd: Receiver<Response>,
    /// Queued for the next `poll` when the thread cannot be reached.
    lost: Option<Response>,
    /// Whether the thread's death has already been reported. A `poll`
    /// that answers a disconnected channel every time turns
    /// `Conversation::pump` into a spin, because pump loops while poll
    /// keeps yielding.
    reported_loss: bool,
    /// Set by the greetd thread when it gives up on the socket.
    ///
    /// Not something the conversation can express: it reports a failure
    /// and offers a retry, and every retry after this one fails the same
    /// way, because the thread that would carry it has gone. The host
    /// reads this and exits, so greetd starts a fresh greeter on a fresh
    /// connection — the only way back, and greetd's job.
    gone: Arc<AtomicBool>,
    /// The launch in flight: its reply, and when to stop waiting for it.
    launching: Option<(Receiver<Result<(), String>>, Instant)>,
}

impl GreetdBackend {
    /// Connects to the socket greetd named.
    pub fn connect() -> Result<GreetdBackend, GreetdError> {
        let path = std::env::var(SOCKET_ENV).map_err(|_| GreetdError::NotAGreeter)?;
        let stream = UnixStream::connect(&path)
            .map_err(|source| GreetdError::Connect { path: path.clone(), source })?;
        Ok(GreetdBackend::over(stream))
    }

    /// Talks to an already-connected socket.
    ///
    /// Split out so the whole exchange can be driven against a stand-in
    /// greetd speaking the real wire format, with no daemon installed
    /// and nothing touching how this machine logs in.
    pub fn over(stream: UnixStream) -> GreetdBackend {
        GreetdBackend::over_with_timeout(stream, REPLY_TIMEOUT)
    }

    /// [`over`](Self::over) with the reply bound spelled out, so a test
    /// need not wait a minute to exercise a missed reply.
    pub fn over_with_timeout(stream: UnixStream, timeout: std::time::Duration) -> GreetdBackend {
        // Best effort: a socket that will not take a timeout is still
        // better than no greeter, and the loop below reports a failure
        // either way.
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(timeout));
        let (to_greetd, asks) = std::sync::mpsc::channel();
        let (answers, from_greetd) = std::sync::mpsc::channel();
        let gone = Arc::new(AtomicBool::new(false));
        let thread_gone = Arc::clone(&gone);
        std::thread::Builder::new()
            .name("greetd".into())
            .spawn(move || run(stream, asks, answers, thread_gone))
            .expect("failed to start the greetd thread");
        GreetdBackend { to_greetd, from_greetd, lost: None, reported_loss: false, gone, launching: None }
    }

    /// Asks greetd to run `command` once this process exits.
    ///
    /// Only valid after the conversation reported success. greetd starts
    /// the session when the greeter goes away — but only once it has
    /// *taken* the request, so the caller waits for [`launched`] to say
    /// `Started` before exiting. Exiting straight after posting it raced
    /// the greetd thread: a greeter gone before the request reached the
    /// socket left greetd with an authenticated session and nothing to
    /// run, and a refusal was never seen by anyone.
    ///
    /// [`launched`]: Self::launched
    pub fn launch(&mut self, command: Vec<String>, environment: Vec<String>) {
        self.launch_within(command, environment, LAUNCH_TIMEOUT);
    }

    /// [`launch`](Self::launch) with the bound spelled out, for tests.
    pub fn launch_within(&mut self, command: Vec<String>, environment: Vec<String>, bound: Duration) {
        let (reply, outcome) = std::sync::mpsc::channel();
        self.launching = Some((outcome, Instant::now() + bound));
        self.post(Ask::Launch { command, environment, reply });
    }

    /// Where the last [`launch`](Self::launch) stands. Never blocks.
    ///
    /// No answer within the bound is not a refusal to show and retry:
    /// the greetd thread is still inside that exchange, and a socket with
    /// a reply outstanding is one this greeter can no longer trust (see
    /// [`run`]). It counts as the connection lost, so the host exits and
    /// greetd starts over.
    pub fn launched(&mut self) -> Launch {
        let Some((outcome, deadline)) = &self.launching else {
            return Launch::Waiting;
        };
        let result = match outcome.try_recv() {
            Ok(Ok(())) => Launch::Started,
            Ok(Err(reason)) => Launch::Refused(reason),
            Err(std::sync::mpsc::TryRecvError::Empty) if Instant::now() < *deadline => return Launch::Waiting,
            // Out of time, or the thread dropped the reply on its way
            // out: either way nobody is going to answer.
            Err(_) => {
                self.gone.store(true, Ordering::SeqCst);
                return Launch::Waiting;
            }
        };
        self.launching = None;
        result
    }

    /// Puts a failure in front of the conversation, through the same
    /// door as everything greetd says — for a refusal that arrived
    /// outside the conversation, like a launch greetd would not take.
    pub fn report(&mut self, reason: String) {
        self.lost = Some(Response::Failure { reason });
    }

    /// Whether the connection to greetd is beyond use.
    ///
    /// Once true it stays true: a retry posts to a thread that has
    /// returned. The host's move is to exit unsuccessfully so greetd
    /// restarts the greeter; a login screen that only says "the login
    /// service stopped responding" and takes no further input is a
    /// machine nobody can get into until someone changes VT.
    pub fn connection_lost(&self) -> bool {
        self.reported_loss || self.gone.load(Ordering::SeqCst)
    }

    fn post(&mut self, ask: Ask) {
        if self.to_greetd.send(ask).is_err() {
            // A failed send and a disconnected receiver are the same
            // fact, so they share one "already said" flag rather than
            // each counting to one separately.
            self.reported_loss = true;
            self.lost = Some(unreachable_service());
        }
    }
}

impl Backend for GreetdBackend {
    fn start(&mut self, username: &str) {
        self.post(Ask::Start(username.to_string()));
    }

    fn answer(&mut self, answer: &str) {
        self.post(Ask::Answer(Some(Wiped::new(answer))));
    }

    /// Acknowledges a message greetd did not want an answer to.
    ///
    /// greetd still expects a `PostAuthMessageResponse` for an info or
    /// error message — with no response in it. Skipping it leaves the
    /// conversation waiting forever on a question the user has already
    /// read and dismissed.
    fn proceed(&mut self) {
        self.post(Ask::Answer(None));
    }

    fn poll(&mut self) -> Option<Response> {
        if let Some(lost) = self.lost.take() {
            return Some(lost);
        }
        match self.from_greetd.try_recv() {
            Ok(response) => Some(response),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) if !self.reported_loss => {
                self.reported_loss = true;
                Some(unreachable_service())
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
        }
    }
}

/// One request, one reply, on the socket.
///
/// Every write is paired with exactly one read and nothing else touches
/// the stream in between, which is the property the whole module depends
/// on — see [`run`] for what a missed reply would cost.
fn exchange(stream: &mut UnixStream, request: &Request) -> Result<Greetd, ()> {
    request.write_to(stream).map_err(|_| ())?;
    Greetd::read_from(stream).map_err(|_| ())
}

/// The greetd thread. Kept small: everything here can start a session.
fn run(mut stream: UnixStream, asks: Receiver<Ask>, answers: Sender<Response>, gone: Arc<AtomicBool>) {
    // Set before the failure is sent, so a host that reads the failure
    // and then asks `connection_lost` cannot see the one without the
    // other.
    let give_up = || {
        gone.store(true, Ordering::SeqCst);
        let _ = answers.send(unreachable_service());
    };
    while let Ok(ask) = asks.recv() {
        let request = match ask {
            Ask::Start(username) => {
                // Cancel before creating, unconditionally, and ignore
                // whatever comes back.
                //
                // greetd's own documentation says "the session is
                // cancelled automatically on error", and this code
                // believed it. On this machine it is not true of a
                // failed password: one wrong attempt left the session
                // half-configured, and every attempt after it — correct
                // password included — came back "a session is already
                // being configured". A single typo wedged the login
                // screen until greetd was restarted from another VT,
                // and the message blamed the session rather than saying
                // "wrong password, try again".
                //
                // Cancelling first is correct whichever way greetd
                // actually behaves: if it already cancelled there is
                // nothing to cancel and it answers with an error nobody
                // reads, and if it did not, this is what clears the way.
                // It also cleans up after a *previous* greeter that
                // died mid-conversation, which nothing did before.
                //
                // The reply is discarded rather than translated,
                // because a failure to cancel is not a failure to log
                // in: `CreateSession` below is what decides that, and
                // reporting a cancel error would put a confusing
                // sentence in front of someone whose password was
                // about to work.
                if exchange(&mut stream, &Request::CancelSession).is_err() {
                    give_up();
                    return;
                }
                Request::CreateSession { username }
            }
            Ask::Answer(response) => {
                // greetd_ipc wants an owned `String`. It gets a copy, the
                // `Wiped` original is erased at the end of this arm, and
                // the copy is erased once it has been written, below.
                Request::PostAuthMessageResponse { response: response.as_ref().map(|answer| answer.0.clone()) }
            }
            Ask::Launch { command, environment, reply } => {
                let request = Request::StartSession { cmd: command, env: environment };
                let Ok(answer) = exchange(&mut stream, &request) else {
                    gone.store(true, Ordering::SeqCst);
                    let _ = reply.send(Err("the login service stopped responding".into()));
                    return;
                };
                let _ = reply.send(match answer {
                    Greetd::Success => Ok(()),
                    Greetd::Error { description, .. } => Err(format!("Couldn't start the session: {description}")),
                    // A question after the login already succeeded is not
                    // part of any exchange greetd documents.
                    Greetd::AuthMessage { .. } => {
                        Err("Couldn't start the session: the login service asked something unexpected".into())
                    }
                });
                continue;
            }
        };

        let exchanged = exchange(&mut stream, &request);
        if let Request::PostAuthMessageResponse { response: Some(text) } = request {
            wipe(text.into_bytes());
        }
        let Ok(reply) = exchanged else {
            // Returning rather than trying again, and this is the
            // security-relevant part. A timed-out read leaves the
            // connection desynchronised: greetd's late answer to *this*
            // question would be read as the answer to the next one, so
            // a stale `Success` could be matched to a later attempt.
            // Once a reply is missed the socket is unusable, and the way
            // back is a fresh greeter — which is greetd's job when this
            // process exits, and why `give_up` marks the connection
            // gone: `main` reads that and exits. It once did not, and a
            // single missed reply left a login screen that failed every
            // retry until someone changed VT.
            give_up();
            return;
        };
        if answers.send(translate(reply)).is_err() {
            return;
        }
    }
}

fn unreachable_service() -> Response {
    Response::Failure { reason: "the login service stopped responding".into() }
}

/// greetd's reply in the terms the shared screen understands.
///
/// The mapping is almost the identity, which is the point: greetd is
/// relaying a PAM conversation, and this is the same conversation.
fn translate(reply: Greetd) -> Response {
    match reply {
        Greetd::Success => Response::Success,
        Greetd::AuthMessage { auth_message_type, auth_message } => match auth_message_type {
            // Secret and visible are both questions; which one decides
            // whether what is typed is shown. Getting this backwards
            // would either hide a 2FA code or print a password.
            AuthMessageType::Secret => Response::Ask(Prompt::secret(auth_message)),
            AuthMessageType::Visible => Response::Ask(Prompt::visible(auth_message)),
            // Info and error are statements. They still have to be
            // acknowledged, which `proceed` does.
            AuthMessageType::Info => Response::Tell { text: auth_message, error: false },
            AuthMessageType::Error => Response::Tell { text: auth_message, error: true },
        },
        // An auth error is a wrong password and reads as one; anything
        // else is the service itself having a problem, and saying
        // "incorrect password" for a broken PAM stack would send someone
        // retyping a password that was right.
        Greetd::Error { error_type: ErrorType::AuthError, description } => {
            Response::Failure { reason: plainly(&description) }
        }
        Greetd::Error { error_type: ErrorType::Error, description } => {
            Response::Failure { reason: format!("Login failed: {description}") }
        }
    }
}

/// greetd passes PAM's wording through, and PAM's wording for the common
/// case reads like the program broke. The lock screen makes the same
/// substitution for the same reason.
fn plainly(description: &str) -> String {
    if description.to_lowercase().contains("authentication failure") {
        return "Incorrect password".into();
    }
    description.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyprforge_authui::conversation::{Conversation, State};
    use std::os::unix::net::UnixListener;

    /// A stand-in greetd, speaking the real wire format over a real
    /// socket.
    ///
    /// Not a mock of this crate's own types: it uses `greetd_ipc`'s
    /// codec to read requests and write replies, so a mistake in the
    /// framing or the JSON tags would fail here rather than at the login
    /// screen. Which matters more than usual — the thing this stands in
    /// for is the only way into the machine.
    fn fake_greetd(replies: Vec<Greetd>) -> (UnixStream, std::thread::JoinHandle<Vec<Request>>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("greetd.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let client = UnixStream::connect(&path).expect("connect");

        let server = std::thread::spawn(move || {
            // `dir` is moved in so the socket outlives the bind.
            let _dir = dir;
            let (mut stream, _) = listener.accept().expect("accept");
            let mut seen = Vec::new();
            let mut replies = replies.into_iter();
            // `ExactSizeIterator`, so `len()` below is the count remaining.
            while let Ok(request) = Request::read_from(&mut stream) {
                // `CancelSession` is answered by the harness rather than
                // from the script, because the real greetd answers it
                // whether or not there is anything to cancel — and the
                // client now sends one before every `CreateSession`.
                // Scripting it would mean every test here carrying a
                // reply for a request it is not about, and the first
                // test that forgot would fail somewhere unrelated.
                //
                // An error is what greetd gives for cancelling nothing,
                // so that is what this gives, and it is the case the
                // client must shrug off.
                let is_cancel = matches!(request, Request::CancelSession);
                seen.push(request);
                let (reply, was_scripted) = if is_cancel {
                    (
                        Greetd::Error {
                            error_type: ErrorType::Error,
                            description: "no session to cancel".into(),
                        },
                        false,
                    )
                } else {
                    match replies.next() {
                        Some(reply) => (reply, true),
                        None => break,
                    }
                };
                if reply.write_to(&mut stream).is_err() {
                    break;
                }
                // Stop once the script is spent, rather than blocking on
                // a read that will never come. A harness that waits
                // forever turns "the client asked one thing too few"
                // into a hung test suite instead of a failed assertion,
                // and a hang is far harder to read than a failure.
                if was_scripted && replies.len() == 0 {
                    break;
                }
            }
            seen
        });
        (client, server)
    }

    /// How long to wait for the worker thread before calling it wedged.
    ///
    /// Thirty seconds, against the twenty milliseconds this actually
    /// takes — and the absurdity of that ratio is the point. The bound
    /// is here so a worker that never answers fails the suite instead
    /// of hanging it forever (CLAUDE.md's rule about never waiting on
    /// another thread of control without one). It is *not* a claim
    /// about how fast the handshake is, and nothing is measured against
    /// it.
    ///
    /// It used to be one second, which looked generous — fifty times
    /// the real figure, and it survived fifty-two local runs including
    /// twelve with all sixteen cores saturated — and still went red on
    /// a shared CI runner. A bound a loaded machine can cross is a test
    /// that reports someone else's scheduling as a bug in this code.
    const ANSWER_WITHIN: std::time::Duration = std::time::Duration::from_secs(30);

    /// Waits for the worker thread to answer.
    ///
    /// The backend is deliberately non-blocking, so a test has to pump
    /// like the real host does rather than assume the reply is instant.
    ///
    /// It waits for the conversation to leave `Working` — the state that
    /// means "an answer is owed" — and not for `pump` to report a change.
    /// `new`, `submit` and `retry` each pump once themselves, so a worker
    /// quick enough to answer before that pump has its reply applied
    /// there, and every pump after it truthfully reports nothing new.
    /// Waiting on the change hung for the whole thirty seconds whenever
    /// that happened: about one suite run in fifteen, and 3 runs in 300
    /// of this module pinned to a single CPU.
    fn settle<B: Backend>(conversation: &mut Conversation<B>) {
        let giving_up_at = std::time::Instant::now() + ANSWER_WITHIN;
        while std::time::Instant::now() < giving_up_at {
            conversation.pump();
            if !matches!(conversation.state(), State::Working) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("greetd never answered in {ANSWER_WITHIN:?}");
    }

    #[test]
    fn a_password_login_runs_end_to_end() {
        let (stream, server) = fake_greetd(vec![
            Greetd::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "Password:".into(),
            },
            Greetd::Success,
        ]);
        let mut conversation = Conversation::new(GreetdBackend::over(stream), "apost");
        settle(&mut conversation);

        match conversation.state() {
            State::Asking { prompt, .. } => {
                assert_eq!(prompt.text, "Password:");
                assert!(prompt.secret, "a password must not be shown as it is typed");
            }
            other => panic!("expected a question, got {other:?}"),
        }

        conversation.type_into("hunter2".into());
        conversation.submit();
        settle(&mut conversation);
        assert!(conversation.state().is_authenticated());

        // And the requests that actually went down the socket were the
        // ones greetd expects, in order.
        let seen = server.join().expect("server thread");
        assert!(matches!(&seen[0], Request::CancelSession), "a stale session is cleared before asking for a new one");
        assert!(matches!(&seen[1], Request::CreateSession { username } if username == "apost"));
        assert!(
            matches!(&seen[2], Request::PostAuthMessageResponse { response } if response.as_deref() == Some("hunter2"))
        );
    }

    /// The bug that locked someone out of their own machine.
    ///
    /// greetd's documentation says "the session is cancelled
    /// automatically on error", and this module believed it. On a real
    /// machine it is not true of a failed password: one wrong attempt
    /// left a half-configured session behind, and every attempt after
    /// it — with the *correct* password — came back "a session is
    /// already being configured". The login screen was wedged until
    /// greetd was restarted from another VT.
    ///
    /// The fix is not to trust the answer either way: cancel before
    /// every create, and this pins that a retry does so.
    #[test]
    fn a_retry_after_a_wrong_password_clears_the_session_first() {
        let (stream, server) = fake_greetd(vec![
            // The first attempt is asked for a password and told no.
            Greetd::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "Password:".into(),
            },
            Greetd::Error {
                error_type: ErrorType::AuthError,
                description: "authentication failed".into(),
            },
            // The retry is asked again.
            Greetd::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "Password:".into(),
            },
        ]);

        let mut conversation = Conversation::new(GreetdBackend::over(stream), "apost");
        settle(&mut conversation);
        conversation.type_into("wrong".to_string());
        conversation.submit();
        settle(&mut conversation);
        assert!(matches!(conversation.state(), State::Failed { .. }), "a wrong password fails");

        conversation.retry();
        settle(&mut conversation);

        let seen = server.join().expect("the fake greetd thread");
        let creates: Vec<usize> = seen
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, Request::CreateSession { .. }))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(creates.len(), 2, "one create for the first attempt and one for the retry");
        for i in creates {
            assert!(i > 0, "a CreateSession must never be the first thing sent");
            assert!(
                matches!(seen[i - 1], Request::CancelSession),
                "every CreateSession must be preceded by a CancelSession; request {} was {:?}",
                i - 1,
                seen[i - 1]
            );
        }
    }

    /// greetd relays whatever PAM asks, which is not always a password.
    /// A greeter that assumed otherwise would break the first time
    /// someone enabled a token.
    #[test]
    fn a_visible_question_is_not_treated_as_a_password() {
        let (stream, _server) = fake_greetd(vec![Greetd::AuthMessage {
            auth_message_type: AuthMessageType::Visible,
            auth_message: "Verification code:".into(),
        }]);
        let mut conversation = Conversation::new(GreetdBackend::over(stream), "apost");
        settle(&mut conversation);
        match conversation.state() {
            State::Asking { prompt, .. } => {
                assert_eq!(prompt.text, "Verification code:");
                assert!(!prompt.secret, "a 2FA code is not hidden input");
            }
            other => panic!("expected a visible question, got {other:?}"),
        }
    }

    /// An info message is a statement, and greetd still wants it
    /// acknowledged with a response containing nothing. Sending an empty
    /// *string* instead would answer a question nobody asked.
    #[test]
    fn a_message_is_acknowledged_with_no_answer() {
        let (stream, server) = fake_greetd(vec![
            Greetd::AuthMessage {
                auth_message_type: AuthMessageType::Info,
                auth_message: "Password expires in 3 days".into(),
            },
            Greetd::Success,
        ]);
        let mut conversation = Conversation::new(GreetdBackend::over(stream), "apost");
        settle(&mut conversation);
        assert_eq!(
            conversation.state(),
            &State::Telling { text: "Password expires in 3 days".into(), error: false }
        );

        conversation.acknowledge();
        settle(&mut conversation);
        assert!(conversation.state().is_authenticated());

        let seen = server.join().expect("server thread");
        assert!(
            matches!(&seen[2], Request::PostAuthMessageResponse { response } if response.is_none()),
            "an info message must be answered with no response, got {:?}",
            seen[2]
        );
    }

    /// The message a user sees most often should read as "you typed it
    /// wrong", not as "the program broke" — the same substitution the
    /// lock screen makes, for the same reason.
    #[test]
    fn a_wrong_password_reads_plainly() {
        let (stream, _server) = fake_greetd(vec![Greetd::Error {
            error_type: ErrorType::AuthError,
            description: "Authentication failure".into(),
        }]);
        let mut conversation = Conversation::new(GreetdBackend::over(stream), "apost");
        settle(&mut conversation);
        assert_eq!(
            conversation.state(),
            &State::Failed { reason: "Incorrect password".into() }
        );
    }

    /// A broken PAM stack is not a wrong password, and saying so would
    /// send someone retyping a password that was right. This is exactly
    /// the mistake the lock screen made until its own account check was
    /// reported separately.
    #[test]
    fn a_service_error_is_not_blamed_on_the_password() {
        let (stream, _server) = fake_greetd(vec![Greetd::Error {
            error_type: ErrorType::Error,
            description: "PAM: module is not known".into(),
        }]);
        let mut conversation = Conversation::new(GreetdBackend::over(stream), "apost");
        settle(&mut conversation);
        match conversation.state() {
            State::Failed { reason } => {
                assert!(reason.contains("module is not known"), "{reason}");
                assert!(
                    !reason.to_lowercase().contains("incorrect password"),
                    "a service error must not be blamed on the password: {reason}"
                );
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    /// A greetd that stops answering must not hang the login screen.
    ///
    /// Found by pointing the greeter at a greetd that accepted the
    /// connection, read the request, and said nothing: the worker thread
    /// blocked forever and the screen sat on "Checking…" with no way to
    /// retry and no way in. Which is a denial of login, and a violation
    /// of this project's own rule that nothing waits on another process
    /// without a bound.
    #[test]
    fn a_greetd_that_never_answers_reports_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Bound to a short relative name: AF_UNIX paths cap near 108
        // bytes and a temp dir can exceed it.
        let cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(dir.path()).expect("chdir");
        let listener = UnixListener::bind("sock").expect("bind");
        let client = UnixStream::connect("sock").expect("connect");
        std::env::set_current_dir(cwd).expect("restore cwd");

        // Accepted and then ignored, which is the case that hung.
        let server = listener.accept().expect("accept").0;

        // A real reply timeout is a minute, which no test should wait
        // for; the behaviour under test is what happens when the read
        // fails, so the read is made to fail promptly.
        let mut conversation = Conversation::new(
            GreetdBackend::over_with_timeout(client, std::time::Duration::from_millis(200)),
            "apost",
        );
        settle(&mut conversation);
        assert!(
            matches!(conversation.state(), State::Failed { .. }),
            "a silent greetd must be reported, got {:?}",
            conversation.state()
        );
        drop(server);
    }

    /// After a missed reply the socket is abandoned, and every retry
    /// fails the same way — so the host has to be told, to exit and let
    /// greetd start a greeter with a connection that works. It was not:
    /// the screen said "stopped responding" and nothing ever got past
    /// it until someone changed VT.
    #[test]
    fn a_missed_reply_marks_the_connection_lost_for_good() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s");
        let listener = UnixListener::bind(&path).expect("bind");
        let client = UnixStream::connect(&path).expect("connect");
        let server = listener.accept().expect("accept").0;

        let mut conversation = Conversation::new(
            GreetdBackend::over_with_timeout(client, std::time::Duration::from_millis(200)),
            "apost",
        );
        settle(&mut conversation);
        assert!(matches!(conversation.state(), State::Failed { .. }));
        assert!(conversation.backend().connection_lost(), "the host was never told to give up");

        conversation.retry();
        settle(&mut conversation);
        assert!(conversation.backend().connection_lost(), "a retry cannot bring it back");
        drop(server);
    }

    /// A healthy connection is not reported lost — exiting on a false
    /// alarm would throw someone off the login screen mid-password.
    #[test]
    fn a_working_connection_is_not_reported_lost() {
        let (stream, _server) = fake_greetd(vec![Greetd::AuthMessage {
            auth_message_type: AuthMessageType::Secret,
            auth_message: "Password:".into(),
        }]);
        let mut conversation = Conversation::new(GreetdBackend::over(stream), "apost");
        settle(&mut conversation);
        assert!(!conversation.backend().connection_lost());
    }

    /// Logs in through the fake greetd and asks for the session.
    fn log_in_and_launch(start_session: Greetd) -> (Conversation<GreetdBackend>, std::thread::JoinHandle<Vec<Request>>) {
        let (stream, server) = fake_greetd(vec![
            Greetd::AuthMessage { auth_message_type: AuthMessageType::Secret, auth_message: "Password:".into() },
            Greetd::Success,
            start_session,
        ]);
        let mut conversation = Conversation::new(GreetdBackend::over(stream), "apost");
        settle(&mut conversation);
        conversation.type_into("dummy".into());
        conversation.submit();
        settle(&mut conversation);
        assert!(conversation.state().is_authenticated(), "the fixture must log in first");
        conversation.backend().launch(vec!["true".into()], Vec::new());
        (conversation, server)
    }

    fn wait_for_launch(backend: &mut GreetdBackend) -> Launch {
        let giving_up_at = std::time::Instant::now() + ANSWER_WITHIN;
        while std::time::Instant::now() < giving_up_at {
            match backend.launched() {
                Launch::Waiting if !backend.connection_lost() => std::thread::sleep(std::time::Duration::from_millis(5)),
                other => return other,
            }
        }
        panic!("the launch never settled in {ANSWER_WITHIN:?}");
    }

    /// The greeter exits only once greetd has taken `StartSession`. It
    /// used to post the request and exit in the same breath, racing the
    /// thread that was to send it.
    #[test]
    fn a_launch_is_started_only_once_greetd_has_taken_it() {
        let (mut conversation, server) = log_in_and_launch(Greetd::Success);
        assert_eq!(wait_for_launch(conversation.backend()), Launch::Started);
        let seen = server.join().expect("server thread");
        assert!(
            matches!(seen.last(), Some(Request::StartSession { cmd, .. }) if cmd == &["true".to_string()]),
            "the session was never asked for"
        );
    }

    /// A refusal is something to show, not a reason to exit as though
    /// the session had started.
    #[test]
    fn a_launch_greetd_refuses_is_reported_rather_than_exited_on() {
        let (mut conversation, _server) = log_in_and_launch(Greetd::Error {
            error_type: ErrorType::Error,
            description: "no such session".into(),
        });
        match wait_for_launch(conversation.backend()) {
            Launch::Refused(reason) => assert!(reason.contains("no such session"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(!conversation.backend().connection_lost(), "a refusal is an answer; the connection is fine");
    }

    /// A launch greetd never answers is bounded, and counts as the
    /// connection lost: the thread is still inside that exchange, so the
    /// socket cannot be trusted with another request.
    #[test]
    fn a_launch_greetd_never_answers_counts_as_the_connection_lost() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s");
        let listener = UnixListener::bind(&path).expect("bind");
        let client = UnixStream::connect(&path).expect("connect");
        let (release, released) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            // Cancel, then create: both answered, the second with a
            // straight success (autologin's shape). The launch is read
            // and never answered.
            for reply in [
                Greetd::Error { error_type: ErrorType::Error, description: "nothing to cancel".into() },
                Greetd::Success,
            ] {
                Request::read_from(&mut stream).expect("a request");
                reply.write_to(&mut stream).expect("a reply");
            }
            let _ = Request::read_from(&mut stream);
            let _ = released.recv();
        });

        let mut conversation = Conversation::new(GreetdBackend::over(client), "apost");
        settle(&mut conversation);
        assert!(conversation.state().is_authenticated());
        conversation.backend().launch_within(vec!["true".into()], Vec::new(), std::time::Duration::from_millis(50));
        assert_eq!(wait_for_launch(conversation.backend()), Launch::Waiting);
        assert!(conversation.backend().connection_lost());

        drop(release);
        server.join().expect("server thread");
    }

    /// greetd going away must not hang the login screen, which would be
    /// a machine nobody can get into.
    ///
    /// Built without a server thread on purpose. The first version used
    /// `fake_greetd(vec![])` and then joined it, which hung in the
    /// test's own `accept` rather than in the code under test — a test
    /// that hangs while proving something does not hang is worse than no
    /// test. Here the peer is closed explicitly and nothing is waited
    /// on.
    #[test]
    fn a_greetd_that_disappears_reports_failure_rather_than_hanging() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("greetd.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let client = UnixStream::connect(&path).expect("connect");
        let server = listener.accept().expect("accept").0;

        // greetd's side goes away, as it would if the daemon died.
        drop(server);
        drop(listener);

        let mut conversation = Conversation::new(GreetdBackend::over(client), "apost");
        settle(&mut conversation);
        assert!(
            matches!(conversation.state(), State::Failed { .. }),
            "a dead login service must be reported, got {:?}",
            conversation.state()
        );
    }
}
