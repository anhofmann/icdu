"""What the system says about a terminal whose other end has been closed."""
import fcntl, os, platform, select, termios


def attempt(name, action):
    try:
        print("  %-22s -> %r" % (name, action()))
    except Exception as error:
        print("  %-22s -> ERROR %s: %s" % (name, type(error).__name__, error))


def poll_flags(fd):
    poller = select.poll()
    poller.register(fd, select.POLLIN | select.POLLOUT)
    names = {select.POLLIN: "IN", select.POLLOUT: "OUT", select.POLLHUP: "HUP", select.POLLERR: "ERR", select.POLLNVAL: "NVAL"}
    return [[name for bit, name in names.items() if flags & bit] for _, flags in poller.poll(0)]


def read_nothing(fd):
    os.set_blocking(fd, False)
    return os.read(fd, 1)


print(platform.platform())
for raw in (False, True):
    master, slave = os.openpty()
    if raw:
        settings = termios.tcgetattr(slave)
        settings[3] &= ~(termios.ICANON | termios.ECHO | termios.ISIG)
        termios.tcsetattr(slave, termios.TCSANOW, settings)
    print("raw mode:" if raw else "line mode:")
    attempt("isatty, open", lambda: os.isatty(slave))
    attempt("write 0 bytes, open", lambda: os.write(slave, b""))
    os.close(master)
    copy = os.dup(slave)
    attempt("isatty", lambda: os.isatty(slave))
    attempt("isatty of a dup", lambda: os.isatty(copy))
    attempt("tcgetattr", lambda: termios.tcgetattr(slave)[3])
    attempt("TIOCGWINSZ", lambda: fcntl.ioctl(slave, termios.TIOCGWINSZ, b"\0" * 8))
    attempt("ttyname", lambda: os.ttyname(slave))
    attempt("fstat nlink", lambda: os.fstat(slave).st_nlink)
    attempt("poll", lambda: poll_flags(slave))
    attempt("write 0 bytes", lambda: os.write(slave, b""))
    attempt("write 0 bytes, dup", lambda: os.write(copy, b""))
    attempt("write NUL", lambda: os.write(slave, b"\0"))
    attempt("read", lambda: read_nothing(slave))
    attempt("tcdrain", lambda: termios.tcdrain(slave))
    attempt("reopen by name", lambda: os.close(os.open(os.ttyname(slave), os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)))
    os.close(slave)
    os.close(copy)
