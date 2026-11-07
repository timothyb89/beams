use super::{Task, Template};
use crate::graph::Capability;

/// The pinned starship release (`steps::starship`, also a fixed step on
/// legacy Beams) as a template, so profiles choose it and its phase.
/// `builtin:development` runs it in pre, so the first shell's prompt is
/// there; it downloads while pre's apt batch runs.
pub struct InstallStarship;

impl Task for InstallStarship {
    fn id(&self) -> String {
        Self::NAME.to_string()
    }

    fn provides(&self) -> Vec<Capability> {
        vec!["starship-installed".into()]
    }

    fn run(&self) -> anyhow::Result<Option<String>> {
        crate::steps::install_starship()
    }
}

impl Template for InstallStarship {
    const NAME: &'static str = "install-starship";
}
