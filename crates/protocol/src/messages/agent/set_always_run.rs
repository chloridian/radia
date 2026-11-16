use uuid::Uuid;

use crate::{
    LLUdpError,
    lludp::{
        codec::{Reader, Writer},
        message::{AllocationStrategy, TypedMessage},
        types::MessageId,
    },
    schema::ids::SET_ALWAYS_RUN,
};

#[derive(Clone, Debug, PartialEq)]
pub struct SetAlwaysRun {
    pub agent_id: Uuid,
    pub session_id: Uuid,
    pub always_run: bool,
}

impl TypedMessage for SetAlwaysRun {
    const ID: MessageId = SET_ALWAYS_RUN;
    const ALLOCATION: AllocationStrategy = AllocationStrategy::NoAlloc;
    const INBOUND_ONLY: bool = false;

    fn decode(reader: &mut Reader<'_>) -> Result<Self, LLUdpError> {
        Ok(Self { agent_id: reader.uuid()?, session_id: reader.uuid()?, always_run: reader.u8()? != 0 })
    }

    fn encode(&self, writer: &mut Writer<'_>) -> Result<(), LLUdpError> {
        writer.uuid(self.agent_id);
        writer.uuid(self.session_id);
        writer.u8(u8::from(self.always_run));
        Ok(())
    }
}
