use serde::{Deserialize, Serialize};

use crate::storage::SessionModelBinding;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadOnlyReason {
    ModelNotConfigured,
    ReasoningEffortNotSupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionAccess {
    ReadWrite,
    ReadOnly { reason: ReadOnlyReason },
}

#[derive(Debug, Clone, Serialize)]
pub struct UnavailableModel {
    pub binding: SessionModelBinding,
    pub reason: ReadOnlyReason,
}

pub enum SessionModelResolution {
    Available {
        provider_id: String,
        reasoning_effort: Option<String>,
    },
    Unavailable(UnavailableModel),
}

impl SessionModelResolution {
    /// Classify a durable binding using the model's configured effort list, or None
    /// when that provider/model is absent. Unavailable values retain the original binding.
    pub fn resolve(binding: SessionModelBinding, efforts: Option<&[String]>) -> Self {
        let reason = match efforts {
            None => ReadOnlyReason::ModelNotConfigured,
            Some(efforts) => {
                if binding
                    .reasoning_effort
                    .as_ref()
                    .is_none_or(|effort| efforts.contains(effort))
                {
                    return Self::Available {
                        provider_id: binding.selection_key(),
                        reasoning_effort: binding
                            .reasoning_effort
                            .or_else(|| efforts.first().cloned()),
                    };
                }
                ReadOnlyReason::ReasoningEffortNotSupported
            }
        };
        Self::Unavailable(UnavailableModel { binding, reason })
    }

    pub fn access(&self) -> SessionAccess {
        match self {
            Self::Available { .. } => SessionAccess::ReadWrite,
            Self::Unavailable(model) => SessionAccess::ReadOnly {
                reason: model.reason,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_models_and_efforts_keep_the_original_binding() {
        let binding = SessionModelBinding {
            provider_id: "deepseek".into(),
            model_id: "removed".into(),
            reasoning_effort: Some("high".into()),
        };
        for (efforts, reason) in [
            (None, ReadOnlyReason::ModelNotConfigured),
            (
                Some(vec!["low".into()]),
                ReadOnlyReason::ReasoningEffortNotSupported,
            ),
            (Some(vec![]), ReadOnlyReason::ReasoningEffortNotSupported),
        ] {
            let SessionModelResolution::Unavailable(model) =
                SessionModelResolution::resolve(binding.clone(), efforts.as_deref())
            else {
                panic!("unavailable")
            };
            assert_eq!(model.binding, binding);
            assert_eq!(model.reason, reason);
        }
    }

    #[test]
    fn valid_bindings_keep_existing_effort_normalization() {
        for (saved, configured, expected) in [
            (Some("high"), vec!["low", "high"], Some("high")),
            (None, vec!["low", "high"], Some("low")),
            (None, vec![], None),
        ] {
            let binding = SessionModelBinding {
                provider_id: "p".into(),
                model_id: "m".into(),
                reasoning_effort: saved.map(String::from),
            };
            let efforts: Vec<String> = configured.into_iter().map(String::from).collect();
            let SessionModelResolution::Available {
                provider_id,
                reasoning_effort,
            } = SessionModelResolution::resolve(binding, Some(&efforts))
            else {
                panic!("available")
            };
            assert_eq!(provider_id, "p:m");
            assert_eq!(reasoning_effort.as_deref(), expected);
        }
    }
}
