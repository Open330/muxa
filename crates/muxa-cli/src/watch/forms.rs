use super::{
    char_to_byte_idx, collab_screen, io, CollabLayout, CollaborationComposeDefaults,
    ComposeSendMode, ConfirmAction, MailboxDetail, MessageSkillPalette, Path, RequestKind,
    TopologyNodeKey, WINDOWS_GET_CLIPBOARD,
};

/// Modal confirmation popup state — mirrors the preview-popup pattern
/// (centred ratatui rect over the table) but with a binary y/N choice
/// instead of scrollable content.
///
/// **Why default focus is "No"**: the actions this gates (`K` kills the
/// pane, `R` aborts the agent's turn, `D` clears ask history) are destructive
/// enough that we want the user to deliberately type `y` rather than
/// fat-finger Enter.
/// Anything other than `y` / `Y` cancels — including Esc, `n`, `q`,
/// arrow keys, Tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfirmPopup {
    /// One-line message, e.g. `Kill pane main:2.0?`. Caller pre-formats
    /// the pane label so the popup itself stays presentation-only.
    pub message: String,
    /// What to dispatch when the user confirms with `y` / `Y`. Held
    /// here so the input handler can complete the round-trip without
    /// re-resolving the selected row (which might have moved between
    /// the popup opening and the user's reply).
    pub on_confirm: ConfirmAction,
}

/// Inline prompt composer opened from the table with Enter. It pins the
/// target pane at open time so background refreshes or resorting cannot
/// redirect a typed prompt to a different row.
/// `a` — a headless question to the configured agent. One text field;
/// the contract lives in config, not in the composer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct AskComposer {
    pub(super) input: String,
    pub(super) cursor: usize,
    /// `true` means Enter creates the conversation together with its first
    /// turn. This is only intent until submit, so cancelling never leaves an
    /// empty durable conversation.
    pub(super) new_conversation: bool,
    /// Ask shares the reusable text palette with message composition, but
    /// selection never changes the daemon-owned agent/permission contract.
    pub(super) skill_palette: Option<MessageSkillPalette>,
}

/// The `w` composer: a work id, nothing else.
///
/// Deliberately not folded into [`AskComposer`]. Ask is a read-only question
/// to a headless agent; this one spends money and creates panes, and the two
/// wanting the same text box is not a reason to give them the same key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct WorkComposer {
    pub(super) input: String,
    pub(super) cursor: usize,
}

/// Which level of the tree `R` is renaming, and what tmux calls the thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RenameLevel {
    Session,
    Window,
    Pane,
}

impl RenameLevel {
    pub(super) fn label(self) -> &'static str {
        match self {
            RenameLevel::Session => "session",
            RenameLevel::Window => "window",
            // tmux has no pane *name*; the closest durable, displayable
            // string is the pane title, which is what watch already shows.
            RenameLevel::Pane => "pane title",
        }
    }
}

/// Rename form for the row under the cursor. Carries the node key rather than
/// re-reading the selection on submit: a refresh can move the cursor while the
/// form is open, and renaming whatever happens to be selected a second later
/// is not what the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RenameComposer {
    pub(super) key: TopologyNodeKey,
    pub(super) level: RenameLevel,
    pub(super) original: String,
    pub(super) input: String,
    pub(super) cursor: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum MessageSkillEditorField {
    #[default]
    Name,
    Prompt,
}

/// In-watch registration form opened from either `/` palette. Persistence is
/// dispatched by the run loop so input handling stays side-effect free.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct MessageSkillEditor {
    pub(super) name: String,
    pub(super) name_cursor: usize,
    pub(super) prompt: String,
    pub(super) prompt_cursor: usize,
    pub(super) field: MessageSkillEditorField,
}

impl MessageSkillEditor {
    pub(super) fn field_mut(&mut self) -> (&mut String, &mut usize) {
        match self.field {
            MessageSkillEditorField::Name => (&mut self.name, &mut self.name_cursor),
            MessageSkillEditorField::Prompt => (&mut self.prompt, &mut self.prompt_cursor),
        }
    }

    pub(super) fn cycle_field(&mut self) {
        self.field = match self.field {
            MessageSkillEditorField::Name => MessageSkillEditorField::Prompt,
            MessageSkillEditorField::Prompt => MessageSkillEditorField::Name,
        };
    }
}

/// Compatibility filter for a daemon from before durable conversations.
/// Current daemons show one conversation transcript and use Tab to select
/// another; this preserves the former provider-filtered history instead of
/// blanking the panel during a rolling CLI/daemon upgrade.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum AskFilter {
    #[default]
    All,
    Claude,
    Codex,
}

