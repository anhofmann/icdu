#!/usr/bin/env python3
"""Try icdu on a terminal: what the unit tests cannot reach.

    tools/terminal-test.py [BINARY [WORD]]

BINARY is the icdu to try (default: target/release/icdu). With WORD, only the checks
whose name contains it are run.

Each check starts icdu on a pseudo-terminal of its own, the way a terminal emulator
does, and then looks at three things: whether the process ends when it should (and how:
by which signal, or with which status), how long that takes, and in what state the
terminal is left.

What is covered:
  - quitting, and SIGTERM, SIGHUP, SIGINT and SIGQUIT in the browser
  - a terminal that is closed, with and without the hang-up signal
  - a terminal that stops taking output
  - Ctrl-Z, SIGTSTP and SIGCONT, and SIGTERM or a closed terminal for a stopped icdu
  - the signals while a command started with `b`, or the delete command, has the terminal
  - Ctrl-L
  - signals before the browser has started, and a start without any terminal
  - output into a pipe that nobody reads (`icdu -o- | head`)

Needs Python 3 and nothing else. The exit status is 0 if every check passed.
"""

import errno
import fcntl
import os
import resource
import select
import shutil
import signal
import struct
import sys
import tempfile
import termios
import time

# How long a process gets to do what is expected of it. Far more than it should need:
# the point is that it happens at all, also on a slow machine.
LIMIT = 6.0

# How long one check may take altogether before it is given up.
CHECK_LIMIT = 90

ALT_ON, ALT_OFF = b"\x1b[?1049h", b"\x1b[?1049l"
PASTE_ON, PASTE_OFF = b"\x1b[?2004h", b"\x1b[?2004l"
CURSOR_ON, CURSOR_OFF = b"\x1b[?25h", b"\x1b[?25l"


# What a shell ignores for itself, and gives back to the programs it starts.
SHELL_IGNORES = (
    signal.SIGHUP,
    signal.SIGINT,
    signal.SIGQUIT,
    signal.SIGTERM,
    signal.SIGTSTP,
    signal.SIGTTIN,
    signal.SIGTTOU,
)


