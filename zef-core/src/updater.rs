// Copyright (c) Facebook, Inc. and its affiliates.
// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::node::ValidatorNode;
use futures::{future, StreamExt};
use std::{collections::HashMap, time::Duration};
use zef_base::{base_types::*, chain::ChainState, committee::Committee, error::Error, messages::*};
use zef_storage::Storage;

/// Used for `communicate_chain_updates`
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum CommunicateAction {
    SubmitRequestForConfirmation(BlockProposal),
    SubmitRequestForValidation(BlockProposal),
    FinalizeRequest(Certificate),
    AdvanceToNextSequenceNumber(SequenceNumber),
}

pub struct ValidatorUpdater<A, S> {
    pub name: ValidatorName,
    pub client: A,
    pub store: S,
    pub delay: Duration,
    pub retries: usize,
}

/// Execute a sequence of actions in parallel for all validators.
/// Try to stop early when a quorum is reached.
pub async fn communicate_with_quorum<'a, A, V, F>(
    validator_clients: &'a [(ValidatorName, A)],
    committee: &Committee,
    execute: F,
) -> Result<Vec<V>, Option<Error>>
where
    A: ValidatorNode + Send + Sync + 'static + Clone,
    F: Fn(ValidatorName, A) -> future::BoxFuture<'a, Result<V, Error>> + Clone,
{
    let mut responses: futures::stream::FuturesUnordered<_> = validator_clients
        .iter()
        .filter_map(|(name, client)| {
            let client = client.clone();
            let execute = execute.clone();
            if committee.weight(name) > 0 {
                Some(async move { (*name, execute(*name, client).await) })
            } else {
                // This should not happen but better prevent it because certificates
                // are not allowed to include votes with weight 0.
                None
            }
        })
        .collect();

    let mut values = Vec::new();
    let mut value_score = 0;
    let mut error_scores = HashMap::new();
    while let Some((name, result)) = responses.next().await {
        match result {
            Ok(value) => {
                values.push(value);
                value_score += committee.weight(&name);
                if value_score >= committee.quorum_threshold() {
                    // Success!
                    return Ok(values);
                }
            }
            Err(err) => {
                let entry = error_scores.entry(err.clone()).or_insert(0);
                *entry += committee.weight(&name);
                if *entry >= committee.validity_threshold() {
                    // At least one honest node returned this error.
                    // No quorum can be reached, so return early.
                    return Err(Some(err));
                }
            }
        }
    }

    // No specific error is available to report reliably.
    Err(None)
}

