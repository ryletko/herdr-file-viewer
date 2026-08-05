//! End-to-end coverage for the `!` run-a-command hand-off: the real
//! key → intent → controller → herdr-CLI chain (`map_key` decode included), against a recording
//! [`HerdrCli`] fake so no tab is ever really created.
//!
//! What is pinned here:
//!
//! * the two host calls and their exact argv (`tab create …` then `pane run …`), the shapes
//!   verified against a live herdr 0.7.x;
//! * the command reaching the shell as ONE argv element, unsplit, so shell syntax survives;
//! * the target directory rule (a directory runs in itself, a file in its parent);
//! * every degradation path — no herdr, a host error, a reply with no pane — showing a notice and
//!   leaving the viewer otherwise untouched;
//! * that a run mutates nothing on disk or in git (the viewer hands off; it does not execute).

mod common;

use common::{TempDir, git, init_repo_with_commit};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use herdr_file_viewer::controller::{
    Components, ContentProvider, Controller, EditorHandoff, EditorOutcome, GitService,
    RenderResult, RootProviders,
};
use herdr_file_viewer::git::{Baseline, Status};
use herdr_file_viewer::herdr::HerdrCli;
use herdr_file_viewer::input::map_key;
use herdr_file_viewer::intent::Intent;
use herdr_file_viewer::view_policy::ViewMode;
use ratatui::text::Text;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Type `text` into an open prompt, one character at a time, through the real key path.
fn type_text(ctrl: &mut Controller, text: &str) {
    for c in text.chars() {
        ctrl.handle_prompt_key(key(KeyCode::Char(c)));
    }
}

// ── inert stubs (integration files are separate crates, so these are local) ───────────────────

#[derive(Default, Clone)]
struct StubGit;
impl GitService for StubGit {
    fn status(&self) -> BTreeMap<PathBuf, Status> {
        BTreeMap::new()
    }
    fn changed_set(&self, _baseline: Baseline) -> BTreeMap<PathBuf, Status> {
        BTreeMap::new()
    }
    fn diff(&self, _p: &Path, _b: Baseline, _full: bool) -> String {
        String::new()
    }
    fn diff_directory(&self, _rel_dir: &Path, _baseline: Baseline) -> String {
        String::new()
    }
}

struct NoopEditor;
impl EditorHandoff for NoopEditor {
    fn open(&mut self, _file: &Path) -> EditorOutcome {
        EditorOutcome::NoTakeover
    }
}

#[derive(Clone, Copy)]
struct StubContent;
impl ContentProvider for StubContent {
    fn render(&self, _path: &Path, _mode: ViewMode, _raw_diff: Option<&str>) -> RenderResult {
        RenderResult {
            content: Text::raw("stub"),
            notices: Vec::new(),
            source: None,
        }
    }
}

// ── the herdr fake ───────────────────────────────────────────────────────────────────────────

/// How the fake host answers `tab create`.
#[derive(Clone, Copy)]
enum Reply {
    /// A well-formed reply naming pane `w9:pQ`.
    Pane,
    /// A well-formed reply with no pane in it (herdr changed shape / partial failure).
    NoPane,
    /// The CLI call itself failed (herdr not running, non-zero exit).
    Error,
}

/// Records every argv the controller sends to herdr, and answers `tab create` per [`Reply`].
struct FakeHerdr {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    reply: Reply,
}

impl HerdrCli for FakeHerdr {
    fn run_json(&self, args: &[&str]) -> io::Result<String> {
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|s| s.to_string()).collect());
        match self.reply {
            Reply::Pane => Ok(
                r#"{"result":{"root_pane":{"pane_id":"w9:pQ"},"tab":{"tab_id":"w9:t3"}}}"#
                    .to_string(),
            ),
            Reply::NoPane => Ok(r#"{"result":{"tab":{"tab_id":"w9:t3"}}}"#.to_string()),
            Reply::Error => Err(io::Error::other("herdr not running")),
        }
    }
}