class Run:
    """One icdu on a pseudo-terminal of its own.

    Normally it runs the way it does when it is typed at a prompt: a stand-in for the shell
    is the session leader of the terminal, and icdu is its foreground job. With `leader`,
    icdu is the terminal's own command instead (`xterm -e icdu`): the session leader itself,
    with no shell above it. Without `controlling`, the terminal is nobody's controlling
    terminal, and then no hang-up signal is sent when it goes away.
    """

    def __init__(self, binary, args, env=None, controlling=True, leader=False):
        self.master, self.slave = os.openpty()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        # The settings icdu finds, and has to leave behind.
        self.found = termios.tcgetattr(self.slave)
        self.out = bytearray()
        self.pid = self.status = None
        self.stops = self.stops_seen = 0
        self.shell = self.events = self.control = None
        self.told = b""
        environment = dict(os.environ, TERM="xterm-256color")
        environment.pop("NCDU_SHELL", None)
        environment.update(env or {})
        command = [binary] + args
        if leader:
            self.pid = os.fork()
            if self.pid == 0:
                try:
                    os.setsid()
                    if controlling:
                        fcntl.ioctl(self.slave, termios.TIOCSCTTY, 0)
                    self.become(command, environment)
                finally:
                    os._exit(127)
        else:
            self.start_under_a_shell(command, environment, controlling)
        os.set_blocking(self.master, False)

    def become(self, command, environment):
        """In a child process: turn into icdu on the terminal."""
        for sig in SHELL_IGNORES + (signal.SIGPIPE,):
            signal.signal(sig, signal.SIG_DFL)
        for fd in (0, 1, 2):
            os.dup2(self.slave, fd)
        os.closerange(3, 1024)
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        os.execve(command[0], command, environment)

    def start_under_a_shell(self, command, environment, controlling):
        # The stand-in for the shell tells what becomes of its job, and stays until it is
        # told to go: a terminal whose session leader has gone is taken away from everybody.
        events, tell = os.pipe()
        stay, control = os.pipe()
        self.shell = os.fork()
        if self.shell == 0:
            try:
                os.close(events)
                os.close(control)
                os.close(self.master)
                os.setsid()
                if controlling:
                    fcntl.ioctl(self.slave, termios.TIOCSCTTY, 0)
                for sig in SHELL_IGNORES:
                    signal.signal(sig, signal.SIG_IGN)
                wait, go = os.pipe()
                job = os.fork()
                if job:
                    # What a shell does when its terminal is closed: it passes the hang-up
                    # on to its jobs, and continues the stopped ones so that they get it.
                    def hang_up(signum, frame):
                        for sig in (signal.SIGHUP, signal.SIGCONT):
                            try:
                                os.killpg(job, sig)
                            except OSError:
                                pass

                    signal.signal(signal.SIGHUP, hang_up)
                if job == 0:
                    try:
                        os.close(go)
                        os.setpgid(0, 0)
                        os.read(wait, 1)  # until the job is in the foreground
                        self.become(command, environment)
                    finally:
                        os._exit(127)
                os.close(wait)
                try:
                    os.setpgid(job, job)
                except OSError:
                    pass
                if controlling:
                    os.tcsetpgrp(self.slave, job)
                os.write(tell, b"started %d\n" % job)
                os.close(go)
                while True:
                    _, status = os.waitpid(job, os.WUNTRACED)
                    if os.WIFSTOPPED(status):
                        os.write(tell, b"stopped\n")
                    else:
                        os.write(tell, b"ended %d\n" % status)
                        break
                os.read(stay, 1)
            finally:
                os._exit(0)
        os.close(tell)
        os.close(stay)
        self.events, self.control = events, control
        os.set_blocking(self.events, False)
        deadline = time.monotonic() + LIMIT
        while self.pid is None:
            if time.monotonic() > deadline:
                raise RuntimeError("icdu could not be started")
            self.look()
            time.sleep(0.005)

    def look(self):
        """Take in what has become of the process since the last look."""
        if self.status is not None:
            return
        if self.shell is None:
            pid, status = os.waitpid(self.pid, os.WNOHANG | os.WUNTRACED)
            if pid and os.WIFSTOPPED(status):
                self.stops += 1
            elif pid:
                self.status = status
            return
        try:
            self.told += os.read(self.events, 4096)
        except BlockingIOError:
            return
        while b"\n" in self.told:
            line, self.told = self.told.split(b"\n", 1)
            word, _, value = line.partition(b" ")
            if word == b"started":
                self.pid = int(value)
            elif word == b"stopped":
                self.stops += 1
            elif word == b"ended":
                self.status = int(value)

    def drain(self, wait=0.0):
        """Take what icdu has written, waiting up to `wait` seconds for more."""
        if self.master is None:
            time.sleep(wait)
            return
        ready, _, _ = select.select([self.master], [], [], wait)
        if not ready:
            return
        try:
            self.out += os.read(self.master, 65536)
        except BlockingIOError:
            pass
        except OSError as error:
            if error.errno != errno.EIO:
                raise

    def until(self, wanted, limit=LIMIT):
        """Wait until `wanted()` holds. False if it does not within the limit."""
        deadline = time.monotonic() + limit
        while not wanted():
            if time.monotonic() > deadline:
                return False
            self.drain(0.02)
        return True

    def send(self, keys):
        os.write(self.master, keys)

    def seen(self, text):
        return text in self.out

    def in_browser(self, times=1):
        """True once icdu has entered its screen for the `times`-th time and drawn on it."""
        return self.out.count(ALT_ON) >= times and b"first-file" in self.out[self.out.rfind(ALT_ON):]

    def poll(self):
        """The wait status of the process, or None while it is there."""
        self.look()
        return self.status

    def ended(self, limit=LIMIT):
        """Wait for the process to end. Its wait status, or None if it is still there."""
        self.until(lambda: self.poll() is not None, limit)
        self.drain()
        return self.status

    def stopped(self, limit=LIMIT):
        """Wait for the process to be stopped (as by Ctrl-Z)."""

        def stopped_or_gone():
            self.look()
            return self.stops > self.stops_seen or self.status is not None

        self.until(stopped_or_gone, limit)
        if self.stops == self.stops_seen:
            return False
        self.stops_seen = self.stops
        return True

    def close_terminal(self):
        """What closing the window does: the terminal is gone."""
        os.close(self.slave)
        os.close(self.master)
        self.slave = self.master = None

    def terminal_problems(self, screen=True):
        """What is wrong with the terminal as icdu left it."""
        problems = []
        if termios.tcgetattr(self.slave)[:4] != self.found[:4]:
            problems.append("the terminal is still in raw mode")
        if screen and self.out.rfind(ALT_OFF) < self.out.rfind(ALT_ON):
            problems.append("the alternate screen was not left")
        if screen and self.out.rfind(PASTE_OFF) < self.out.rfind(PASTE_ON):
            problems.append("bracketed paste was not switched off")
        if screen and self.out.rfind(CURSOR_ON) < self.out.rfind(CURSOR_OFF):
            problems.append("the cursor was left hidden")
        return problems

    def cleanup(self):
        self.look()
        if self.status is None and self.pid:
            try:
                os.kill(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        # The terminal goes before the process is waited for: on some systems a process
        # cannot finish ending while output of its waits in a terminal that nobody reads.
        # Closing `control` lets the stand-in for the shell go.
        for fd in (self.master, self.slave, self.control):
            if fd is not None:
                os.close(fd)
        self.master = self.slave = self.control = None
        child = self.shell if self.shell is not None else self.pid
        deadline = time.monotonic() + LIMIT
        while True:
            try:
                if os.waitpid(child, os.WNOHANG)[0]:
                    break
            except ChildProcessError:
                break
            if time.monotonic() > deadline:
                os.kill(child, signal.SIGKILL)
                deadline += LIMIT
            time.sleep(0.01)
        if self.events is not None:
            os.close(self.events)
            self.events = None


def how(status):
    """A wait status in words."""
    if status is None:
        return "still running"
    if os.WIFSIGNALED(status):
        return "killed by " + signal.Signals(os.WTERMSIG(status)).name
    return "exit status %d" % os.WEXITSTATUS(status)


def by_signal(status, wanted):
    return status is not None and os.WIFSIGNALED(status) and os.WTERMSIG(status) == wanted


def exit_status(status, wanted):
    return status is not None and os.WIFEXITED(status) and os.WEXITSTATUS(status) == wanted


CHECKS = []


def check(name):
    def register(function):
        CHECKS.append((name, function))
        return function

    return register


def browser(binary, tree, options=(), **more):
    """icdu in the browser, with the scan of the small tree over."""
    run = Run(binary, ["--ignore-config", *options, tree], **more)
    if not run.until(run.in_browser):
        run.cleanup()
        raise RuntimeError("the browser did not come up: %r" % bytes(run.out[-200:]))
    return run


def ends_by(sig):
    def function(binary, tree):
        run = browser(binary, tree)
        try:
            os.kill(run.pid, sig)
            status = run.ended()
            problems = []
            if not by_signal(status, sig):
                problems.append("expected to end by %s: %s" % (sig.name, how(status)))
            return problems + run.terminal_problems()
        finally:
            run.cleanup()

    return function


for _signal in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT, signal.SIGQUIT):
    check("%s in the browser ends icdu and gives the terminal back" % _signal.name)(ends_by(_signal))


