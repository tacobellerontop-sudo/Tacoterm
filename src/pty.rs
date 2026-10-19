//! Windows pseudo-console (ConPTY) wrapper.
//!
//! Spawns the shell attached to a pseudo-console and hands back two pipes:
//! write keystrokes to `input`, read terminal bytes from `output`.
//!
//! The read side blocks, so the app drains it on a dedicated thread and
//! forwards chunks to the UI thread over a channel. Everything else —
//! writing input, resizing — happens on the UI thread.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
use windows::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
    InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    STARTUPINFOW,
};

pub struct Pty {
    hpc: HPCON,
    input: File,
    output: File,
    child: HANDLE,
}

fn pipe() -> io::Result<(HANDLE, HANDLE)> {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    unsafe {
        CreatePipe(&mut read, &mut write, None, 0).map_err(io::Error::from)?;
    }
    Ok((read, write))
}

impl Pty {
    /// Spawn `shell` (e.g. `C:\Windows\System32\cmd.exe`) on a new
    /// pseudo-console of `cols`×`rows`, starting in `cwd` if given.
    pub fn spawn(shell: &str, cols: u16, rows: u16, cwd: Option<&str>) -> io::Result<Pty> {
        unsafe {
            // Two pipes: we write stdin, the console reads it; the console
            // writes stdout, we read it.
            let (pty_in_read, our_in_write) = pipe()?;
            let (our_out_read, pty_out_write) = pipe()?;

            let size = COORD {
                X: cols as i16,
                Y: rows as i16,
            };
            let created = CreatePseudoConsole(size, pty_in_read, pty_out_write, 0);
            // The console holds its own references to its ends; close ours.
            let _ = CloseHandle(pty_in_read);
            let _ = CloseHandle(pty_out_write);
            let hpc = match created {
                Ok(hpc) => hpc,
                Err(e) => {
                    let _ = CloseHandle(our_in_write);
                    let _ = CloseHandle(our_out_read);
                    return Err(io::Error::from(e));
                }
            };
            // From here on, dropping `pty` cleans everything up.
            let mut pty = Pty {
                hpc,
                input: File::from_raw_handle(our_in_write.0),
                output: File::from_raw_handle(our_out_read.0),
                child: HANDLE::default(),
            };

            // Attribute list carrying the pseudo-console handle into the child.
            let mut needed: usize = 0;
            let _ = InitializeProcThreadAttributeList(
                LPPROC_THREAD_ATTRIBUTE_LIST(std::ptr::null_mut()),
                1,
                0,
                &mut needed,
            );
            // usize-aligned backing store for the opaque list.
            let mut attr_mem = vec![0usize; needed.div_ceil(std::mem::size_of::<usize>())];
            let attr_list = LPPROC_THREAD_ATTRIBUTE_LIST(attr_mem.as_mut_ptr().cast());
            InitializeProcThreadAttributeList(attr_list, 1, 0, &mut needed)
                .map_err(io::Error::from)?;
            let updated = UpdateProcThreadAttribute(
                attr_list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                Some(hpc.0 as *const core::ffi::c_void),
                std::mem::size_of::<HPCON>(),
                None,
                None,
            );
            if let Err(e) = updated {
                DeleteProcThreadAttributeList(attr_list);
                return Err(io::Error::from(e));
            }

            let mut si_ex = STARTUPINFOEXW::default();
            si_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            si_ex.lpAttributeList = attr_list;
            // Null std handles: without this, a child of a process that has
            // its own console (debug builds) can attach to that console
            // instead of the pseudo-console.
            si_ex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;

            // CreateProcessW wants a *mutable* UTF-16 buffer.
            let mut cmd: Vec<u16> = OsStr::new(shell).encode_wide().chain([0]).collect();
            let dir = cwd
                .filter(|d| std::path::Path::new(d).is_dir())
                .map(HSTRING::from);
            let dir_ptr = dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr()));
            let mut pi = PROCESS_INFORMATION::default();
            let spawned = CreateProcessW(
                None,
                PWSTR(cmd.as_mut_ptr()),
                None,
                None,
                false,
                EXTENDED_STARTUPINFO_PRESENT,
                None,
                dir_ptr,
                std::ptr::addr_of_mut!(si_ex).cast::<STARTUPINFOW>(),
                &mut pi,
            );
            DeleteProcThreadAttributeList(attr_list);
            spawned.map_err(io::Error::from)?;

            let _ = CloseHandle(pi.hThread);
            pty.child = pi.hProcess;
            Ok(pty)
        }
    }

    /// Clone the read end for the drain thread; reads only happen there.
    pub fn try_clone_reader(&self) -> io::Result<File> {
        self.output.try_clone()
    }

    /// Send keystrokes to the shell.
    pub fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.input.write_all(bytes)
    }

    /// Tell the console the window changed size.
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        unsafe {
            ResizePseudoConsole(
                self.hpc,
                COORD {
                    X: cols as i16,
                    Y: rows as i16,
                },
            )
            .map_err(io::Error::from)
        }
    }

    /// A handle the caller can block on (on another thread) until the shell
    /// exits. ConPTY keeps the output pipe open after the child dies, so a
    /// reader never sees EOF; this is how the app notices.
    pub fn exit_waiter(&self) -> io::Result<ExitWaiter> {
        let mut dup = HANDLE::default();
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.child,
                GetCurrentProcess(),
                &mut dup,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
            .map_err(io::Error::from)?;
        }
        Ok(ExitWaiter(dup.0 as isize))
    }

    /// Force-kill the child (used on quit; dropping also closes the console).
    pub fn terminate(&mut self) -> io::Result<()> {
        unsafe { TerminateProcess(self.child, 1).map_err(io::Error::from) }
    }
}

/// An owned process handle that can move to another thread.
pub struct ExitWaiter(isize);

impl ExitWaiter {
    /// Block until the process exits.
    pub fn wait(self) {
        let h = HANDLE(self.0 as *mut core::ffi::c_void);
        unsafe {
            WaitForSingleObject(h, INFINITE);
            let _ = CloseHandle(h);
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe {
            ClosePseudoConsole(self.hpc);
            if !self.child.is_invalid() {
                let _ = CloseHandle(self.child);
            }
        }
    }
}
