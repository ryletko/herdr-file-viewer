//! Command Runner — hand a shell command off to a NEW herdr tab rooted at the selected directory.
//!
//! The pure half of the `!` run-a-command gesture: it decides the target directory, builds the two
//! herdr argv the Host Adapter runs, and reads the new tab's pane id back out of herdr's JSON. The
//! viewer itself never executes the command — herdr's shell does, in a tab of its own — so this
//! module spawns nothing and is fully unit-testable without a live host.
//!
//! **This is the one place a user-supplied string reaches a shell.** Everything about how that
//! string is carried is deliberate, and verified against a live herdr (the argv are pinned in the
//! tests):
//!
//! - The command is passed as **one** argv element, never split. herdr types it into the tab's
//!   shell, so pipes, `&&`, quoting and globbing are the user's own shell semantics — tokenizing it
//!   here would break exactly the commands people reach for (`npm test && echo done`).
//! - A command starting with `-` would be eaten by herdr's own CLI as a flag and never reach the
//!   shell (verified: `herdr pane run <id> --version` printed herdr's version instead). herdr does
//!   NOT honour a `--` end-of-options separator here — it forwards it into the shell — so the guard
//!   is a single leading SPACE, which every shell ignores. See [`shell_arg`].
//! - The pane id herdr reports is validated before it reaches an argv, so a malformed host reply
//!   cannot option-inject into the second command ([`parse_root_pane_id`]).

use crate::launch::is_flag_safe;
use crate::tree::NodeKind;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// How long a tab label may get before it is ellipsized. Long enough for the commands people
/// actually run (`cargo test --all`), short enough that herdr's tab strip stays readable.
const LABEL_MAX: usize = 24;

/// How many commands the session remembers. A viewer session is short and the history is only ever
/// walked with `↑`, so this is a bound against unbounded growth, not a working limit.
const HISTORY_MAX: usize = 50;

#[derive(Deserialize)]
struct TabCreated {
    result: TabCreatedResult,
}
#[derive(Deserialize)]
struct TabCreatedResult {
    root_pane: Option<RootPane>,
}
#[derive(Deserialize)]
struct RootPane {
    pane_id: Option<String>,
}

/// The directory a command launched from `node` should run in: the directory itself, or a file's
/// parent — "open a terminal here", the file-manager convention. `None` when a file has no parent
/// (a filesystem root), which is the one case with nowhere sensible to run.
pub fn target_dir(path: &Path, kind: NodeKind) -> Option<PathBuf> {
    match kind {
        NodeKind::Dir => Some(path.to_path_buf()),
        NodeKind::File => path.parent().map(Path::to_path_buf),
    }
}

/// The label for the created tab: the command itself, trimmed and ellipsized to [`LABEL_MAX`]
/// characters (not bytes — a multibyte command must not be cut mid-character). Naming the tab after
/// the command is what makes it findable in herdr's tab strip: two runs in the same directory would
/// otherwise be indistinguishable.
pub fn tab_label(command: &str) -> String {
    let command = command.trim();
    if command.chars().count() <= LABEL_MAX {
        return command.to_string();
    }
    let head: String = command.chars().take(LABEL_MAX.saturating_sub(1)).collect();
    format!("{head}…")
}

/// argv for the FIRST host call: create a focused tab rooted at `cwd`, labelled `label`.
///
/// Pinned against a live herdr (0.7.x):
/// `herdr tab create --cwd <PATH> --label <TEXT> --focus` → `{"result":{"root_pane":{"pane_id":…}}}`.
pub fn tab_create_args<'a>(cwd: &'a str, label: &'a str) -> Vec<&'a str> {
    vec!["tab", "create", "--cwd", cwd, "--label", label, "--focus"]
}

/// argv for the SECOND host call: type `command` into the new tab's shell and run it.
///
/// Pinned against a live herdr (0.7.x): `herdr pane run <PANE_ID> <COMMAND>`. The command is a
/// single element on purpose — see the module docs.
pub fn pane_run_args<'a>(pane_id: &'a str, command: &'a str) -> Vec<&'a str> {
    vec!["pane", "run", pane_id, command]
}

