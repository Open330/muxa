use clap::ValueEnum;
use comfy_table::{Attribute, Cell, CellAlignment, Color};
use muxa::config::{Config, WatchTheme};
use muxa::AgentState;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ThemeArg {
    Classic,
    #[value(alias = "oh_my_muxa")]
    OhMyMuxa,
    Focus,
    Ops,
    Mono,
    #[value(alias = "high_contrast")]
    HighContrast,
    Minimal,
}

impl From<ThemeArg> for WatchTheme {
    fn from(value: ThemeArg) -> Self {
        match value {
            ThemeArg::Classic => Self::Classic,
            ThemeArg::OhMyMuxa => Self::OhMyMuxa,
            ThemeArg::Focus => Self::Focus,
            ThemeArg::Ops => Self::Ops,
            ThemeArg::Mono => Self::Mono,
            ThemeArg::HighContrast => Self::HighContrast,
            ThemeArg::Minimal => Self::Minimal,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TableTone {
    Header,
    Accent,
    Dim,
    Good,
    Warn,
    Choice,
    Error,
    Tmux,
    Human,
    Thinking,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CliTheme {
    enabled: bool,
    header: Color,
    accent: Color,
    dim: Color,
    good: Color,
    warn: Color,
    choice: Color,
    error: Color,
    tmux: Color,
    human: Color,
    thinking: Color,
    starting: Color,
}

impl CliTheme {
    #[cfg(test)]
    pub(crate) fn plain() -> Self {
        Self {
            enabled: false,
            ..classic_theme()
        }
    }

    pub(crate) fn cell(self, content: impl ToString, tone: TableTone) -> Cell {
        self.apply(Cell::new(content), tone)
    }

    pub(crate) fn right_cell(self, content: impl ToString, tone: TableTone) -> Cell {
        self.cell(content, tone).set_alignment(CellAlignment::Right)
    }

    pub(crate) fn state_cell(self, label: &str, state: AgentState) -> Cell {
        let cell = Cell::new(label);
        if !self.enabled {
            return cell;
        }
        match state {
            AgentState::Working => cell.fg(self.good).add_attribute(Attribute::Bold),
            AgentState::WaitingInput => cell.fg(self.warn).add_attribute(Attribute::Bold),
            AgentState::WaitingChoice => cell.fg(self.choice).add_attribute(Attribute::Bold),
            AgentState::Idle => cell.fg(self.dim).add_attribute(Attribute::Dim),
            AgentState::Error => cell.fg(self.error).add_attribute(Attribute::Bold),
            AgentState::Stopped => cell
                .fg(self.dim)
                .add_attribute(Attribute::Dim)
                .add_attribute(Attribute::CrossedOut),
            AgentState::Starting => cell.fg(self.starting),
        }
    }

    fn apply(self, cell: Cell, tone: TableTone) -> Cell {
        if !self.enabled {
            return cell;
        }
        let cell = cell.fg(self.color(tone));
        match tone {
            TableTone::Header
            | TableTone::Good
            | TableTone::Warn
            | TableTone::Choice
            | TableTone::Error => cell.add_attribute(Attribute::Bold),
            TableTone::Dim => cell.add_attribute(Attribute::Dim),
            TableTone::Accent | TableTone::Tmux | TableTone::Human | TableTone::Thinking => cell,
        }
    }

    fn color(self, tone: TableTone) -> Color {
        match tone {
            TableTone::Header => self.header,
            TableTone::Accent => self.accent,
            TableTone::Dim => self.dim,
            TableTone::Good => self.good,
            TableTone::Warn => self.warn,
            TableTone::Choice => self.choice,
            TableTone::Error => self.error,
            TableTone::Tmux => self.tmux,
            TableTone::Human => self.human,
            TableTone::Thinking => self.thinking,
        }
    }
}

pub(crate) fn for_config(
    cfg: &Config,
    override_theme: Option<ThemeArg>,
    enabled: bool,
) -> CliTheme {
    let theme = override_theme.map_or(cfg.ui.theme, WatchTheme::from);
    for_theme(theme, enabled)
}

pub(crate) fn for_theme(theme: WatchTheme, enabled: bool) -> CliTheme {
    CliTheme {
        enabled,
        ..match theme {
            WatchTheme::Classic => classic_theme(),
            WatchTheme::OhMyMuxa => oh_my_muxa_theme(),
            WatchTheme::Focus => focus_theme(),
            WatchTheme::Ops => ops_theme(),
            WatchTheme::Mono => mono_theme(),
            WatchTheme::HighContrast => high_contrast_theme(),
            WatchTheme::Minimal => minimal_theme(),
        }
    }
}

fn classic_theme() -> CliTheme {
    CliTheme {
        enabled: true,
        header: Color::Grey,
        accent: Color::Cyan,
        dim: Color::DarkGrey,
        good: Color::Green,
        warn: Color::Yellow,
        choice: Color::Yellow,
        error: Color::Red,
        tmux: Color::Cyan,
        human: Color::Magenta,
        thinking: Color::Blue,
        starting: Color::Cyan,
    }
}

fn oh_my_muxa_theme() -> CliTheme {
    CliTheme {
        enabled: true,
        header: Color::Rgb {
            r: 94,
            g: 234,
            b: 212,
        },
        accent: Color::Rgb {
            r: 177,
            g: 139,
            b: 255,
        },
        dim: Color::Grey,
        good: Color::Rgb {
            r: 93,
            g: 230,
            b: 138,
        },
        warn: Color::Rgb {
            r: 255,
            g: 176,
            b: 86,
        },
        choice: Color::Rgb {
            r: 219,
            g: 181,
            b: 255,
        },
        error: Color::Rgb {
            r: 255,
            g: 91,
            b: 107,
        },
        tmux: Color::Rgb {
            r: 94,
            g: 234,
            b: 212,
        },
        human: Color::Rgb {
            r: 219,
            g: 181,
            b: 255,
        },
        thinking: Color::Rgb {
            r: 255,
            g: 211,
            b: 105,
        },
        starting: Color::Rgb {
            r: 94,
            g: 234,
            b: 212,
        },
    }
}

fn focus_theme() -> CliTheme {
    CliTheme {
        enabled: true,
        header: Color::Rgb {
            r: 125,
            g: 211,
            b: 252,
        },
        accent: Color::Rgb {
            r: 134,
            g: 239,
            b: 172,
        },
        dim: Color::DarkGrey,
        good: Color::Rgb {
            r: 125,
            g: 211,
            b: 252,
        },
        warn: Color::Yellow,
        choice: Color::Cyan,
        error: Color::Red,
        tmux: Color::Cyan,
        human: Color::Green,
        thinking: Color::Yellow,
        starting: Color::Cyan,
    }
}

fn ops_theme() -> CliTheme {
    CliTheme {
        enabled: true,
        header: Color::Yellow,
        accent: Color::Cyan,
        dim: Color::Grey,
        good: Color::Green,
        warn: Color::Yellow,
        choice: Color::Magenta,
        error: Color::Red,
        tmux: Color::Cyan,
        human: Color::Magenta,
        thinking: Color::Yellow,
        starting: Color::Cyan,
    }
}

fn mono_theme() -> CliTheme {
    CliTheme {
        enabled: true,
        header: Color::White,
        accent: Color::Grey,
        dim: Color::DarkGrey,
        good: Color::White,
        warn: Color::White,
        choice: Color::White,
        error: Color::White,
        tmux: Color::Grey,
        human: Color::Grey,
        thinking: Color::White,
        starting: Color::Grey,
    }
}

fn high_contrast_theme() -> CliTheme {
    CliTheme {
        enabled: true,
        header: Color::White,
        accent: Color::Yellow,
        dim: Color::Grey,
        good: Color::Green,
        warn: Color::Yellow,
        choice: Color::Magenta,
        error: Color::Red,
        tmux: Color::Cyan,
        human: Color::Magenta,
        thinking: Color::Yellow,
        starting: Color::Blue,
    }
}

fn minimal_theme() -> CliTheme {
    CliTheme {
        enabled: true,
        header: Color::White,
        accent: Color::Grey,
        dim: Color::DarkGrey,
        good: Color::White,
        warn: Color::White,
        choice: Color::White,
        error: Color::White,
        tmux: Color::White,
        human: Color::White,
        thinking: Color::White,
        starting: Color::White,
    }
}