@check("q quits with status 0 and gives the terminal back")
def quit_with_q(binary, tree):
    run = browser(binary, tree)
    try:
        run.send(b"q")
        status = run.ended()
        problems = []
        if not exit_status(status, 0):
            problems.append("expected to quit with status 0: %s" % how(status))
        return problems + run.terminal_problems()
    finally:
        run.cleanup()


@check("Ctrl-L draws the screen again, without asking the terminal anything")
def redraw(binary, tree):
    run = browser(binary, tree)
    try:
        run.drain(0.3)
        drawn = len(run.out)
        run.send(b"\x0c")
        problems = []
        if not run.until(lambda: b"first-file" in run.out[drawn:]):
            problems.append("the screen was not drawn again: %s" % how(run.poll()))
        # A question to the terminal (where is the cursor?) is one that not every terminal
        # answers, and icdu would wait for the answer.
        if b"\x1b[6n" in run.out:
            problems.append("icdu asked the terminal for the cursor position")
        run.send(b"q")
        if not exit_status(run.ended(), 0):
            problems.append("expected to quit with status 0: %s" % how(run.status))
        return problems
    finally:
        run.cleanup()


@check("closing the terminal ends icdu")
def closed_terminal(binary, tree):
    run = browser(binary, tree)
    try:
        run.close_terminal()
        status = run.ended()
        if status is None:
            return ["icdu is still running %.0f s after its terminal was closed" % LIMIT]
        return []
    finally:
        run.cleanup()


@check("closing the terminal ends icdu when it is the terminal's own command")
def closed_terminal_of_its_own(binary, tree):
    run = browser(binary, tree, leader=True)
    try:
        run.close_terminal()
        status = run.ended()
        if status is None:
            return ["icdu is still running %.0f s after its terminal was closed" % LIMIT]
        return []
    finally:
        run.cleanup()


