// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

use a2a::{A2AError, AgentCard, EXTENSION_URI_COLLABORATIVE_TASK, EXTENSION_URI_SHARED_TASK};

/// Verify that the card declares both required extension URIs.
///
/// Returns `Ok(())` when both `EXTENSION_URI_SHARED_TASK` and
/// `EXTENSION_URI_COLLABORATIVE_TASK` are present in `capabilities.extensions`.
/// Returns `A2AError::extension_support_required` naming the first missing URI.
pub fn require_collaborative_extensions(card: &AgentCard) -> Result<(), A2AError> {
    let extensions = card.capabilities.extensions.as_deref().unwrap_or_default();

    for required in [EXTENSION_URI_SHARED_TASK, EXTENSION_URI_COLLABORATIVE_TASK] {
        if !extensions.iter().any(|e| e.uri == required) {
            return Err(A2AError::extension_support_required(required));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{AgentCapabilities, AgentExtension};

    fn card_with(uris: &[&str]) -> AgentCard {
        let extensions = if uris.is_empty() {
            None
        } else {
            Some(
                uris.iter()
                    .map(|u| AgentExtension {
                        uri: u.to_string(),
                        description: None,
                        required: None,
                        params: None,
                    })
                    .collect(),
            )
        };
        AgentCard {
            name: "test".into(),
            description: "test".into(),
            version: "0.1.0".into(),
            provider: None,
            capabilities: AgentCapabilities {
                streaming: Some(true),
                push_notifications: None,
                extensions,
                extended_agent_card: None,
            },
            skills: vec![],
            default_input_modes: vec![],
            default_output_modes: vec![],
            supported_interfaces: vec![],
            security_schemes: None,
            security_requirements: None,
            documentation_url: None,
            icon_url: None,
            signatures: None,
        }
    }

    #[test]
    fn passes_when_both_uris_present() {
        let card = card_with(&[EXTENSION_URI_SHARED_TASK, EXTENSION_URI_COLLABORATIVE_TASK]);
        assert!(require_collaborative_extensions(&card).is_ok());
    }

    #[test]
    fn fails_when_no_extensions() {
        let card = card_with(&[]);
        let err = require_collaborative_extensions(&card).unwrap_err();
        assert!(err.message.contains(EXTENSION_URI_SHARED_TASK));
    }

    #[test]
    fn fails_when_missing_shared_task() {
        let card = card_with(&[EXTENSION_URI_COLLABORATIVE_TASK]);
        let err = require_collaborative_extensions(&card).unwrap_err();
        assert!(err.message.contains(EXTENSION_URI_SHARED_TASK));
    }

    #[test]
    fn fails_when_missing_collaborative_task() {
        let card = card_with(&[EXTENSION_URI_SHARED_TASK]);
        let err = require_collaborative_extensions(&card).unwrap_err();
        assert!(err.message.contains(EXTENSION_URI_COLLABORATIVE_TASK));
    }

    #[test]
    fn passes_with_extra_extensions() {
        let card = card_with(&[
            "https://example.org/ext",
            EXTENSION_URI_SHARED_TASK,
            EXTENSION_URI_COLLABORATIVE_TASK,
        ]);
        assert!(require_collaborative_extensions(&card).is_ok());
    }
}
