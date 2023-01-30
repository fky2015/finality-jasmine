use core::{
    task::{Poll, Waker},
    time::Duration,
};

use crate::{messages::QC, std::sync::Arc};
use tracing_attributes::instrument;

use futures::{FutureExt, SinkExt, StreamExt};
use parking_lot::Mutex;
use tracing::{info, trace, warn, Value};

use crate::{
    environment::{Environment, RoundData, VoterData},
    messages::{FinalizedCommit, Message, Propose, SignedMessage, Vote},
    VoterSet,
};

use self::report::VoterStateT;

#[derive(PartialEq, Eq, Clone, Debug)]
pub enum CurrentState<N, D, Sig, Id> {
    Voter,
    Leader,
    LeaderWithQC(QC<N, D, Sig, Id>),
}

impl<N, D, Sig, Id> CurrentState<N, D, Sig, Id> {
    pub fn new() -> Self {
        CurrentState::Voter
    }
}

pub mod report {
    use super::CurrentState;

    /// Trait for querying the state of the voter. Used by `Voter` to return a queryable object
    /// without exposing too many data types.
    pub trait VoterStateT<N, D, Sig, Id: Eq + std::hash::Hash> {
        /// Returns a plain data type, `report::VoterState`, describing the current state
        /// of the voter relevant to the voting process.
        fn get(&self) -> VoterState<N, D, Sig, Id>;
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct RoundState<N, D, Sig, Id: Eq + std::hash::Hash> {
        pub state: CurrentState<N, D, Sig, Id>,
        pub total_voters: usize,
        pub threshold: usize,
        pub proposal_hash: Option<D>,
    }

