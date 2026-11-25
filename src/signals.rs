//! Signals the TUI has to answer itself.
//!
//! While the UI runs, the terminal is in raw mode on the alternate screen. A process that dies
//! from a signal without undoing that leaves the shell behind it without echo until the user
//! types `reset`. So SIGTERM, SIGHUP, SIGINT and SIGQUIT do not take their default course
//! here. A thread of its own waits for them (the watcher) and decides what each one means, by
//! who has the terminal at that moment:
//!
//! - **The UI has it.** The signal is passed on as a flag. The event loop sees the flag within
//!   one poll interval, gives the terminal back, and lets the signal have its normal effect.
//!   The loop is asked first because only it can give the terminal back between two frames.
//!   But it is not relied on: if the process is still there after [`GRACE`], the watcher gives
//!   the terminal back as far as it can and ends the process itself. The loop can be unable to
//!   answer: stuck in a write to a terminal that takes no output, or inside the terminal
//!   library, which reads forever from a terminal that has been closed. A process that is
//!   asked to end and does not is the worst a program can do to the machine it runs on.
//! - **A child has it** (the shell of `b`, or the delete command): there is nothing to give
//!   back, so SIGTERM and SIGHUP end icdu at once. SIGINT and SIGQUIT are left to the child:
//!   they come from Ctrl-C and Ctrl-\ at the keyboard, which the terminal sends to everything
//!   in the foreground, and they are meant for the command the user is looking at. That is
//!   what `system(3)` does, too.
//!
//! A second thread looks every [`TERMINAL_CHECK`] whether the terminal is still there
//! ([`terminal_gone`]). Closing a terminal normally sends SIGHUP, but not to a process that is
//! not in the terminal's session, and then nothing else would end it.
//!
//! **SIGTSTP has no handler, and must not get one.** Whether a process is stopped is for the
//! system to say: it does not stop one that nothing could continue (a process group without a
//! shell above it, as when icdu is a terminal's own command). A handler has to stop the
//! process by other means, which go around that rule and leave such a terminal frozen. In raw
//! mode Ctrl-Z arrives as a key press, so the event loop gives the terminal back and raises the
//! signal itself ([`stop_process`]). A SIGTSTP from outside stops icdu as it is, like SIGSTOP,
//! which nothing can catch. SIGCONT is reported to the loop, which sets the terminal up again.
//!
//! All of this is installed only for the TUI: a headless run (`-o`) keeps the default
//! behaviour of every signal, and gets back the one that Rust takes away: see
//! [`end_if_broken_pipe`].

use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGCONT, SIGHUP, SIGINT, SIGPIPE, SIGQUIT, SIGTERM, SIGTSTP};
use signal_hook::{iterator, low_level};

/// How long the event loop gets to end the process in an orderly way before the watcher ends
/// it. Several times the longest poll interval, and longer than a slow frame.
const GRACE: Duration = Duration::from_millis(1000);

/// How long the watcher waits for the terminal to be given back when it ends the process
/// itself. Giving it back means writing to it, and that can wait forever.
const RESTORE_LIMIT: Duration = Duration::from_millis(300);

/// How often to look whether the terminal is still there.
const TERMINAL_CHECK: Duration = Duration::from_secs(2);

/// Who has the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
    /// icdu: raw mode, alternate screen, the event loop running.
    Ui,
    /// A command icdu started and waits for. The terminal is as icdu found it.
    Child,
}

/// What the watcher does about a signal.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// Ask the event loop to end the process, and do it in its place if it does not.
    End,
    /// End the process now.
    Die,
    /// Tell the event loop that the process was stopped and runs again.
    Resume,
    Nothing,
}

/// The one place that says what a signal means. See the module documentation for the reasons.
fn answer(signal: i32, holder: Holder) -> Answer {
    match (signal, holder) {
        (SIGTERM | SIGHUP | SIGINT | SIGQUIT, Holder::Ui) => Answer::End,
        (SIGTERM | SIGHUP, Holder::Child) => Answer::Die,
        (SIGINT | SIGQUIT, Holder::Child) => Answer::Nothing,
        (SIGCONT, Holder::Ui) => Answer::Resume,
        _ => Answer::Nothing,
    }
}

