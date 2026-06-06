use coda_core::tool::{Tool, ToolError, ToolObject, ToolResult, ToolWrapper};
use coda_tools::{BuildContext, ToolSpec};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub struct AskUserTool {
    schema: Schema,
}

impl Default for AskUserTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AskUserTool {
    pub fn new() -> Self {
        AskUserTool {
            schema: schemars::schema_for!(AskUserParams),
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AskUserParams {
    /// The question to present to the user.
    pub question: String,
    /// The list of options for the user to choose from.
    pub options: Vec<String>,
}

impl Tool for AskUserTool {
    type Parameters = AskUserParams;
    type Output = String;

    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Present the user with a question and a numbered list of options to choose from. \
         Use this when you need a decision from the user before proceeding."
    }

    fn parameter_schema(&self) -> &Value {
        self.schema.as_value()
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        _params: Self::Parameters,
    ) -> impl Future<Output = ToolResult<String>> + Send + 'static {
        async {
            Err(ToolError::ExecutionError(
                "ask_user must be handled interactively".to_string(),
            ))
        }
    }
}

pub struct AskUserToolSpec;

impl ToolSpec for AskUserToolSpec {
    fn name(&self) -> &str {
        "ask_user"
    }
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(AskUserTool::new()))
    }
}
