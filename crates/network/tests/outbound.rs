use network::OutboundMessage;
use protocol::messages::{
    AgentUpdate, ChatFromViewer, MapBlockRequest, RegionHandleRequest, RequestMultipleObjects, SetAlwaysRun, TeleportCancel,
};

#[test]
fn messages_implement_send_contract() {
    fn outbound<T: OutboundMessage>() {}

    outbound::<AgentUpdate>();
    outbound::<SetAlwaysRun>();
    outbound::<ChatFromViewer>();
    outbound::<MapBlockRequest>();
    outbound::<RegionHandleRequest>();
    outbound::<RequestMultipleObjects>();
    outbound::<TeleportCancel>();
}
