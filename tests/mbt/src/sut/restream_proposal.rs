//! Translates RestreamProposalAction from Quint to AppMsg::RestreamProposal.

use anyhow::{anyhow, ensure, Result};
use emerald::app::process_consensus_message;
use malachitebft_app_channel::app::types::core::{Round as EmeraldRound, Validity};
use malachitebft_app_channel::{AppMsg, Channels, NetworkMsg};
use malachitebft_eth_types::{EmeraldContext, Height as EmeraldHeight, Value};
use tokio::sync::mpsc;

use super::Sut;
use crate::history::History;
use crate::state::Proposal;

struct NetworkChannelOverride<'a> {
    channels: &'a mut Channels<EmeraldContext>,
    original: Option<mpsc::Sender<NetworkMsg<EmeraldContext>>>,
}

impl<'a> NetworkChannelOverride<'a> {
    fn new(
        channels: &'a mut Channels<EmeraldContext>,
        replacement: mpsc::Sender<NetworkMsg<EmeraldContext>>,
    ) -> Self {
        let original = core::mem::replace(&mut channels.network, replacement);
        Self {
            channels,
            original: Some(original),
        }
    }

    fn channels(&mut self) -> &mut Channels<EmeraldContext> {
        self.channels
    }
}

impl Drop for NetworkChannelOverride<'_> {
    fn drop(&mut self) {
        if let Some(original) = self.original.take() {
            self.channels.network = original;
        }
    }
}

impl Sut {
    /// Replays the RestreamProposal Quint action (see emerald.qnt
    /// handle_restream_proposal).
    ///
    /// The real handler publishes proposal parts to the network channel. This
    /// adapter captures that stream, validates its round metadata, and records
    /// it so later ReceivedProposalAction steps can deliver it to peers.
    pub async fn restream_proposal(
        &mut self,
        hist: &mut History,
        source_proposal: Proposal,
        proposal: Proposal,
    ) -> Result<()> {
        ensure!(
            source_proposal.height == proposal.height,
            "Restreamed proposal changed height"
        );
        ensure!(
            source_proposal.payload == proposal.payload,
            "Restreamed proposal changed payload"
        );
        ensure!(
            hist.get_address(&proposal.proposer)? == self.address,
            "Restreamed proposal has the wrong proposer"
        );

        let height = EmeraldHeight::new(proposal.height);
        let round = EmeraldRound::new(proposal.round);
        let valid_round = EmeraldRound::new(source_proposal.round);
        let value_id = hist.get_value_id(&source_proposal)?;
        let value = hist.get_value(&source_proposal.id())?;

        let msg = AppMsg::RestreamProposal {
            height,
            round,
            valid_round,
            address: self.address,
            value_id,
        };

        // Temporarily replace the network sender so this action can observe the
        // real handler output without changing production channels.
        let (network_tx, mut network_rx) = tokio::sync::mpsc::channel(1);

        let capture = tokio::spawn(async move {
            let mut stream = Vec::new();
            while let Some(NetworkMsg::PublishProposalPart(part)) = network_rx.recv().await {
                let is_fin = part.is_fin();
                stream.push(part);
                if is_fin {
                    return Ok(stream);
                }
            }

            Err(anyhow!(
                "RestreamProposal did not publish a complete stream"
            ))
        });

        let components = &mut self.components;
        let state = &mut components.state;
        let channels = &mut components.channels;
        let engine = &components.engine;
        let emerald_config = &components.emerald_config;
        let process_result = {
            let mut network_override = NetworkChannelOverride::new(channels, network_tx);
            process_consensus_message(
                msg,
                state,
                network_override.channels(),
                engine,
                emerald_config,
            )
            .await
        };

        process_result
            .map_err(|err| anyhow!("Failed to process RestreamProposal message: {err:?}"))?;

        let stored_proposal = self
            .components
            .state
            .store
            .get_undecided_proposal(height, round, value_id)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "RestreamProposal did not store value {value_id} at height {height}, round {round}"
                )
            })?;
        ensure!(
            stored_proposal.valid_round == valid_round,
            "Stored re-proposal has the wrong valid round"
        );
        ensure!(
            stored_proposal.proposer == self.address,
            "Stored re-proposal has the wrong proposer"
        );
        ensure!(
            stored_proposal.validity == Validity::Valid,
            "Stored re-proposal is not valid"
        );

        let stream = capture
            .await
            .map_err(|err| anyhow!("Failed to capture RestreamProposal stream: {err}"))??;
        let init = stream
            .iter()
            .find_map(|message| message.content.as_data()?.as_init())
            .ok_or_else(|| anyhow!("RestreamProposal stream did not contain ProposalInit"))?;

        ensure!(init.height == height, "ProposalInit has the wrong height");
        ensure!(init.round == round, "ProposalInit has the wrong round");
        ensure!(
            init.pol_round == valid_round,
            "ProposalInit has the wrong polka round"
        );

        let mut restreamed_bytes = Vec::new();
        for data in stream
            .iter()
            .filter_map(|message| message.content.as_data()?.as_data())
        {
            restreamed_bytes.extend_from_slice(&data.bytes);
        }

        let restreamed_value = Value::new(restreamed_bytes.into());
        ensure!(
            restreamed_value.id() == value_id,
            "Restreamed value does not match source value {value_id}"
        );

        hist.record_proposal(proposal, value, stream);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_channel_override_restores_original_sender_when_dropped() {
        let (_consensus_tx, consensus_rx) = mpsc::channel(1);
        let (network_tx, _network_rx) = mpsc::channel(1);
        let (requests_tx, _requests_rx) = mpsc::channel(1);
        let mut channels = Channels::<EmeraldContext> {
            consensus: consensus_rx,
            network: network_tx.clone(),
            events: Default::default(),
            requests: requests_tx,
        };
        let (replacement_tx, _replacement_rx) = mpsc::channel(1);

        {
            let mut network_override =
                NetworkChannelOverride::new(&mut channels, replacement_tx.clone());
            assert!(network_override
                .channels()
                .network
                .same_channel(&replacement_tx));
        }

        assert!(channels.network.same_channel(&network_tx));
        assert!(!channels.network.is_closed());
    }
}
