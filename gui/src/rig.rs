//! Keying the transmitter.
//!
//! Everything above this module knows one thing about a rig: it can be asked to
//! transmit, and it can be asked whether it is. What is underneath — a CAT link
//! through hamlib, a serial line pulled high, nothing at all — is this module's
//! business and nobody else's.
//!
//! The one implemented here is [`RigCtld`], which talks to hamlib's `rigctld`
//! over TCP. That is a deliberate choice of the daemon over the library: the
//! wire protocol is one line of text per command, so two hundred rigs are
//! reachable for the cost of a socket, with no C linkage, no FFI, nothing to
//! build, and — the part that decides it — a protocol simple enough to stand a
//! fake server up against in a test. Every path through this module is exercised
//! that way, including the ones a real rig makes hard to produce on purpose:
//! a rejected command, a daemon restarted underneath us, a rig that stops
//! answering with the key down.
//!
//! The operator runs `rigctld -m <model> -r <device>` and points this at it,
//! exactly as they already do for WSJT-X or fldigi.
//!
//! A [`Transmitter`] does as it is told and reports what happened, and that is
//! all it does. Above it sits [`Ptt`], which is where the promise lives: a
//! key-down is always followed by a key-up. It owns the transmitter on a thread
//! of its own, so that
//!
//! * the interface never waits on a socket to key,
//! * a transmission scheduled for a cycle boundary half a minute away is keyed
//!   by something whose only job is to be awake then,
//! * and there is exactly one place that knows whether the rig is down, which
//!   is the only way to promise anything about it.
//!
//! Three things un-key without being asked, because each of them has left a rig
//! transmitting into an empty band on somebody's bench: a key-down that outlasts
//! its limit, an owner that goes away, and a panic anywhere in the thread.

use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Longest to wait for a rig to answer.
///
/// Generous for a local daemon and short enough that a key-down does not hang
/// the caller. It is a whole second because `rigctld` sits in front of a serial
/// link at 4800 baud on older rigs, and a CI-V transaction there is not
/// instant.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// How the transmitter is keyed.
///
/// [`Keying::None`] is what this app did before it could key anything: the
/// audio goes to the output device and the rig decides for itself, which is
/// what VOX and a straight-through interface want. It stays the default, so
/// nothing starts talking to hardware because a new version was installed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Keying {
    #[default]
    None,
    RigCtld { host: String, port: u16 },
}

/// Why a rig could not be made to do something.
///
/// Kept apart rather than flattened into a string because the four mean
/// different things to the operator, and two of them are not faults in the
/// usual sense: a rig that rejects a command is answering, and a link that has
/// not been made is a setting to fix rather than a failure to report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// No link: the daemon has not been reached, or the link has dropped.
    Link(String),
    /// Asked, and no answer in time.
    Timeout,
    /// Answered, and the answer was no: what was being asked of it, and
    /// hamlib's own code for why not.
    Rejected { doing: &'static str, code: i32 },
    /// Answered something the protocol does not allow.
    Protocol(String),
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::Link(e) => write!(f, "no link to the rig: {e}"),
            Fault::Timeout => write!(f, "the rig did not answer"),
            Fault::Rejected { doing, code } => {
                write!(f, "the rig would not {doing}")?;
                match hamlib_says(*code) {
                    Some(why) => write!(f, ": {why}")?,
                    None => write!(f, " (hamlib code {code})")?,
                }
                // The overwhelmingly common cause, and one nothing on this side
                // can fix: hamlib will refuse to key a rig it has not been told
                // how to key, including its own dummy. Worth saying outright —
                // "not available on this rig" sends an operator to the radio,
                // and the radio is not the problem.
                if *doing == KEYING && matches!(code, -1 | -4 | -11) {
                    write!(
                        f,
                        " — check that rigctld has a PTT method: \
                         `--set-conf=ptt_type=RIG` (or RTS, or DTR)"
                    )?;
                }
                Ok(())
            }
            Fault::Protocol(e) => write!(f, "the rig answered nonsense: {e}"),
        }
    }
}

/// What hamlib's error codes mean, for the ones worth telling apart.
///
/// From `rig.h`. Only the ones whose meaning changes what the operator should
/// do are named; anything else is reported as its number rather than guessed
/// at, because a wrong gloss on an error is worse than no gloss.
fn hamlib_says(code: i32) -> Option<&'static str> {
    Some(match code {
        -1 => "invalid parameter",
        -4 => "not implemented for this rig",
        -5 => "it timed out talking to the rig",
        -6 => "I/O error talking to the rig",
        -9 => "the rig rejected the command",
        -11 => "not available on this rig",
        _ => return None,
    })
}

/// What a command was trying to do, for a refusal to quote back.
const KEYING: &str = "key";
const READING_PTT: &str = "say whether it is transmitting";
const READING_DIAL: &str = "say what it is tuned to";
const TUNING: &str = "tune";

/// Something that can put a transmitter on the air.
pub trait Transmitter: Send {
    /// Key or unkey. Idempotent: keying a keyed rig is not an error, and is how
    /// a caller makes sure of the state it thinks it is in.
    fn key(&mut self, on: bool) -> Result<(), Fault>;

    /// Whether the rig says it is transmitting — which is not the same question
    /// as whether it was asked to. A front panel PTT, a foot switch, or a line
    /// stuck high all show up here and nowhere else.
    ///
    /// `None` from a transport that cannot be asked.
    fn keyed(&mut self) -> Result<Option<bool>, Fault>;

    /// What the rig is tuned to, in Hz, if it can be asked. `None` from a
    /// transport that cannot.
    fn dial_hz(&mut self) -> Result<Option<f64>, Fault> {
        Ok(None)
    }

    /// Tune it.
    fn tune(&mut self, hz: f64) -> Result<(), Fault> {
        let _ = hz;
        Err(Fault::Rejected { doing: TUNING, code: -4 })
    }

    /// The mode it is in — `USB`, `PKTUSB`, `FM` — and nothing about what that
    /// means, which is the rig's business and not this app's.
    fn dial_mode(&mut self) -> Result<Option<String>, Fault> {
        Ok(None)
    }

    /// What this is, for the log and the settings menu.
    fn describe(&self) -> String;
}

/// A transmitter nobody is keying.
///
/// Not a stub: it is the honest description of an interface where the rig
/// decides for itself, and it answers `keyed()` with "cannot be asked" rather
/// than with a guess.
pub struct Unkeyed;

impl Transmitter for Unkeyed {
    fn key(&mut self, _on: bool) -> Result<(), Fault> {
        Ok(())
    }
    fn keyed(&mut self) -> Result<Option<bool>, Fault> {
        Ok(None)
    }
    fn describe(&self) -> String {
        "not keyed (VOX or a straight-through interface)".to_string()
    }
}

