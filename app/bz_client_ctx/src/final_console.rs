/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use superconsole::style::Attribute;
use superconsole::style::Color;
use superconsole::style::ContentStyle;
use superconsole::style::StyledContent;

/// A way to uniformly print to the console after a command has finished. This should
/// only be used at the end of a command, after the event context from the buckd client
/// is not available.
pub struct FinalConsole {
    is_tty: bool,
}

impl FinalConsole {
    pub fn new_with_tty() -> Self {
        Self { is_tty: true }
    }

    pub fn new_without_tty() -> Self {
        Self { is_tty: false }
    }

    fn stderr_colored_ln(&self, message: &str, color: Color) -> bz_error::Result<()> {
        self.stderr_colored(message, color)?;
        crate::eprintln!()?;
        Ok(())
    }

    fn stderr_colored(&self, message: &str, color: Color) -> bz_error::Result<()> {
        if self.is_tty {
            if let Some(code) = standard_ansi_foreground(color) {
                crate::eprint!("\x1b[{code}m{message}\x1b[0m")?;
            } else {
                let sc = StyledContent::new(
                    ContentStyle {
                        foreground_color: Some(color),
                        background_color: None,
                        underline_color: None,
                        attributes: Default::default(),
                    },
                    message,
                );
                crate::eprint!("{}", sc)?;
            }
        } else {
            crate::eprint!("{}", message)?;
        }
        Ok(())
    }

    fn stderr_colored_prefix_ln(
        &self,
        prefix: &str,
        color: Color,
        bold: bool,
        message: &str,
    ) -> bz_error::Result<()> {
        self.stderr_colored_with_style(prefix, color, bold)?;
        crate::eprintln!(" {}", message)?;
        Ok(())
    }

    fn stderr_colored_with_style(
        &self,
        message: &str,
        color: Color,
        bold: bool,
    ) -> bz_error::Result<()> {
        if self.is_tty {
            if let Some(code) = standard_ansi_foreground(color) {
                let bold = if bold { "1;" } else { "" };
                crate::eprint!("\x1b[{bold}{code}m{message}\x1b[0m")?;
            } else {
                let sc = StyledContent::new(
                    ContentStyle {
                        foreground_color: Some(color),
                        background_color: None,
                        underline_color: None,
                        attributes: if bold {
                            Attribute::Bold.into()
                        } else {
                            Default::default()
                        },
                    },
                    message,
                );
                crate::eprint!("{}", sc)?;
            }
        } else {
            crate::eprint!("{}", message)?;
        }
        Ok(())
    }

    /// Print the given message to stderr, in red if possible
    pub fn print_error(&self, message: &str) -> bz_error::Result<()> {
        self.stderr_colored_ln(message, Color::DarkRed)
    }

    /// Print an INFO-style line with only the prefix in green.
    pub fn print_info_prefix(&self, message: &str) -> bz_error::Result<()> {
        self.stderr_colored_prefix_ln("INFO:", Color::Green, false, message)
    }

    /// Print an ERROR-style line with only the prefix in red.
    pub fn print_error_prefix(&self, message: &str) -> bz_error::Result<()> {
        self.stderr_colored_prefix_ln("ERROR:", Color::DarkRed, true, message)
    }

    /// Print the given message to stderr, in yellow if possible
    pub fn print_warning(&self, message: &str) -> bz_error::Result<()> {
        self.stderr_colored_ln(message, Color::Yellow)
    }

    /// Print the given message to stderr, in green if possible
    pub fn print_success(&self, message: &str) -> bz_error::Result<()> {
        self.stderr_colored_ln(message, Color::Green)
    }

    /// Print the given message to stderr, in green if possible
    pub fn print_success_no_newline(&self, message: &str) -> bz_error::Result<()> {
        self.stderr_colored(message, Color::Green)
    }

    /// Print a string directly to stderr with no extra formatting
    pub fn print_stderr(&self, message: &str) -> bz_error::Result<()> {
        crate::eprintln!("{}", message)
    }

    pub fn is_tty(&self) -> bool {
        self.is_tty
    }

    /// Returns true if the terminal is likely to support OSC 8 hyperlinks.
    /// Detection is based on environment variables set by known terminals.
    pub fn supports_hyperlinks(&self) -> bool {
        self.is_tty && terminal_supports_hyperlinks()
    }
}

