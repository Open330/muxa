//! Zellij plugin entrypoint.
//!
//! Host builds intentionally compile to a no-op library so
//! `cargo test --workspace` keeps working on machines without the WASM target.
//! The real plugin is compiled for `wasm32-wasip1`.

#[cfg(not(target_arch = "wasm32"))]
pub fn host_build_stub() {}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::collections::BTreeMap;

    use serde::Serialize;
    use zellij_tile::prelude::*;

    #[derive(Debug, Serialize)]
    struct PaneSnapshot {
        pane_id: String,
        session: String,
        window_index: String,
        pane_index: String,
        tty: String,
        current_command: String,
        title: String,
        pane_pid: u32,
    }

    register_plugin!(State);

    #[derive(Default)]
    struct State {
        permission_ready: bool,
    }

    impl ZellijPlugin for State {
        fn load(&mut self, _configuration: BTreeMap<String, String>) {
            request_permission(&[
                PermissionType::ReadApplicationState,
                PermissionType::RunCommands,
            ]);
            subscribe(&[EventType::PermissionRequestResult, EventType::PaneUpdate]);
        }

        fn update(&mut self, event: Event) -> bool {
            match event {
                Event::PermissionRequestResult(PermissionStatus::Granted) => {
                    self.permission_ready = true;
                    true
                }
                Event::PaneUpdate(panes) if self.permission_ready => {
                    let snapshots = panes
                        .panes
                        .into_iter()
                        .flat_map(|(tab, panes)| {
                            panes.into_iter().filter_map(move |pane| {
                                if pane.is_plugin {
                                    return None;
                                }
                                let pane_id = format!("zellij:terminal:{}", pane.id);
                                let pid = get_pane_pid(PaneId::Terminal(pane.id))
                                    .ok()
                                    .and_then(|pid| u32::try_from(pid).ok())
                                    .unwrap_or(0);
                                let current_command =
                                    get_pane_running_command(PaneId::Terminal(pane.id))
                                        .ok()
                                        .map(|argv| argv.join(" "))
                                        .or(pane.terminal_command)
                                        .unwrap_or_default();
                                Some(PaneSnapshot {
                                    pane_id,
                                    session: format!("zellij-tab-{tab}"),
                                    window_index: tab.to_string(),
                                    pane_index: pane.id.to_string(),
                                    tty: String::new(),
                                    current_command,
                                    title: pane.title,
                                    pane_pid: pid,
                                })
                            })
                        })
                        .collect::<Vec<_>>();
                    if let Ok(payload) = serde_json::to_string(&snapshots) {
                        run_command(
                            &["muxa", "zellij-plugin-snapshot", "--json", payload.as_str()],
                            BTreeMap::new(),
                        );
                    }
                    false
                }
                _ => false,
            }
        }
    }
}
