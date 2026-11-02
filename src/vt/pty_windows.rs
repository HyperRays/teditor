//! Starts a program on a Windows pseudo-console (ConPTY).
//!
//! Adapted from the `portable-pty` crate in WezTerm, Copyright (c) 2018-Present Wez
//! Furlong, under the MIT license; see `licenses/portable-pty-MIT.txt`.

use super::{ENVIRONMENT, Spawned, command_line};
use std::{
    env,
    ffi::{OsStr, OsString},
    io, mem,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{INVALID_HANDLE_VALUE, S_OK},
    System::{
        Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole},
        Threading::{
            CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
            EXTENDED_STARTUPINFO_PRESENT, INFINITE, InitializeProcThreadAttributeList,
            LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
            STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
        },
    },
};

/// A running program's pseudo-console. Closing it ends the program.
pub(super) struct Handle(HPCON);

// SAFETY: a pseudo-console handle may be used and closed from any thread.
unsafe impl Send for Handle {}

impl Handle {
    pub fn resize(&self, size: [u16; 2]) -> io::Result<()> {
        // SAFETY: the handle is live until drop.
        succeeded(unsafe { ResizePseudoConsole(self.0, coord(size)) })
    }

    /// Nothing to do: dropping this handle closes the pseudo-console, which ends the program.
    pub fn hang_up(&self) {}
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is live, and this is its only close.
        unsafe { ClosePseudoConsole(self.0) };
    }
}

pub(super) fn spawn(command: &[String], cwd: &Path, size: [u16; 2]) -> io::Result<Spawned> {
    let [program, args @ ..] = command else {
        return Err(io::Error::other("No shell to run"));
    };
    let (input_read, input_write) = io::pipe()?;
    let (output_read, output_write) = io::pipe()?;
    let mut console = 0 as HPCON;
    // SAFETY: valid pipe ends, which the pseudo-console duplicates, and a valid out-pointer.
    succeeded(unsafe {
        CreatePseudoConsole(
            coord(size),
            input_read.as_raw_handle(),
            output_write.as_raw_handle(),
            0,
            &mut console,
        )
    })?;
    let pty = Handle(console);
    // Our copies of the pseudo-console's ends must close, or reads would never finish.
    drop((input_read, output_write));

    let process = create_process(&pty, program, args, cwd)?;
    Ok(Spawned {
        pty,
        reader: Box::new(output_read),
        writer: Box::new(input_write),
        wait: Box::new(move || {
            // SAFETY: the process handle stays open for the whole call.
            unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) };
        }),
    })
}

/// Start a program attached to the pseudo-console, returning its process handle.
fn create_process(
    console: &Handle,
    program: &str,
    args: &[String],
    cwd: &Path,
) -> io::Result<OwnedHandle> {
    let program = find_program(program)?;
    let mut arguments = vec![program.to_string_lossy().into_owned()];
    arguments.extend_from_slice(args);
    let mut command_line = wide(OsStr::new(&command_line(&arguments)));
    let program = wide(program.as_os_str());
    let cwd = wide(cwd.as_os_str());
    let mut environment = environment_block();
    let mut attributes = AttributeList::new(console)?;

    // SAFETY: an all-zero STARTUPINFOEXW and PROCESS_INFORMATION are valid values.
    let mut startup: STARTUPINFOEXW = unsafe { mem::zeroed() };
    let mut process: PROCESS_INFORMATION = unsafe { mem::zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    // Otherwise a parent with redirected output would leak those handles to the child,
    // which would then write there instead of to the pseudo-console.
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    startup.lpAttributeList = attributes.as_ptr();
    // SAFETY: every string is nul-terminated UTF-16, and they all outlive the call.
    let created = unsafe {
        CreateProcessW(
            program.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            environment.as_mut_ptr().cast(),
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut process,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateProcessW succeeded, so we own both handles. The thread's closes here.
    unsafe {
        drop(OwnedHandle::from_raw_handle(process.hThread));
        Ok(OwnedHandle::from_raw_handle(process.hProcess))
    }
}

/// The attribute list that attaches a new process to a pseudo-console.
struct AttributeList(Vec<usize>);

impl AttributeList {
    fn new(console: &Handle) -> io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: a null list asks only for the required size, so this "fails" by design.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut bytes) };
        // usize elements, because the system requires pointer alignment.
        let mut list = Self(vec![0; bytes.div_ceil(size_of::<usize>())]);
        // SAFETY: the list has the size the system asked for.
        if unsafe { InitializeProcThreadAttributeList(list.as_ptr(), 1, 0, &mut bytes) } == 0 {
            mem::forget(list); // Never initialized, so it must not be deleted.
            return Err(io::Error::last_os_error());
        }
        // SAFETY: an initialized list; this attribute takes the handle itself as its value.
        let updated = unsafe {
            UpdateProcThreadAttribute(
                list.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                console.0 as *const _,
                size_of::<HPCON>(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if updated == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(list)
    }

    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: construction only returns a list it initialized.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

/// Resolve a program through `PATH`. Leaving this to the system would also search the
/// current directory, which could run a program planted in an opened folder.
fn find_program(program: &str) -> io::Result<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return Ok(path.to_owned());
    }
    let extensions = env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
    for directory in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
        // Skip relative entries, which are the current directory by another name.
        if directory.is_relative() {
            continue;
        }
        let exact = directory.join(program);
        if exact.is_file() {
            return Ok(exact);
        }
        for extension in env::split_paths(&extensions) {
            let mut candidate = exact.clone().into_os_string();
            candidate.push(&extension);
            if Path::new(&candidate).is_file() {
                return Ok(candidate.into());
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{program} was not found on PATH"),
    ))
}

/// This process's environment plus the terminal's own variables, sorted without
/// regard to case as Windows requires.
fn environment_block() -> Vec<u16> {
    let mut variables: Vec<(OsString, OsString)> = env::vars_os()
        .filter(|(name, _)| {
            !ENVIRONMENT
                .iter()
                .any(|(own, _)| name.eq_ignore_ascii_case(own))
        })
        .collect();
    variables.extend(ENVIRONMENT.map(|(name, value)| (name.into(), value.into())));
    variables.sort_by_cached_key(|(name, _)| name.to_ascii_uppercase());
    let mut block = Vec::new();
    for (name, value) in variables {
        block.extend(name.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain([0]).collect()
}

fn coord([columns, rows]: [u16; 2]) -> COORD {
    COORD {
        X: columns.min(i16::MAX as u16) as i16,
        Y: rows.min(i16::MAX as u16) as i16,
    }
}

fn succeeded(result: i32) -> io::Result<()> {
    if result == S_OK {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}
