//! The managed block installer.
//!
//! Mechanism borrowed from Beads (attribution in the README): a marker-delimited section of the
//! agent instruction files, rewritten idempotently, with the profile name and a content hash in
//! the begin-line so drift and staleness are visible at a glance. The *content* is ours.
//!
//! Two profiles:
//! - `conservative` — the safe default for arbitrary users: track work, touch no git.
//! - `maintainer` — the FPL fleet profile: agents commit small and often, keep worktrees fresh,
//!   and never leave one dirty. Push/PR authority stays with the delivery loop, not the claim.

use sha2::{Digest, Sha256};

pub const BEGIN_PREFIX: &str = "<!-- BEGIN MARBLES";
pub const END: &str = "<!-- END MARBLES INTEGRATION -->";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Conservative,
    Maintainer,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "conservative" => Some(Self::Conservative),
            "maintainer" => Some(Self::Maintainer),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Conservative => "conservative",
            Self::Maintainer => "maintainer",
        }
    }
}

fn body(profile: Profile) -> String {
    let common = format!(
        "## Marbles issue tracker\n\n\
         This project tracks work with `marbles` (`mb`). Issues, dependencies, and claims live in\n\
         the central Marbles server — never in markdown task lists, and never in a per-checkout\n\
         database that has to be synced.\n\n\
         ### Quick reference\n\n\
         ```bash\n\
         mb ready --json            # eligible work, already\n\
         mb claim <id> --as <who>   # take work (claims race safely; losing is normal)\n\
         mb touch <id>              # heartbeat a lease mid-work\n\
         mb review <id> --pr URL    # PR opened: work is under review, NOT done\n\
         mb close <id> --pr URL --commit SHA   # done means merged\n\
         mb close <id> --ack \"no delivery expected: <reason>\"  # research/coordination\n\
         ```\n\n\
         The state machine is deliberately strict: `review` is what you set when a PR exists;\n\
         `closed` requires merge evidence (PR or commit) or an explicit acknowledgement that\n\
         none is expected. An agent that finished writing code has produced a review, not a\n\
         delivery.\n\n\
         Claims carry TTLs. Agent claims are minutes long and renewed by heartbeats; an expired\n\
         agent claim re-queues automatically. Human holds are business-hours long and, when they\n\
         lapse, escalate to the owner rather than silently re-queuing.\n"
    );
    let hygiene = match profile {
        Profile::Conservative => "### Git policy\n\n\
             Do not create commits or push unless the repository instructions or the user\n\
             explicitly allow it. Report changed files and the commands you would run.\n"
            .to_string(),
        Profile::Maintainer => "### Git hygiene (this fleet runs hot — non-negotiable)\n\n\
             - **Commit early, commit often.** Verified work lands as a local commit per intent,\n\
               referencing the marble id in the message (`mb show` it, mention it). Uncommitted\n\
               work is work that does not exist when your session dies.\n\
             - **No stale checkouts.** Rebase onto the base branch at claim time and before any\n\
               diff-dependent operation. A checkout older than four hours must rebase or be\n\
               discarded; never build on code you have not refreshed.\n\
             - **No dirty exits.** A session that ends with uncommitted changes either commits\n\
               them (preferred) or releases the claim with the state recorded via `mb touch` +\n\
               notes. A dirty tree with no live claim is a finding, not a to-do.\n\
             - **Claims serialize, pushes are separate authority.** Taking work is free and\n\
               atomic; opening PRs and pushing belongs to the delivery loop with the\n\
               host-held credential. An agent process never receives a push credential.\n\
             - **Release rather than hoard.** If you are done reading and not starting, `mb\n\
               release` beats holding. Expired-by-accident claims double the next agent's work.\n"
            .to_string(),
    };
    format!("{common}\n{hygiene}")
}

pub fn render(profile: Profile, version: &str) -> String {
    let hash = {
        let mut hasher = Sha256::new();
        hasher.update(body(profile).as_bytes());
        let digest = hasher.finalize();
        let mut out = String::new();
        for byte in &digest[..3] {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    };
    format!(
        "{BEGIN_PREFIX} integration v{version} profile:{} hash:{hash} -->\n\n{}\n{END}\n",
        profile.as_str(),
        body(profile)
    )
}

/// Apply the block to one file's content, idempotently. Any previously managed block — same or
/// older version — is replaced; hand-written text around it is untouched.
pub fn apply(existing: &str, profile: Profile, version: &str) -> String {
    let block = render(profile, version);
    let begin = existing.find(BEGIN_PREFIX);
    if let (Some(start), Some(end_rel)) = (begin, existing.find(END)) {
        let end = end_rel + END.len();
        // Include a trailing newline if the managed region had one.
        let end = if existing[end..].starts_with('\n') {
            end + 1
        } else {
            end
        };
        let mut out = String::new();
        out.push_str(&existing[..start]);
        out.push_str(&block);
        out.push_str(&existing[end..]);
        return out;
    }
    let mut out = existing.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&block);
    out
}

pub fn strip(existing: &str) -> Option<String> {
    let start = existing.find(BEGIN_PREFIX)?;
    let end = existing.find(END)? + END.len();
    let mut out = String::new();
    out.push_str(&existing[..start]);
    out.push_str(&existing[end..]);
    Some(out.trim_end().to_string() + "\n")
}

#[cfg(test)]
fn hash_of(block: &str) -> String {
    block
        .split("hash:")
        .nth(1)
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applying_twice_replaces_instead_of_stacking() {
        let once = apply("# My repo\n", Profile::Conservative, "0.1.0");
        let twice = apply(&once, Profile::Maintainer, "0.1.0");
        assert_eq!(twice.matches(BEGIN_PREFIX).count(), 1);
        assert!(twice.contains("profile:maintainer"));
        assert!(twice.starts_with("# My repo\n"));
    }

    #[test]
    fn the_hash_announces_the_language() {
        // Two profiles are two contracts; the begin-line must say which one a repo agreed to.
        assert_ne!(
            hash_of(&render(Profile::Maintainer, "0.1.0")),
            hash_of(&render(Profile::Conservative, "0.1.0"))
        );
    }

    #[test]
    fn strip_removes_only_the_managed_region() {
        let with = apply("# keep me\n\nnotes\n", Profile::Conservative, "0.1.0");
        let stripped = strip(&with).unwrap();
        assert!(stripped.starts_with("# keep me"));
        assert!(!stripped.contains("MARBLES"));
        assert!(strip("nothing here").is_none());
    }
}