impl AskFilter {
    pub(super) fn next(self) -> Self {
        match self {
            Self::All => Self::Claude,
            Self::Claude => Self::Codex,
            Self::Codex => Self::All,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub(super) fn matches(self, agent: &str) -> bool {
        match self {
            Self::All => true,
            Self::Claude => agent == "claude",
            Self::Codex => agent == "codex",
        }
    }
}

/// `A` — one durable Ask conversation at a time. Same shape as the
/// collaboration mailbox because it answers the same question ("what did I
/// send, what came back"), down to `|` cycling the detail height.
#[derive(Debug, Clone, Default)]
pub(super) struct AskPanelState {
    pub(super) open: bool,
    /// Index into the *filtered* list, not the raw history — the cursor
    /// should stay on a row the user can see.
    pub(super) selected: usize,
    pub(super) detail: MailboxDetail,
    pub(super) filter: AskFilter,
    /// Enter opens the selected answer full-height and scrollable. The
    /// detail pane under the list can only ever show its first screenful,
    /// and an agent's answer is routinely longer than that.
    pub(super) reader: Option<AskReader>,
}

impl AskPanelState {
    /// `A` and a finished ask both land on the newest row — never inside a
    /// reader left open from the last visit.
    pub(super) fn open_at(&mut self, selected: usize) {
        self.open = true;
        self.selected = selected;
        self.reader = None;
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.reader = None;
    }
}

/// The full-answer view opened from the ask history.
#[derive(Debug, Clone, Default)]
pub(super) struct AskReader {
    /// The entry being read, addressed by id rather than by row: the
    /// history is re-fetched from the daemon on a timer, so an index would
    /// silently re-point at another answer when one arrives or is deleted.
    pub(super) id: String,
    /// Body lines scrolled past the top.
    pub(super) scroll: usize,
    /// Wrapped body length and viewport height from the last paint, so the
    /// key handler clamps against real content instead of a guess — the
    /// same render-to-input handoff as `table_page_rows`.
    pub(super) total_lines: usize,
    pub(super) view_height: usize,
}

impl AskReader {
    pub(super) fn max_scroll(&self) -> usize {
        self.total_lines.saturating_sub(self.view_height)
    }

    pub(super) fn scroll_by(&mut self, delta: isize) {
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.max_scroll());
    }

    /// One screenful less a line of overlap, so a page turn keeps the
    /// sentence the eye was on.
    pub(super) fn page(&self) -> isize {
        isize::try_from(self.view_height.saturating_sub(1).max(1)).unwrap_or(1)
    }
}

/// Which agent CLI the spawn form launches. `Left`/`Right` cycle it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum SpawnAgent {
    #[default]
    Claude,
    Codex,
    Gemini,
    Antigravity,
    Opencode,
}

impl SpawnAgent {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Antigravity => "agy",
            Self::Opencode => "opencode",
        }
    }

    pub(super) fn next(self) -> Self {
        match self {
            Self::Claude => Self::Codex,
            Self::Codex => Self::Gemini,
            Self::Gemini => Self::Antigravity,
            Self::Antigravity => Self::Opencode,
            Self::Opencode => Self::Claude,
        }
    }

    pub(super) fn prev(self) -> Self {
        match self {
            Self::Claude => Self::Opencode,
            Self::Codex => Self::Claude,
            Self::Gemini => Self::Codex,
            Self::Antigravity => Self::Gemini,
            Self::Opencode => Self::Antigravity,
        }
    }

    /// The interactive launch line, first prompt included, so the agent
    /// boots already working — no fragile "wait for the REPL, then type"
    /// timing. Mirrors callabo-resolve's launcher matrix, bypass flags
    /// included: a spawned session is fire-and-forget by construction, and
    /// an agent parked on its first permission prompt in a window nobody
    /// is watching never runs the command it was created for.
    pub(super) fn launch_command(self, prompt: &str) -> String {
        let quoted = muxa::shell::Dialect::native().quote(prompt);
        match self {
            Self::Claude => format!("claude --dangerously-skip-permissions {quoted}"),
            Self::Codex => {
                format!("codex --dangerously-bypass-approvals-and-sandbox {quoted}")
            }
            Self::Gemini => format!("gemini --approval-mode yolo --skip-trust -i {quoted}"),
            Self::Antigravity => format!("agy --dangerously-skip-permissions -i {quoted}"),
            Self::Opencode => format!("opencode --prompt {quoted}"),
        }
    }
}

/// Cursor-aware multi-char insertion shared by paste paths.
pub(super) fn insert_str_at(text: &mut String, cursor: &mut usize, pasted: &str) {
    let idx = char_to_byte_idx(text, *cursor);
    text.insert_str(idx, pasted);
    *cursor += pasted.chars().count();
}

