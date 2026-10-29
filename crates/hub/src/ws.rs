use ah_common::Message;
use uuid::Uuid;

#[derive(Clone, Default)]
pub struct Registry;

impl Registry {
    pub fn publish(&self, _channel_id: Uuid, _message: &Message) {}
}
