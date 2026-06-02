/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! This module provides {e,}print{ln,} macros that return errors when they fail, unlike the std
//! macros, which yield panics. The errors returned by those methods don't make sense to handle in
//! place, and should usually just be propagated in order to lead to a quick exit.

use std::fmt::Arguments;
use std::fs::File;
use std::io;
use std::io::LineWriter;
use std::io::Stdout;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use bz_error::internal_error;
use crossterm::tty::IsTty as CrosstermIsTty;
use superconsole::Line;
use tokio::sync::mpsc;

use crate::exit_result::ClientIoError;

static HAS_WRITTEN_TO_STDOUT: AtomicBool = AtomicBool::new(false);
static OUTPUT_TAP: Mutex<Option<mpsc::UnboundedSender<OutputEvent>>> = Mutex::new(None);

#[derive(Clone, Copy, Debug)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug)]
pub struct OutputEvent {
    pub stream: OutputStream,
    pub bytes: Vec<u8>,
}

pub struct OutputTapGuard {
    previous: Option<mpsc::UnboundedSender<OutputEvent>>,
}

impl Drop for OutputTapGuard {
    fn drop(&mut self) {
        *OUTPUT_TAP.lock().unwrap() = self.previous.take();
    }
}

pub fn install_output_tap(sender: mpsc::UnboundedSender<OutputEvent>) -> OutputTapGuard {
    let mut tap = OUTPUT_TAP.lock().unwrap();
    OutputTapGuard {
        previous: tap.replace(sender),
    }
}

fn tap_output(stream: OutputStream, bytes: &[u8]) {
    let sender = OUTPUT_TAP.lock().unwrap().clone();
    if let Some(sender) = sender {
        let _ignored = sender.send(OutputEvent {
            stream,
            bytes: bytes.to_vec(),
        });
    }
}

pub fn has_written_to_stdout() -> bool {
    HAS_WRITTEN_TO_STDOUT.load(Ordering::Relaxed)
}

static STDOUT_LOCKED: AtomicBool = AtomicBool::new(false);

fn stdout() -> bz_error::Result<io::Stdout> {
    if STDOUT_LOCKED.load(Ordering::Relaxed) {
        return Err(internal_error!("stdout is already locked"));
    }
    HAS_WRITTEN_TO_STDOUT.store(true, Ordering::Relaxed);
    Ok(io::stdout())
}

#[macro_export]
macro_rules! print {
    () => {
        $crate::stdio::_print(std::format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::stdio::_print(std::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! println {
    () => {
        $crate::stdio::_print(std::format_args!("\n"))
    };
    ($fmt:tt) => {
        $crate::stdio::_print(std::format_args!(concat!($fmt, "\n")))
    };
    ($fmt:tt, $($arg:tt)*) => {
        $crate::stdio::_print(std::format_args!(concat!($fmt, "\n"), $($arg)*))
    };
}

#[macro_export]
macro_rules! eprint {
    () => {
        $crate::stdio::_eprint(std::format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::stdio::_eprint(std::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! eprintln {
    () => {
        $crate::stdio::_eprint(std::format_args!("\n"))
    };
    ($fmt:expr) => {
        $crate::stdio::_eprint(std::format_args!(concat!($fmt, "\n")))
    };
    ($fmt:expr, $($arg:tt)*) => {
        $crate::stdio::_eprint(std::format_args!(concat!($fmt, "\n"), $($arg)*))
    };
}

pub fn _print(fmt: Arguments) -> bz_error::Result<()> {
    let message = fmt.to_string();
    stdout()?
        .lock()
        .write_all(message.as_bytes())
        .map_err(|e| bz_error::Error::from(ClientIoError::from(e)))?;
    tap_output(OutputStream::Stdout, message.as_bytes());
    Ok(())
}

pub fn _eprint(fmt: Arguments) -> bz_error::Result<()> {
    let message = fmt.to_string();
    io::stderr()
        .lock()
        .write_all(message.as_bytes())
        .map_err(|e| bz_error::Error::from(ClientIoError::from(e)))?;
    tap_output(OutputStream::Stderr, message.as_bytes());
    Ok(())
}

pub fn print_bytes(bytes: &[u8]) -> bz_error::Result<()> {
    stdout()?
        .lock()
        .write_all(bytes)
        .map_err(|e| bz_error::Error::from(ClientIoError::from(e)))?;
    tap_output(OutputStream::Stdout, bytes);
    Ok(())
}

pub fn eprint_line(line: &Line) -> bz_error::Result<()> {
    let line = line.render();
    crate::eprintln!("{}", line)
}

pub fn flush() -> bz_error::Result<()> {
    stdout()?.flush().map_err(|e| ClientIoError::from(e).into())
}

fn stdout_to_file(stdout: &Stdout) -> bz_error::Result<File> {
    #[cfg(not(windows))]
    {
        use std::os::fd::AsFd;
        Ok(File::from(stdout.as_fd().try_clone_to_owned()?))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        Ok(File::from(stdout.as_handle().try_clone_to_owned()?))
    }
}

#[derive(Debug)]
pub struct StdoutWriter;

impl StdoutWriter {
    pub fn new() -> Self {
        Self
    }
}

impl Write for StdoutWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if STDOUT_LOCKED.load(Ordering::Relaxed) {
            return Err(io::Error::other("stdout is already locked"));
        }
        HAS_WRITTEN_TO_STDOUT.store(true, Ordering::Relaxed);
        let written = io::stdout().lock().write(buf)?;
        tap_output(OutputStream::Stdout, &buf[..written]);
        Ok(written)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        if STDOUT_LOCKED.load(Ordering::Relaxed) {
            return Err(io::Error::other("stdout is already locked"));
        }
        HAS_WRITTEN_TO_STDOUT.store(true, Ordering::Relaxed);
        io::stdout().lock().write_all(buf)?;
        tap_output(OutputStream::Stdout, buf);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().lock().flush()
    }
}

impl CrosstermIsTty for StdoutWriter {
    fn is_tty(&self) -> bool {
        CrosstermIsTty::is_tty(&io::stdout())
    }
}

#[derive(Debug)]
pub struct StderrWriter;

impl StderrWriter {
    pub fn new() -> Self {
        Self
    }
}

impl Write for StderrWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = io::stderr().lock().write(buf)?;
        tap_output(OutputStream::Stderr, &buf[..written]);
        Ok(written)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        io::stderr().lock().write_all(buf)?;
        tap_output(OutputStream::Stderr, buf);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().lock().flush()
    }
}

pub async fn print_with_writer<E, F>(f: F) -> bz_error::Result<()>
where
    E: Into<bz_error::Error>,
    F: AsyncFnOnce(&mut (dyn Write + Send)) -> Result<(), E>,
{
    let stdout = stdout()?;

    struct StdoutLockedGuard;

    impl Drop for StdoutLockedGuard {
        fn drop(&mut self) {
            assert!(
                STDOUT_LOCKED
                    .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            );
        }
    }

    assert!(
        STDOUT_LOCKED
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    );
    let _guard = StdoutLockedGuard;

    let _guard = stdout.lock();
    let file = stdout_to_file(&stdout)?;
    let mut w = LineWriter::new(file);
    match f(&mut w).await {
        Ok(()) => {}
        Err(e) => return Err(e.into()),
    }
    w.flush()?;
    Ok(())
}