/// Best-effort system clipboard read for terminals where `Ctrl-V` reaches
/// us as a plain keypress instead of a bracketed paste. Mirrors
/// `copy_to_clipboard`'s backend cascade in reverse; first non-empty
/// answer wins.
pub(super) fn system_clipboard_text() -> Option<String> {
    let inside_rmux = std::env::var_os("RMUX").is_some();
    let in_tmux = std::env::var_os("TMUX").is_some() && !inside_rmux;
    let in_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let in_x11 = std::env::var_os("DISPLAY").is_some();
    let powershell = muxa::shell::powershell().to_string_lossy().into_owned();
    let candidates: [(&str, &[&str], bool); 7] = [
        (
            &powershell,
            &["-NoLogo", "-NoProfile", "-Command", WINDOWS_GET_CLIPBOARD],
            cfg!(windows),
        ),
        ("rmux", &["save-buffer", "-"], inside_rmux),
        ("tmux", &["save-buffer", "-"], in_tmux),
        ("pbpaste", &[], cfg!(target_os = "macos")),
        ("wl-paste", &["--no-newline"], in_wayland),
        ("xclip", &["-selection", "clipboard", "-o"], in_x11),
        ("xsel", &["-b", "-o"], in_x11),
    ];
    for (bin, args, applicable) in candidates {
        if !applicable {
            continue;
        }
        let Ok(out) = std::process::Command::new(bin).args(args).output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

/// Wrap `text` into display rows of at most `avail` terminal cells,
/// width-aware so wide glyphs (Korean, CJK) never overflow a row the way
/// char-counted chunking would. Returns each row with the char index it
/// starts at, which is what maps a cursor to (row, column).
pub(super) fn wrap_input_rows(text: &str, avail: usize) -> Vec<(String, usize)> {
    use unicode_width::UnicodeWidthChar;
    let avail = avail.max(4);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut row_start = 0usize;
    let mut row_width = 0usize;
    for (i, c) in text.chars().enumerate() {
        let w = c.width().unwrap_or(0);
        if row_width + w > avail && !row.is_empty() {
            rows.push((std::mem::take(&mut row), row_start));
            row_start = i;
            row_width = 0;
        }
        row.push(c);
        row_width += w;
    }
    rows.push((row, row_start));
    rows
}

/// POSIX-safe single quoting: close, escape the quote, reopen.
#[cfg(test)]
pub(super) fn shell_single_quote(text: &str) -> String {
    muxa::shell::Dialect::Posix.quote(text)
}

/// tmux session names cannot contain `.` or `:`; everything else from a
/// directory basename passes through.
pub(super) fn session_base_name(dir: &str) -> String {
    let base = std::path::Path::new(dir)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("agent");
    let cleaned: String = base
        .chars()
        .map(|c| if c == '.' || c == ':' { '-' } else { c })
        .collect();
    if cleaned.is_empty() {
        "agent".into()
    } else {
        cleaned
    }
}

/// Fields of the spawn form, in Tab order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum SpawnField {
    Dir,
    Name,
    Agent,
    #[default]
    Prompt,
}

/// `n` creates a window with its first agent or adds a sibling agent pane,
/// depending on the selected topology level.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SpawnComposer {
    /// Stable node selected when the form opened. Background refresh cannot
    /// redirect the eventual spawn to another session/window.
    pub(super) target: Option<TopologyNodeKey>,
    pub(super) dir: String,
    pub(super) dir_cursor: usize,
    /// Work id. Empty means derive it from the directory.
    pub(super) name: String,
    pub(super) name_cursor: usize,
    pub(super) agent: SpawnAgent,
    pub(super) prompt: String,
    pub(super) prompt_cursor: usize,
    pub(super) focus: SpawnField,
}

impl SpawnComposer {
    pub(super) fn creates_window(&self) -> bool {
        matches!(self.target, None | Some(TopologyNodeKey::Session(_)))
    }

    pub(super) fn field_mut(&mut self) -> Option<(&mut String, &mut usize)> {
        match self.focus {
            SpawnField::Dir => Some((&mut self.dir, &mut self.dir_cursor)),
            SpawnField::Name if self.creates_window() => {
                Some((&mut self.name, &mut self.name_cursor))
            }
            SpawnField::Prompt => Some((&mut self.prompt, &mut self.prompt_cursor)),
            SpawnField::Name | SpawnField::Agent => None,
        }
    }

    pub(super) fn next_focus(&self) -> SpawnField {
        match self.focus {
            SpawnField::Dir if self.creates_window() => SpawnField::Name,
            SpawnField::Dir | SpawnField::Name => SpawnField::Agent,
            SpawnField::Agent => SpawnField::Prompt,
            SpawnField::Prompt => SpawnField::Dir,
        }
    }