@check("closing the terminal ends icdu also where no hang-up signal is sent")
def closed_terminal_without_signal(binary, tree):
    run = browser(binary, tree, controlling=False)
    try:
        run.close_terminal()
        status = run.ended()
        if status is None:
            return ["icdu is still running %.0f s after its terminal was closed" % LIMIT]
        return []
    finally:
        run.cleanup()


@check("SIGTERM ends icdu while the terminal does not take its output")
def stuck_terminal(binary, tree):
    run = browser(binary, tree)
    try:
        # Ask for the screen to be drawn again and again (Ctrl-L) and never read what is
        # drawn. Sooner or later icdu waits in a write, and from then on it does not take
        # the keys either: that is how to tell.
        progress = time.monotonic()
        started = progress
        while time.monotonic() - progress < 1.0:
            if time.monotonic() - started > 20:
                return None  # the terminal's buffers could not be filled here
            try:
                if os.write(run.master, b"\x0c" * 256) > 0:
                    progress = time.monotonic()
            except BlockingIOError:
                time.sleep(0.02)
        os.kill(run.pid, signal.SIGTERM)
        deadline = time.monotonic() + LIMIT
        while run.poll() is None and time.monotonic() < deadline:
            time.sleep(0.02)
        problems = []
        if not by_signal(run.status, signal.SIGTERM):
            problems.append("expected to end by SIGTERM: %s" % how(run.status))
        # The screen cannot be left on a terminal that takes nothing. Raw mode can.
        return problems + (run.terminal_problems(screen=False) if run.status is not None else [])
    finally:
        run.cleanup()


def suspend_and_resume(stop):
    def function(binary, tree):
        run = browser(binary, tree)
        try:
            stop(run)
            if not run.stopped():
                return ["icdu did not stop: %s" % how(run.status)]
            run.drain(0.1)
            problems = ["while stopped: " + p for p in run.terminal_problems()]
            os.kill(run.pid, signal.SIGCONT)
            if not run.until(lambda: run.in_browser(2)):
                return problems + ["the browser did not come back after SIGCONT"]
            run.drain(0.5)
            if run.out.count(ALT_ON) > 2:
                problems.append("the screen was set up more than once after SIGCONT")
            run.send(b"q")
            status = run.ended()
            if not exit_status(status, 0):
                problems.append("expected to quit with status 0: %s" % how(status))
            return problems + run.terminal_problems()
        finally:
            run.cleanup()

    return function


check("Ctrl-Z stops icdu with the terminal given back, and SIGCONT brings it back")(
    suspend_and_resume(lambda run: run.send(b"\x1a"))
)
check("SIGTSTP stops icdu with the terminal given back, and SIGCONT brings it back")(
    suspend_and_resume(lambda run: os.kill(run.pid, signal.SIGTSTP))
)


@check("SIGTERM for a stopped icdu takes effect when it is continued")
def terminate_while_stopped(binary, tree):
    run = browser(binary, tree)
    try:
        run.send(b"\x1a")
        if not run.stopped():
            return ["icdu did not stop: %s" % how(run.status)]
        # A stopped process gets no signal but SIGKILL and SIGCONT: that is the system's
        # rule, and nothing a program can change.
        os.kill(run.pid, signal.SIGTERM)
        os.kill(run.pid, signal.SIGCONT)
        status = run.ended()
        problems = []
        if not by_signal(status, signal.SIGTERM):
            problems.append("expected to end by SIGTERM: %s" % how(status))
        return problems + run.terminal_problems()
    finally:
        run.cleanup()


class Command:
    """icdu with a command that has the terminal and stays: the shell of `b`, or the
    delete command."""

    LINE = "echo the-command-runs $$ . && exec sleep 300"

    def __init__(self, binary, tree, delete=False):
        if delete:
            # icdu puts the path after the command; `#` keeps it away from sleep.
            options = ["--no-confirm-delete", "--delete-command", self.LINE + " #"]
            self.run = browser(binary, tree, options)
            self.run.send(b"d")
        else:
            self.run = browser(binary, tree, env={"NCDU_SHELL": self.LINE})
            self.run.send(b"b")
        self.child = None
        said = lambda: self.run.out[self.run.out.rfind(b"the-command-runs"):]
        if self.run.until(lambda: b" ." in said()):
            self.child = int(bytes(said()).split()[1])

    def cleanup(self):
        if self.child is not None:
            try:
                os.kill(self.child, signal.SIGKILL)
            except ProcessLookupError:
                pass
        self.run.cleanup()

    def back_in_browser_and_quit(self):
        run = self.run
        if not run.until(lambda: run.in_browser(2)):
            return ["icdu did not come back to the browser: %s" % how(run.poll())]
        run.send(b"q")
        status = run.ended()
        problems = []
        if not exit_status(status, 0):
            problems.append("expected to quit with status 0: %s" % how(status))
        return problems + run.terminal_problems()


