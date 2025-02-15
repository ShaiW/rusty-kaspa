use crate::{flow_context::FlowContext, flow_trait::Flow};
use kaspa_consensus_core::errors::consensus::ConsensusError;
use kaspa_core::debug;
use kaspa_hashes::Hash;
use kaspa_p2p_lib::{
    common::ProtocolError,
    dequeue, make_message,
    pb::{kaspad_message::Payload, BlockHeadersMessage, DoneHeadersMessage},
    IncomingRoute, Router,
};
use std::sync::Arc;

pub struct HandleAnticoneRequests {
    ctx: FlowContext,
    router: Arc<Router>,
    incoming_route: IncomingRoute,
}

#[async_trait::async_trait]
impl Flow for HandleAnticoneRequests {
    fn router(&self) -> Option<Arc<Router>> {
        Some(self.router.clone())
    }

    async fn start(&mut self) -> Result<(), ProtocolError> {
        self.start_impl().await
    }
}

impl HandleAnticoneRequests {
    pub fn new(ctx: FlowContext, router: Arc<Router>, incoming_route: IncomingRoute) -> Self {
        Self { ctx, router, incoming_route }
    }

    async fn start_impl(&mut self) -> Result<(), ProtocolError> {
        loop {
            let msg = dequeue!(self.incoming_route, Payload::RequestAnticone)?;
            let (block, context): (Hash, Hash) = msg.try_into()?;

            debug!("received anticone request with block hash: {}, context hash: {} for peer {}", block, context, self.router);

            let consensus = self.ctx.consensus();
            let session = consensus.session().await;

            // get_anticone_from_pov is expected to be called by the syncee for getting the anticone of the header selected tip
            // intersected by past of the relayed block, and is thus expected to be bounded by mergeset limit since
            // we relay blocks only if they enter virtual's mergeset. We add a 2 factor for possible sync gaps.
            let hashes = session.async_get_anticone_from_pov(block, context, Some(self.ctx.config.mergeset_size_limit * 2)).await?;
            let mut headers = session
                .spawn_blocking(|c| hashes.into_iter().map(|h| c.get_header(h)).collect::<Result<Vec<_>, ConsensusError>>())
                .await?;
            debug!("got {} headers in anticone({}) cap past({}) for peer {}", headers.len(), block, context, self.router);

            // Sort the headers in bottom-up topological order before sending
            headers.sort_by(|a, b| a.blue_work.cmp(&b.blue_work));

            self.router
                .enqueue(make_message!(
                    Payload::BlockHeaders,
                    BlockHeadersMessage { block_headers: headers.into_iter().map(|header| header.as_ref().into()).collect() }
                ))
                .await?;
            self.router.enqueue(make_message!(Payload::DoneHeaders, DoneHeadersMessage {})).await?;
        }
    }
}
