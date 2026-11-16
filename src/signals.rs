//! Signals the TUI has to answer itself.
//!
//! While the UI runs, the terminal is in raw mode on the alternate screen. A process that dies
//! from SIGTERM or SIGHUP, or is stopped by SIGTSTP, without undoing that leaves the shell
//! behind it without echo until the user types `reset`. So those signals only set a flag here;
//! the event loop sees it within one poll interval, gives the terminal back, and then lets the
//! signal have its normal effect.
//!
//! In raw mode Ctrl-Z arrives as a key press, not as SIGTSTP, so the event loop takes the same
//! path for the key. The handlers are installed only for the TUI: a headless run (`-o`) keeps
//! the default behaviour of every signal.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use signal_hook::consts::{SIGCONT, SIGHUP, SIGTERM, SIGTSTP};
use signal_hook::{flag, low_level};

/// The flags the signal handlers set. Setting an atomic is all a handler may safely do.
pub struct Signals {
    /// The terminating signal that arrived (SIGTERM or SIGHUP); 0 while none has.
    terminate: Arc<AtomicUsize>,
    /// SIGTSTP arrived: something outside asked the process to stop.
    stop: Arc<AtomicBool>,
    /// SIGCONT arrived: the process had been stopped and is running again.
    cont: Arc<AtomicBool>,
}

impl Signals {
    /// Replace the default action of SIGTERM, SIGHUP, SIGTSTP and SIGCONT by a flag.
    pub fn install() -> io::Result<Self> {
        let signals = Signals {
            terminate: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            cont: Arc::new(AtomicBool::new(false)),
        };
        for signal in [SIGTERM, SIGHUP] {
            flag::register_usize(signal, Arc::clone(&signals.terminate), signal as usize)?;
        }
        flag::register(SIGTSTP, Arc::clone(&signals.stop))?;
        flag::register(SIGCONT, Arc::clone(&signals.cont))?;
        Ok(signals)
    }

    /// The signal that asked the process to end, if one has arrived.
    pub fn terminated(&self) -> Option<i32> {
        match self.terminate.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal as i32),
        }
    }

    /// True once per SIGTSTP.
    pub fn take_stop(&self) -> bool {
        self.stop.swap(false, Ordering::SeqCst)
    }

    /// True once per SIGCONT.
    pub fn take_cont(&self) -> bool {
        self.cont.swap(false, Ordering::SeqCst)
    }
}

/// Stop the process, as SIGTSTP does without a handler. Returns when it is continued (`fg`).
/// The caller gives the terminal back first and takes it again afterwards.
pub fn stop_process() {
    let _ = low_level::emulate_default_handler(SIGTSTP);
}

/// End the process the way `signal` would have without a handler, so that whoever started it
/// sees the real cause. The caller has restored the terminal.
pub fn die_by(signal: i32) -> ! {
    let _ = low_level::emulate_default_handler(signal);
    // Not reached if the signal did its work; the conventional exit status otherwise.
    std::process::exit(128 + signal)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With the handlers installed, the signals set flags and nothing else: the process
    /// neither dies nor stops, and each flag reads true exactly once. (`raise` delivers to
    /// the calling thread before it returns, so there is nothing to wait for.)
    #[test]
    fn signals_only_set_flags() {
        let signals = Signals::install().unwrap();
        assert_eq!(signals.terminated(), None);
        assert!(!signals.take_stop());

        low_level::raise(SIGTSTP).unwrap();
        assert!(signals.take_stop(), "SIGTSTP was seen");
        assert!(!signals.take_stop(), "and is reported once");

        low_level::raise(SIGCONT).unwrap();
        assert!(signals.take_cont());
        assert!(!signals.take_cont());

        low_level::raise(SIGHUP).unwrap();
        assert_eq!(signals.terminated(), Some(SIGHUP));
        low_level::raise(SIGTERM).unwrap();
        assert_eq!(signals.terminated(), Some(SIGTERM), "the latest one wins");
    }
}