fn standard_ansi_foreground(color: Color) -> Option<&'static str> {
    match color {
        Color::Black => Some("30"),
        Color::DarkRed | Color::Red => Some("31"),
        Color::DarkGreen | Color::Green => Some("32"),
        Color::DarkYellow | Color::Yellow => Some("33"),
        Color::DarkBlue | Color::Blue => Some("34"),
        Color::DarkMagenta | Color::Magenta => Some("35"),
        Color::DarkCyan | Color::Cyan => Some("36"),
        Color::Grey | Color::White => Some("37"),
        Color::DarkGrey => Some("90"),
        _ => None,
    }
}

/// Returns true if the terminal is likely to support OSC 8 hyperlinks,
/// based on environment variables set by known terminals.
/// This does not check whether stderr is a TTY; callers should check that separately
/// if needed.
///
/// Known terminals and their detection:
///
/// ```text
/// Support  Terminal          Detection
/// -------  ----------------  ----------------------------------------
/// Yes      VSCode terminal   TERM_PROGRAM=vscode
/// Yes      iTerm2            TERM_PROGRAM=iTerm.app / ITERM_SESSION_ID
/// Yes      Kitty             KITTY_WINDOW_ID
/// Yes      WezTerm           TERM_PROGRAM=WezTerm / WEZTERM_EXECUTABLE
/// Yes      Ghostty           TERM_PROGRAM=ghostty
/// Yes      GNOME Terminal    VTE_VERSION
/// Yes      Windows Terminal  TERM_PROGRAM=Windows_Terminal / WT_SESSION
/// Yes      Alacritty (Win)   TERM=xterm-256color + OS=Windows_NT / ComSpec
/// No       xterm             (not detected)
/// No       rxvt              (not detected)
/// No       Terminal.app      TERM_PROGRAM=Apple_Terminal
/// ```
///
/// Detection priority: TERM_PROGRAM is checked first (now reliably
/// forwarded over SSH), then terminal-specific env vars as fallback.
fn terminal_supports_hyperlinks() -> bool {
    // macOS Terminal.app does not support OSC 8 hyperlinks.
    if std::env::var_os("TERM_PROGRAM")
        .as_deref()
        .is_some_and(|v| v == "Apple_Terminal")
    {
        return false;
    }
    // TERM_PROGRAM in (Windows_Terminal, vscode, WezTerm, iTerm.app, ghostty)
    // These terminals support OSC 8 hyperlinks.
    if std::env::var_os("TERM_PROGRAM")
        .as_deref()
        .is_some_and(|v| {
            v == "Windows_Terminal"
                || v == "vscode"
                || v == "WezTerm"
                || v == "iTerm.app"
                || v == "ghostty"
        })
    {
        return true;
    }

    // If TERM_PROGRAM is not set, fall back to terminal-specific env vars:
    // iTerm2, Kitty, WezTerm, GNOME Terminal (VTE), Windows Terminal
    if std::env::var_os("ITERM_SESSION_ID").is_some()
        || std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var_os("WEZTERM_EXECUTABLE").is_some()
        || std::env::var_os("VTE_VERSION").is_some()
        || std::env::var_os("WT_SESSION").is_some()
    {
        return true;
    }
    // For TERM=xterm-256color without TERM_PROGRAM, check Windows-specific
    // env vars. The TERM guard is required because ComSpec and OS=Windows_NT
    // are system-level variables set on all Windows installs, including
    // cmd.exe and older PowerShell which do NOT support OSC 8 hyperlinks.
    if std::env::var_os("TERM")
        .as_deref()
        .is_some_and(|v| v == "xterm-256color")
    {
        if std::env::var_os("OS").is_some_and(|v| v == "Windows_NT")
            || std::env::var_os("ComSpec").is_some()
        {
            return true;
        }
        if std::env::var_os("SKS_PLATFORM").is_some_and(|v| v == "WINDOWS") {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_console_uses_standard_ansi_colors() {
        assert_eq!(standard_ansi_foreground(Color::Green), Some("32"));
        assert_eq!(standard_ansi_foreground(Color::Yellow), Some("33"));
        assert_eq!(standard_ansi_foreground(Color::DarkRed), Some("31"));
    }
}
