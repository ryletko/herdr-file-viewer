//! Run-a-command (`!`) — the Session Controller's half of the command hand-off.
//!
//! Opens the prompt, edits its buffer, and on confirm asks the host to open a NEW herdr tab rooted
//! at the selected directory and run the typed command there. The argv, the target directory, and
//! the reply parsing all live in [`crate::runner`] (pure, unit-tested); this file is the state
//! machine around them.
//!
//! The viewer never executes the command itself — herdr's tab shell does. That keeps the gesture
//! the same class of external hand-off as `e` (`$EDITOR`) and `O` (the OS opener).

use super::*;

impl Controller {
    /// Open the run-a-command prompt (`!`) on an EMPTY line, with the history walk reset.
    ///
    /// Deliberately empty: an opening buffer pre-filled with the last command reads exactly like
    /// text the user just typed, so typing a *different* command appends to it
    /// (`git log --oneline -3` + `code .` → `git log --oneline -3code .`). Repeating is still cheap
    /// — `↑` recalls it — but the default case, a new command, starts clean.
    ///
    /// Two gates, each with its own notice rather than a silent no-op: the gesture needs a live
    /// herdr (it is the thing that opens the tab), and it needs somewhere to run — the selected
    /// directory, or a selected file's parent.
    pub(super) fn open_run_command(&mut self) -> Effects {
        if self.modal.picker().is_some() || self.modal.finder().is_some() {
            return Effects::noop();
        }
        if self.herdr.is_none() {
            self.action_notice = Some("Run: needs herdr to open a tab".into());
            return Effects::redraw();
        }
        if self.command_target().is_none() {
            self.action_notice = Some("Run: select a file or directory first".into());
            return Effects::redraw();
        }
        self.history_pos = None;
        self.modal = Modal::Prompt(PromptState {
            mode: PromptMode::RunCommand,
            input: crate::prompt::PromptInput::new(),
            saved_scroll: self.content_scroll,
        });
        Effects::redraw()
    }

    /// Walk the session's command history into the prompt buffer: `↑` toward older commands, `↓`
    /// back toward the newest and then to the empty line the prompt opened on.
    ///
    /// The recalled text replaces the buffer whole, cursor at the end — ready to run, or to edit.
    /// Editing does not move the walk position, so a following `↑` continues from where it was,
    /// the way a shell behaves.
    fn walk_history(&mut self, back: bool) -> Effects {
        let next = crate::runner::history_step(self.command_history.len(), self.history_pos, back);
        if next == self.history_pos {
            return Effects::noop(); // already at the oldest / already on the empty line
        }
        self.history_pos = next;
        let text = next
            .and_then(|i| self.command_history.get(i))
            .cloned()
            .unwrap_or_default();
        if let Some(p) = self.modal.prompt_mut() {
            p.input = crate::prompt::PromptInput::with_text(text);
        }
        Effects::redraw()
    }

    /// Key handling while the run prompt is open: any printable character types into the buffer
    /// (a shell command is arbitrary text — unlike go-to-line, nothing is filtered), `Backspace`
    /// deletes, `Enter` runs, `Esc` cancels.
    pub(super) fn run_command_key(&mut self, key: KeyEvent) -> Effects {
        match key.code {
            KeyCode::Char(c) if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                if let Some(p) = self.modal.prompt_mut() {
                    p.input.push(c);
                }
                Effects::redraw()
            }
            KeyCode::Backspace => {
                if let Some(p) = self.modal.prompt_mut() {
                    p.input.backspace();
                }
                Effects::redraw()
            }
            // Cursor movement inside the buffer: a command line is long enough to want editing in
            // the middle (fixing a flag), not just backspacing from the end.
            KeyCode::Left => {
                if let Some(p) = self.modal.prompt_mut() {
                    p.input.move_left();
                }
                Effects::redraw()
            }
            KeyCode::Right => {
                if let Some(p) = self.modal.prompt_mut() {
                    p.input.move_right();
                }
                Effects::redraw()
            }
            KeyCode::Up => self.walk_history(true),
            KeyCode::Down => self.walk_history(false),
            KeyCode::Home => {
                if let Some(p) = self.modal.prompt_mut() {
                    p.input.move_home();
                }
                Effects::redraw()
            }
            KeyCode::End => {
                if let Some(p) = self.modal.prompt_mut() {
                    p.input.move_end();
                }
                Effects::redraw()
            }
            KeyCode::Enter => {
                let command = self
                    .modal
                    .prompt()
                    .map(|p| p.input.query().trim().to_string())
                    .unwrap_or_default();
                self.modal = Modal::None; // confirm always closes, empty included
                if command.is_empty() {
                    return Effects::redraw();
                }
                self.run_command(&command)
            }
            KeyCode::Esc => {
                self.modal = Modal::None;
                Effects::redraw()
            }
            _ => Effects::noop(),
        }
    }

    /// The directory a command would run in: the selected directory, or the parent of the selected
    /// file. `None` when nothing is selected, or the path has no usable directory.
    pub(super) fn command_target(&self) -> Option<PathBuf> {
        let node = self.tree.selected()?;
        crate::runner::target_dir(&node.path, node.kind)
    }

    /// Hand `command` to a new herdr tab rooted at the command target.
    ///
    /// Two host calls: `tab create` (whose JSON names the new tab's root pane) then `pane run`. A
    /// failure at either step degrades to a notice and leaves the viewer untouched (AC-15) — the
    /// worst case is an empty shell tab the user can close, never a half-applied viewer state.
    /// The command enters the history whether or not the host call succeeded, so a retry after e.g.
    /// a herdr hiccup is `!` `↑` `Enter` rather than retyping it.
    fn run_command(&mut self, command: &str) -> Effects {
        crate::runner::push_history(&mut self.command_history, command);
        let Some(dir) = self.command_target() else {
            self.action_notice = Some("Run: select a file or directory first".into());
            return Effects::redraw();
        };
        // herdr's CLI takes &str args, so a non-UTF-8 path cannot be expressed. Degrade with a
        // notice rather than lossily mangling the directory and running somewhere unintended.
        let Some(cwd) = dir.to_str() else {
            self.action_notice = Some("Run: directory path is not valid UTF-8".into());
            return Effects::redraw();
        };
        let Some(herdr) = self.herdr.as_ref() else {
            self.action_notice = Some("Run: needs herdr to open a tab".into());
            return Effects::redraw();
        };
        let label = crate::runner::tab_label(command);
        let Ok(json) = herdr.run_json(&crate::runner::tab_create_args(cwd, &label)) else {
            self.action_notice = Some("Run: herdr could not open a tab".into());
            return Effects::redraw();
        };
        let Some(pane) = crate::runner::parse_root_pane_id(&json) else {
            self.action_notice = Some("Run: herdr reported no pane for the new tab".into());
            return Effects::redraw();
        };
        let arg = crate::runner::shell_arg(command);
        if herdr
            .run(&crate::runner::pane_run_args(&pane, &arg))
            .is_err()
        {
            self.action_notice = Some("Run: herdr could not start the command".into());
            return Effects::redraw();
        }
        self.action_notice = Some(format!("Running `{label}`"));
        Effects::redraw()
    }
}