/// hamlib's `rigctld`, over TCP.
///
/// The link is made when it is first needed and remade when it breaks, so a
/// daemon restarted while the app is running costs one command rather than a
/// restart. Nothing is held open in the meantime that a rig would notice.
pub struct RigCtld {
    addr: String,
    io: Option<BufReader<TcpStream>>,
    timeout: Duration,
}

impl RigCtld {
    /// A link to `host:port`, not yet made — see [`RigCtld::link`].
    pub fn new(host: &str, port: u16) -> RigCtld {
        RigCtld { addr: format!("{host}:{port}"), io: None, timeout: REPLY_TIMEOUT }
    }

    /// How long to wait for an answer before giving up on it.
    pub fn with_timeout(mut self, t: Duration) -> RigCtld {
        self.timeout = t;
        self
    }

    /// Make the link now, so that a settings dialogue can say whether it works
    /// before the operator finds out with the key down.
    pub fn link(&mut self) -> Result<(), Fault> {
        self.connect().map(|_| ())
    }

    fn connect(&mut self) -> Result<&mut BufReader<TcpStream>, Fault> {
        if self.io.is_none() {
            let addr = self
                .addr
                .to_socket_addrs()
                .map_err(|e| Fault::Link(format!("{}: {e}", self.addr)))?
                .next()
                .ok_or_else(|| Fault::Link(format!("{} resolves to nothing", self.addr)))?;
            let s = TcpStream::connect_timeout(&addr, self.timeout)
                .map_err(|e| Fault::Link(format!("{}: {e}", self.addr)))?;
            s.set_read_timeout(Some(self.timeout)).map_err(|e| Fault::Link(e.to_string()))?;
            s.set_write_timeout(Some(self.timeout)).map_err(|e| Fault::Link(e.to_string()))?;
            // Keying is one short line and the answer is wanted now, not when
            // the kernel has a full segment to send.
            let _ = s.set_nodelay(true);
            self.io = Some(BufReader::new(s));
        }
        Ok(self.io.as_mut().expect("just connected"))
    }

    /// One command, one line of answer.
    ///
    /// A dropped link is retried once. The failure it exists for is the common
    /// one — `rigctld` restarted, or the link idle long enough for something in
    /// between to drop it — where the previous command left a socket that looks
    /// open and is not, so the caller would otherwise be told the rig is
    /// unreachable when it is sitting there answering. It is safe to repeat the
    /// commands this module sends: keying is idempotent and reading is free.
    ///
    /// A *timeout* is not retried. The rig is reachable and slow, or hung, and
    /// waiting a second time only doubles how long the caller waits to be told.
    fn ask(&mut self, cmd: &str) -> Result<String, Fault> {
        match self.round_trip(cmd) {
            Err(Fault::Link(_)) => {
                self.io = None;
                self.round_trip(cmd)
            }
            other => other,
        }
    }

    /// A command whose answer runs to more than one line.
    ///
    /// `m` is the reason: it answers with the mode and then the passband, on
    /// two lines. Reading one and leaving the other in the socket does not
    /// fail — it desynchronises the stream, and every answer after it belongs
    /// to the command before, which is the kind of fault that reads as the rig
    /// having gone mad.
    ///
    /// An error is a single line whatever was asked, so the count is what is
    /// wanted only when the first line is not one.
    fn ask_lines(&mut self, cmd: &str, lines: usize) -> Result<Vec<String>, Fault> {
        let first = self.ask(cmd)?;
        if first.starts_with("RPRT ") || lines <= 1 {
            return Ok(vec![first]);
        }
        let mut out = vec![first];
        for _ in 1..lines {
            out.push(self.read_line(cmd)?);
        }
        Ok(out)
    }

    fn round_trip(&mut self, cmd: &str) -> Result<String, Fault> {
        let io = self.connect()?;
        let line = format!("{cmd}\n");
        io.get_mut()
            .write_all(line.as_bytes())
            .map_err(|e| Fault::Link(format!("sending {cmd:?}: {e}")))?;
        io.get_mut().flush().map_err(|e| Fault::Link(format!("sending {cmd:?}: {e}")))?;

        self.read_line(cmd)
    }

    fn read_line(&mut self, cmd: &str) -> Result<String, Fault> {
        let io = self.connect()?;
        let mut answer = String::new();
        match io.read_line(&mut answer) {
            // End of stream: the daemon has gone. A link fault, so `ask` gives
            // it one more try on a fresh socket.
            Ok(0) => Err(Fault::Link("the rig control daemon closed the link".to_string())),
            Ok(_) => Ok(answer.trim().to_string()),
            Err(e) if timed_out(&e) => Err(Fault::Timeout),
            Err(e) => Err(Fault::Link(format!("reading the answer to {cmd:?}: {e}"))),
        }
    }

    /// `RPRT n` — hamlib's answer to a command that sets something. Zero is
    /// yes; anything else is the rig, or hamlib, saying why not.
    fn expect_rprt(answer: &str, doing: &'static str) -> Result<(), Fault> {
        let code = answer
            .strip_prefix("RPRT ")
            .and_then(|n| n.trim().parse::<i32>().ok())
            .ok_or_else(|| Fault::Protocol(format!("expected RPRT, got {answer:?}")))?;
        match code {
            0 => Ok(()),
            code => Err(Fault::Rejected { doing, code }),
        }
    }
}

impl Transmitter for RigCtld {
    fn key(&mut self, on: bool) -> Result<(), Fault> {
        let answer = self.ask(if on { "T 1" } else { "T 0" })?;
        RigCtld::expect_rprt(&answer, KEYING)
    }

    fn keyed(&mut self) -> Result<Option<bool>, Fault> {
        let answer = self.ask("t")?;
        match answer.trim() {
            "0" => Ok(Some(false)),
            // Hamlib has more than one kind of transmit — data PTT and mic PTT
            // key on different lines — and they are all transmitting as far as
            // anything above here is concerned.
            n if n.parse::<i32>().is_ok_and(|v| v > 0) => Ok(Some(true)),
            // A rig hamlib cannot read the PTT of is ordinary — keying over
            // RTS with no CAT read-back is a whole class of station — so it
            // answers "cannot be asked" rather than failing. Keying it still
            // works; only knowing what it did is lost.
            _ => match RigCtld::expect_rprt(&answer, READING_PTT) {
                Err(Fault::Rejected { code: -4 | -11, .. }) => Ok(None),
                Err(e) => Err(e),
                Ok(()) => Err(Fault::Protocol(format!("expected a PTT state, got {answer:?}"))),
            },
        }
    }