/// A controller over `root`, wired to a recording herdr fake. Returns the call log too.
fn controller_with_host(root: &Path, reply: Reply) -> (Controller, Arc<Mutex<Vec<Vec<String>>>>) {
    let components = Components {
        providers: Box::new(move |_resolved| RootProviders {
            git: Arc::new(StubGit),
            content: Box::new(StubContent),
        }),
        editor: Box::new(NoopEditor),
        clipboard: Box::new(common::RecordingClipboard::default()),
        renderers: None,
    };
    let mut ctrl = Controller::new(
        common::resolved(root.to_path_buf(), true),
        Baseline::Head,
        components,
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    ctrl.set_host(
        Box::new(FakeHerdr {
            calls: Arc::clone(&calls),
            reply,
        }),
        Some("w9".to_string()),
    );
    (ctrl, calls)
}

/// A repo with `sub/` (holding one file) and a top-level file, committed so git is clean.
fn repo_with_subdir() -> TempDir {
    let dir = TempDir::new();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub").join("inner.txt"), "inner\n").unwrap();
    std::fs::write(dir.path().join("top.txt"), "top\n").unwrap();
    init_repo_with_commit(dir.path());
    dir
}

/// Move the tree cursor onto the visible node whose file name is `name`.
fn select_by_name(ctrl: &mut Controller, name: &str) {
    for _ in 0..ctrl.tree().visible_nodes().len() {
        ctrl.handle(Intent::NavUp);
    }
    for _ in 0..ctrl.tree().visible_nodes().len() {
        let sel = ctrl.tree().selected().expect("a node is selected");
        if sel.path.file_name().and_then(|n| n.to_str()) == Some(name) {
            return;
        }
        ctrl.handle(Intent::NavDown);
    }
    panic!("no visible node named {name}");
}

#[test]
fn bang_runs_the_command_in_a_new_tab_rooted_at_the_selected_directory() {
    // The whole gesture through the real key path: `!` opens the prompt, the typed command is
    // confirmed with Enter, and the host gets exactly two calls — create the tab at the selected
    // directory, then run the command in that tab's root pane.
    let dir = repo_with_subdir();
    let (mut ctrl, calls) = controller_with_host(dir.path(), Reply::Pane);
    select_by_name(&mut ctrl, "sub");

    assert_eq!(map_key(key(KeyCode::Char('!'))), Some(Intent::RunCommand));
    ctrl.handle(Intent::RunCommand);
    assert!(ctrl.prompt_open(), "`!` opens the prompt");
    type_text(&mut ctrl, "npm test");
    ctrl.handle_prompt_key(key(KeyCode::Enter));
    assert!(!ctrl.prompt_open(), "confirming closes the prompt");

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "exactly two host calls: {calls:?}");
    let sub = dir.path().join("sub");
    assert_eq!(
        calls[0],
        vec![
            "tab",
            "create",
            "--cwd",
            sub.to_str().unwrap(),
            "--label",
            "npm test",
            "--focus",
        ],
        "the tab is created at the SELECTED directory and named after the command"
    );
    assert_eq!(
        calls[1],
        vec!["pane", "run", "w9:pQ", "npm test"],
        "the command runs in the pane herdr reported for the new tab"
    );
}

#[test]
fn a_selected_file_runs_in_the_directory_holding_it() {
    // "Open a terminal here": with a file selected there is still an obvious place to run — the
    // directory it lives in — so the gesture works everywhere in the tree, not only on folders.
    let dir = repo_with_subdir();
    let (mut ctrl, calls) = controller_with_host(dir.path(), Reply::Pane);
    ctrl.handle(Intent::Expand); // reveal sub/'s children so the file is selectable
    select_by_name(&mut ctrl, "inner.txt");

    ctrl.handle(Intent::RunCommand);
    type_text(&mut ctrl, "code .");
    ctrl.handle_prompt_key(key(KeyCode::Enter));

    let calls = calls.lock().unwrap();
    let sub = dir.path().join("sub");
    assert_eq!(
        calls[0][3],
        sub.to_str().unwrap(),
        "a file's command runs in its parent directory"
    );
}

#[test]
fn the_command_reaches_the_shell_as_one_unsplit_argument() {
    // herdr types the command into the tab's shell, so shell syntax is the user's own. Splitting it
    // here would break exactly the commands people reach for — pin that it is passed whole.
    let dir = repo_with_subdir();
    let (mut ctrl, calls) = controller_with_host(dir.path(), Reply::Pane);
    select_by_name(&mut ctrl, "sub");

    ctrl.handle(Intent::RunCommand);
    type_text(&mut ctrl, "npm test && echo done");
    ctrl.handle_prompt_key(key(KeyCode::Enter));

    let calls = calls.lock().unwrap();
    assert_eq!(
        calls[1],
        vec!["pane", "run", "w9:pQ", "npm test && echo done"],
        "pipes/&&/quotes travel as one element for the shell to parse"
    );
}

#[test]
fn a_command_starting_with_a_dash_is_hidden_from_herdrs_own_cli() {
    // Verified against a live herdr: `herdr pane run <id> --version` prints herdr's version — the
    // command never reaches the shell — and herdr forwards a `--` separator into the shell instead
    // of consuming it. One leading space is the guard; shells ignore it.
    let dir = repo_with_subdir();
    let (mut ctrl, calls) = controller_with_host(dir.path(), Reply::Pane);
    select_by_name(&mut ctrl, "sub");

    ctrl.handle(Intent::RunCommand);
    type_text(&mut ctrl, "--version");
    ctrl.handle_prompt_key(key(KeyCode::Enter));

    let calls = calls.lock().unwrap();
    assert_eq!(
        calls[1][3], " --version",
        "a leading dash is prefixed with a space so herdr cannot read it as a flag"
    );
}

