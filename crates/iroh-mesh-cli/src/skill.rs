//! `imesh skill`: prints the agent skill, or installs it where coding agents look for skills.

use std::path::PathBuf;

use anyhow::{Context as _, Result};

/// The skill file, embedded in the binary.
pub const TEXT: &str = include_str!("skill.md");

/// A coding agent, by the skills directory it reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    /// `~/.agents/skills`, the shared directory that many agents read.
    Agents,
    /// Claude Code: `~/.claude/skills`.
    Claude,
    /// Codex: `~/.codex/skills`.
    Codex,
    /// GitHub Copilot: `~/.copilot/skills`.
    Copilot,
    /// OpenCode: `~/.config/opencode/skills`.
    Opencode,
    /// Pi: `~/.pi/agent/skills`.
    Pi,
}

impl Agent {
    const ALL: [Agent; 6] = [Agent::Agents, Agent::Claude, Agent::Codex, Agent::Copilot, Agent::Opencode, Agent::Pi];

    fn skills_dir(self) -> Result<PathBuf> {
        let home = dirs::home_dir().context("no home directory")?;
        Ok(match self {
            Agent::Agents => home.join(".agents/skills"),
            Agent::Claude => home.join(".claude/skills"),
            Agent::Codex => home.join(".codex/skills"),
            Agent::Copilot => home.join(".copilot/skills"),
            Agent::Opencode => home.join(".config/opencode/skills"),
            Agent::Pi => home.join(".pi/agent/skills"),
        })
    }

    fn file(self) -> Result<PathBuf> {
        Ok(self.skills_dir()?.join("imesh").join("SKILL.md"))
    }
}

impl Agent {
    fn name(self) -> &'static str {
        match self {
            Agent::Agents => "agents",
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Copilot => "copilot",
            Agent::Opencode => "opencode",
            Agent::Pi => "pi",
        }
    }
}

/// Asks which agents to use. Lists each agent with its directory, marks where the skill is
/// installed. For install, preselects `~/.agents/skills` and the agents whose directory exists.
/// For uninstall, preselects where the skill is installed.
pub fn pick(verb: &str) -> Result<Vec<Agent>> {
    use std::io::{IsTerminal, Write};
    anyhow::ensure!(std::io::stdin().is_terminal(), "-i needs a terminal; use --agent instead");
    let installed: Vec<bool> = Agent::ALL.iter().map(|a| a.file().is_ok_and(|f| f.exists())).collect();
    let present: Vec<bool> = Agent::ALL
        .iter()
        .map(|a| a.skills_dir().ok().and_then(|d| d.parent().map(|p| p.exists())).unwrap_or(false))
        .collect();
    let preselect: Vec<usize> = if verb == "install" {
        (0..Agent::ALL.len()).filter(|&i| i == 0 || present[i]).collect()
    } else {
        (0..Agent::ALL.len()).filter(|&i| installed[i]).collect()
    };
    for (i, a) in Agent::ALL.iter().enumerate() {
        let dir = a.skills_dir()?;
        let note = if installed[i] {
            " (installed)"
        } else if !present[i] {
            " (not found)"
        } else {
            ""
        };
        println!("  {}) {:<9} {}{note}", i + 1, a.name(), dir.display());
    }
    let default: Vec<String> = preselect.iter().map(|i| (i + 1).to_string()).collect();
    print!(
        "Agents to {verb}, as numbers separated by spaces or commas, or `all`{}: ",
        if default.is_empty() { String::new() } else { format!(" [{}]", default.join(" ")) }
    );
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim();
    if line.is_empty() {
        return Ok(preselect.iter().map(|&i| Agent::ALL[i]).collect());
    }
    if line.eq_ignore_ascii_case("all") {
        return Ok(Agent::ALL.to_vec());
    }
    let mut out = Vec::new();
    for part in line.split([' ', ',']).filter(|p| !p.is_empty()) {
        let a = part
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| Agent::ALL.get(n).copied())
            .or_else(|| Agent::ALL.iter().copied().find(|a| a.name() == part))
            .with_context(|| format!("no agent {part:?}"))?;
        if !out.contains(&a) {
            out.push(a);
        }
    }
    Ok(out)
}

fn targets(agents: &[Agent], all: bool) -> Vec<Agent> {
    if all {
        Agent::ALL.to_vec()
    } else if agents.is_empty() {
        vec![Agent::Agents]
    } else {
        let mut v = agents.to_vec();
        v.dedup();
        v
    }
}

/// `imesh skill install`: writes `imesh/SKILL.md` in each agent's skills directory. A new
/// version of imesh writes its own skill again.
pub fn install(agents: &[Agent], all: bool) -> Result<()> {
    for a in targets(agents, all) {
        let path = a.file()?;
        std::fs::create_dir_all(path.parent().expect("has a parent"))?;
        std::fs::write(&path, TEXT).with_context(|| format!("write {}", path.display()))?;
        println!("installed {}", path.display());
    }
    Ok(())
}

/// `imesh skill uninstall`: removes the `imesh` skill directory from each agent's skills
/// directory. Removes nothing else.
pub fn uninstall(agents: &[Agent], all: bool) -> Result<()> {
    for a in targets(agents, all) {
        let path = a.file()?;
        let dir = path.parent().expect("has a parent");
        if !path.exists() {
            if !all {
                println!("not installed: {}", path.display());
            }
            continue;
        }
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        // Only an empty directory goes, so files the user added stay.
        let _ = std::fs::remove_dir(dir);
        println!("removed {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_the_shared_directory() {
        assert_eq!(targets(&[], false), [Agent::Agents]);
        assert_eq!(targets(&[Agent::Claude, Agent::Claude], false), [Agent::Claude]);
        assert_eq!(targets(&[], true).len(), 6);
    }
}