    #[derive(Clone, Debug)]
    pub struct VoterState<N, D, Sig, Id: Eq + std::hash::Hash> {
        // Voting rounds running in the background.
        // pub background_views: HashMap<u64, ViewState<Hash, Id>>,
        /// The current best voting view.
        pub best_round: (u64, RoundState<N, D, Sig, Id>),
    }
}

pub struct Voter<E: Environment> {
    id: E::Id,
    env: Arc<E>,
    global: Arc<Mutex<GlobalState<E>>>,
    global_in: E::GlobalIn,
    global_out: E::GlobalOut,
    best: Arc<Mutex<InnerVoterState<E>>>,
}

impl<E: Environment> Voter<E>
where
    E::Id: Value,
{
    pub fn new(
        env: Arc<E>,
        global_in: E::GlobalIn,
        global_out: E::GlobalOut,
        voters: VoterSet<E::Id>,
        finalized_target: (E::Number, E::Hash),
    ) -> Self {
        let VoterData { local_id } = env.init_voter();
        let global = Arc::new(Mutex::new(GlobalState::new(
            local_id.clone(),
            voters,
            QC::from_target(finalized_target),
        )));
        Voter {
            id: local_id.clone(),
            env,
            global_in,
            global_out,
            best: Arc::new(Mutex::new(InnerVoterState::new(local_id, global.clone()))),
            global,
        }
    }

    pub async fn start(&mut self) {
        loop {
            let round = self.global.lock().round;
            info!(id = self.id, "start voter in round {round}.");

            let voting_round = Round::new(self.env.clone(), round, self.global.clone());

            let round_state = voting_round.round_state.clone();
            let incoming = async move {
                let mut incoming = round_state.lock().incoming.take().unwrap();
                while let Some(Ok(signed_msg)) = incoming.next().await {
                    round_state.lock().process_incoming(signed_msg);
                }
            };

            tokio::select! {
                _ = incoming => {},
                res = voting_round.run() => {
                    match res {
                        Ok(f_commit) => {
                            // Send commit to global_out;
                         if let Some(f_commit) = f_commit {
                            self.env.finalize_block(
                                round,
                                f_commit.target_hash.clone(),
                                f_commit.target_number.to_owned(),
                                f_commit,
                            ).unwrap();
                            }
                        }
                        Err(e) => {
                        warn!(id = self.id, "error: {:?}", e)
                        }
                    }
                },
            }

            // Sleep for a while
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    }
}

pub struct Round<E: Environment> {
    local_id: E::Id,
    env: Arc<E>,
    outgoing: E::Out,
    round_state: Arc<Mutex<RoundState<E>>>,
}

impl<E: Environment> Round<E>
where
    E::Id: Value,
{
    fn new(env: Arc<E>, round: u64, global: Arc<Mutex<GlobalState<E>>>) -> Self {
        let RoundData {
            local_id,
            incoming,
            outgoing,
            ..
        } = env.init_round(round);
        let proposer = global.lock().voters.get_proposer(round);
        let round_state = Arc::new(Mutex::new(RoundState::new(incoming, proposer, global)));
        Round {
            env,
            outgoing,
            local_id,
            round_state,
        }
    }

    async fn new_propose(&self) -> Propose<E::Number, E::Hash, E::Signature, E::Id> {
        let global = self.round_state.lock().global.clone();
        let round = global.lock().round;

        // get the hash of generic_qc
        let hash = global.lock().generic_qc.hash.clone();

        let (number, hash, (qc_height, qc_hash)) = self
            .env
            .propose(round, hash)
            .await
            .expect("must success")
            .expect("must have target.");
        Propose {
            round,
            target_hash: hash,
            target_height: number,
            qc: QC::from_target((qc_height, qc_hash)),
        }
    }

    #[instrument(level = "trace", skip(self))]
    fn is_extend_relationship(
        &self,
        ancestor: &(E::Number, E::Hash),
        descendant: &E::Hash,
    ) -> bool {
        let mut parent = self.env.parent_key_block(descendant.clone()).unwrap();
        while parent.0 >= ancestor.0 {
            if parent.1 == ancestor.1 {
                return true;
            }
            let new_parent = self.env.parent_key_block(parent.1.clone()).unwrap();
            trace!("parent: {:?}, new_parent: {:?}", parent, new_parent);
            // Genesis block has no parent.
            if new_parent.0 == parent.0 {
                return false;
            }
            parent = new_parent;
        }
        false
    }

    #[instrument(level = "trace", skip(self))]
    fn is_parent_relationship(&self, ancestor: &E::Hash, descendant: &E::Hash) -> bool {
        let parent = self.env.parent_key_block(descendant.clone()).unwrap();
        trace!("parent: {:?}, ancestor: {:?}", parent, ancestor);
        parent.1 == ancestor.clone()
    }

    fn get_parent_block(
        &self,
        hash: &E::Hash,
    ) -> Option<Propose<E::Number, E::Hash, E::Signature, E::Id>> {
        let parent = self.env.parent_key_block(hash.clone()).unwrap();
        let parent_hash = parent.1;
        let parent_number = parent.0;
        let qc = QC::from_target(parent.2);
        Some(Propose {
            // mock round
            round: 0,
            target_hash: parent_hash,
            target_height: parent_number,
            qc,
        })
    }

    fn get_block(
        &self,
        hash: &E::Hash,
    ) -> Option<Propose<E::Number, E::Hash, E::Signature, E::Id>> {
        let block = self.env.get_block(hash.clone()).unwrap();
        let qc = QC::from_target(block.2);

        Some(Propose {
            round: 0,
            target_hash: block.1,
            target_height: block.0,
            qc,
        })
    }

    async fn process_proposal(
        &mut self,
        propose: Propose<E::Number, E::Hash, E::Signature, E::Id>,
    ) -> Option<FinalizedCommit<E::Number, E::Hash, E::Signature, E::Id>> {
        let global = self.round_state.lock().global.clone();
        let qc = propose.qc;
        let locked_qc = global.lock().locked_qc.clone();

        // 2. update the generic_qc if needed.
        if qc.height > global.lock().generic_qc.height {
            global.lock().generic_qc = qc.clone();
        }

        self.env
            .gathered_a_qc(propose.round, propose.target_hash, qc.to_owned());

        // 3. update the lock_qc if needed.
        let block_x = self.get_block(&qc.hash).unwrap();
        let larger_than_lock_qc = block_x.qc.height > locked_qc.height;
        if larger_than_lock_qc {
            global.lock().locked_qc = block_x.qc.clone();
        }

        // 4. finalize block if needed.
        let block_y = self.get_block(&block_x.qc.hash).unwrap();
        let block_z = self.get_block(&block_y.qc.hash).unwrap();
        trace!(
            "block_x: {:?} -> {:?}, block_y: {:?} -> {:?}, block_z: {:?} -> {:?}",
            block_x.target_height,
            block_x.qc.height,
            block_y.target_height,
            block_y.qc.height,
            block_z.target_height,
            block_z.qc.height,
        );
        if block_z.target_height <= global.lock().finalized_height {
            trace!("Skip already finalized block: {:?}", block_z);
            return None;
        }
        let is_parent_x_y = self.is_parent_relationship(&block_y.target_hash, &block_x.target_hash);
        let is_parent_y_z = self.is_parent_relationship(&block_z.target_hash, &block_y.target_hash);

        if is_parent_x_y && is_parent_y_z {
            trace!(
                "Finalized block: {:?}, block_x: {:?}, block_y: {:?}",
                block_z,
                block_x,
                block_y
            );
            global.lock().finalized_height = block_z.target_height;
            Some(FinalizedCommit {
                target_hash: block_z.target_hash,
                target_number: block_z.target_height,
                qcs: [block_y.qc, block_x.qc, qc],
            })
        } else {
            trace!(
                "Skip block_x: {:?}, block_y: {:?}, block_z: {:?}, is_parent_x_y: {}, is_parent_y_z: {}",
                block_x,
                block_y,
                block_z,
                is_parent_x_y,
                is_parent_y_z
            );
            None
        }
    }

    fn next_leader_round(&self, mut round: u64) -> u64 {
        let voters = self.round_state.lock().global.lock().voters.clone();
        let cur_leader = voters.get_proposer(round);
        while voters.get_proposer(round) == cur_leader {
            round += 1
        }
        trace!("next_leader_round: {}", round);
        round
    }

    async fn new_vote(
        &self,
        propose: Propose<E::Number, E::Hash, E::Signature, E::Id>,
    ) -> Vote<E::Number, E::Hash> {
        Vote {
            target_hash: propose.target_hash,
            target_height: propose.target_height,
            round: propose.round,
        }
    }

    async fn save_latest_vote(&self, vote: Vote<E::Number, E::Hash>) {
        self.round_state.lock().global.lock().latest_vote = Some(vote);
    }

    async fn get_latest_vote(&self) -> Option<Vote<E::Number, E::Hash>> {
        self.round_state.lock().global.lock().latest_vote.clone()
    }

    #[instrument(level = "trace", skip(self))]
    async fn try_generate_vote(
        &self,
        propose: Propose<E::Number, E::Hash, E::Signature, E::Id>,
    ) -> Option<Vote<E::Number, E::Hash>> {
        // Check safety and liveness rule.
        let global = self.round_state.lock().global.clone();
        let qc = propose.qc.clone();
        let locked_qc = global.lock().locked_qc.clone();

        let safety_rule =
            self.is_extend_relationship(&(locked_qc.height, locked_qc.hash), &qc.hash);
        let liveness_rule = qc.height >= locked_qc.height;

        trace!(
            "safety_rule: {}, liveness_rule: {}",
            safety_rule,
            liveness_rule
        );

        if !safety_rule && !liveness_rule {
            return None;
        }

        let vote = self.new_vote(propose).await;

        Some(vote)
    }

    async fn run(
        mut self,
    ) -> Result<Option<FinalizedCommit<E::Number, E::Hash, E::Signature, E::Id>>, ()> {
        let global = self.round_state.lock().global.clone();

        let round = global.lock().round;

        let is_proposer = self.round_state.lock().is_proposer();

        if is_proposer {
            info!(id = self.local_id, "I am the proposer of round {}", round);
            // If we are the proposer, propose a block.
            let propose = self.new_propose().await;
            let msg = Message::Propose(propose);

            // A leader can only enter his round after collecting enough votes to form a QC.
            // So we can assume that the QC is available, and we can propose new block
            // immediately.
            self.outgoing.send(msg).await.unwrap();
        } else {
        }

        // Wait for a propose.

        let timeout = tokio::time::sleep(Duration::from_millis(1000));
        tokio::pin!(timeout);
        let fu = futures::future::poll_fn(|cx| {
            let mut round_lock = self.round_state.lock();
            let proposal = &round_lock.proposal;
            if let Some(proposal) = &proposal {
                Poll::Ready(Ok(proposal.clone()))
            } else {
                round_lock.waker = Some(cx.waker().clone());
                timeout.poll_unpin(cx).map(|_| Err(()))
            }
        });

        let ret = if let Ok(proposal) = fu.await {
            info!(id = self.local_id, "Got a proposal {:?}", proposal);
            let is_next_proposer = self.round_state.lock().is_next_proposer();
            // Update current state if we are the next leader.
            if is_next_proposer {
                global.lock().current_state = CurrentState::Leader;
            } else {
                global.lock().current_state = CurrentState::Voter;
            }

            // TODO: save the qc

            // If we have a proposal, send a vote.
            let vote = self.try_generate_vote(proposal.clone()).await;

            trace!("generate a vote {:?}", vote);
            if let Some(vote) = vote {
                let msg = Message::Vote(vote);
                self.outgoing.send(msg).await.unwrap();
            }

            self.process_proposal(proposal).await
        } else {
            warn!(target: "afj", "No proposal");
            let is_next_proposer = self.round_state.lock().is_next_proposer();
            if is_next_proposer {
                global.lock().current_state = CurrentState::Leader;
            } else {
                global.lock().current_state = CurrentState::Voter;
            }

            global.lock().round = self.next_leader_round(round);
            return Err(());
        };

        // 2. If we are next proposer, wait for enough votes
        let next_proposer = self.round_state.lock().is_next_proposer();
        trace!(id = self.local_id, "next_proposer: {}", next_proposer);
        if next_proposer {
            // Wait for enough votes.
            let timeout = tokio::time::sleep(Duration::from_millis(1000));
            tokio::pin!(timeout);
            let fu = futures::future::poll_fn(|cx| {
                let mut round_lock = self.round_state.lock();
                let qc = &round_lock.qc();
                if let Some(qc) = &qc {
                    Poll::Ready(Ok(qc.clone()))
                } else {
                    round_lock.waker = Some(cx.waker().clone());
                    timeout.poll_unpin(cx).map(|_| Err(()))
                }
            });

            if let Ok(qc) = fu.await {
                info!(id = self.local_id, "Got a QC {:?}", qc);
                // Now update qc in global state.
                global.lock().current_state = CurrentState::LeaderWithQC(qc.to_owned());
                let hash = self
                    .round_state
                    .lock()
                    .proposal
                    .clone()
                    .unwrap()
                    .target_hash;
                self.env.gathered_a_qc(round, hash, qc);
            } else {
                warn!(target: "afj",id = self.local_id,  "No QC");
                global.lock().round += 1;
                return Err(());
            }
        }

        // round add 1
        global.lock().round += 1;
        Ok(ret)
    }
}

pub struct GlobalState<E: Environment> {
    local_id: E::Id,
    height: E::Number,
    round: u64,
    voters: VoterSet<E::Id>,
    current_state: CurrentState<E::Number, E::Hash, E::Signature, E::Id>,
    generic_qc: QC<E::Number, E::Hash, E::Signature, E::Id>,
    locked_qc: QC<E::Number, E::Hash, E::Signature, E::Id>,
    finalized_height: E::Number,
    latest_vote: Option<Vote<E::Number, E::Hash>>,
}

impl<E: Environment> GlobalState<E> {
    pub fn new(
        local_id: E::Id,
        voters: VoterSet<E::Id>,
        generic_qc: QC<E::Number, E::Hash, E::Signature, E::Id>,
    ) -> Self {
        // When we are the leader, we should init with CurrentState::LeaderWithQC
        let current_state = if voters.get_proposer(num::zero()) == local_id {
            CurrentState::LeaderWithQC(generic_qc.clone())
        } else {
            CurrentState::new()
        };
        GlobalState {
            local_id,
            height: num::zero(),
            round: num::zero(),
            voters,
            current_state,
            locked_qc: generic_qc.clone(),
            generic_qc,
            // Genesis.
            finalized_height: num::zero(),
            latest_vote: None,
        }
    }

    // pub fn set_finalized_target(&mut self, target: (E::Number, E::Hash)) {
    //     self.decision.insert(target.0, target.1);
    //     self.height = target.0;
    // }
}

pub struct RoundState<E: Environment> {
    global: Arc<Mutex<GlobalState<E>>>,
    proposer: E::Id,
    proposal: Option<Propose<E::Number, E::Hash, E::Signature, E::Id>>,
    incoming: Option<E::In>,
    votes: Vec<(Vote<E::Number, E::Hash>, E::Id, E::Signature)>,
    waker: Option<Waker>,
}

impl<E: Environment> RoundState<E> {
    fn new(incoming: E::In, proposer: E::Id, global: Arc<Mutex<GlobalState<E>>>) -> Self {
        RoundState {
            incoming: Some(incoming),
            proposal: None,
            proposer,
            global,
            waker: None,
            votes: Vec::new(),
        }
    }

    fn is_proposer(&self) -> bool {
        self.proposer == self.global.lock().local_id
    }

    fn is_next_proposer(&self) -> bool {
        let voters = self.global.lock().voters.clone();
        let next_round = self.global.lock().round + 1;
        let next_proposer = voters.get_proposer(next_round);
        next_proposer == self.global.lock().local_id
    }

    fn process_incoming(
        &mut self,
        signed_msg: SignedMessage<
            <E as Environment>::Number,
            <E as Environment>::Hash,
            <E as Environment>::Signature,
            <E as Environment>::Id,
        >,
    ) {
        trace!("Processing incoming message {:?}", signed_msg);
        let SignedMessage {
            id,
            message: msg,
            signature,
        } = signed_msg;
        match msg {
            Message::Propose(proposal) => {
                if self.proposer == id {
                    self.proposal = Some(proposal.clone());
                }

                // If there are votes received before the proposal, filter them.
                self.votes.retain(|(v, _, _)| {
                    v.round == proposal.round
                        && v.target_hash == proposal.target_hash
                        && v.target_height == proposal.target_height
                });
            }
            Message::Vote(vote) => {
                if let Some(proposal) = &self.proposal {
                    if !(vote.round == proposal.round
                        && vote.target_hash == proposal.target_hash
                        && vote.target_height == proposal.target_height)
                    {
                        return;
                    }
                }
                self.votes.push((vote, id, signature));
            }
        }
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    // Return the QC for the current round if it exists.
    pub(crate) fn qc(&self) -> Option<QC<E::Number, E::Hash, E::Signature, E::Id>> {
        let threshold = self.global.lock().voters.threshold();

        // We must wait for proposal to be received.
        if let Some(proposal) = &self.proposal {
            // Check if the QC is valid.
            if self.votes.len() >= threshold {
                let hash = proposal.target_hash.clone();
                let height = proposal.target_height;

                let sigs = self
                    .votes
                    .iter()
                    .map(|(_, id, s)| (s.clone(), id.clone()))
                    .collect();

                Some(QC {
                    height,
                    hash,
                    signatures: sigs,
                })
            } else {
                None
            }
        } else {
            None
        }
    }
}

impl<'a, E> Voter<E>
where
    E: Environment + Sync + Send + 'a,
{
    /// Returns an object allowing to query the voter state.
    pub fn voter_state(
        &self,
    ) -> Box<dyn VoterStateT<E::Number, E::Hash, E::Signature, E::Id> + Send + Sync + 'a>
    where
        <E as Environment>::Signature: Send,
        <E as Environment>::Id: std::hash::Hash + Send,
        <E as Environment>::Timer: Send,
        <E as Environment>::Out: Send,
        <E as Environment>::In: Send,
        <E as Environment>::Number: Send,
        <E as Environment>::Hash: Send,
    {
        Box::new(SharedVoterState(self.best.clone()))
    }
}

/// The inner state of a voter aggregating the currently running round state
/// (i.e. best and background rounds). This state exists separately since it's
/// useful to wrap in a `Arc<Mutex<_>>` for sharing.
#[derive(Clone)]
pub struct InnerVoterState<E>
where
    E: Environment,
{
    best: Arc<Mutex<RoundState<E>>>,
}

impl<E> InnerVoterState<E>
where
    E: Environment,
{
    /// Mock at start.
    pub fn new(proposer: E::Id, global: Arc<Mutex<GlobalState<E>>>) -> Self {
        InnerVoterState {
            best: Arc::new(Mutex::new(RoundState {
                votes: Vec::new(),
                waker: None,
                incoming: None,
                global,
                proposal: None,
                proposer,
            })),
        }
    }
}

struct SharedVoterState<E>(Arc<Mutex<InnerVoterState<E>>>)
where
    E: Environment;

impl<E: Environment> VoterStateT<E::Number, E::Hash, E::Signature, E::Id> for SharedVoterState<E> {
    fn get(&self) -> report::VoterState<E::Number, E::Hash, E::Signature, E::Id> {
        let round = self.0.lock();
        let round_state = round.best.lock();
        let round = round_state.global.lock().round;
        let current_state = round_state.global.lock().current_state.clone();
        let voters = round_state.global.lock().voters.clone();
        report::VoterState {
            best_round: (
                round,
                report::RoundState {
                    state: current_state,
                    total_voters: voters.len().get(),
                    threshold: voters.threshold,
                    proposal_hash: round_state.proposal.as_ref().map(|p| p.target_hash.clone()),
                },
            ),
        }
    }
}

#[cfg(test)]
mod test {
    use futures::StreamExt;
    #[cfg(feature = "deadlock_detection")]
    use parking_lot::deadlock;

    use std::sync::Arc;

    use crate::testing::{environment::DummyEnvironment, network::make_network};

    use super::*;

    #[cfg(deadlock_detection)]
    async fn deadlock_detection() {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let deadlocks = deadlock::check_deadlock();
            if deadlocks.is_empty() {
                trace!("No deadlocks detected");
                continue;
            }

            error!("{} deadlocks detected", deadlocks.len());
            for (i, threads) in deadlocks.iter().enumerate() {
                error!("Deadlock #{}", i);
                for t in threads {
                    error!("Thread Id {:#?}", t.thread_id());
                    error!("{:#?}", t.backtrace());
                }
            }
        }
    }

    use std::sync::Once;
    static INIT: Once = Once::new();
    fn init() {
        INIT.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .finish();

            tracing::subscriber::set_global_default(subscriber)
                .map_err(|_err| eprintln!("Unable to set global default subscriber"));

            #[cfg(feature = "deadlock_detection")]
            {
                #[cfg(test)]
                info!("deadlock_detection is enabled");
                tokio::spawn(deadlock_detection());
            }
        });
    }