    fn dial_hz(&mut self) -> Result<Option<f64>, Fault> {
        let answer = self.ask("f")?;
        match answer.trim().parse::<f64>() {
            Ok(hz) if hz > 0.0 => Ok(Some(hz)),
            _ => match RigCtld::expect_rprt(&answer, READING_DIAL) {
                // As with the PTT: a rig that cannot be asked is ordinary.
                Err(Fault::Rejected { code: -4 | -11, .. }) => Ok(None),
                Err(e) => Err(e),
                Ok(()) => Err(Fault::Protocol(format!("expected a frequency, got {answer:?}"))),
            },
        }
    }

    fn tune(&mut self, hz: f64) -> Result<(), Fault> {
        let answer = self.ask(&format!("F {:.0}", hz.max(0.0)))?;
        RigCtld::expect_rprt(&answer, TUNING)
    }

    fn dial_mode(&mut self) -> Result<Option<String>, Fault> {
        // Mode then passband, on two lines. The passband is the rig's own
        // business; what is worth showing is which sideband it is in.
        let answer = self.ask_lines("m", 2)?;
        let first = answer.first().cloned().unwrap_or_default();
        if first.starts_with("RPRT ") {
            return match RigCtld::expect_rprt(&first, READING_DIAL) {
                Err(Fault::Rejected { code: -4 | -11, .. }) => Ok(None),
                Err(e) => Err(e),
                Ok(()) => Ok(None),
            };
        }
        Ok(Some(first))
    }

    fn describe(&self) -> String {
        format!("rigctld at {}", self.addr)
    }
}

/// Whether an I/O error is the socket's read timeout expiring.
///
/// Both kinds are produced by the platforms this runs on for the same
/// situation, so both count.
fn timed_out(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

/// Build the transmitter a setting asks for.
///
/// The link is not made here. Nothing about choosing a rig should touch one.
pub fn open(keying: &Keying) -> Box<dyn Transmitter> {
    match keying {
        Keying::None => Box::new(Unkeyed),
        Keying::RigCtld { host, port } => Box::new(RigCtld::new(host, *port)),
    }
}

// ---- the supervisor ----

/// How long a rig may be keyed, and how often it is asked what it is doing.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Longest a single key-down may last before it is broken by force.
    ///
    /// Two minutes is longer than any transmission this app makes — the longest
    /// is a JS8 Slow frame at twenty-five seconds — and short enough that a rig
    /// left down by a fault is not left down for the afternoon. It is a
    /// backstop, not a schedule: nothing normal should ever reach it.
    pub max_key_s: f64,
    /// How often the rig is asked what its PTT is doing.
    ///
    /// Every poll is a CAT transaction, and on an older rig that is a CI-V
    /// exchange at 4800 baud, so this is not free. Half a second is quick
    /// enough that an indicator does not feel stale and slow enough to leave
    /// the link alone.
    pub poll_s: f64,
    /// How long to leave a link that will not come up before trying it again,
    /// so a daemon that is not running costs one connection attempt every few
    /// seconds rather than one per poll.
    pub retry_s: f64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_key_s: 120.0, poll_s: 0.5, retry_s: 5.0 }
    }
}

/// What the transmitter is doing, as far as anything can tell.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    /// Whether the last thing asked of the rig worked.
    pub linked: bool,
    /// What it was last told to do. *Intent*, not fact.
    pub asked: bool,
    /// What the rig says it is doing, when it can be asked. Fact — and not
    /// always the same as `asked`: a front panel PTT, a foot switch and a stuck
    /// line all show up here and nowhere else.
    pub rig_says: Option<bool>,
    /// How long it has been keyed, in seconds; zero when it is not.
    pub keyed_for: f64,
    /// The last thing that went wrong, still worth showing after it has.
    pub fault: Option<String>,
    /// What the rig is tuned to, in Hz, and the mode it is in — as the rig
    /// says, not as this app last asked for. Turning the knob shows up here.
    pub dial_hz: Option<f64>,
    pub dial_mode: Option<String>,
}

/// One stretch of time the rig should be transmitting for.
#[derive(Clone, Copy, Debug)]
struct Span {
    on: f64,
    off: f64,
}

enum Cmd {
    /// Key or unkey now, and cancel anything scheduled — a hand on a button
    /// outranks a queue.
    Now(bool),
    /// Be transmitting between these two instants (UNIX seconds), which is what
    /// a scheduled frame wants: the caller knows when its audio starts, and the
    /// lead and tail are already in these numbers.
    Span(Span),
    /// Drop everything scheduled and un-key if a span is what is holding it
    /// down.
    Cancel,
    /// Tune to this many Hz.
    Tune(f64),
}