    pub(super) fn previous_focus(&self) -> SpawnField {
        match self.focus {
            SpawnField::Dir => SpawnField::Prompt,
            SpawnField::Agent if self.creates_window() => SpawnField::Name,
            SpawnField::Name | SpawnField::Agent => SpawnField::Dir,
            SpawnField::Prompt => SpawnField::Agent,
        }
    }
}

/// The three table/inspector splits `|` cycles through. Presets rather
/// than free resize: two keystrokes reach any of them, and a TUI resize
/// mode would cost a modal state for a knob that has three useful values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum InspectorSplit {
    #[default]
    Balanced,
    ListWide,
    InspectorWide,
}

impl InspectorSplit {
    pub(super) fn list_pct(self) -> u16 {
        match self {
            Self::Balanced => 50,
            Self::ListWide => 70,
            Self::InspectorWide => 30,
        }
    }

    pub(super) fn next(self) -> Self {
        match self {
            Self::Balanced => Self::ListWide,
            Self::ListWide => Self::InspectorWide,
            Self::InspectorWide => Self::Balanced,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Balanced => "50/50",
            Self::ListWide => "70/30",
            Self::InspectorWide => "30/70",
        }
    }

    pub(super) fn from_label(label: &str) -> Option<Self> {
        match label {
            "50/50" => Some(Self::Balanced),
            "70/30" => Some(Self::ListWide),
            "30/70" => Some(Self::InspectorWide),
            _ => None,
        }
    }
}

/// Persist the cycled split next to the persisted sort, so the divider a
/// user drags into place survives the popup closing.
pub(super) fn persist_watch_inspector_split(
    path: &Path,
    label: &str,
) -> std::result::Result<(), String> {
    let original = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let mut doc = if original.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        original
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| format!("parse {}: {e}", path.display()))?
    };
    match doc.get("watch") {
        Some(toml_edit::Item::Table(_)) | None => {}
        Some(_) => return Err("[watch] is not a table".to_string()),
    }
    if doc.get("watch").is_none() {
        doc["watch"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let watch = doc["watch"]
        .as_table_mut()
        .ok_or_else(|| "[watch] is not a table".to_string())?;
    watch["inspector_split"] = toml_edit::value(label);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, doc.to_string()).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Persist the last request badges as watch preferences. They belong in the
/// existing `[watch]` table because they are composer UI state, not daemon
/// collaboration policy.
pub(super) fn persist_watch_collaboration_defaults(
    path: &Path,
    defaults: CollaborationComposeDefaults,
) -> std::result::Result<(), String> {
    let original = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let mut doc = if original.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        original
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| format!("parse {}: {e}", path.display()))?
    };
    match doc.get("watch") {
        Some(toml_edit::Item::Table(_)) | None => {}
        Some(_) => return Err("[watch] is not a table".to_string()),
    }
    if doc.get("watch").is_none() {
        doc["watch"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let watch = doc["watch"]
        .as_table_mut()
        .ok_or_else(|| "[watch] is not a table".to_string())?;
    let kind = match defaults.kind {
        RequestKind::Question => "question",
        RequestKind::Review => "review",
        RequestKind::Task => "task",
        RequestKind::Notice => "notice",
    };
    let mode = match defaults.mode {
        ComposeSendMode::ReadOnly => "read_only",
        ComposeSendMode::Execute => "execute",
        ComposeSendMode::JustSend => "just_send",
    };
    watch["collaboration_kind"] = toml_edit::value(kind);
    watch["collaboration_mode"] = toml_edit::value(mode);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, doc.to_string()).map_err(|e| format!("write {}: {e}", path.display()))
}

pub(super) fn persist_watch_collab_layout(
    path: &Path,
    layout: CollabLayout,
) -> std::result::Result<(), String> {
    persist_watch_value(path, "collab_layout", collab_screen::layout_label(layout))
}

/// Write one `[watch]` key, keeping the rest of the file as it was.
pub(super) fn persist_watch_value(
    path: &Path,
    key: &str,
    value: &str,
) -> std::result::Result<(), String> {
    let original = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let mut doc = if original.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        original
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| format!("parse {}: {e}", path.display()))?
    };
    match doc.get("watch") {
        Some(toml_edit::Item::Table(_)) | None => {}
        Some(_) => return Err("[watch] is not a table".to_string()),
    }
    if doc.get("watch").is_none() {
        doc["watch"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let watch = doc["watch"]
        .as_table_mut()
        .ok_or_else(|| "[watch] is not a table".to_string())?;
    watch[key] = toml_edit::value(value);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, doc.to_string()).map_err(|e| format!("write {}: {e}", path.display()))
}
