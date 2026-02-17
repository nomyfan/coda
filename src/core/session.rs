use crate::core::llm::Message;

pub(crate) struct Session {
    messages: Vec<Message>,
}

impl Session {
    pub(crate) fn new() -> Self {
        Session { messages: vec![] }
    }

    pub(crate) fn add_message(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub(crate) fn messages(&self) -> &[Message] {
        &self.messages
    }
}