#[test]
fn the_prompt_opens_empty_so_typing_never_appends_to_a_leftover_command() {
    // Regression: the prompt used to open pre-filled with the last command, which reads exactly
    // like text the user just typed — so typing a DIFFERENT command appended to it
    // (`git log --oneline -3` + `code .` → `git log --oneline -3code .`). It opens empty now; `↑`
    // is how you get the old one back.
    let dir = repo_with_subdir();
    let (mut ctrl, calls) = controller_with_host(dir.path(), Reply::Pane);
    select_by_name(&mut ctrl, "sub");

    ctrl.handle(Intent::RunCommand);
    type_text(&mut ctrl, "git log --oneline -3");
    ctrl.handle_prompt_key(key(KeyCode::Enter));

    ctrl.handle(Intent::RunCommand);
    assert_eq!(ctrl.prompt_query(), "", "the prompt reopens empty");
    type_text(&mut ctrl, "code .");
    assert_eq!(
        ctrl.prompt_query(),
        "code .",
        "what you type is the whole command"
    );
    ctrl.handle_prompt_key(key(KeyCode::Enter));

    let calls = calls.lock().unwrap();
    assert_eq!(calls[3][3], "code .", "and that is what runs");
}

#[test]
fn up_and_down_walk_the_session_history() {
    // `↑` recalls previous commands newest-first and stops at the oldest; `↓` walks back and off
    // the end to the empty line the prompt opened on — the shell behaviour people already know.
    let dir = repo_with_subdir();
    let (mut ctrl, _) = controller_with_host(dir.path(), Reply::Pane);
    select_by_name(&mut ctrl, "sub");

    for cmd in ["npm test", "code ."] {
        ctrl.handle(Intent::RunCommand);
        type_text(&mut ctrl, cmd);
        ctrl.handle_prompt_key(key(KeyCode::Enter));
    }

    ctrl.handle(Intent::RunCommand);
    assert_eq!(ctrl.prompt_query(), "");
    ctrl.handle_prompt_key(key(KeyCode::Up));
    assert_eq!(ctrl.prompt_query(), "code .", "↑ recalls the newest");
    ctrl.handle_prompt_key(key(KeyCode::Up));
    assert_eq!(ctrl.prompt_query(), "npm test", "↑ again goes older");
    ctrl.handle_prompt_key(key(KeyCode::Up));
    assert_eq!(ctrl.prompt_query(), "npm test", "and stops at the oldest");
    ctrl.handle_prompt_key(key(KeyCode::Down));
    assert_eq!(ctrl.prompt_query(), "code .", "↓ walks back toward newest");
    ctrl.handle_prompt_key(key(KeyCode::Down));
    assert_eq!(
        ctrl.prompt_query(),
        "",
        "↓ past the newest returns to the empty line"
    );
}

#[test]
fn a_recalled_command_can_be_edited_and_run() {
    // The point of the recall: `!` `↑` and either Enter, or tweak it first. Editing must not
    // disturb the buffer's contents beyond the edit itself.
    let dir = repo_with_subdir();
    let (mut ctrl, calls) = controller_with_host(dir.path(), Reply::Pane);
    select_by_name(&mut ctrl, "sub");

    ctrl.handle(Intent::RunCommand);
    type_text(&mut ctrl, "npm test");
    ctrl.handle_prompt_key(key(KeyCode::Enter));

    ctrl.handle(Intent::RunCommand);
    ctrl.handle_prompt_key(key(KeyCode::Up)); // "npm test"
    for _ in 0..4 {
        ctrl.handle_prompt_key(key(KeyCode::Backspace)); // → "npm "
    }
    type_text(&mut ctrl, "run build");
    ctrl.handle_prompt_key(key(KeyCode::Enter));

    let calls = calls.lock().unwrap();
    assert_eq!(calls[3][3], "npm run build");
}

#[test]
fn esc_leaves_no_trace() {
    // Cancel must be free: prompt closed, no host call, nothing added to the history.
    let dir = repo_with_subdir();
    let (mut ctrl, calls) = controller_with_host(dir.path(), Reply::Pane);
    select_by_name(&mut ctrl, "sub");

    ctrl.handle(Intent::RunCommand);
    type_text(&mut ctrl, "rm -rf /");
    ctrl.handle_prompt_key(key(KeyCode::Esc));

    assert!(!ctrl.prompt_open(), "Esc closes the prompt");
    assert!(
        calls.lock().unwrap().is_empty(),
        "a cancelled command never reaches the host"
    );

    ctrl.handle(Intent::RunCommand);
    ctrl.handle_prompt_key(key(KeyCode::Up));
    assert_eq!(
        ctrl.prompt_query(),
        "",
        "and it is not in the history either — ↑ finds nothing"
    );
}