@check("closing the terminal ends a stopped icdu")
def closed_terminal_while_stopped(binary, tree):
    run = browser(binary, tree)
    try:
        run.send(b"\x1a")
        if not run.stopped():
            return ["icdu did not stop: %s" % how(run.status)]
        # The system continues the stopped processes of a terminal that goes away, so that
        # they get to see the hang-up.
        run.close_terminal()
        if run.ended() is None:
            return ["icdu is still there %.0f s after its terminal was closed" % LIMIT]
        return []
    finally:
        run.cleanup()


@check("SIGTERM ends icdu at once while a command started with `b` has the terminal")
def terminate_during_shell(binary, tree):
    return terminate_during(Command(binary, tree))


@check("SIGTERM ends icdu at once while the delete command has the terminal")
def terminate_during_delete_command(binary, tree):
    return terminate_during(Command(binary, tree, delete=True))


def terminate_during(command):
    try:
        if command.child is None:
            return ["the command did not start"]
        os.kill(command.run.pid, signal.SIGTERM)
        status = command.run.ended()
        if not by_signal(status, signal.SIGTERM):
            return ["expected to end by SIGTERM: %s" % how(status)]
        return command.run.terminal_problems()
    finally:
        command.cleanup()


@check("Ctrl-C for a command started with `b` ends the command and not icdu")
def interrupt_during_shell(binary, tree):
    shell = Command(binary, tree)
    try:
        if shell.child is None:
            return ["the command did not start"]
        # What the terminal does on Ctrl-C: the signal goes to everything in the foreground,
        # the command and icdu.
        os.killpg(shell.run.pid, signal.SIGINT)
        return shell.back_in_browser_and_quit()
    finally:
        shell.cleanup()


@check("Ctrl-C for the delete command ends the command and not icdu")
def interrupt_during_delete_command(binary, tree):
    command = Command(binary, tree, delete=True)
    try:
        if command.child is None:
            return ["the command did not start"]
        os.killpg(command.run.pid, signal.SIGINT)
        # icdu says that the command failed and waits for Enter. A second Ctrl-C there is
        # still not icdu's.
        if not command.run.until(lambda: command.run.seen(b"Press Enter")):
            return ["icdu did not report the failed command: %s" % how(command.run.poll())]
        os.killpg(command.run.pid, signal.SIGINT)
        time.sleep(0.2)
        command.run.send(b"\n")
        return command.back_in_browser_and_quit()
    finally:
        command.cleanup()


@check("SIGTSTP stops icdu while a command started with `b` has the terminal")
def stop_during_shell(binary, tree):
    shell = Command(binary, tree)
    try:
        if shell.child is None:
            return ["the command did not start"]
        os.kill(shell.run.pid, signal.SIGTSTP)
        if not shell.run.stopped():
            return ["icdu did not stop: %s" % how(shell.run.status)]
        os.kill(shell.run.pid, signal.SIGCONT)
        os.kill(shell.child, signal.SIGTERM)
        return shell.back_in_browser_and_quit()
    finally:
        shell.cleanup()


@check("SIGTERM and SIGINT end icdu before the browser has started")
def before_the_browser(binary, tree):
    problems = []
    for sig in (signal.SIGTERM, signal.SIGINT):
        # A dump that never arrives: icdu waits for it, and has not touched the terminal.
        fifo = os.path.join(os.path.dirname(tree), "never-written-" + sig.name)
        os.mkfifo(fifo)
        run = Run(binary, ["--ignore-config", "-f", fifo])
        try:
            time.sleep(0.3)
            os.kill(run.pid, sig)
            status = run.ended()
            if not by_signal(status, sig):
                problems.append("expected to end by %s: %s" % (sig.name, how(status)))
            problems += run.terminal_problems(screen=False)
        finally:
            run.cleanup()
    return problems