/// The command as it must be handed to herdr's CLI: unchanged, unless it starts with `-`, in which
/// case one leading space is prepended so the CLI cannot mistake it for a flag of its own.
///
/// A leading space is inert in every shell (at most it keeps the line out of history under bash's
/// `HISTCONTROL=ignorespace`), so the user still gets exactly the command they typed. The
/// alternative — an end-of-options `--` — does not work here: herdr forwards it into the shell,
/// where it becomes a stray argument.
pub fn shell_arg(command: &str) -> String {
    if command.starts_with('-') {
        format!(" {command}")
    } else {
        command.to_string()
    }
}

/// Record `command` as the most recent entry of the session's history.
///
/// Consecutive duplicates are collapsed — running the same thing in three directories in a row
/// should leave one entry to walk back through, not three. Oldest entries fall off at
/// [`HISTORY_MAX`].
pub fn push_history(history: &mut Vec<String>, command: &str) {
    if history.last().map(String::as_str) == Some(command) {
        return;
    }
    history.push(command.to_string());
    if history.len() > HISTORY_MAX {
        history.remove(0);
    }
}

/// Where `↑`/`↓` moves in a `len`-entry history from position `pos`, shell-style.
///
/// `None` is "the empty line the prompt opened on", newer than every entry. `↑` (`back`) walks from
/// there to the newest entry and on toward the oldest, stopping at it; `↓` walks back toward the
/// newest and then off the end, returning to the empty line. Pure index arithmetic so the walk is
/// testable without a prompt.
pub fn history_step(len: usize, pos: Option<usize>, back: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match (back, pos) {
        (true, None) => Some(len - 1), // ↑ from the empty line → newest
        (true, Some(i)) => Some(i.saturating_sub(1)), // ↑ → older, stopping at the oldest
        (false, None) => None,         // ↓ on the empty line → stay
        (false, Some(i)) if i + 1 < len => Some(i + 1), // ↓ → newer
        (false, Some(_)) => None,      // ↓ past the newest → back to the empty line
    }
}

