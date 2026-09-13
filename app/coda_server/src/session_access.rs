use serde::{Deserialize, Serialize};

use crate::storage::SessionModelBinding;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadOnlyReason {
    ModelNotConfigured,
    ModelFamilyChanged,
    RuntimeOpenFailed,
    BindingUnconfirmed,
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
    Available(SessionModelBinding),
    Unavailable(UnavailableModel),
}

impl SessionModelResolution {
    /// Classify a durable binding using the model's configured effort list, or None
    /// when that provider/model is absent. Unavailable values retain the original binding.
    pub fn resolve(
        binding: SessionModelBinding,
        efforts: Option<&[String]>,
        family: Option<&str>,
    ) -> Self {
        let reason = match efforts {
            None => ReadOnlyReason::ModelNotConfigured,
            Some(_)
                if binding
                    .family
                    .as_deref()
                    .is_some_and(|saved| Some(saved) != family) =>
            {
                ReadOnlyReason::ModelFamilyChanged
            }
            Some(efforts) => {
                if binding
                    .reasoning_effort
                    .as_ref()
                    .is_none_or(|effort| efforts.contains(effort))
                {
                    return Self::Available(binding);
                }
                ReadOnlyReason::ReasoningEffortNotSupported
            }
        };
        Self::Unavailable(UnavailableModel { binding, reason })
    }

    pub fn access(&self) -> SessionAccess {
        match self {
            Self::Available(_) => SessionAccess::ReadWrite,
            Self::Unavailable(model) => SessionAccess::ReadOnly {
                reason: model.reason,
            },
        }
    }
}

/// Fixed family applies even when only effort changes or a failed open is retried.
pub fn can_select_model(current: &SessionModelBinding, target: &SessionModelBinding) -> bool {
    match current.family.as_deref() {
        Some(family) => target.family.as_deref() == Some(family),
        None => current.provider_id == target.provider_id && current.model_id == target.model_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_family_precedes_same_model_effort_changes_and_retries() {
        let current = SessionModelBinding {
            provider_id: "p1".into(),
            model_id: "preview".into(),
            family: Some("f".into()),
            reasoning_effort: Some("high".into()),
        };
        for family in [None, Some("g")] {
            for effort in [Some("high"), Some("low")] {
                let target = SessionModelBinding {
                    family: family.map(str::to_owned),
                    reasoning_effort: effort.map(str::to_owned),
                    ..current.clone()
                };
                assert!(!can_select_model(&current, &target));
                assert!(matches!(
                    SessionModelResolution::resolve(
                        current.clone(),
                        Some(&["high".into(), "low".into()]),
                        family
                    ),
                    SessionModelResolution::Unavailable(UnavailableModel {
                        reason: ReadOnlyReason::ModelFamilyChanged,
                        ..
                    })
                ));
            }
        }
        let replacement = SessionModelBinding {
            provider_id: "p2".into(),
            model_id: "released".into(),
            ..current.clone()
        };
        assert!(can_select_model(&current, &replacement));
        let unbound = SessionModelBinding {
            family: None,
            ..current.clone()
        };
        assert!(can_select_model(
            &unbound,
            &SessionModelBinding {
                reasoning_effort: Some("low".into()),
                ..unbound.clone()
            }
        ));
        assert!(!can_select_model(&unbound, &replacement));
    }

    #[test]
    fn removed_models_and_efforts_keep_the_original_binding() {
        let binding = SessionModelBinding {
            family: None,
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
                SessionModelResolution::resolve(binding.clone(), efforts.as_deref(), None)
            else {
                panic!("unavailable")
            };
            assert_eq!(model.binding, binding);
            assert_eq!(model.reason, reason);
        }
    }

    #[test]
    fn valid_bindings_preserve_the_durable_effort() {
        for (saved, configured, expected) in [
            (Some("high"), vec!["low", "high"], Some("high")),
            (None, vec!["low", "high"], None),
            (None, vec![], None),
        ] {
            let binding = SessionModelBinding {
                family: None,
                provider_id: "p".into(),
                model_id: "m".into(),
                reasoning_effort: saved.map(String::from),
            };
            let efforts: Vec<String> = configured.into_iter().map(String::from).collect();
            let SessionModelResolution::Available(binding) =
                SessionModelResolution::resolve(binding, Some(&efforts), None)
            else {
                panic!("available")
            };
            assert_eq!(binding.selection_key(), "p:m");
            assert_eq!(binding.reasoning_effort.as_deref(), expected);
        }
    }
}
