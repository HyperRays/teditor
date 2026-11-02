//! Runs a program on a pseudoterminal, with I/O on background threads.
//!
//! The threads and channels are shared; `pty_unix.rs` and `pty_windows.rs` each start
//! the process and hand back its streams.

use std::{
    io::{self, Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
};

#[cfg(unix)]
#[path = "pty_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "pty_windows.rs"]
mod platform;

/// Called from I/O threads when output or exit needs the UI's attention.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Environment every program in the panel gets, so it knows what it is talking to.
const ENVIRONMENT: [(&str, &str); 3] = [
    ("TERM", "xterm-256color"),
    ("COLORTERM", "truecolor"),
    ("TERM_PROGRAM", "teditor"),
];

/// What a platform hands back after starting a program.
struct Spawned {
    pty: platform::Handle,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    /// Blocks until the program ends.
    wait: Box<dyn FnOnce() + Send>,
}

/// A child process on a pseudoterminal, with background threads doing its I/O.
pub struct Pty {
    input: Sender<Vec<u8>>,
    output: Receiver<Vec<u8>>,
    wake_pending: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    /// Last, so the channels close first. Closing a Windows pseudo-console can wait for
    /// its output to be read, and the reader thread stops once `output` is gone.
    pty: platform::Handle,
}

impl Pty {
    /// Start `command` in `cwd`, calling `waker` when output arrives.
    pub fn spawn(command: &[String], cwd: &Path, size: [u16; 2], waker: Waker) -> io::Result<Self> {
        let Spawned {
            pty,
            mut reader,
            mut writer,
            wait,
        } = platform::spawn(command, cwd, size)?;
        let wake_pending = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let (output_send, output) = mpsc::sync_channel(32);
        let (pending, wake) = (wake_pending.clone(), waker.clone());
        std::thread::spawn(move || {
            let mut buffer = vec![0; 64 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if output_send.send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                        if !pending.swap(true, Ordering::AcqRel) {
                            wake();
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    // How Linux reports the last slave closing (EIO).
                    Err(_) => break,
                }
            }
        });
        let (input, input_receive) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            while let Ok(bytes) = input_receive.recv() {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
            }
        });
        let finished = exited.clone();
        std::thread::spawn(move || {
            wait();
            finished.store(true, Ordering::Release);
            waker();
        });
        Ok(Self {
            input,
            output,
            wake_pending,
            exited,
            pty,
        })
    }

    /// Tell the child its window changed size.
    pub fn resize(&self, size: [u16; 2]) -> io::Result<()> {
        self.pty.resize(size)
    }

    /// Queue input; a child that stops reading can never block the UI.
    pub fn write(&self, bytes: Vec<u8>) {
        let _ = self.input.send(bytes);
    }

    /// Take buffered output, up to about `budget` bytes. Returns whether more remains.
    pub fn read(&self, budget: usize, mut sink: impl FnMut(&[u8])) -> bool {
        self.wake_pending.store(false, Ordering::Release);
        let mut taken = 0;
        while taken < budget {
            match self.output.try_recv() {
                Ok(bytes) => {
                    taken += bytes.len();
                    sink(&bytes);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return false,
            }
        }
        true
    }

    /// Whether the child has ended.
    pub fn exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if !self.exited() {
            self.pty.hang_up();
        }
    }
}

/// Join arguments into one Windows command line, quoted the way programs parse it
/// back: backslashes are only special when they come before a quote.
#[cfg(any(windows, test))]
fn command_line(arguments: &[String]) -> String {
    let mut line = String::new();
    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        if !argument.is_empty() && !argument.contains([' ', '\t', '\n', '\x0b', '"']) {
            line.push_str(argument);
            continue;
        }
        line.push('"');
        let mut backslashes = 0;
        for character in argument.chars() {
            match character {
                '\\' => backslashes += 1,
                '"' => {
                    line.push_str(&"\\".repeat(backslashes * 2 + 1));
                    line.push('"');
                    backslashes = 0;
                }
                other => {
                    line.push_str(&"\\".repeat(backslashes));
                    line.push(other);
                    backslashes = 0;
                }
            }
        }
        line.push_str(&"\\".repeat(backslashes * 2));
        line.push('"');
    }
    line
}

#[cfg(test)]
mod tests {
    use super::command_line;

    #[test]
    fn windows_command_lines_quote_only_what_needs_it() {
        for (arguments, expected) in [
            (&["cmd.exe"][..], "cmd.exe"),
            (&["a", "b c"][..], r#"a "b c""#),
            (&["a", ""][..], r#"a """#),
            (
                &[r"C:\Program Files\x.exe"][..],
                r#""C:\Program Files\x.exe""#,
            ),
            (&[r"C:\no\spaces\"][..], r"C:\no\spaces\"),
            (&[r"trailing slash\ "][..], r#""trailing slash\ ""#),
            (&[r"with space\"][..], r#""with space\\""#),
            (&[r#"say "hi""#][..], r#""say \"hi\"""#),
            (&[r#"a\"b"#][..], r#""a\\\"b""#),
            (&[r#"é "ü""#][..], r#""é \"ü\"""#),
        ] {
            let arguments: Vec<_> = arguments.iter().map(ToString::to_string).collect();
            assert_eq!(command_line(&arguments), expected);
        }
    }
}
