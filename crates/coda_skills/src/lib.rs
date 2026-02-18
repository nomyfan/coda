mod error;
mod models;
mod parser;

pub use error::{Result, SkillError};
pub use models::{SkillProperties, SkillPropertiesWithLocation, Skills};
pub use parser::read_skills;