    #[tokio::test]
    async fn basic_test() {
        // init();

        let local_id = 5;
        let voter_set = Arc::new(Mutex::new(VoterSet::new(vec![5]).unwrap()));

        let (network, routing_network) = make_network();

        let env = Arc::new(DummyEnvironment::new(
            network.to_owned(),
            local_id,
            voter_set.to_owned(),
        ));

        // init chain
        let last_finalized = env.with_chain(|chain| {
            chain.generate_init_blocks(40);
            log::trace!(
                "chain: {:?}, last_finalized: {:?}, next_to_be_finalized: {:?}",
                chain,
                chain.last_finalized(),
                chain.next_to_be_finalized()
            );
            chain.last_finalized()
        });

        let (global_incoming, global_outgoing) = network.make_global_comms(local_id);
        let mut voter = Voter::new(
            env.clone(),
            Box::new(global_incoming),
            Box::pin(global_outgoing),
            voter_set.lock().clone(),
            last_finalized,
        );

        tokio::spawn(routing_network);

        tokio::spawn(async move {
            voter.start().await;
        });

        // run voter in background. scheduling it to shut down at the end.
        let finalized = env.finalized_stream();

        // wait for the best block to finalized.
        finalized
            .take_while(|&(_, n)| {
                assert!((n - 1) % 3 == 0);
                println!("n: {n}");
                futures::future::ready(n < 20)
            })
            .for_each(|v| {
                println!("v: {v:?}");
                futures::future::ready(())
            })
            .await
    }

