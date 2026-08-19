//! Finding the coding agents installed on this machine.
//!
//! There are four of them, and all four are found from Windows. That is the
//! whole reason this application has no component inside WSL: a distribution's
//! filesystem is reachable at `\\wsl.localhost\<distro>\`, so its agents can be
//! detected and configured from here like any other directory.
//!
//! Detection reports what it found and where, and says "not found" plainly. It
//! never guesses: an agent this cannot see is one the user can be told about,
//! whereas an agent this *invents* produces an Install button that writes a file
//! nothing will ever read.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    /// The directory the agent keeps in a home directory.
    fn dir(self) -> &'static str {
        match self {
            Self::Claude => ".claude",
            Self::Codex => ".codex",
        }
    }

    /// The file hooks are registered in. Different names, identical JSON shape,
    /// which is why one installer serves both.
    fn config(self) -> &'static str {
        match self {
            Self::Claude => "settings.json",
            Self::Codex => "hooks.json",
        }
    }

    /// Directories that only exist once the agent has actually been used. An
    /// empty `.claude` proves nothing; `.claude/projects` means someone worked
    /// in it.
    fn evidence_of_use(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["projects", "sessions", "history.jsonl"],
            Self::Codex => &["sessions", "cache", "memories", "history.jsonl"],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    /// Which agent a `--source` name belongs to.
    ///
    /// Events carry the flavor and nothing else about who sent them, so this is
    /// how a session knows which agent it is — and therefore which states that
    /// agent is even able to report.
    pub fn from_source(source: &str) -> Option<Self> {
        match source.split('-').next() {
            Some("claude") => Some(Self::Claude),
            Some("codex") => Some(Self::Codex),
            _ => None,
        }
    }
}

/// "Windows" or "WSL", from a `--source` name.
pub fn host_label(source: &str) -> &'static str {
    match source.rsplit('-').next() {
        Some("wsl") => "WSL",
        Some("win") => "Windows",
        _ => "unknown host",
    }
}

/// Where an agent runs. A WSL agent is a distribution and a user inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    Windows,
    Wsl { distro: String, user: String },
}

/// One of the four supported combinations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flavor {
    pub agent: Agent,
    pub host: Host,
}

impl Flavor {
    /// The `--source` value passed to the hook, and the name used everywhere a
    /// person has to be told which agent is misbehaving.
    pub fn source(&self) -> String {
        let agent = match self.agent {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        };
        match &self.host {
            Host::Windows => format!("{agent}-win"),
            Host::Wsl { .. } => format!("{agent}-wsl"),
        }
    }

    /// What the user still has to do themselves before hooks actually run.
    ///
    /// Codex refuses to run a command hook until it has been reviewed, and
    /// *where* you do that differs: the CLI has a `/hooks` command, while the
    /// Windows desktop app puts it under its settings. Sending someone to the
    /// wrong one is worse than saying nothing, because everything looks
    /// installed and nothing happens.
    pub fn trust_hint(&self) -> &'static str {
        match (self.agent, &self.host) {
            (Agent::Codex, Host::Windows) => {
                "Then approve it in Codex under Settings → Hooks, or it never runs."
            }
            (Agent::Codex, Host::Wsl { .. }) => {
                "Then run /hooks inside Codex and trust the entry, or it never runs."
            }
            (Agent::Claude, _) => "Then restart the agent so it reads its hooks.",
        }
    }

    pub fn describe(&self) -> String {
        match &self.host {
            Host::Windows => format!("{} on Windows", self.agent.label()),
            Host::Wsl { distro, user } => {
                format!("{} in WSL ({distro}, {user})", self.agent.label())
            }
        }
    }
}

/// What detection found for one flavor.
#[derive(Debug, Clone)]
pub struct Found {
    pub flavor: Flavor,
    /// The agent's own directory.
    pub home: PathBuf,
    /// The file we would write hooks into.
    pub config: PathBuf,
    /// Sub-directories showing the agent has actually been used here.
    pub evidence: Vec<String>,
}

