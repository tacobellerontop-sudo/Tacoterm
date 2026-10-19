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
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Console::{
    ClosePseudoConsole, CreatePipe, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    UpdateProcThreadAttribute, TerminateProcess, EXTENDED_STARTUPINFO_PRESENT,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
    STARTUPINFOEXW, STARTUPINFOW,
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
    /// pseudo-console of `cols`×`rows`.
    pub fn spawn(shell: &str, cols: u16, rows: u16) -> io::Result<Pty> {
        unsafe {
            // Two pipes: we write stdin, the console reads it; the console
            // writes stdout, we read it.
            let (pty_in_read, our_in_write) = pipe()?;
            let (our_out_read, pty_out_write) = pipe()?;

            let size = COORD {
                X: cols as i16,
                Y: rows as i16,
            };
            let mut hpc = HPCON::default();
            if let Err(e) = CreatePseudoConsole(size, pty_in_read, pty_out_write, 0, &mut hpc) {
                let _ = CloseHandle(pty_in_read);
                let _ = CloseHandle(pty_out_write);
                let _ = CloseHandle(our_in_write);
                let _ = CloseHandle(our_out_read);
                return Err(io::Error::from(e));
            }
            // The console owns its ends now; close our copies.
            let _ = CloseHandle(pty_in_read);
            let _ = CloseHandle(pty_out_write);

            // Attribute list carrying the pseudo-console handle into the child.
            let mut needed: usize = 0;
            let _ = InitializeProcThreadAttributeList(None, 1, 0, &mut needed);
            let mut attr_mem = vec![0u8; needed];
            let attr_list = LPPROC_THREAD_ATTRIBUTE_LIST(attr_mem.as_mut_ptr().cast());
            InitializeProcThreadAttributeList(Some(attr_list), 1, 0, &mut needed)
                .map_err(io::Error::from)?;
            UpdateProcThreadAttribute(
                attr_list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                Some(hpc.0 as *const core::ffi::c_void),
                std::mem::size_of::<HPCON>(),
                None,
                None,
            )
            .map_err(io::Error::from)?;

            let mut si_ex = STARTUPINFOEXW::default();
            si_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            si_ex.lpAttributeList = attr_list;

            // CreateProcessW wants a *mutable* UTF-16 buffer.
            let mut cmd: Vec<u16> =
                OsStr::new(shell).encode_wide().chain([0]).collect();
            let mut pi = PROCESS_INFORMATION::default();
            let spawned = CreateProcessW(
                None,
                Some(PWSTR(cmd.as_mut_ptr())),
                None,
                None,
                false,
                EXTENDED_STARTUPINFO_PRESENT,
                None,
                None,
                std::ptr::addr_of_mut!(si_ex).cast::<STARTUPINFOW>(),
                &mut pi,
            );

            DeleteProcThreadAttributeList(attr_list);

            if let Err(e) = spawned {
                ClosePseudoConsole(hpc);
                let _ = CloseHandle(our_in_write);
                let _ = CloseHandle(our_out_read);
                return Err(io::Error::from(e));
            }
            let _ = CloseHandle(pi.hThread);

            Ok(Pty {
                hpc,
                input: File::from_raw_handle(our_in_write.0),
                output: File::from_raw_handle(our_out_read.0),
                child: pi.hProcess,
            })
        }
    }

    /// Clone the read end for the drain thread. The original stays here for
    /// nothing — reads only happen on the clone.
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

    /// Force-kill the child (used on quit; dropping also closes the console).
    pub fn terminate(&mut self) -> io::Result<()> {
        unsafe { TerminateProcess(self.child, 1).map_err(io::Error::from) }
    }

    /// Drain whatever is available without blocking. Returns bytes read.
    /// (The drain thread mostly does blocking reads; this is for shutdown.)
    #[allow(dead_code)]
    pub fn read_available(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.output.read(buf)
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe {
            ClosePseudoConsole(self.hpc);
            let _ = CloseHandle(self.child);
        }
    }
}