    #[tokio::test]
    async fn consensus_test() {
        // init();
        let voters_num = 4;

        let voter_set = Arc::new(Mutex::new(
            VoterSet::new((0..voters_num).collect()).unwrap(),
        ));

        let (network, routing_network) = make_network();

        let finalized_stream = (0..voters_num)
            .map(|local_id| {
                let env = Arc::new(DummyEnvironment::new(
                    network.clone(),
                    local_id,
                    voter_set.clone(),
                ));

                // init chain
                let last_finalized = env.with_chain(|chain| {
                    chain.generate_init_blocks(40);
                    log::trace!(
                        "chain: {:?}, last_finalized: {:?}, next_to_be_finalized: {:?}",
                        chain,
                        chain.last_finalized(),
                        chain.next_to_be_finalized()
                    );
                    chain.last_finalized()
                });

                let (global_incoming, global_outgoing) = network.make_global_comms(local_id);

                let mut voter = Voter::new(
                    env.clone(),
                    Box::new(global_incoming),
                    Box::pin(global_outgoing),
                    voter_set.lock().clone(),
                    last_finalized,
                );

                tokio::spawn(async move {
                    voter.start().await;
                });

                // run voter in background. scheduling it to shut down at the end.
                let finalized = env.finalized_stream();

                // wait for the best block to finalized.
                finalized
                    .take_while(|&(_, n)| {
                        assert!((n - 1) % 3 == 0);
                        println!("n: {n}");
                        futures::future::ready(n < 20)
                    })
                    .for_each(|v| {
                        println!("v: {v:?}");
                        futures::future::ready(())
                    })
            })
            .collect::<Vec<_>>();

        tokio::spawn(routing_network);

        futures::future::join_all(finalized_stream.into_iter()).await;
    }