/// What the watcher tells the event loop.
#[derive(Clone, Default)]
struct Flags {
    /// The signal that asked the process to end; 0 while none has.
    terminate: Arc<AtomicUsize>,
    /// SIGCONT arrived: the process had been stopped and is running again.
    cont: Arc<AtomicBool>,
}

/// The event loop's side of the signal handling.
pub struct Signals {
    flags: Flags,
    holder: Arc<Mutex<Holder>>,
}

/// While this lives, a child process has the terminal. See [`Signals::hand_over`].
pub struct HandOver<'a>(&'a Signals);

impl Drop for HandOver<'_> {
    fn drop(&mut self) {
        *lock(&self.0.holder) = Holder::Ui;
    }
}

/// The watcher's side: what it needs to act on a signal.
struct Watcher {
    flags: Flags,
    holder: Arc<Mutex<Holder>>,
    grace: Duration,
    /// End the process by a signal. Gets who has the terminal, to know whether there is
    /// something to give back first.
    die: Box<dyn Fn(i32, Holder) + Send>,
}

/// The lock on who has the terminal. A panic elsewhere must not take signal handling with it.
fn lock(holder: &Mutex<Holder>) -> MutexGuard<'_, Holder> {
    holder.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Watcher {
    fn on(&self, signal: i32) {
        // Held while the answer is carried out: the event loop takes the same lock before it
        // sets the terminal up again, so "a child has it" cannot become untrue underneath.
        let holder = lock(&self.holder);
        match answer(signal, *holder) {
            Answer::End => {
                self.flags
                    .terminate
                    .store(signal as usize, Ordering::SeqCst);
                drop(holder);
                thread::sleep(self.grace);
                // Still here, so the loop has not done it.
                let holder = lock(&self.holder);
                (self.die)(signal, *holder);
            }
            Answer::Die => (self.die)(signal, *holder),
            Answer::Resume => self.flags.cont.store(true, Ordering::SeqCst),
            Answer::Nothing => {}
        }
    }
}

impl Signals {
    /// Take over SIGTERM, SIGHUP, SIGINT, SIGQUIT and SIGCONT, and watch the terminal.
    /// `restore` gives the terminal back; it is used when the event loop does not answer.
    pub fn install(restore: fn()) -> io::Result<Self> {
        let signals = Self::start(
            GRACE,
            Box::new(move |signal, holder| {
                if holder == Holder::Ui {
                    within(RESTORE_LIMIT, restore);
                }
                die_by(signal)
            }),
        )?;
        if io::stdout().is_terminal() {
            // A handle of its own on the terminal: `Stdout` collects what is written to it,
            // and the question has to reach the terminal.
            let terminal = File::from(io::stdout().as_fd().try_clone_to_owned()?);
            thread::Builder::new()
                .name("terminal".into())
                .spawn(move || loop {
                    thread::sleep(TERMINAL_CHECK);
                    if terminal_gone(&terminal) {
                        // Nothing is left to give back, and nothing to wait for.
                        die_by(SIGHUP);
                    }
                })?;
        }
        Ok(signals)
    }

    /// Start the watcher. Apart from [`Signals::install`], this is for the tests, which
    /// cannot let the process die.
    fn start(grace: Duration, die: Box<dyn Fn(i32, Holder) + Send>) -> io::Result<Self> {
        let signals = Signals {
            flags: Flags::default(),
            holder: Arc::new(Mutex::new(Holder::Ui)),
        };
        let watcher = Watcher {
            flags: signals.flags.clone(),
            holder: Arc::clone(&signals.holder),
            grace,
            die,
        };
        let mut incoming = iterator::Signals::new([SIGTERM, SIGHUP, SIGINT, SIGQUIT, SIGCONT])?;
        thread::Builder::new()
            .name("signals".into())
            .spawn(move || {
                for signal in incoming.forever() {
                    watcher.on(signal);
                }
            })?;
        Ok(signals)
    }

