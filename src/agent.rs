pub(crate) mod tools;

use crate::core::session::Session;
use crate::core::tool::ToolManager;

pub(crate) struct Agent {
    name: String,
    pub(crate) session: Session,
    pub(crate) tools: ToolManager,
}

impl Agent {
    pub(crate) fn new(name: String) -> Self {
        Agent {
            name,
            session: Session::new(),
            tools: ToolManager::new(),
        }
    }
}