def without_terminal(binary, args, closed_output=False):
    """Run icdu with no terminal anywhere: not on standard input or output, and none to
    open. Returns its wait status (None if it did not end) and what it said. With
    `closed_output`, standard output is a pipe that nobody reads, and only standard error
    is collected."""
    reader, writer = os.pipe()
    null = os.open(os.devnull, os.O_RDWR)
    nobody, output = os.pipe() if closed_output else (None, writer)
    pid = os.fork()
    if pid == 0:
        try:
            os.setsid()
            if nobody is not None:
                os.close(nobody)
            # As a shell starts it. (Python itself runs with this signal ignored.)
            signal.signal(signal.SIGPIPE, signal.SIG_DFL)
            os.dup2(null, 0)
            os.dup2(output, 1)
            os.dup2(writer, 2)
            os.closerange(3, 1024)
            resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
            os.execv(binary, [binary, "--ignore-config"] + args)
        finally:
            os._exit(127)
    os.close(writer)
    os.close(null)
    if nobody is not None:
        os.close(nobody)
        os.close(output)
    said = b""
    deadline = time.monotonic() + LIMIT
    while time.monotonic() < deadline:
        ready, _, _ = select.select([reader], [], [], 0.1)
        if ready:
            data = os.read(reader, 65536)
            if not data:
                break
            said += data
    os.close(reader)
    done, status = os.waitpid(pid, os.WNOHANG)
    if not done:
        time.sleep(0.2)
        done, status = os.waitpid(pid, os.WNOHANG)
    if not done:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        return None, said
    return status, said


def no_terminal_problems(status, said):
    problems = []
    if not exit_status(status, 1):
        problems.append("expected exit status 1: %s" % how(status))
    if b"terminal" not in said or b"panicked" in said:
        problems.append("expected a plain message about the terminal: %r" % said[:200])
    if b"\x1b" in said:
        problems.append("escape sequences were written to what is not a terminal")
    return problems


@check("without a terminal, icdu says so and exits with status 1")
def no_terminal(binary, tree):
    return no_terminal_problems(*without_terminal(binary, [tree]))


@check("without a terminal, icdu does not start on work that it could not show")
def no_terminal_before_work(binary, tree):
    # A dump that never arrives. With a terminal, icdu would wait for it.
    fifo = os.path.join(os.path.dirname(tree), "never-written")
    os.mkfifo(fifo)
    return no_terminal_problems(*without_terminal(binary, ["-f", fifo]))


def quiet_end(binary, args):
    status, said = without_terminal(binary, args, closed_output=True)
    problems = []
    if not by_signal(status, signal.SIGPIPE):
        problems.append("expected to end by SIGPIPE: %s" % how(status))
    if said:
        problems.append("expected no message: %r" % said[:200])
    return problems


@check("output that nobody reads ends icdu quietly: the help")
def help_into_closed_pipe(binary, tree):
    return quiet_end(binary, ["--help"])


@check("output that nobody reads ends icdu quietly: a dump")
def dump_into_closed_pipe(binary, tree):
    return quiet_end(binary, ["-o-", tree])


def main():
    binary = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/release/icdu")
    word = sys.argv[2] if len(sys.argv) > 2 else ""
    if not os.access(binary, os.X_OK):
        sys.exit("terminal-test.py: %s is not there (cargo build --release)" % binary)
    top = tempfile.mkdtemp(prefix="icdu-terminal-test-")
    tree = os.path.join(top, "tree")
    os.makedirs(os.path.join(tree, "a-directory"))
    for name, size in (("first-file", 5000), ("second-file", 300), ("a-directory/third", 70)):
        with open(os.path.join(tree, name), "wb") as file:
            file.write(b"x" * size)

    def too_long(signum, frame):
        raise TimeoutError("the check did not come back within %d s" % CHECK_LIMIT)

    signal.signal(signal.SIGALRM, too_long)
    failed = skipped = 0
    try:
        for name, function in CHECKS:
            if word not in name:
                continue
            started = time.monotonic()
            signal.alarm(CHECK_LIMIT)
            try:
                problems = function(binary, tree)
            except Exception as error:  # a check that could not be made is a failed check
                problems = ["%s: %s" % (type(error).__name__, error)]
            finally:
                signal.alarm(0)
            took = time.monotonic() - started
            if problems is None:
                skipped += 1
                print("skipped       %s" % name)
            elif problems:
                failed += 1
                print("FAILED %5.1fs  %s" % (took, name))
                for problem in problems:
                    print("                - %s" % problem)
            else:
                print("ok     %5.1fs  %s" % (took, name))
            sys.stdout.flush()
    finally:
        shutil.rmtree(top, ignore_errors=True)
    if failed:
        sys.exit("%d of the checks failed" % failed)
    if skipped:
        print("%d could not be made here" % skipped)


if __name__ == "__main__":
    main()