    /// The signal that asked the process to end, if one has arrived.
    pub fn terminated(&self) -> Option<i32> {
        match self.flags.terminate.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal as i32),
        }
    }

    /// True once per SIGCONT.
    pub fn take_cont(&self) -> bool {
        self.flags.cont.swap(false, Ordering::SeqCst)
    }

    /// Call this when [`stop_process`] has returned. It says whether the process had been
    /// stopped at all. If it had, a SIGCONT continued it, and the report of that is taken
    /// here, so that the loop does not answer it a second time. The report comes from the
    /// watcher's thread a moment after the signal, so it is waited for, briefly. If none
    /// comes, the system did not stop the process.
    pub fn continued(&self) -> bool {
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            if self.take_cont() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Say that a child process gets the terminal, for as long as the returned value lives.
    /// Call it after the terminal has been given back, and drop the value before the UI takes
    /// the terminal again.
    pub fn hand_over(&self) -> HandOver<'_> {
        *lock(&self.holder) = Holder::Child;
        HandOver(self)
    }
}

/// Whether the terminal behind this handle has been closed. Asked by writing nothing to it:
/// on a terminal that is there, that succeeds and changes nothing, and on one that is gone it
/// fails. Asking whether it is still a terminal does not do: on macOS a closed terminal is one.
fn terminal_gone(mut terminal: &File) -> bool {
    match terminal.write(&[]) {
        Ok(written) => {
            debug_assert_eq!(written, 0, "nothing was to be written");
            false
        }
        Err(e) => !matches!(
            e.kind(),
            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
        ),
    }
}

/// Run `work` on a thread of its own and wait for it, but not longer than `limit`.
fn within(limit: Duration, work: fn()) {
    let (done, wait) = mpsc::channel();
    let spawned = thread::Builder::new().spawn(move || {
        work();
        let _ = done.send(());
    });
    if spawned.is_ok() {
        let _ = wait.recv_timeout(limit);
    }
}

/// Stop the process the way Ctrl-Z stops any program: by SIGTSTP, which has no handler here,
/// so the system does it, or does not, where nothing could continue the process. Returns when
/// the process is continued (`fg`), or at once. The caller gives the terminal back first and
/// takes it again afterwards, and asks [`Signals::continued`] which of the two it was.
pub fn stop_process() {
    let _ = low_level::raise(SIGTSTP);
}

/// End the process the way `signal` would have without a handler, so that whoever started it
/// sees the real cause. The caller has restored the terminal.
pub fn die_by(signal: i32) -> ! {
    let _ = low_level::emulate_default_handler(signal);
    // Not reached if the signal did its work; the conventional exit status otherwise.
    std::process::exit(128 + signal)
}

