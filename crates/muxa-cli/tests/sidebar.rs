//! Opt-in real rmux coverage; isolated socket, shells only, no agent calls.
#![cfg(unix)]
use std::{
    collections::BTreeMap,
    process::Command,
    time::{Duration, Instant},
};

struct Server {
    socket: String,
    _directory: tempfile::TempDir,
}
impl Server {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        Self {
            socket: directory
                .path()
                .join("sidebar.sock")
                .to_string_lossy()
                .into_owned(),
            _directory: directory,
        }
    }
    fn mux(&self, args: &[&str]) -> String {
        let out = Command::new("rmux")
            .args(["-S", &self.socket])
            .args(args)
            .env("MUXA_HOST", "rmux")
            .env("MUXA_HOSTS", "rmux")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    fn cli(&self, action: &str, window: &str, extra: &[&str]) {
        let out = Command::new(env!("CARGO_BIN_EXE_muxa"))
            .args([
                "sidebar",
                action,
                "--host",
                "rmux",
                "--mux-socket",
                &self.socket,
                "--window",
                window,
            ])
            .args(extra)
            .env("MUXA_HOST", "rmux")
            .env("MUXA_HOSTS", "rmux")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{action}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn panes(&self, window: &str) -> Vec<Vec<String>> {
        self.mux(&["list-panes","-t",window,"-F","#{pane_id}\t#{pane_pid}\t#{pane_left}\t#{pane_width}\t#{pane_height}\t#{@muxa_sidebar}"])
            .lines().map(|line|line.split('\t').map(str::to_string).collect()).collect()
    }
    fn layout(&self, window: &str) -> String {
        self.mux(&["display-message", "-p", "-t", window, "#{window_layout}"])
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("rmux")
            .args(["-S", &self.socket, "kill-server"])
            .output();
    }
}
fn eventually(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !check() {
        assert!(
            Instant::now() < deadline,
            "rmux sidebar hook did not converge"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "requires rmux; cargo test -p muxa-cli --test sidebar -- --ignored"]
fn live_sidebar_keeps_agents_in_place_across_docking_focus_and_native_zoom() {
    let server = Server::new();
    let first = server.mux(&[
        "new-session",
        "-d",
        "-x",
        "150",
        "-y",
        "45",
        "-s",
        "sidebar-regression",
        "-P",
        "-F",
        "#{pane_id}",
        "sh",
    ]);
    let window = server.mux(&["display-message", "-p", "-t", &first, "#{window_id}"]);
    let second = server.mux(&[
        "split-window",
        "-d",
        "-v",
        "-t",
        &first,
        "-P",
        "-F",
        "#{pane_id}",
        "sh",
    ]);
    server.mux(&["split-window", "-d", "-h", "-t", &second, "sh"]);
    let original: BTreeMap<_, _> = server
        .panes(&window)
        .iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect();
    server.cli("show", &window, &[]);
    let rows = server.panes(&window);
    let sidebar = rows
        .iter()
        .find(|row| row.get(5).is_some_and(|mark| mark == "1"))
        .unwrap();
    assert_eq!(
        (&sidebar[2], &sidebar[3], &sidebar[4]),
        (&"0".into(), &"36".into(), &"45".into())
    );
    let sidebar_id = sidebar[0].clone();
    check_navigation(&server, &window, &second, &sidebar_id);
    server.mux(&["set-option", "-w", "-t", &window, "@muxa_sidebar_pane", ""]);
    server.cli("show", &window, &[]);
    assert_eq!(server.panes(&window).len(), 4);

    let layout = check_focus(&server, &window, &original, &sidebar_id);
    check_native_zoom(&server, &window, &first, &layout);
    server.cli("show", &window, &["--side", "right", "--width", "30"]);
    let rows = server.panes(&window);
    let sidebar = rows
        .iter()
        .find(|row| row.get(5).is_some_and(|mark| mark == "1"))
        .unwrap();
    assert_eq!(sidebar[3], "30");
    assert_ne!(sidebar[2], "0");
    let new = server.mux(&[
        "new-window",
        "-d",
        "-t",
        "sidebar-regression:",
        "-P",
        "-F",
        "#{window_id}",
        "sh",
    ]);
    eventually(|| {
        server
            .panes(&new)
            .iter()
            .any(|row| row.get(5).is_some_and(|mark| mark == "1"))
    });
    server.cli("hide", &window, &[]);
    server.cli("toggle", &window, &[]);
    let restored = server.panes(&window);
    assert!(restored
        .iter()
        .any(|row| row[5] == "1" && row[3] == "30" && row[2] != "0"));
    server.cli("hide", &window, &[]);
    let after: BTreeMap<_, _> = server
        .panes(&window)
        .iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect();
    assert_eq!(after, original);
    assert_eq!(server.panes(&new).len(), 1);
    assert!(server
        .mux(&["list-keys", "-T", "prefix"])
        .contains("keep-sidebar-test"));
}

fn check_focus(
    server: &Server,
    window: &str,
    original: &BTreeMap<String, String>,
    sidebar_id: &str,
) -> String {
    let layout = server.layout(window);
    server.cli("show", window, &[]);
    assert_eq!(server.panes(window).len(), 4);
    server.cli("zoom", window, &[]);
    let focused = server.panes(window);
    assert_eq!(focused.len(), 4);
    assert_eq!(
        focused.iter().find(|row| row[0] == *sidebar_id).unwrap()[4],
        "45"
    );
    for row in &focused {
        if let Some(pid) = original.get(&row[0]) {
            assert_eq!(pid, &row[1]);
        }
    }
    server.cli("zoom", window, &[]);
    assert_eq!(server.layout(window), layout);
    layout
}
fn check_native_zoom(server: &Server, window: &str, first: &str, layout: &str) {
    // A direct native zoom is also converted, even with a custom z binding.
    server.mux(&[
        "bind-key",
        "-T",
        "prefix",
        "z",
        "display-message",
        "keep-sidebar-test",
    ]);
    server.cli("show", window, &[]);
    assert!(server
        .mux(&["list-keys", "-T", "prefix"])
        .contains("keep-sidebar-test"));
    server.mux(&["resize-pane", "-Z", "-t", first]);
    eventually(|| {
        server.mux(&[
            "display-message",
            "-p",
            "-t",
            first,
            "#{window_zoomed_flag}",
        ]) == "0"
            && !server
                .mux(&[
                    "show-options",
                    "-w",
                    "-qv",
                    "-t",
                    window,
                    "@muxa_sidebar_focus",
                ])
                .is_empty()
    });
    server.cli("zoom", window, &[]);
    assert_eq!(server.layout(window), layout);
}

fn check_navigation(server: &Server, window: &str, pane: &str, sidebar: &str) {
    assert!(server
        .mux(&["list-keys", "-T", "prefix"])
        .contains("sidebar toggle"));
    server.mux(&["select-pane", "-t", pane]);
    let last = || {
        server.mux(&[
            "show-options",
            "-w",
            "-qv",
            "-t",
            window,
            "@muxa_sidebar_last_pane",
        ])
    };
    eventually(|| last() == pane);
    server.mux(&["select-pane", "-t", sidebar]);
    assert_eq!(
        last(),
        pane,
        "focusing the explorer must retain the work pane marker"
    );
    server.mux(&[
        "bind-key",
        "-T",
        "prefix",
        "e",
        "display-message",
        "keep-sidebar-toggle",
    ]);
    server.cli("show", window, &[]);
    assert!(server
        .mux(&["list-keys", "-T", "prefix"])
        .contains("keep-sidebar-toggle"));
    server.mux(&["select-pane", "-t", pane]);
}
