//! Interactive and printable Muxa onboarding.
//!
//! The guide teaches the product policy, the normal work/agent workflow,
//! watch operation, and the compact MCP pattern. Watch shortcuts are read
//! from `watch::help_overlay_text` so the tutorial cannot drift from the TUI.

use anyhow::Result;
use std::io::IsTerminal;

#[derive(Debug, Clone, clap::Args, Default)]
pub struct Args {
    /// Print the complete guide without interactive prompts.
    #[arg(long)]
    pub print: bool,
    /// Skip the short knowledge check.
    #[arg(long)]
    pub no_quiz: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Interactive,
    Print,
}

impl Mode {
    fn detect(force_print: bool) -> Self {
        if force_print || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            Self::Print
        } else {
            Self::Interactive
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Section {
    title: &'static str,
    body: &'static str,
}

const POLICY: &str = "\
session = work/ticket\n\
pane    = agent\n\
window  = layout only\n\
\n\
Muxa owns tmux lifecycle, location, state, and collaboration routing.\n\
Agents own files, code, Git, tests, and reasoning.\n\
One ticket reuses one managed session; it never silently becomes ticket-2.";

const WORKFLOW: &str = "\
1. Start the work with its first agent:\n\
   muxa work start CAL-7041 --cwd /repo --agent codex --role implementer --prompt \"Implement CAL-7041\"\n\
\n\
2. Add another agent to the same work:\n\
   muxa agent start --work CAL-7041 --agent claude --role reviewer --prompt \"Review the current changes\"\n\
\n\
3. Inspect or operate it:\n\
   muxa work list\n\
   muxa work show CAL-7041\n\
   muxa watch\n\
   muxa agent control --pane %42 --action interrupt\n\
\n\
4. Close explicitly when the work is finished:\n\
   muxa work close CAL-7041";

const MCP_PATTERN: &str = "\
Use the existing muxa MCP server; do not add a second tmux MCP.\n\
\n\
muxa_start_agent(work=\"CAL-7041\", agent=\"codex\", role=\"reviewer\", prompt=\"Review ...\")\n\
muxa_wait_for_change(pane=\"%42\", until=\"settled\", include_capture=true)\n\
muxa_status(pane=\"%42\", include_capture=true, history_limit=1)\n\
muxa_manage_tmux(action=\"interrupt_agent\", pane=\"%42\")\n\
\n\
terminate_agent and close_work require confirm=true. Muxa refuses to terminate unmanaged panes.";

const SAFETY: &str = "\
Muxa deliberately does not expose arbitrary shell or generic tmux commands.\n\
Use exact pane ids for agent control. Destructive actions require confirmation.\n\
Use collaboration review + read_only by default; grant execute only with narrow paths.";

const SECTIONS: &[Section] = &[
    Section {
        title: "1 · Mental model",
        body: POLICY,
    },
    Section {
        title: "2 · Default workflow",
        body: WORKFLOW,
    },
    Section {
        title: "3 · Agent-facing MCP pattern",
        body: MCP_PATTERN,
    },
    Section {
        title: "4 · Safety boundary",
        body: SAFETY,
    },
];

pub fn run(args: Args) -> Result<()> {
    let mode = Mode::detect(args.print);
    match mode {
        Mode::Print => print_guide(),
        Mode::Interactive => interactive_guide(args.no_quiz)?,
    }
    Ok(())
}

fn print_guide() {
    println!("Muxa onboarding");
    println!("===============");
    for section in SECTIONS {
        println!("\n{}\n{}\n", section.title, "-".repeat(section.title.len()));
        println!("{}", section.body);
    }
    println!("\n5 · muxa watch shortcuts\n------------------------");
    for line in crate::watch::help_overlay_text() {
        println!("{line}");
    }
    println!("\nNext: run muxa watch or press tmux prefix+s.");
}

fn interactive_guide(no_quiz: bool) -> Result<()> {
    cliclack::intro("muxa onboarding")?;
    for section in SECTIONS {
        cliclack::note(section.title, section.body)?;
    }
    cliclack::note(
        "5 · muxa watch shortcuts",
        crate::watch::help_overlay_text().join("\n"),
    )?;

    if !no_quiz {
        let mut score = 0;
        score += usize::from(quiz(
            "What does one tmux session represent?",
            &["One work/ticket", "One agent", "One repository forever"],
            0,
            "A managed session is one work/ticket; panes inside it are agents.",
        )?);
        score += usize::from(quiz(
            "How should a second reviewer join CAL-7041?",
            &[
                "Start it with --work CAL-7041",
                "Create CAL-7041-2",
                "Ask an agent to type tmux commands",
            ],
            0,
            "Reuse the work id so Muxa adds a managed agent pane.",
        )?);
        score += usize::from(quiz(
            "Which wait avoids polling every intermediate transition?",
            &[
                "until=settled with include_capture",
                "Repeated status calls",
                "A fixed shell sleep",
            ],
            0,
            "Settled returns on idle, blocked, error, or stopped and can include the screen.",
        )?);
        cliclack::note("Knowledge check", format!("{score}/3 correct"))?;
    }

    cliclack::outro("Ready. Run muxa watch or press tmux prefix+s.")?;
    Ok(())
}

fn quiz(prompt: &str, choices: &[&str], correct: usize, explanation: &str) -> Result<bool> {
    let mut select = cliclack::select(prompt);
    for (index, choice) in choices.iter().enumerate() {
        select = select.item(index, choice, "");
    }
    let answer = select.interact()?;
    let correct = answer == correct;
    if correct {
        cliclack::log::success(explanation)?;
    } else {
        cliclack::log::warning(explanation)?;
    }
    Ok(correct)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onboarding_policy_and_workflow_pin_the_domain_model() {
        assert!(POLICY.contains("session = work/ticket"));
        assert!(POLICY.contains("pane    = agent"));
        assert!(POLICY.contains("window  = layout only"));
        assert!(WORKFLOW.contains("muxa work start CAL-7041"));
        assert!(WORKFLOW.contains("muxa agent start --work CAL-7041"));
    }

    #[test]
    fn onboarding_reuses_the_canonical_watch_shortcuts() {
        let shortcuts = crate::watch::help_overlay_text().join("\n");
        assert!(shortcuts.contains("m / M"));
        assert!(shortcuts.contains("a / A"));
        assert!(shortcuts.contains("Alt-K"));
        assert!(shortcuts.contains("n              new/reused work session + agent"));
    }

    #[test]
    fn mcp_training_uses_focused_settled_observation() {
        assert!(MCP_PATTERN.contains("until=\"settled\""));
        assert!(MCP_PATTERN.contains("include_capture=true"));
        assert!(MCP_PATTERN.contains("confirm=true"));
    }
}