/// A transmitter under supervision.
///
/// Dropping it un-keys the rig and waits for that to have happened. That is why
/// it is worth holding one rather than a bare [`Transmitter`]: a rig that is
/// transmitting when the program ends is a rig that is transmitting until
/// somebody notices.
pub struct Ptt {
    /// Dropped before the worker is waited for: closing the channel is how the
    /// worker is told to finish, so it has to go first and cannot go in a
    /// field that outlives the wait.
    cmds: Option<std::sync::mpsc::Sender<Cmd>>,
    state: std::sync::Arc<std::sync::Mutex<State>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Ptt {
    /// Supervise the transmitter a setting asks for.
    pub fn start(keying: &Keying, limits: Limits) -> Ptt {
        Ptt::with_transmitter(open(keying), limits)
    }

    /// Supervise a transmitter that has already been built.
    pub fn with_transmitter(tx: Box<dyn Transmitter>, limits: Limits) -> Ptt {
        let (cmds, rx) = std::sync::mpsc::channel();
        let state = std::sync::Arc::new(std::sync::Mutex::new(State::default()));
        let mine = state.clone();
        let worker = std::thread::Builder::new()
            .name("ragchew-ptt".into())
            .spawn(move || Session::new(tx, limits, mine).run(rx))
            .ok();
        Ptt { cmds: Some(cmds), state, worker }
    }

    /// Key or unkey now. Cancels anything scheduled: a hand on a button means
    /// what it says.
    pub fn key(&self, on: bool) {
        self.tell(Cmd::Now(on));
    }

    /// Transmit from `on` until `off`, both UNIX seconds, both already carrying
    /// whatever lead and tail the caller wants. Spans queue up, so a message
    /// that goes out as several frames is several spans.
    pub fn schedule(&self, on: f64, off: f64) {
        self.tell(Cmd::Span(Span { on, off }));
    }

    /// Forget everything scheduled, and stop transmitting if that is what is
    /// keeping the rig down.
    pub fn cancel(&self) {
        self.tell(Cmd::Cancel);
    }

    /// Tune the rig. The reading comes back through [`Ptt::state`] when the rig
    /// confirms it, not when this is called: what the dial says is the rig's
    /// answer, never this app's intention.
    pub fn tune(&self, hz: f64) {
        self.tell(Cmd::Tune(hz));
    }

    /// Say something to the worker, or say nothing at all: a worker that has
    /// gone has already un-keyed, which is the only thing a caller here needs
    /// to be true.
    fn tell(&self, cmd: Cmd) {
        if let Some(cmds) = &self.cmds {
            let _ = cmds.send(cmd);
        }
    }

    /// What the rig is doing, for the interface to draw.
    pub fn state(&self) -> State {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for Ptt {
    fn drop(&mut self) {
        // Dropping the sender is the signal: the worker's `recv` fails, it
        // un-keys, and it returns. Then wait for it, because the point of the
        // exercise is that the rig is not still transmitting afterwards, and a
        // process that exits without waiting has not established that.
        self.cmds = None;
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// The supervised transmitter, on its own thread.
///
/// Everything about the rig's state lives here, in one place, on one thread.
struct Session {
    tx: Box<dyn Transmitter>,
    limits: Limits,
    state: std::sync::Arc<std::sync::Mutex<State>>,
    /// Whether the rig is believed to be keyed, and since when. Monotonic, so
    /// that a clock stepped by NTP under a keyed rig cannot extend the
    /// key-down — the one place in this app where the wall clock is the wrong
    /// clock for a duration.
    down_since: Option<std::time::Instant>,
    /// Spans still to come, and the one holding the key down if any.
    queued: Vec<Span>,
    holding: Option<Span>,
    next_poll: std::time::Instant,
    /// Cleared once the rig has said it cannot report its PTT, so a station
    /// that keys without read-back is asked once rather than twice a second
    /// for the rest of the session.
    can_ask: bool,
    /// The same for the dial, which is a separate question: plenty of rigs
    /// report a frequency and no PTT.
    can_ask_dial: bool,
    next_dial: std::time::Instant,
    /// When the link may next be tried, after one has failed.
    retry_at: Option<std::time::Instant>,
}

impl Drop for Session {
    /// The promise, in the only form that survives a panic: whatever else
    /// happened in this thread, the rig is not left transmitting. Unwinding a
    /// panic runs this; so does returning normally.
    fn drop(&mut self) {
        if self.down_since.is_some() {
            let _ = self.tx.key(false);
        }
    }
}

impl Session {
    fn new(
        tx: Box<dyn Transmitter>,
        limits: Limits,
        state: std::sync::Arc<std::sync::Mutex<State>>,
    ) -> Session {
        Session {
            tx,
            limits,
            state,
            down_since: None,
            queued: Vec::new(),
            holding: None,
            next_poll: std::time::Instant::now(),
            can_ask: true,
            can_ask_dial: true,
            next_dial: std::time::Instant::now(),
            retry_at: None,
        }
    }

    fn run(mut self, cmds: std::sync::mpsc::Receiver<Cmd>) {
        'pumping: loop {
            let wait = self.until_something_happens();
            match cmds.recv_timeout(wait) {
                Ok(Cmd::Now(on)) => {
                    self.queued.clear();
                    self.holding = None;
                    self.set(on);
                }
                Ok(Cmd::Span(s)) => self.queued.push(s),
                Ok(Cmd::Tune(hz)) => {
                    // Only the last one. A hand on a wheel outruns a CAT link
                    // at 4800 baud by a wide margin, and a queue of frequencies
                    // to pass through on the way is a rig that keeps tuning
                    // after the hand has stopped.
                    let mut hz = hz;
                    while let Ok(next) = cmds.try_recv() {
                        match next {
                            Cmd::Tune(newer) => hz = newer,
                            // Anything else is not a frequency and must not be
                            // dropped on the floor with them.
                            other => {
                                self.tune(hz);
                                self.act(other);
                                continue 'pumping;
                            }
                        }
                    }
                    self.tune(hz);
                }
                Ok(Cmd::Cancel) => {
                    self.queued.clear();
                    if self.holding.take().is_some() {
                        self.set(false);
                    }
                }
                // The owner has gone. `Session::drop` un-keys on the way out.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            self.work_the_schedule();
            self.mind_the_limit();
            self.ask_the_rig();
            self.ask_the_dial();
            self.publish();
        }
    }

    /// Do what a command says. Only for the ones pulled out of the queue while
    /// coalescing tunes; the rest are handled where they arrive.
    fn act(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Now(on) => {
                self.queued.clear();
                self.holding = None;
                self.set(on);
            }
            Cmd::Span(s) => self.queued.push(s),
            Cmd::Cancel => {
                self.queued.clear();
                if self.holding.take().is_some() {
                    self.set(false);
                }
            }
            Cmd::Tune(hz) => self.tune(hz),
        }
    }

    /// How long the thread may sleep before it must do something whether or not
    /// anyone has asked it to.
    fn until_something_happens(&self) -> Duration {
        let now = unix_now();
        let mut soonest = self.limits.poll_s.max(0.05);
        for s in self.queued.iter().chain(self.holding.iter()) {
            for at in [s.on, s.off] {
                if at > now {
                    soonest = soonest.min(at - now);
                }
            }
        }
        if let Some(since) = self.down_since {
            let left = self.limits.max_key_s - since.elapsed().as_secs_f64();
            soonest = soonest.min(left.max(0.0));
        }
        Duration::from_secs_f64(soonest.clamp(0.005, 1.0))
    }

    /// Key and unkey for the spans that have come due.
    fn work_the_schedule(&mut self) {
        let now = unix_now();
        // Anything whose whole span went by while nothing was running is gone,
        // not fired late: keying for a transmission whose audio has already
        // played is a carrier with nothing on it.
        self.queued.retain(|s| s.off > now);
        if let Some(h) = self.holding {
            if now >= h.off {
                self.holding = None;
                self.set(false);
            }
        }
        if self.holding.is_none() {
            if let Some(i) = self.queued.iter().position(|s| now >= s.on && now < s.off) {
                let s = self.queued.remove(i);
                self.holding = Some(s);
                self.set(true);
            }
        }
    }

    /// Break a key-down that has gone on too long.
    fn mind_the_limit(&mut self) {
        let Some(since) = self.down_since else { return };
        if since.elapsed().as_secs_f64() < self.limits.max_key_s {
            return;
        }
        self.queued.clear();
        self.holding = None;
        self.set(false);
        self.fault(format!(
            "the rig was keyed for longer than the {:.0}s limit and has been unkeyed",
            self.limits.max_key_s
        ));
    }

    /// Ask the rig what it is doing, now and then.
    fn ask_the_rig(&mut self) {
        let now = std::time::Instant::now();
        if !self.can_ask || now < self.next_poll {
            return;
        }
        if self.retry_at.is_some_and(|t| now < t) {
            return;
        }
        self.next_poll = now + Duration::from_secs_f64(self.limits.poll_s.max(0.05));
        match self.tx.keyed() {
            Ok(says) => {
                self.retry_at = None;
                self.can_ask = says.is_some();
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.linked = true;
                st.rig_says = says;
            }
            Err(e) => {
                self.retry_at = Some(now + Duration::from_secs_f64(self.limits.retry_s));
                self.no_link(e);
            }
        }
    }

    /// Send the rig somewhere. The state follows from the next reading, not
    /// from this: a tune that the rig declines must not leave the window
    /// showing a frequency nothing is listening on.
    fn tune(&mut self, hz: f64) {
        match self.tx.tune(hz) {
            Ok(()) => {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.linked = true;
                st.fault = None;
                // Ask again promptly rather than waiting out the slow beat.
                self.next_dial = std::time::Instant::now();
            }
            Err(e) => self.no_link(e),
        }
    }

    /// What the rig is tuned to. Asked for on a slower beat than the PTT: a
    /// dial moves when a hand moves it, and every reading is a CAT transaction.
    fn ask_the_dial(&mut self) {
        let now = std::time::Instant::now();
        if !self.can_ask_dial || now < self.next_dial || self.retry_at.is_some_and(|t| now < t) {
            return;
        }
        self.next_dial = now + Duration::from_secs_f64((self.limits.poll_s * 2.0).max(0.2));
        match self.tx.dial_hz() {
            Ok(hz) => {
                self.can_ask_dial = hz.is_some();
                let mode = self.tx.dial_mode().ok().flatten();
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.linked = true;
                st.dial_hz = hz;
                if mode.is_some() {
                    st.dial_mode = mode;
                }
            }
            Err(e) => self.no_link(e),
        }
    }

    /// Key or unkey, and remember which.
    ///
    /// The record is kept even when the command failed, because a command that
    /// failed may still have keyed the rig — the answer can be lost on the way
    /// back — and a supervisor that forgets a key-down it might have caused is
    /// no supervisor at all.
    fn set(&mut self, on: bool) {
        let was = self.down_since;
        self.down_since = match on {
            true => was.or_else(|| Some(std::time::Instant::now())),
            false => None,
        };
        let outcome = self.tx.key(on);
        match outcome {
            Ok(()) => {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.asked = on;
                st.linked = true;
                st.fault = None;
            }
            // A refusal is the rig answering, and the answer was no: it is not
            // transmitting, so nothing here should say it is or watch a clock
            // over it.
            Err(e @ Fault::Rejected { .. }) => {
                self.down_since = None;
                self.state.lock().unwrap_or_else(|e| e.into_inner()).asked = false;
                self.no_link(e);
            }
            // Anything else and we do not know. It was asked to transmit and
            // the answer was lost, which has to be read as "possibly keyed" —
            // the whole point of the limit is the case where nobody knows.
            Err(e) => {
                if on {
                    self.down_since = Some(std::time::Instant::now());
                }
                self.state.lock().unwrap_or_else(|e| e.into_inner()).asked = on;
                self.no_link(e);
            }
        }
    }

    fn no_link(&mut self, e: Fault) {
        let linked = !matches!(e, Fault::Link(_) | Fault::Timeout);
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.linked = linked;
        st.fault = Some(e.to_string());
    }

    fn fault(&mut self, what: String) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.fault = Some(what);
    }

    fn publish(&mut self) {
        let down = self.down_since.map_or(0.0, |t| t.elapsed().as_secs_f64());
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.keyed_for = down;
    }
}

/// Seconds since the epoch, which is the clock a scheduled transmission is
/// scheduled against — see `tx::next_start_after`.
fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufWriter;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// A stand-in for `rigctld`: the same line protocol, and the failures a
    /// real one produces only when something is wrong with the station.
    struct FakeRig {
        port: u16,
        /// Every command it was sent, in order.
        heard: Arc<Mutex<Vec<String>>>,
        /// What it would tell you its PTT is doing.
        ptt: Arc<AtomicBool>,
        /// And where it is tuned, in Hz — a real daemon answers `f` and `m`
        /// whether or not anyone asked it to key, so this one must too.
        dial: Arc<Mutex<f64>>,
        /// Answer the next command with this hamlib code instead of obeying.
        reject: Arc<Mutex<Option<i32>>>,
        /// Refuse to say what the PTT is doing, for ever — which is what
        /// hamlib does for a rig it has no PTT read-back for, its own dummy
        /// included until it is told a PTT method.
        deny_reads: Arc<AtomicBool>,
        /// Take the answer away: accept the command and say nothing.
        deaf: Arc<AtomicBool>,
        /// Close the link after this many commands, then wait for another.
        drop_after: Arc<AtomicUsize>,
    }

    impl FakeRig {
        fn start() -> FakeRig {
            let listener = TcpListener::bind("127.0.0.1:0").expect("no loopback to listen on");
            let port = listener.local_addr().unwrap().port();
            let rig = FakeRig {
                port,
                heard: Arc::new(Mutex::new(Vec::new())),
                ptt: Arc::new(AtomicBool::new(false)),
                dial: Arc::new(Mutex::new(14_074_000.0)),
                reject: Arc::new(Mutex::new(None)),
                deny_reads: Arc::new(AtomicBool::new(false)),
                deaf: Arc::new(AtomicBool::new(false)),
                drop_after: Arc::new(AtomicUsize::new(usize::MAX)),
            };
            let (heard, ptt) = (rig.heard.clone(), rig.ptt.clone());
            let dial = rig.dial.clone();
            let (reject, deaf) = (rig.reject.clone(), rig.deaf.clone());
            let deny_reads = rig.deny_reads.clone();
            let drop_after = rig.drop_after.clone();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { return };
                    let mut out = BufWriter::new(stream.try_clone().unwrap());
                    let mut lines = BufReader::new(stream).lines();
                    let mut served = 0usize;
                    while let Some(Ok(cmd)) = lines.next() {
                        heard.lock().unwrap().push(cmd.clone());
                        served += 1;
                        if deaf.load(Ordering::SeqCst) {
                            continue;
                        }
                        let answer = match reject.lock().unwrap().take() {
                            Some(code) => format!("RPRT {code}\n"),
                            None => match cmd.as_str() {
                                "T 1" => {
                                    ptt.store(true, Ordering::SeqCst);
                                    "RPRT 0\n".to_string()
                                }
                                "T 0" => {
                                    ptt.store(false, Ordering::SeqCst);
                                    "RPRT 0\n".to_string()
                                }
                                "t" if deny_reads.load(Ordering::SeqCst) => {
                                    "RPRT -11\n".to_string()
                                }
                                "t" => {
                                    format!("{}\n", u8::from(ptt.load(Ordering::SeqCst)))
                                }
                                "f" => format!("{:.0}\n", dial.lock().unwrap()),
                                // Mode then passband, on two lines, which is
                                // the shape that catches a client reading one.
                                "m" => "PKTUSB\n3000\n".to_string(),
                                c if c.starts_with("F ") => {
                                    match c[2..].trim().parse::<f64>() {
                                        Ok(hz) => {
                                            *dial.lock().unwrap() = hz;
                                            "RPRT 0\n".to_string()
                                        }
                                        Err(_) => "RPRT -1\n".to_string(),
                                    }
                                }
                                _ => "RPRT -1\n".to_string(),
                            },
                        };
                        if out.write_all(answer.as_bytes()).is_err() || out.flush().is_err() {
                            break;
                        }
                        if served >= drop_after.load(Ordering::SeqCst) {
                            break; // and round again to accept a new link
                        }
                    }
                }
            });
            rig
        }

