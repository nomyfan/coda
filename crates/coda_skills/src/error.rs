use std::fmt;

#[derive(Debug)]
pub enum SkillError {
    Parse(String),
    Io(std::io::Error),
}

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkillError::Parse(msg) => write!(f, "{}", msg),
            SkillError::Io(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for SkillError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SkillError::Io(e) => Some(e),
            SkillError::Parse(_) => None,
        }
    }
}

impl From<std::io::Error> for SkillError {
    fn from(e: std::io::Error) -> Self {
        SkillError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, SkillError>;