impl<A, S> ValidatorUpdater<A, S>
where
    A: ValidatorNode + Send + Sync + 'static + Clone,
    S: Storage + Clone + 'static,
{
    pub async fn send_certificate(
        &mut self,
        certificate: Certificate,
        retryable: bool,
    ) -> Result<ChainInfo, Error> {
        let mut count = 0;
        loop {
            match self.client.handle_certificate(certificate.clone()).await {
                Ok(response) => {
                    response.check(self.name)?;
                    // Succeed
                    return Ok(response.info);
                }
                Err(Error::InactiveChain(_)) if retryable && count < self.retries => {
                    // Retry
                    tokio::time::sleep(self.delay).await;
                    count += 1;
                    continue;
                }
                Err(e) => {
                    // Fail
                    return Err(e);
                }
            }
        }
    }

    pub async fn send_block_proposal(
        &mut self,
        proposal: BlockProposal,
    ) -> Result<ChainInfo, Error> {
        let mut count = 0;
        loop {
            match self.client.handle_block_proposal(proposal.clone()).await {
                Ok(response) => {
                    response.check(self.name)?;
                    // Succeed
                    return Ok(response.info);
                }
                Err(Error::InactiveChain(_)) if count < self.retries => {
                    // Retry
                    tokio::time::sleep(self.delay).await;
                    count += 1;
                    continue;
                }
                Err(e) => {
                    // Fail
                    return Err(e);
                }
            }
        }
    }

    pub async fn send_chain_information(
        &mut self,
        mut chain_id: ChainId,
        mut target_sequence_number: SequenceNumber,
    ) -> Result<(), Error> {
        let mut jobs = Vec::new();
        loop {
            // Figure out which certificates this validator is missing.
            let query = ChainInfoQuery {
                chain_id: chain_id.clone(),
                check_next_sequence_number: None,
                query_committee: false,
                query_sent_certificates_in_range: None,
                query_received_certificates_excluding_first_nth: None,
            };
            match self.client.handle_chain_info_query(query).await {
                Ok(response) if response.info.manager.is_active() => {
                    response.check(self.name)?;
                    jobs.push((
                        chain_id,
                        response.info.next_sequence_number,
                        target_sequence_number,
                        false,
                    ));
                    break;
                }
                Ok(response) => {
                    response.check(self.name)?;
                    match chain_id.split() {
                        None => return Err(Error::InactiveChain(chain_id)),
                        Some((parent_id, number)) => {
                            jobs.push((
                                chain_id,
                                SequenceNumber::from(0),
                                target_sequence_number,
                                true,
                            ));
                            chain_id = parent_id;
                            target_sequence_number = number.try_add_one()?;
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        for (chain_id, initial_sequence_number, target_sequence_number, retryable) in
            jobs.into_iter().rev()
        {
            // Obtain chain state.
            let chain = self.store.read_chain_or_default(&chain_id).await?;
            // Send the requested certificates in order.
            for number in usize::from(initial_sequence_number)..usize::from(target_sequence_number)
            {
                let key = chain
                    .confirmed_log
                    .get(number)
                    .expect("certificate should be known locally");
                let cert = self.store.read_certificate(*key).await?;
                self.send_certificate(cert, retryable).await?;
            }
        }
        Ok(())
    }

    pub async fn send_chain_information_as_a_receiver(
        &mut self,
        chain_id: ChainId,
    ) -> Result<(), Error> {
        // Obtain chain state.
        let chain = self.store.read_chain_or_default(&chain_id).await?;
        for (sender_id, sequence_number) in chain.received_index.iter() {
            self.send_chain_information(sender_id.clone(), sequence_number.try_add_one()?)
                .await?;
        }
        Ok(())
    }

    pub async fn send_chain_update(
        &mut self,
        chain_id: ChainId,
        action: CommunicateAction,
    ) -> Result<Option<Vote>, Error> {
        let target_sequence_number = match &action {
            CommunicateAction::SubmitRequestForValidation(proposal)
            | CommunicateAction::SubmitRequestForConfirmation(proposal) => {
                proposal.request.sequence_number
            }
            CommunicateAction::FinalizeRequest(certificate) => {
                certificate
                    .value
                    .validated_request()
                    .unwrap()
                    .sequence_number
            }
            CommunicateAction::AdvanceToNextSequenceNumber(seq) => *seq,
        };
        // Update the validator with missing information, if needed.
        self.send_chain_information(chain_id.clone(), target_sequence_number)
            .await?;
        // Send the block proposal (if any) and return a vote.
        match action {
            CommunicateAction::SubmitRequestForValidation(proposal)
            | CommunicateAction::SubmitRequestForConfirmation(proposal) => {
                let result = self.send_block_proposal(proposal.clone()).await;
                let info = match result {
                    Ok(info) => info,
                    Err(e) if ChainState::is_retriable_validation_error(&proposal.request, &e) => {
                        // Some received certificates may be missing for this validator
                        // (e.g. to make the balance sufficient) so we are going to
                        // synchronize them now.
                        self.send_chain_information_as_a_receiver(chain_id).await?;
                        // Now retry the request.
                        self.send_block_proposal(proposal).await?
                    }
                    Err(e) => {
                        return Err(e);
                    }
                };
                match info.manager.pending() {
                    Some(vote) => {
                        vote.check(self.name)?;
                        return Ok(Some(vote.clone()));
                    }
                    None => return Err(Error::ClientErrorWhileProcessingBlockProposal),
                }
            }
            CommunicateAction::FinalizeRequest(certificate) => {
                // The only cause for a retry is that the first certificate of a newly opened chain.
                let retryable = target_sequence_number == SequenceNumber::from(0);
                let info = self.send_certificate(certificate, retryable).await?;
                match info.manager.pending() {
                    Some(vote) => {
                        vote.check(self.name)?;
                        return Ok(Some(vote.clone()));
                    }
                    None => return Err(Error::ClientErrorWhileProcessingBlockProposal),
                }
            }
            CommunicateAction::AdvanceToNextSequenceNumber(_) => (),
        }
        Ok(None)
    }
}