#[test]
fn an_empty_command_runs_nothing() {
    // Enter on an empty buffer closes the prompt and does nothing — no empty tab, no stray call.
    let dir = repo_with_subdir();
    let (mut ctrl, calls) = controller_with_host(dir.path(), Reply::Pane);
    select_by_name(&mut ctrl, "sub");

    ctrl.handle(Intent::RunCommand);
    type_text(&mut ctrl, "   "); // whitespace only — trimmed to empty
    ctrl.handle_prompt_key(key(KeyCode::Enter));

    assert!(!ctrl.prompt_open());
    assert!(
        calls.lock().unwrap().is_empty(),
        "an empty command never reaches the host"
    );
}

#[test]
fn without_herdr_the_gesture_explains_itself_instead_of_opening_a_prompt() {
    // The tab comes from herdr; run outside it and there is nowhere to put the command. Say so
    // rather than opening a prompt that could only fail on Enter.
    let dir = repo_with_subdir();
    let components = Components {
        providers: Box::new(move |_resolved| RootProviders {
            git: Arc::new(StubGit),
            content: Box::new(StubContent),
        }),
        editor: Box::new(NoopEditor),
        clipboard: Box::new(common::RecordingClipboard::default()),
        renderers: None,
    };
    let mut ctrl = Controller::new(
        common::resolved(dir.path().to_path_buf(), true),
        Baseline::Head,
        components,
    ); // deliberately no set_host

    ctrl.handle(Intent::RunCommand);
    assert!(!ctrl.prompt_open(), "no prompt without a host");
    assert!(
        ctrl.action_notice().is_some_and(|n| n.contains("herdr")),
        "the notice names the missing piece, got {:?}",
        ctrl.action_notice()
    );
}

#[test]
fn a_failing_host_call_degrades_to_a_notice() {
    // herdr present but unhappy (not running, non-zero exit): notice, no crash, nothing else
    // changed. The second call is never attempted — there is no pane to run in.
    let dir = repo_with_subdir();
    let (mut ctrl, calls) = controller_with_host(dir.path(), Reply::Error);
    select_by_name(&mut ctrl, "sub");

    ctrl.handle(Intent::RunCommand);
    type_text(&mut ctrl, "npm test");
    ctrl.handle_prompt_key(key(KeyCode::Enter));

    assert_eq!(calls.lock().unwrap().len(), 1, "only the create was tried");
    assert!(
        ctrl.action_notice().is_some_and(|n| n.contains("Run:")),
        "the failure is reported, got {:?}",
        ctrl.action_notice()
    );
}

#[test]
fn a_reply_without_a_pane_degrades_to_a_notice() {
    // A well-formed-but-paneless reply must not send a second call with a guessed id.
    let dir = repo_with_subdir();
    let (mut ctrl, calls) = controller_with_host(dir.path(), Reply::NoPane);
    select_by_name(&mut ctrl, "sub");

    ctrl.handle(Intent::RunCommand);
    type_text(&mut ctrl, "npm test");
    ctrl.handle_prompt_key(key(KeyCode::Enter));

    assert_eq!(calls.lock().unwrap().len(), 1, "no run without a pane id");
    assert!(ctrl.action_notice().is_some_and(|n| n.contains("Run:")));
}

#[test]
fn running_a_command_mutates_nothing_on_disk_or_in_git() {
    // The viewer hands the command off; it never executes it. So a full exercise — including the
    // failure paths — must leave the working tree byte-for-byte identical and git clean.
    let dir = repo_with_subdir();
    let before: Vec<(PathBuf, Vec<u8>)> = walk(dir.path());

    for reply in [Reply::Pane, Reply::Error, Reply::NoPane] {
        let (mut ctrl, _) = controller_with_host(dir.path(), reply);
        select_by_name(&mut ctrl, "sub");
        ctrl.handle(Intent::RunCommand);
        type_text(&mut ctrl, "rm -rf .");
        ctrl.handle_prompt_key(key(KeyCode::Enter));
    }

    assert_eq!(before, walk(dir.path()), "no file changed");
    assert_eq!(
        git(dir.path(), &["status", "--porcelain"]).trim(),
        "",
        "git is still clean — even `rm -rf .` was only ever handed to a fake host"
    );
}

/// Every file under `root` (excluding `.git`), as (path, bytes), sorted — a byte-exact snapshot.
fn walk(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                out.push((path, bytes));
            }
        }
    }
    out.sort();
    out
}
