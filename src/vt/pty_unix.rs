//! Starts a program on a Unix pseudoterminal.

use super::{ENVIRONMENT, Spawned};
use std::{
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    path::Path,
    process::{Command, Stdio},
};

/// The master side of a running program's pseudoterminal.
pub(super) struct Handle {
    master: File,
    pid: libc::pid_t,
}

impl Handle {
    pub fn resize(&self, size: [u16; 2]) -> io::Result<()> {
        let winsize = window_size(size);
        // SAFETY: a valid descriptor and a winsize that outlives the call.
        check(unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &winsize) })?;
        Ok(())
    }

    /// End the running program's process group, as closing a terminal window does.
    pub fn hang_up(&self) {
        // SAFETY: the child is unreaped, so its pid and process group are still ours.
        unsafe {
            libc::kill(-self.pid, libc::SIGHUP);
        }
    }
}

pub(super) fn spawn(command: &[String], cwd: &Path, size: [u16; 2]) -> io::Result<Spawned> {
    let [program, args @ ..] = command else {
        return Err(io::Error::other("No shell to run"));
    };
    let mut master = -1;
    let mut slave = -1;
    let mut winsize = window_size(size);
    // SAFETY: the out-pointers are valid; a null name and termios are permitted.
    check(unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut winsize,
        )
    })?;
    // SAFETY: openpty returned these descriptors and nothing else owns them.
    let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    // SAFETY: a valid descriptor; the child must only inherit the slave.
    check(unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
    let mut shell = Command::new(program);
    shell
        .args(args)
        .current_dir(cwd)
        .envs(ENVIRONMENT)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls, on the descriptors Command just set up.
    unsafe {
        shell.pre_exec(|| {
            // A parent ignoring these, as under nohup, must not pass that on: a
            // shell that ignores SIGHUP would outlive its panel.
            for signal in [
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGALRM,
            ] {
                libc::signal(signal, libc::SIG_DFL);
            }
            check(libc::setsid())?;
            check(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
            Ok(())
        });
    }
    let mut child = shell.spawn()?;
    Ok(Spawned {
        reader: Box::new(master.try_clone()?),
        writer: Box::new(master.try_clone()?),
        pty: Handle {
            pid: child.id() as libc::pid_t,
            master,
        },
        wait: Box::new(move || {
            let _ = child.wait();
        }),
    })
}

fn window_size([columns, rows]: [u16; 2]) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}
