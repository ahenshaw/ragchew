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
//! Nothing here un-keys anything by itself. A [`Transmitter`] does as it is
//! told and reports what happened; the timing, and the promise that a key-down
//! is always followed by a key-up, belong above it where they can be made once
//! for every transport.

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

    fn round_trip(&mut self, cmd: &str) -> Result<String, Fault> {
        let io = self.connect()?;
        let line = format!("{cmd}\n");
        io.get_mut()
            .write_all(line.as_bytes())
            .map_err(|e| Fault::Link(format!("sending {cmd:?}: {e}")))?;
        io.get_mut().flush().map_err(|e| Fault::Link(format!("sending {cmd:?}: {e}")))?;

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
                reject: Arc::new(Mutex::new(None)),
                deny_reads: Arc::new(AtomicBool::new(false)),
                deaf: Arc::new(AtomicBool::new(false)),
                drop_after: Arc::new(AtomicUsize::new(usize::MAX)),
            };
            let (heard, ptt) = (rig.heard.clone(), rig.ptt.clone());
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
}