        fn client(&self) -> RigCtld {
            RigCtld::new("127.0.0.1", self.port).with_timeout(Duration::from_millis(300))
        }
        fn heard(&self) -> Vec<String> {
            self.heard.lock().unwrap().clone()
        }
        fn transmitting(&self) -> bool {
            self.ptt.load(Ordering::SeqCst)
        }
        fn tuned_to(&self) -> f64 {
            *self.dial.lock().unwrap()
        }
    }

    /// The whole of it: ask the rig to transmit, and the rig transmits.
    #[test]
    fn keying_puts_the_rig_on_the_air_and_taking_it_off_takes_it_off() {
        let rig = FakeRig::start();
        let mut tx = rig.client();

        tx.key(true).expect("could not key");
        assert!(rig.transmitting(), "the rig was told to transmit and did not");
        assert_eq!(tx.keyed(), Ok(Some(true)), "the rig would not admit it was transmitting");

        tx.key(false).expect("could not unkey");
        assert!(!rig.transmitting(), "the rig stayed keyed");
        assert_eq!(tx.keyed(), Ok(Some(false)));

        assert_eq!(rig.heard(), ["T 1", "t", "T 0", "t"], "spoke a protocol rigctld does not");
    }

    /// A rig that says no is reported as having said no, with what it said.
    ///
    /// The distinction that matters: this is the rig answering. Reporting it as
    /// a lost link would send the operator to check cables over a rig that is
    /// sitting there talking to us and declining.
    #[test]
    fn a_rig_that_refuses_is_quoted_rather_than_swallowed() {
        let rig = FakeRig::start();
        let mut tx = rig.client();
        *rig.reject.lock().unwrap() = Some(-9);

        let fault = tx.key(true).expect_err("a refusal was read as success");
        assert_eq!(fault, Fault::Rejected { doing: KEYING, code: -9 });
        // Says what it would not do, why, and — for a refusal to key — the one
        // thing worth checking before walking over to the radio.
        let said = fault.to_string();
        assert!(said.contains("would not key"), "unhelpful: {said}");
        assert!(said.contains("rejected the command"), "unhelpful: {said}");
        assert!(!rig.transmitting(), "keyed the rig anyway");
    }

    /// A hamlib code we have no words for is passed on as a number rather than
    /// guessed at.
    #[test]
    fn an_unfamiliar_refusal_keeps_its_number() {
        let rig = FakeRig::start();
        let mut tx = rig.client();
        *rig.reject.lock().unwrap() = Some(-17);

        let fault = tx.key(true).expect_err("a refusal was read as success");
        assert_eq!(fault.to_string(), "the rig would not key (hamlib code -17)");
    }

    /// `rigctld` restarted underneath us costs one command, not a restart.
    #[test]
    fn a_daemon_that_goes_away_is_picked_up_again() {
        let rig = FakeRig::start();
        rig.drop_after.store(1, Ordering::SeqCst);
        let mut tx = rig.client();

        tx.key(true).expect("could not key");
        // The link this used is now closed, and the client does not know it.
        tx.key(false).expect("did not recover from a dropped link");
        assert!(!rig.transmitting(), "the rig stayed keyed across the reconnect");
        assert_eq!(rig.heard(), ["T 1", "T 0"], "lost or repeated a command");
    }

    /// A rig that stops answering is a fault the caller hears about promptly,
    /// not a caller that waits for ever with the key down.
    #[test]
    fn a_rig_that_does_not_answer_is_given_up_on() {
        let rig = FakeRig::start();
        rig.deaf.store(true, Ordering::SeqCst);
        let mut tx = rig.client();

        let began = std::time::Instant::now();
        let fault = tx.key(true).expect_err("silence was read as success");
        let waited = began.elapsed();

        assert_eq!(fault, Fault::Timeout, "{fault}");
        // The client is built with a 300 ms timeout. Anything near a second
        // means a timeout is not being applied to the read at all.
        assert!(waited < Duration::from_millis(900), "waited {waited:?} for an answer");
    }

    /// A daemon that is not there is a setting to fix, and says so.
    #[test]
    fn a_daemon_that_is_not_there_says_which_one() {
        // Bound and dropped, so nothing is listening on it.
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut tx = RigCtld::new("127.0.0.1", port).with_timeout(Duration::from_millis(300));

        let fault = tx.link().expect_err("linked to nothing");
        assert!(matches!(fault, Fault::Link(_)), "{fault}");
        assert!(fault.to_string().contains(&port.to_string()), "does not say where: {fault}");
    }

    /// Anything but a PTT state where a PTT state belongs is nonsense, and is
    /// reported as such rather than read as "not transmitting".
    #[test]
    fn nonsense_where_the_ptt_state_belongs_is_not_read_as_off() {
        let rig = FakeRig::start();
        let mut tx = rig.client();
        // Not one of the codes that mean "this rig cannot be asked" — those
        // are an answer in themselves and are tested below — but a refusal,
        // which must not be read as "not transmitting".
        *rig.reject.lock().unwrap() = Some(-8);
        let fault = tx.keyed().expect_err("an error code was read as a PTT state");
        assert_eq!(fault, Fault::Rejected { doing: READING_PTT, code: -8 });
    }

    /// Keying without a rig configured is not an error and not a lie: it does
    /// nothing, and admits it cannot say what the rig is doing.
    #[test]
    fn an_unkeyed_interface_does_nothing_and_claims_nothing() {
        let mut tx = open(&Keying::None);
        assert_eq!(tx.key(true), Ok(()));
        assert_eq!(tx.keyed(), Ok(None), "claimed to know what an unkeyed rig is doing");
        assert!(tx.describe().contains("VOX"), "{}", tx.describe());
    }

    /// The default is the behaviour this app had before it could key anything.
    #[test]
    fn nothing_is_keyed_until_something_is_configured() {
        assert_eq!(Keying::default(), Keying::None);
        assert!(open(&Keying::default()).keyed().unwrap().is_none());
    }

    // ---- the supervisor ----

    /// A transmitter that writes down everything asked of it, and can be told
    /// to come apart at a chosen moment.
    #[derive(Clone)]
    struct Bench {
        did: Arc<Mutex<Vec<(bool, std::time::Instant)>>>,
        keyed: Arc<AtomicBool>,
        /// Panic on the n-th `keyed()` — a fault in the middle of the loop,
        /// which is the one case no amount of error handling covers.
        explode_on_poll: Arc<AtomicUsize>,
        polls: Arc<AtomicUsize>,
    }

    impl Bench {
        fn new() -> Bench {
            Bench {
                did: Arc::new(Mutex::new(Vec::new())),
                keyed: Arc::new(AtomicBool::new(false)),
                explode_on_poll: Arc::new(AtomicUsize::new(usize::MAX)),
                polls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn keyings(&self) -> Vec<bool> {
            self.did.lock().unwrap().iter().map(|(on, _)| *on).collect()
        }
        fn at(&self, i: usize) -> std::time::Instant {
            self.did.lock().unwrap()[i].1
        }
        fn transmitting(&self) -> bool {
            self.keyed.load(Ordering::SeqCst)
        }
    }

    impl Transmitter for Bench {
        fn key(&mut self, on: bool) -> Result<(), Fault> {
            self.keyed.store(on, Ordering::SeqCst);
            self.did.lock().unwrap().push((on, std::time::Instant::now()));
            Ok(())
        }
        fn keyed(&mut self) -> Result<Option<bool>, Fault> {
            let n = self.polls.fetch_add(1, Ordering::SeqCst) + 1;
            assert!(n < self.explode_on_poll.load(Ordering::SeqCst), "bench came apart on purpose");
            Ok(Some(self.keyed.load(Ordering::SeqCst)))
        }
        fn describe(&self) -> String {
            "a bench".to_string()
        }
    }

    fn quick() -> Limits {
        Limits { max_key_s: 0.3, poll_s: 0.02, retry_s: 0.05 }
    }

    fn wait_until(what: impl Fn() -> bool) -> bool {
        let began = std::time::Instant::now();
        while began.elapsed() < Duration::from_secs(2) {
            if what() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// The promise: an owner that goes away takes the transmission with it.
    ///
    /// This is the one that matters at the end of a session. A program that
    /// exits with the rig keyed leaves it keyed until somebody walks over to
    /// it, and dropping is what a program does on its way out.
    #[test]
    fn dropping_the_supervisor_takes_the_rig_off_the_air() {
        let bench = Bench::new();
        let ptt = Ptt::with_transmitter(Box::new(bench.clone()), quick());
        ptt.key(true);
        assert!(wait_until(|| bench.transmitting()), "never keyed");

        drop(ptt);
        // Not "eventually": `drop` waits for it, so it is off by the time the
        // line above has returned.
        assert!(!bench.transmitting(), "the rig was left transmitting");
        assert_eq!(bench.keyings(), [true, false]);
    }

    /// A key-down that outlasts its limit is broken by force, and said so.
    #[test]
    fn a_rig_left_keyed_is_unkeyed_by_the_watchdog() {
        let bench = Bench::new();
        let ptt = Ptt::with_transmitter(Box::new(bench.clone()), quick());
        ptt.key(true);
        assert!(wait_until(|| bench.transmitting()), "never keyed");
        assert!(wait_until(|| !bench.transmitting()), "the watchdog never fired");

        // At the limit, not whenever it got round to it.
        let held = bench.at(1).duration_since(bench.at(0)).as_secs_f64();
        assert!((0.28..0.45).contains(&held), "held the key for {held:.3}s, limit was 0.30");
        let fault = ptt.state().fault.unwrap_or_default();
        assert!(fault.contains("longer than the") && fault.contains("unkeyed"), "{fault:?}");
        assert_eq!(bench.keyings(), [true, false], "keyed or unkeyed more than once");
    }

    /// A panic in the thread still takes the rig off the air.
    ///
    /// Error handling cannot cover this one — the point of a panic is that the
    /// code stopped running — so it is covered by unwinding through a `Drop`
    /// instead. Worth a test precisely because it is invisible in the source.
    #[test]
    fn a_thread_that_comes_apart_still_unkeys() {
        let bench = Bench::new();
        bench.explode_on_poll.store(2, Ordering::SeqCst);
        let ptt = Ptt::with_transmitter(Box::new(bench.clone()), quick());
        ptt.key(true);

        assert!(wait_until(|| bench.keyings().len() >= 2), "never unkeyed after the panic");
        assert!(!bench.transmitting(), "left transmitting by a panicking thread");
        assert_eq!(bench.keyings(), [true, false]);
    }

    /// A scheduled span keys at its start and unkeys at its end, without
    /// anything having to be awake to ask.
    ///
    /// This is what a transmission on a cycle boundary needs: the caller says
    /// when, in UNIX seconds, lead and tail already in the numbers, and then
    /// forgets about it.
    #[test]
    fn a_scheduled_span_keys_itself_and_lets_go() {
        let bench = Bench::new();
        let ptt = Ptt::with_transmitter(Box::new(bench.clone()), quick());
        let began = std::time::Instant::now();
        let at = unix_now();
        ptt.schedule(at + 0.15, at + 0.30);

        assert!(wait_until(|| bench.keyings().len() >= 2), "the span never ran");
        assert_eq!(bench.keyings(), [true, false]);
        let down = bench.at(0).duration_since(began).as_secs_f64();
        let up = bench.at(1).duration_since(began).as_secs_f64();
        assert!((0.13..0.25).contains(&down), "keyed at {down:.3}s, wanted 0.15");
        assert!((0.28..0.40).contains(&up), "unkeyed at {up:.3}s, wanted 0.30");
    }

    /// Frames are spans, one each, and each gets its own key-down: a message
    /// that goes out over several cycles must not hold the key through the
    /// silence between them.
    #[test]
    fn one_key_down_per_span_not_one_per_message() {
        let bench = Bench::new();
        let ptt = Ptt::with_transmitter(Box::new(bench.clone()), quick());
        let at = unix_now();
        ptt.schedule(at + 0.05, at + 0.15);
        ptt.schedule(at + 0.25, at + 0.35);

        assert!(wait_until(|| bench.keyings().len() >= 4), "both spans did not run");
        assert_eq!(bench.keyings(), [true, false, true, false]);
    }

    /// A hand on the button outranks the queue.
    #[test]
    fn keying_by_hand_drops_what_was_scheduled() {
        let bench = Bench::new();
        let ptt = Ptt::with_transmitter(Box::new(bench.clone()), quick());
        let at = unix_now();
        ptt.schedule(at + 0.10, at + 0.20);
        ptt.key(false);

        thread::sleep(Duration::from_millis(300));
        assert!(!bench.keyings().contains(&true), "the cancelled span transmitted anyway");
    }

    /// A span whose time has passed is dropped, not fired late. Keying for a
    /// transmission whose audio has already played is a bare carrier.
    #[test]
    fn a_span_that_was_missed_is_not_transmitted_late() {
        let bench = Bench::new();
        let ptt = Ptt::with_transmitter(Box::new(bench.clone()), quick());
        let at = unix_now();
        ptt.schedule(at - 5.0, at - 4.0);

        thread::sleep(Duration::from_millis(200));
        assert!(bench.keyings().is_empty(), "transmitted a span from five seconds ago");
    }

    /// What the rig says is reported, and it is not the same question as what
    /// the rig was told: a front panel PTT shows up here and nowhere else.
    #[test]
    fn the_rig_is_asked_rather_than_assumed() {
        let bench = Bench::new();
        let ptt = Ptt::with_transmitter(Box::new(bench.clone()), quick());
        assert!(wait_until(|| ptt.state().linked), "never reached the rig");
        assert_eq!(ptt.state().rig_says, Some(false));
        assert!(!ptt.state().asked);

        // Somebody presses the PTT on the front of the radio.
        bench.keyed.store(true, Ordering::SeqCst);
        assert!(wait_until(|| ptt.state().rig_says == Some(true)), "never noticed the rig keying");
        assert!(!ptt.state().asked, "reported it as ours");
    }

    /// A real rig, through the real client, from the button down to the socket.
    #[test]
    fn the_whole_path_keys_a_rigctld() {
        let rig = FakeRig::start();
        let ptt = Ptt::with_transmitter(Box::new(rig.client()), quick());

        ptt.key(true);
        assert!(wait_until(|| rig.transmitting()), "the rig never keyed");
        assert!(wait_until(|| ptt.state().rig_says == Some(true)), "never read it back");

        drop(ptt);
        assert!(!rig.transmitting(), "left a real link transmitting");
    }

    /// A rig that will not say what its PTT is doing is an ordinary rig, not a
    /// broken one.
    ///
    /// Keying over RTS with no CAT read-back is a whole class of station, and
    /// hamlib answers for those exactly as it does for its own dummy before it
    /// has been given a PTT method: `RPRT -11`. Reading that as a fault put a
    /// red error on a link that works.
    #[test]
    fn a_rig_that_cannot_report_its_ptt_is_not_a_fault() {
        let rig = FakeRig::start();
        rig.deny_reads.store(true, Ordering::SeqCst);
        // In its own scope: the fake serves one link at a time, as the real
        // daemon does not, so this one has to be let go before the supervisor
        // below can be served.
        {
            let mut tx = rig.client();
            assert_eq!(tx.keyed(), Ok(None), "read a refusal as a state");
        }

        // And it is asked once, not twice a second for the rest of the day.
        let ptt = Ptt::with_transmitter(Box::new(rig.client()), quick());
        assert!(wait_until(|| ptt.state().linked), "never reached the rig");
        thread::sleep(Duration::from_millis(200));
        let asked = rig.heard().iter().filter(|c| *c == "t").count();
        assert!(asked <= 3, "asked a rig that cannot answer {asked} times");
        assert_eq!(ptt.state().fault, None, "a rig that cannot answer is not a fault");
        assert_eq!(ptt.state().rig_says, None);
    }

    /// A rig that refuses to key is not reported as transmitting.
    ///
    /// A refusal is the rig answering, and the answer was no. Showing TX over
    /// it would be the interface inventing a transmission — and starting a
    /// watchdog over one that is not happening.
    #[test]
    fn a_rig_that_refuses_to_key_is_not_shown_as_transmitting() {
        let rig = FakeRig::start();
        // Refuse everything, so the refusal under test cannot be spent on a
        // poll that happened to go first.
        let ptt = Ptt::with_transmitter(Box::new(AlwaysRefuses), quick());
        ptt.key(true);
        let _ = &rig;

        assert!(wait_until(|| ptt.state().fault.is_some()), "said nothing about the refusal");
        assert!(!ptt.state().asked, "claimed to be transmitting after a refusal");
        assert_eq!(ptt.state().keyed_for, 0.0, "started a clock over a refused key-down");
        let said = ptt.state().fault.unwrap();
        assert!(said.contains("ptt_type"), "did not say what to check: {said}");
    }

    /// A rig that says no to everything, for the cases about what a refusal
    /// means rather than about which command drew it.
    struct AlwaysRefuses;
    impl Transmitter for AlwaysRefuses {
        fn key(&mut self, _on: bool) -> Result<(), Fault> {
            Err(Fault::Rejected { doing: KEYING, code: -1 })
        }
        fn keyed(&mut self) -> Result<Option<bool>, Fault> {
            Ok(None)
        }
        fn describe(&self) -> String {
            "a rig that says no".to_string()
        }
    }

    /// What the rig is tuned to comes from the rig, and tuning it moves it.
    #[test]
    fn the_dial_is_read_from_the_rig_and_can_be_moved() {
        let rig = FakeRig::start();
        let ptt = Ptt::with_transmitter(Box::new(rig.client()), quick());
        assert!(
            wait_until(|| ptt.state().dial_hz == Some(14_074_000.0)),
            "never read the dial: {:?}",
            ptt.state()
        );
        // The mode comes back off the two-line answer, not half of it.
        assert_eq!(ptt.state().dial_mode.as_deref(), Some("PKTUSB"));

        ptt.tune(7_078_000.0);
        assert!(wait_until(|| rig.tuned_to() == 7_078_000.0), "the rig did not move");
        assert!(
            wait_until(|| ptt.state().dial_hz == Some(7_078_000.0)),
            "the reading did not follow the rig"
        );
        assert_eq!(ptt.state().fault, None);
    }

    /// A daemon that is not there does not become a busy loop against it, and
    /// the fault is on show rather than in a log nobody is reading.
    #[test]
    fn a_rig_that_cannot_be_reached_is_reported_and_left_alone() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let client = RigCtld::new("127.0.0.1", port).with_timeout(Duration::from_millis(50));
        let ptt = Ptt::with_transmitter(Box::new(client), quick());

        assert!(wait_until(|| ptt.state().fault.is_some()), "said nothing about a missing rig");
        assert!(!ptt.state().linked);
        let said = ptt.state().fault.unwrap();
        assert!(said.contains("no link to the rig"), "{said}");
    }
}