    #[tokio::test]
    async fn consensus_with_failed_node() {
        // init();
        //
        let voters_num = 4;
        let online_voters_num = 3;

        let default_panic = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            default_panic(info);
            std::process::exit(1);
        }));

        let voter_set = Arc::new(Mutex::new(
            VoterSet::new((0..voters_num).collect()).unwrap(),
        ));

        let (network, routing_network) = make_network();

        let finalized_stream = (0..online_voters_num)
            .map(|local_id| {
                let env = Arc::new(DummyEnvironment::new(
                    network.clone(),
                    local_id,
                    voter_set.clone(),
                ));

                // init chain
                let last_finalized = env.with_chain(|chain| {
                    chain.generate_init_blocks(40);
                    log::trace!(
                        "chain: {:?}, last_finalized: {:?}, next_to_be_finalized: {:?}",
                        chain,
                        chain.last_finalized(),
                        chain.next_to_be_finalized()
                    );
                    chain.last_finalized()
                });

                let (global_incoming, global_outgoing) = network.make_global_comms(local_id);

                let mut voter = Voter::new(
                    env.clone(),
                    Box::new(global_incoming),
                    Box::pin(global_outgoing),
                    voter_set.lock().clone(),
                    last_finalized,
                );

                tokio::spawn(async move {
                    voter.start().await;
                });

                // run voter in background. scheduling it to shut down at the end.
                let finalized = env.finalized_stream();

                // wait for the best block to finalized.
                finalized
                    .take_while(|&(_, n)| {
                        assert!((n - 1) % 3 == 0);
                        println!("n: {n}");
                        futures::future::ready(n < 20)
                    })
                    .for_each(|v| {
                        println!("v: {v:?}");
                        futures::future::ready(())
                    })
            })
            .collect::<Vec<_>>();

        tokio::spawn(routing_network);

        futures::future::join_all(finalized_stream.into_iter()).await;
    }
}
