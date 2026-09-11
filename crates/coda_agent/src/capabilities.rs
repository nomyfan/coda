use std::collections::BTreeSet;

use serde::Deserialize;

/// Runtime capabilities available independently of an agent's ordinary tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Background,
    Ptc,
}

/// A resolved capability selection. Default enables all supported capabilities;
/// deserializing an explicit list selects exactly those entries, including none.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Capabilities(BTreeSet<Capability>);

impl Capabilities {
    pub fn all() -> Self {
        [Capability::Background, Capability::Ptc]
            .into_iter()
            .collect()
    }

    pub fn none() -> Self {
        Self(BTreeSet::new())
    }

    pub fn contains(&self, capability: Capability) -> bool {
        self.0.contains(&capability)
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::all()
    }
}

impl FromIterator<Capability> for Capabilities {
    fn from_iter<T: IntoIterator<Item = Capability>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_lists_replace_the_default_and_deduplicate() {
        assert_eq!(Capabilities::default(), Capabilities::all());
        assert_eq!(
            serde_json::from_str::<Capabilities>("[]").unwrap(),
            Capabilities::none()
        );
        let ptc = serde_json::from_str::<Capabilities>(r#"["ptc", "ptc"]"#).unwrap();
        assert!(ptc.contains(Capability::Ptc));
        assert!(!ptc.contains(Capability::Background));
    }

    #[test]
    fn rejects_unknown_names_and_non_lists() {
        for input in [r#"["unknown"]"#, r#"["PTC"]"#, "null", "{}", r#""ptc""#] {
            assert!(
                serde_json::from_str::<Capabilities>(input).is_err(),
                "{input}"
            );
        }
    }
}
