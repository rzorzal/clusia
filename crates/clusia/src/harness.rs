//! The agent the commands talk about.

use clusia_core::PrRef;
use clusia_core::config::{Harness, HarnessKind};
use clusia_protocol::{Client, Command as Request, Reply};

/// The agent a review runs on: its kind, and the name every line printed about it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Active {
    pub kind: HarnessKind,
    pub name: String,
}

/// The agent of a review whose session was made by `session` (kind and the name it was stored
/// with), else the one `config` names, else the default kind. A session stored before names were
/// kept is named after its kind.
fn choose(session: Option<(HarnessKind, Option<String>)>, config: Option<Harness>) -> Active {
    if let Some((kind, name)) = session {
        let name = name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| kind.name().to_string());
        return Active { kind, name };
    }
    match config {
        Some(harness) => Active {
            kind: harness.kind,
            name: harness.display_name(),
        },
        None => Active {
            kind: HarnessKind::default(),
            name: HarnessKind::default().name().to_string(),
        },
    }
}

/// The agent `pr` runs on: the one its session started with, else the one Config names, else
/// the default kind when the daemon does not answer.
pub(crate) async fn active_of(client: &mut Client, pr: &PrRef) -> Active {
    let session = match client.request(Request::GetReview { pr: pr.clone() }).await {
        Ok(Reply::ReviewFile(review)) => review
            .harness_session_kind
            .map(|kind| (kind, review.harness_session_name.clone())),
        _ => None,
    };
    if session.is_some() {
        return choose(session, None);
    }
    let config = match client.request(Request::GetConfig).await {
        Ok(Reply::Config(config)) => Some(config.harness),
        _ => None,
    };
    choose(None, config)
}

/// The name of the agent `pr` runs on.
pub(crate) async fn name_of(client: &mut Client, pr: &PrRef) -> String {
    active_of(client, pr).await.name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(name: &str, program: &str) -> Harness {
        Harness {
            kind: HarnessKind::Custom,
            custom_name: name.to_string(),
            program: (!program.is_empty()).then(|| program.to_string()),
            ..Harness::default()
        }
    }

    #[test]
    fn a_session_names_the_agent_that_made_it() {
        let active = choose(
            Some((HarnessKind::Custom, Some("Echo".into()))),
            Some(Harness::default()),
        );
        assert_eq!(active.kind, HarnessKind::Custom);
        assert_eq!(active.name, "Echo");
    }

    #[test]
    fn an_old_session_has_the_name_of_its_kind() {
        assert_eq!(choose(Some((HarnessKind::Codex, None)), None).name, "Codex");
        assert_eq!(
            choose(Some((HarnessKind::Codex, Some("  ".into()))), None).name,
            "Codex"
        );
    }

    #[test]
    fn with_no_session_the_config_names_the_agent() {
        assert_eq!(
            choose(None, Some(custom("Echo", "/opt/bin/x"))).name,
            "Echo"
        );
        assert_eq!(
            choose(None, Some(custom("", "/opt/bin/echo-harness"))).name,
            "echo-harness"
        );
        assert_eq!(choose(None, Some(custom("", ""))).name, "Custom command");
        assert_eq!(choose(None, Some(Harness::default())).name, "Claude Code");
        let unknown = choose(None, None);
        assert_eq!(unknown.kind, HarnessKind::ClaudeCode);
        assert_eq!(unknown.name, "Claude Code");
    }
}
