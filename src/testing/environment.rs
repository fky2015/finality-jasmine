#![allow(dead_code)]
use core::pin::Pin;

use futures::channel::mpsc::{self, UnboundedSender};
use futures::{channel::mpsc::UnboundedReceiver, Sink};
use futures::{Future, Stream};
use parking_lot::Mutex;
use std::sync::Arc;
use tracing::trace;

use tracing_attributes::instrument;

use crate::environment::{Environment, RoundData, VoterData};
use crate::messages::{
    self, FinalizedCommit, GlobalMessageIn, GlobalMessageOut, Message, SignedMessage,
};
use crate::testing::network::VoterState;
use crate::{Error, VoterSet};

/// A implementation of `Environment`.
use super::{chain::DummyChain, network::Network, *};

pub struct DummyEnvironment {
    local_id: Id,
    network: Network,
    listeners: Mutex<Vec<UnboundedSender<(Hash, BlockNumber)>>>,
    chain: Mutex<DummyChain>,
    voters: Arc<Mutex<VoterSet<Id>>>,
}

impl DummyEnvironment {
    pub fn new(network: Network, local_id: Id, voters: Arc<Mutex<VoterSet<Id>>>) -> Self {
        DummyEnvironment {
            voters,
            network,
            local_id,
            chain: Mutex::new(DummyChain::new()),
            listeners: Mutex::new(Vec::new()),
        }
    }

    pub fn with_chain<F, U>(&self, f: F) -> U
    where
        F: FnOnce(&mut DummyChain) -> U,
    {
        let mut chain = self.chain.lock();
        f(&mut chain)
    }

    pub fn finalized_stream(&self) -> UnboundedReceiver<(Hash, BlockNumber)> {
        let (tx, rx) = mpsc::unbounded();
        self.listeners.lock().push(tx);
        rx
    }
}

impl Environment for DummyEnvironment {
    type Timer = Box<dyn Future<Output = Result<(), Error>> + Unpin + Send>;
    type Id = Id;
    type Signature = Signature;
    type BestChain = Box<
        dyn Future<
                Output = Result<
                    Option<(Self::Number, Self::Hash, (Self::Number, Self::Hash))>,
                    Error,
                >,
            > + Unpin
            + Send,
    >;
    type In = Box<
        dyn Stream<Item = Result<SignedMessage<BlockNumber, Hash, Signature, Id>, Error>>
            + Unpin
            + Send,
    >;
    type Out =
        Pin<Box<dyn Sink<Message<BlockNumber, Hash, Signature, Id>, Error = Error> + Sync + Send>>;
    type Error = Error;
    type Hash = Hash;
    type Number = BlockNumber;
    type GlobalIn = Box<
        dyn Stream<
                Item = Result<
                    GlobalMessageIn<Self::Hash, Self::Number, Self::Signature, Self::Id>,
                    Self::Error,
                >,
            > + Unpin
            + Send,
    >;
    type GlobalOut = Pin<
        Box<
            dyn Sink<
                    GlobalMessageOut<Self::Hash, Self::Number, Self::Signature, Self::Id>,
                    Error = Error,
                > + Send,
        >,
    >;

    fn init_voter(&self) -> VoterData<Self::Id> {
        let _globals = self.network.make_global_comms(self.local_id);
        VoterData {
            local_id: self.local_id,
        }
    }

    fn init_round(&self, round: u64) -> RoundData<Self::Id, Self::In, Self::Out> {
        tracing::trace!("{:?} round_data view: {}", self.local_id, round);

        let (incomming, outgoing) = self.network.make_round_comms(round, self.local_id);
        RoundData {
            local_id: self.local_id,
            incoming: Box::new(incomming),
            outgoing: Box::pin(outgoing),
        }
    }

    fn finalize_block(
        &self,
        view: u64,
        hash: Self::Hash,
        number: Self::Number,
        _f_commit: FinalizedCommit<Self::Number, Self::Hash, Self::Signature, Self::Id>,
    ) -> Result<(), Self::Error> {
        tracing::trace!(
            "voter {:?} finalize_block: {:?}",
            self.local_id,
            (number, hash.to_owned())
        );
        self.chain.lock().finalize_block(hash.to_owned());
        self.listeners
            .lock()
            .retain(|s| s.unbounded_send((hash.to_owned(), number)).is_ok());

        // Update Network RoutingRule's state.
        self.network.rule.lock().update_state(
            self.local_id,
            VoterState {
                view_number: view,
                last_finalized: number,
            },
        );

        Ok(())
    }

    fn propose(&self, _round: u64, _block: Self::Hash) -> Self::BestChain {
        Box::new(futures::future::ok(Some(
            self.with_chain(|chain| chain.next_to_be_voted().unwrap()),
        )))
    }

    #[instrument(skip(self), level = "trace")]
    fn gathered_a_qc(
        &self,
        _round: u64,
        _hash: Self::Hash,
        qc: crate::messages::QC<Self::Number, Self::Hash, Self::Signature, Self::Id>,
    ) {
        tracing::trace!("voter {:?} gathered_a_qc: {:?}", self.local_id, qc);
        let hash = qc.hash.to_owned();
        let qc = qc.into();
        self.with_chain(|chain| chain.save_qc(hash, qc)).unwrap();
    }

    fn parent_key_block(
        &self,
        block: Self::Hash,
    ) -> Option<(Self::Number, Self::Hash, (Self::Number, Self::Hash))> {
        self.with_chain(|chain| chain.parent_key_block(block))
    }

    fn get_block(
        &self,
        block: Self::Hash,
    ) -> Option<(Self::Number, Self::Hash, (Self::Number, Self::Hash))> {
        self.with_chain(|chain| chain.get_block(&block))
    }

    fn update_state(
        &self,
        round: u64,
        state: crate::voter::CurrentState<Self::Number, Self::Hash, Self::Signature, Self::Id>,
    ) {
    }
}