/// If `error` says that nobody reads the output any more, end the way programs do then: by
/// SIGPIPE, and without a word. (Rust starts every program with that signal ignored, so that a
/// write to a closed pipe is an error to handle. For output that was the whole point of the
/// run, the error has one right answer.)
pub fn end_if_broken_pipe(error: &io::Error) {
    if error.kind() == io::ErrorKind::BrokenPipe {
        die_by(SIGPIPE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_signal_means_depends_on_who_has_the_terminal() {
        use Answer::*;
        // With the UI on the terminal, the event loop is asked: only it can give the terminal
        // back between two frames.
        for signal in [SIGTERM, SIGHUP, SIGINT, SIGQUIT] {
            assert_eq!(answer(signal, Holder::Ui), End, "signal {signal}");
        }
        assert_eq!(answer(SIGCONT, Holder::Ui), Resume);

        // While a child has it, there is nothing to give back and nothing to wait for.
        assert_eq!(answer(SIGTERM, Holder::Child), Die);
        assert_eq!(answer(SIGHUP, Holder::Child), Die);
        // Ctrl-C and Ctrl-\ are typed for the command on screen, not for icdu behind it.
        assert_eq!(answer(SIGINT, Holder::Child), Nothing);
        assert_eq!(answer(SIGQUIT, Holder::Child), Nothing);
        // The UI is set up again when the child ends, whatever happened in between.
        assert_eq!(answer(SIGCONT, Holder::Child), Nothing);
    }

    fn wait_for(what: &str, mut holds: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !holds() {
            assert!(Instant::now() < deadline, "waited in vain for: {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// The watcher with real signals. This is the only test that raises any: handlers belong
    /// to the whole process, so a second test of this kind would see the signals of the first.
    /// The process must not die here, so that is recorded instead of done. SIGTSTP is not
    /// raised: it has no handler, and would stop the test.
    #[test]
    fn the_watcher_asks_the_loop_first_and_acts_itself_when_it_has_to() {
        let died: Arc<Mutex<Vec<(i32, Holder)>>> = Arc::default();
        // Once the test is over, a signal for the test process is a real one again
        // (Ctrl-C on `cargo test`).
        let over = Arc::new(AtomicBool::new(false));
        let signals = Signals::start(
            Duration::from_millis(50),
            Box::new({
                let (died, over) = (Arc::clone(&died), Arc::clone(&over));
                move |signal, holder| {
                    if over.load(Ordering::SeqCst) {
                        die_by(signal);
                    }
                    died.lock().unwrap().push((signal, holder));
                }
            }),
        )
        .unwrap();
        let deaths = || died.lock().unwrap().clone();

        // The UI has the terminal: that the process was continued is for the loop to know.
        assert!(!signals.take_cont());
        low_level::raise(SIGCONT).unwrap();
        wait_for("SIGCONT to reach the loop", || signals.take_cont());
        assert!(!signals.take_cont(), "it is reported once");
        // After stopping itself, the loop takes the report of the SIGCONT that woke it.
        low_level::raise(SIGCONT).unwrap();
        assert!(signals.continued(), "the process had been stopped");
        thread::sleep(Duration::from_millis(20));
        assert!(!signals.take_cont(), "the report is gone");
        // And without a SIGCONT, it had not been stopped.
        assert!(!signals.continued());

        // A child has it: the keyboard's signals are not icdu's, and SIGTERM ends at once,
        // without a word to the loop.
        {
            let _child = signals.hand_over();
            low_level::raise(SIGINT).unwrap();
            low_level::raise(SIGQUIT).unwrap();
            low_level::raise(SIGTERM).unwrap();
            wait_for("the end", || !deaths().is_empty());
            assert_eq!(deaths(), [(SIGTERM, Holder::Child)]);
            assert_eq!(signals.terminated(), None, "the loop was not asked");
        }

        // The UI has it again. The loop is asked, and as this one never answers, the watcher
        // ends the process after the grace period, knowing there is a terminal to give back.
        low_level::raise(SIGHUP).unwrap();
        wait_for("SIGHUP to reach the loop", || {
            signals.terminated() == Some(SIGHUP)
        });
        wait_for("the watcher to insist", || deaths().len() == 2);
        assert_eq!(deaths()[1], (SIGHUP, Holder::Ui));

        over.store(true, Ordering::SeqCst);
    }

    /// The other side, a terminal that has been closed, needs a terminal to close:
    /// `tools/terminal-test.py` has that.
    #[test]
    fn something_that_can_be_written_to_is_not_a_terminal_that_is_gone() {
        let there = File::options().write(true).open("/dev/null").unwrap();
        assert!(!terminal_gone(&there));
    }

    #[test]
    fn work_that_does_not_finish_is_not_waited_for() {
        let started = Instant::now();
        within(Duration::from_millis(20), || {
            thread::sleep(Duration::from_secs(5))
        });
        assert!(started.elapsed() < Duration::from_secs(2));
        // And work that finishes is waited for.
        static DONE: AtomicBool = AtomicBool::new(false);
        within(Duration::from_secs(10), || {
            DONE.store(true, Ordering::SeqCst)
        });
        assert!(DONE.load(Ordering::SeqCst));
    }
}