/// The new tab's root pane id, read from herdr's `tab create` JSON — or `None` when the reply is
/// unparseable, carries no pane, or names one that is not safe to place in an argv.
///
/// The flag-safety filter is the same guard the launcher applies to host-supplied ids: a
/// host reply is still untrusted input, and this id goes straight into the next argv.
pub fn parse_root_pane_id(json: &str) -> Option<String> {
    serde_json::from_str::<TabCreated>(json)
        .ok()?
        .result
        .root_pane?
        .pane_id
        .filter(|id| is_flag_safe(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_dir_is_the_directory_itself_or_a_files_parent() {
        assert_eq!(
            target_dir(Path::new("/r/src"), NodeKind::Dir),
            Some(PathBuf::from("/r/src")),
            "a directory runs in itself"
        );
        assert_eq!(
            target_dir(Path::new("/r/src/main.rs"), NodeKind::File),
            Some(PathBuf::from("/r/src")),
            "a file runs in the directory holding it"
        );
        assert_eq!(
            target_dir(Path::new("/"), NodeKind::File),
            None,
            "a parentless file has nowhere to run"
        );
    }

    #[test]
    fn tab_label_is_the_command_ellipsized_on_character_boundaries() {
        assert_eq!(tab_label("code ."), "code .");
        assert_eq!(tab_label("  npm test  "), "npm test", "trimmed");
        let long = "cargo test --all-features --workspace";
        let label = tab_label(long);
        assert_eq!(label.chars().count(), LABEL_MAX, "ellipsized to the cap");
        assert!(label.ends_with('…'));
        // Multibyte must not be cut mid-character (it would panic on a byte slice).
        let cyrillic = "эхо ".repeat(20);
        assert_eq!(tab_label(&cyrillic).chars().count(), LABEL_MAX);
    }

    #[test]
    fn host_argv_are_the_verified_shapes() {
        // Pinned against a live herdr 0.7.x — if either shape changes, this is the tripwire.
        assert_eq!(
            tab_create_args("/r/src", "npm test"),
            vec![
                "tab", "create", "--cwd", "/r/src", "--label", "npm test", "--focus"
            ]
        );
        assert_eq!(
            pane_run_args("w2:pA", "npm test"),
            vec!["pane", "run", "w2:pA", "npm test"]
        );
    }

    #[test]
    fn shell_arg_guards_only_a_leading_dash() {
        assert_eq!(
            shell_arg("code ."),
            "code .",
            "ordinary commands are untouched"
        );
        assert_eq!(
            shell_arg("--version"),
            " --version",
            "a leading dash is hidden from herdr's own CLI by one space"
        );
        assert_eq!(
            shell_arg("echo -n hi"),
            "echo -n hi",
            "a dash INSIDE the command is not the CLI's business"
        );
    }

    #[test]
    fn push_history_appends_and_collapses_repeats() {
        let mut h = Vec::new();
        push_history(&mut h, "npm test");
        push_history(&mut h, "npm test"); // same command again — one entry to walk back through
        push_history(&mut h, "code .");
        assert_eq!(h, vec!["npm test", "code ."]);
        // A repeat that is not consecutive is a real entry: the walk order stays chronological.
        push_history(&mut h, "npm test");
        assert_eq!(h, vec!["npm test", "code .", "npm test"]);
    }

    #[test]
    fn push_history_drops_the_oldest_past_the_cap() {
        let mut h = Vec::new();
        for i in 0..HISTORY_MAX + 5 {
            push_history(&mut h, &format!("cmd {i}"));
        }
        assert_eq!(h.len(), HISTORY_MAX);
        assert_eq!(h.first().unwrap(), "cmd 5", "the oldest fell off");
        assert_eq!(h.last().unwrap(), &format!("cmd {}", HISTORY_MAX + 4));
    }

    #[test]
    fn history_step_walks_shell_style_from_the_empty_line() {
        let len = 3; // [oldest, middle, newest] = indices 0,1,2
        // ↑ from the empty line lands on the NEWEST, then walks older and stops at the oldest.
        assert_eq!(history_step(len, None, true), Some(2));
        assert_eq!(history_step(len, Some(2), true), Some(1));
        assert_eq!(history_step(len, Some(1), true), Some(0));
        assert_eq!(history_step(len, Some(0), true), Some(0), "stops at oldest");
        // ↓ walks back toward the newest, then off the end to the empty line.
        assert_eq!(history_step(len, Some(0), false), Some(1));
        assert_eq!(
            history_step(len, Some(2), false),
            None,
            "past newest → empty"
        );
        assert_eq!(history_step(len, None, false), None, "already empty");
    }

    #[test]
    fn history_step_is_inert_on_an_empty_history() {
        assert_eq!(history_step(0, None, true), None);
        assert_eq!(history_step(0, None, false), None);
    }

    #[test]
    fn parse_root_pane_id_reads_the_new_tabs_pane() {
        // The real reply shape, trimmed to what we read.
        let json = r#"{"id":"cli:tab:create","result":{"root_pane":{"pane_id":"w2:pD","cwd":"/r"},
                       "tab":{"tab_id":"w2:t7"}},"type":"tab_created"}"#;
        assert_eq!(parse_root_pane_id(json), Some("w2:pD".to_string()));
    }

    #[test]
    fn parse_root_pane_id_rejects_junk_and_unsafe_ids() {
        assert_eq!(parse_root_pane_id("not json"), None);
        assert_eq!(parse_root_pane_id(r#"{"result":{}}"#), None, "no pane");
        assert_eq!(
            parse_root_pane_id(r#"{"result":{"root_pane":{"pane_id":"--workspace"}}}"#),
            None,
            "an id that could option-inject is refused, not passed along"
        );
        assert_eq!(
            parse_root_pane_id(r#"{"result":{"root_pane":{"pane_id":""}}}"#),
            None,
            "an empty id is not a pane"
        );
    }
}
