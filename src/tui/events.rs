//! Keyboard input → [`Action`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    /// Open the share URL in the local browser.
    Open,
    /// Copy the share URL to the system clipboard via OSC 52.
    CopyUrl,
    /// Toggle uploads on/off at runtime.
    ToggleUploads,
    /// Cancel the selected active transfer.
    KillTransfer,
    /// Re-detect network addresses.
    Refresh,
    ToggleLogs,
    ClearCompleted,
    ToggleHelp,
    ToggleQr,
    NextUrl,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    /// Close any open overlay.
    Close,
    None,
}

pub fn map_key(key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') | KeyCode::Char('d') => Action::Quit,
            _ => Action::None,
        };
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
        KeyCode::Char('o') | KeyCode::Char('O') => Action::Open,
        KeyCode::Char('y') | KeyCode::Char('Y') => Action::CopyUrl,
        KeyCode::Char('u') | KeyCode::Char('U') => Action::ToggleUploads,
        KeyCode::Char('x') | KeyCode::Char('X') => Action::KillTransfer,
        KeyCode::Char('r') | KeyCode::Char('R') => Action::Refresh,
        KeyCode::Char('l') | KeyCode::Char('L') => Action::ToggleLogs,
        KeyCode::Char('c') | KeyCode::Char('C') => Action::ClearCompleted,
        KeyCode::Char('?') | KeyCode::Char('h') | KeyCode::F(1) => Action::ToggleHelp,
        KeyCode::Char('p') | KeyCode::Char('P') => Action::ToggleQr,
        KeyCode::Tab => Action::NextUrl,
        KeyCode::Up | KeyCode::Char('k') => Action::ScrollUp,
        KeyCode::Down | KeyCode::Char('j') => Action::ScrollDown,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Esc => Action::Close,
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(c, m)
    }

    #[test]
    fn quit_keys() {
        assert_eq!(
            map_key(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Action::Quit
        );
        assert_eq!(
            map_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
        // Plain 'c' clears finished transfers instead of quitting.
        assert_eq!(
            map_key(key(KeyCode::Char('c'), KeyModifiers::NONE)),
            Action::ClearCompleted
        );
    }

    #[test]
    fn documented_keys() {
        let k = |c| map_key(key(KeyCode::Char(c), KeyModifiers::NONE));
        assert_eq!(k('o'), Action::Open);
        assert_eq!(k('y'), Action::CopyUrl);
        assert_eq!(k('u'), Action::ToggleUploads);
        assert_eq!(k('x'), Action::KillTransfer);
        assert_eq!(k('r'), Action::Refresh);
        assert_eq!(k('l'), Action::ToggleLogs);
        assert_eq!(k('?'), Action::ToggleHelp);
        assert_eq!(k('p'), Action::ToggleQr);
        assert_eq!(k('z'), Action::None);
    }
}