impl Found {
    /// Whether this looks like a live installation rather than a leftover
    /// directory. Reported rather than enforced — the user can still install
    /// into a bare directory if they know better than we do.
    pub fn looks_used(&self) -> bool {
        !self.evidence.is_empty()
    }
}

/// Every agent installation this machine appears to have.
pub fn detect() -> Vec<Found> {
    let mut found = Vec::new();
    if let Some(profile) = windows_profile() {
        for agent in [Agent::Claude, Agent::Codex] {
            if let Some(entry) = probe(agent, Host::Windows, &profile) {
                found.push(entry);
            }
        }
    }
    for (distro, user, home) in wsl_homes() {
        for agent in [Agent::Claude, Agent::Codex] {
            let host = Host::Wsl {
                distro: distro.clone(),
                user: user.clone(),
            };
            if let Some(entry) = probe(agent, host, &home) {
                found.push(entry);
            }
        }
    }
    found
}

fn probe(agent: Agent, host: Host, home: &Path) -> Option<Found> {
    let agent_home = home.join(agent.dir());
    if !agent_home.is_dir() {
        return None;
    }
    let evidence = agent
        .evidence_of_use()
        .iter()
        .filter(|name| agent_home.join(name).exists())
        .map(|name| (*name).to_owned())
        .collect();
    Some(Found {
        flavor: Flavor { agent, host },
        config: agent_home.join(agent.config()),
        home: agent_home,
        evidence,
    })
}

pub fn windows_profile() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Every `(distro, user, home)` visible on this machine.
///
/// `\\wsl.localhost` serves a running distribution's filesystem to Windows. A
/// distribution that is stopped will not answer, which is why a flavor can go
/// from found to not-found between launches; that is the truth about it, and the
/// UI says when it last saw one rather than pretending.
fn wsl_homes() -> Vec<(String, String, PathBuf)> {
    let mut homes = Vec::new();
    for distro in wsl_distros() {
        let root = PathBuf::from(format!(r"\\wsl.localhost\{distro}\home"));
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let user = entry.file_name().to_string_lossy().into_owned();
            homes.push((distro.clone(), user, entry.path()));
        }
    }
    homes
}

/// Installed distributions, from `wsl.exe -l -q`.
///
/// The output is UTF-16LE, not UTF-8 — reading it as bytes yields a string with
/// a null between every character, which is exactly the sort of thing that
/// silently produces zero distributions and no explanation.
pub fn wsl_distros() -> Vec<String> {
    let Ok(output) = std::process::Command::new("wsl.exe")
        .args(["-l", "-q"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    decode_utf16le(&output.stdout)
        .lines()
        .map(|line| line.trim().trim_end_matches('\r').to_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn utf16_output_decodes_to_distribution_names() {
        // What `wsl.exe -l -q` actually writes: UTF-16LE with CRLF endings.
        let mut bytes = Vec::new();
        for ch in "Ubuntu\r\nDebian\r\n".encode_utf16() {
            bytes.extend_from_slice(&ch.to_le_bytes());
        }
        let names: Vec<String> = decode_utf16le(&bytes)
            .lines()
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(names, vec!["Ubuntu", "Debian"]);
    }

    #[test]
    fn source_names_cover_the_four_flavors() {
        let wsl = Host::Wsl {
            distro: "Ubuntu".into(),
            user: "jerome".into(),
        };
        let cases = [
            (Agent::Claude, Host::Windows, "claude-win"),
            (Agent::Codex, Host::Windows, "codex-win"),
            (Agent::Claude, wsl.clone(), "claude-wsl"),
            (Agent::Codex, wsl, "codex-wsl"),
        ];
        for (agent, host, expected) in cases {
            assert_eq!(Flavor { agent, host }.source(), expected);
        }
    }
}
