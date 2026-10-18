use std::io;

use anyhow::Result;
use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

#[derive(Debug, Clone, Copy, Default)]
struct SessionState {
    active: bool,
}

impl SessionState {
    fn enter(&mut self) {
        self.active = true;
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn leave(&mut self) -> bool {
        std::mem::take(&mut self.active)
    }
}

pub struct TerminalSession {
    state: SessionState,
}

impl TerminalSession {
    pub fn enter() -> Result<Self> {
        let mut session = Self {
            state: SessionState::default(),
        };
        session.resume()?;
        Ok(session)
    }

    pub fn suspend(&mut self) -> Result<()> {
        if self.state.is_active() {
            restore_terminal()?;
            self.state.leave();
        }
        Ok(())
    }

    pub fn suspend_for_resume(&mut self) -> Result<()> {
        if !self.state.is_active() {
            return Ok(());
        }
        restore_terminal()?;
        self.state.leave();

        #[cfg(unix)]
        {
            let result = unsafe { libc::kill(0, libc::SIGTSTP) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                let _ = self.resume();
                return Err(error.into());
            }
            self.resume()?;
        }

        #[cfg(not(unix))]
        {
            self.resume()?;
            anyhow::bail!("terminal suspension is unavailable on this platform");
        }
        Ok(())
    }

    pub fn resume(&mut self) -> Result<()> {
        if self.state.is_active() {
            return Ok(());
        }

        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        self.state.enter();
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.state.leave() {
            let _ = restore_terminal();
        }
    }
}

fn restore_terminal() -> Result<()> {
    execute!(
        io::stdout(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    )?;
    disable_raw_mode()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SessionState;

    #[test]
    fn session_state_transitions_are_idempotent() {
        let mut state = SessionState::default();
        assert!(!state.leave());

        state.enter();
        state.enter();
        assert!(state.leave());
        assert!(!state.leave());
    }
}
