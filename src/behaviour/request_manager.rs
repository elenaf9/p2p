// Copyright 2020-2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use super::{ProtocolSupport, EMPTY_QUEUE_SHRINK_THRESHOLD};
use crate::{
    firewall::{FirewallRules, Rule, RuleDirection, ToPermissionVariants, VariantPermission},
    unwrap_or_return, InboundFailure, OutboundFailure, RequestDirection, RequestId, RequestMessage,
};
mod connections;
use connections::PeerConnectionManager;
use libp2p::{core::connection::ConnectionId, PeerId};
use smallvec::{smallvec, SmallVec};
use std::{
    collections::{HashMap, VecDeque},
    marker::PhantomData,
};

// Actions for the behaviour to handle i.g. the behaviour emits the appropriate `NetworkBehaviourAction`.
pub(super) enum BehaviourAction<Rq, Rs> {
    // Inbound request that was approved and should be emitted as Behaviour Event to the user.
    InboundReady {
        request_id: RequestId,
        peer: PeerId,
        request: RequestMessage<Rq, Rs>,
    },
    // Outbound request to a connected peer that was approved and that should be send to the handler of the connection
    // that this request was assigned to.
    OutboundReady {
        request_id: RequestId,
        peer: PeerId,
        connection: ConnectionId,
        request: RequestMessage<Rq, Rs>,
    },
    // Required dial attempt to connect a peer where at least one approved outbound request is pending.
    RequireDialAttempt(PeerId),
    // Configure if the handler should support inbound / outbound requests.
    SetProtocolSupport {
        peer: PeerId,
        // If a ConnectionId is provided, only the handler of that specific connection will be informed
        // otherwise all handlers of that peer will receive the new settings.
        connection: ConnectionId,
        support: ProtocolSupport,
    },
    // Sending an outbound request failed.
    OutboundFailure {
        peer: PeerId,
        request_id: RequestId,
        reason: OutboundFailure,
    },
    // Receiving / responding to a inbound request failed.
    InboundFailure {
        peer: PeerId,
        request_id: RequestId,
        reason: InboundFailure,
    },
}

// The status of a new request according to the firewall rules of the associated peer.
#[derive(Debug)]
pub(super) enum ApprovalStatus {
    // Neither a peer specific, nor a default rule for the peer + direction exists.
    // A FirewallRequest::PeerSpecificRule has been send and the NetBehaviour currently awaits a response.
    MissingRule,
    // For the peer + direction, the Rule::Ask is set, which requires explicit approval.
    // The NetBehaviour sent a FirewallRequest::RequestApproval and currently awaits the approval.
    MissingApproval,
    Approved,
    Rejected,
}

// Manager for pending requests that are awaiting a peer rule, individual approval, or a connection to the remote.
// Stores pending requests, manages rule, approval and connection changes, and queues required [`BehaviourActions`] for
// the `NetBehaviour` to handle.
pub(super) struct RequestManager<Rq, Rs, P>
where
    Rq: ToPermissionVariants<P>,
    P: VariantPermission,
{
    // Store of inbound requests that have not been approved yet.
    inbound_request_store: HashMap<RequestId, (PeerId, RequestMessage<Rq, Rs>)>,
    // Store of outbound requests that have not been approved, or where the target peer is not connected yet.
    outbound_request_store: HashMap<RequestId, (PeerId, RequestMessage<Rq, Rs>)>,
    // Currently established connections and the requests that have been send/received on the connection, but with no
    // response yet.
    connections: PeerConnectionManager,

    // Approved outbound requests for peers that are currently not connected, but a BehaviourAction::RequireDialAttempt
    // has been issued.
    awaiting_connection: HashMap<PeerId, SmallVec<[RequestId; 10]>>,
    // Pending requests for peers that don't have any firewall rules and currently await the response for a
    // FirewallRequest::PeerSpecificRule that has been sent.
    awaiting_peer_rule: HashMap<PeerId, HashMap<RequestDirection, SmallVec<[RequestId; 10]>>>,
    // Pending requests that require explicit approval due to Rule::Ask, and currently await the response for a
    // FirewallRequest::RequestApproval that has been sent.
    awaiting_approval: SmallVec<[(RequestId, RequestDirection); 10]>,

    // Actions that should be emitted by the NetBehaviour as NetworkBehaviourAction.
    actions: VecDeque<BehaviourAction<Rq, Rs>>,
    marker: PhantomData<P>,
}

impl<Rq, Rs, P> RequestManager<Rq, Rs, P>
where
    Rq: ToPermissionVariants<P>,
    P: VariantPermission,
{
    pub fn new() -> Self {
        RequestManager {
            inbound_request_store: HashMap::new(),
            outbound_request_store: HashMap::new(),
            connections: PeerConnectionManager::new(),
            awaiting_connection: HashMap::new(),
            awaiting_peer_rule: HashMap::new(),
            awaiting_approval: SmallVec::new(),
            actions: VecDeque::new(),
            marker: PhantomData,
        }
    }

    // List of peers to which at least one connection is currently established.
    pub fn connected_peers(&self) -> Vec<PeerId> {
        self.connections.get_connected_peers()
    }

    // New inbound/ outbound request was received / issued.
    // Depending on the approval and connection status, the appropriate [`BehaviourAction`] will be issued
    // and/ or the request will be cached if it is waiting for approval or connection.
    pub fn on_new_request(
        &mut self,
        peer: PeerId,
        request_id: RequestId,
        request: RequestMessage<Rq, Rs>,
        approval_status: ApprovalStatus,
        direction: RequestDirection,
    ) {
        match approval_status {
            ApprovalStatus::MissingRule => {
                // Add request to the list of requests that are awaiting a rule for that peer.
                self.store_request(peer, request_id, request, &direction);
                let await_rule = self.awaiting_peer_rule.entry(peer).or_default();
                await_rule.entry(direction).or_default().push(request_id);
            }
            ApprovalStatus::MissingApproval => {
                // Add request to the list of requests that are awaiting individual approval.
                self.store_request(peer, request_id, request, &direction);
                self.awaiting_approval.push((request_id, direction));
            }
            ApprovalStatus::Approved => {
                // Request is ready to be send if a connection exists.
                // If no connection to the peer exists, add dial attempt (if outbound request) or a failure.
                if let Some(connection) = self.connections.add_request(&peer, request_id, &direction) {
                    // Request is approved and assigned to an existing connection.
                    self.add_ready_request(peer, request_id, connection, request, &direction);
                } else if let RequestDirection::Outbound = direction {
                    self.store_request(peer, request_id, request, &RequestDirection::Outbound);
                    self.add_dial_attempt(peer, request_id);
                } else {
                    let action = BehaviourAction::InboundFailure {
                        request_id,
                        peer,
                        reason: InboundFailure::ConnectionClosed,
                    };
                    self.actions.push_back(action);
                }
            }
            ApprovalStatus::Rejected => {
                // Drop response channel, add failure.
                drop(request.response_tx);
                let action = match direction {
                    RequestDirection::Outbound => BehaviourAction::OutboundFailure {
                        peer,
                        request_id,
                        reason: OutboundFailure::NotPermitted,
                    },
                    RequestDirection::Inbound => BehaviourAction::InboundFailure {
                        peer,
                        request_id,
                        reason: InboundFailure::NotPermitted,
                    },
                };
                self.actions.push_back(action);
            }
        }
    }

    // Handle a newly connected peer i.g. that at least one connection was established.
    // Assign pending request to a connection and mark them as ready.
    pub fn on_peer_connected(&mut self, peer: PeerId) {
        // Check that there is at least one active connection to the remote.
        if !self.connections.is_connected(&peer) {
            return;
        }
        // Handle pending requests
        if let Some(requests) = self.awaiting_connection.remove(&peer) {
            requests.into_iter().for_each(|request_id| {
                let (peer, request) =
                    unwrap_or_return!(self.take_stored_request(&request_id, &RequestDirection::Outbound));
                let connection = self
                    .connections
                    .add_request(&peer, request_id, &RequestDirection::Outbound)
                    .expect("Peer is connected");
                let action = BehaviourAction::OutboundReady {
                    request_id,
                    peer,
                    connection,
                    request,
                };
                self.actions.push_back(action);
            });
        }
    }

    // Handle a remote peer disconnecting completely.
    // Emit failures for the pending responses on all pending connections.
    pub fn on_peer_disconnected(&mut self, peer: PeerId) {
        if let Some(conns) = self.connections.remove_all_connections(&peer) {
            conns
                .iter()
                .for_each(|conn_id| self.on_connection_closed(peer, conn_id))
        }
    }

    // Handle a new individual connection to a remote peer.
    pub fn on_connection_established(&mut self, peer: PeerId, connection: ConnectionId) {
        self.connections.add_connection(peer, connection);
    }

    // Handle an individual connection closing.
    // Emit failures for the pending responses on that connection.
    pub fn on_connection_closed(&mut self, peer: PeerId, connection: &ConnectionId) {
        let pending_res = self.connections.remove_connection(peer, connection);
        if let Some(pending_res) = pending_res {
            let closed_out =
                pending_res
                    .outbound_requests
                    .into_iter()
                    .map(|request_id| BehaviourAction::OutboundFailure {
                        request_id,
                        peer,
                        reason: OutboundFailure::ConnectionClosed,
                    });
            self.actions.extend(closed_out);
            let closed_in =
                pending_res
                    .inbound_requests
                    .into_iter()
                    .map(|request_id| BehaviourAction::InboundFailure {
                        request_id,
                        peer,
                        reason: InboundFailure::ConnectionClosed,
                    });
            self.actions.extend(closed_in);
        }
    }

    // Handle a failed connection attempt to a currently not connected peer.
    // Emit failure for outbound requests that are awaiting the connection.
    pub fn on_dial_failure(&mut self, peer: PeerId) {
        if let Some(requests) = self.awaiting_connection.remove(&peer) {
            requests.into_iter().for_each(|request_id| {
                if let Some((_, req)) = self.take_stored_request(&request_id, &RequestDirection::Outbound) {
                    drop(req.response_tx);
                }
                let action = BehaviourAction::OutboundFailure {
                    request_id,
                    peer,
                    reason: OutboundFailure::DialFailure,
                };
                self.actions.push_back(action);
            });
        }
    }

    // Handle pending requests for a newly received rule.
    // Emit necessary ['BehaviourEvents'] depending on rules and direction.
    // The method return the requests for which the `NetBehaviour` should query a `FirewallRequest::RequestApproval`.
    pub fn on_peer_rule(
        &mut self,
        peer: PeerId,
        rules: FirewallRules,
        direction: RuleDirection,
    ) -> Option<Vec<(RequestId, P, RequestDirection)>> {
        let mut await_rule = self.awaiting_peer_rule.remove(&peer)?;
        // Affected requests.
        let mut requests = vec![];
        if direction.is_inbound() {
            if let Some(in_rqs) = await_rule.remove(&RequestDirection::Inbound) {
                requests.extend(in_rqs.into_iter().map(|rq| (rq, RequestDirection::Inbound)));
            }
        }
        if direction.is_outbound() {
            if let Some(out_rqs) = await_rule.remove(&RequestDirection::Outbound) {
                requests.extend(out_rqs.into_iter().map(|rq| (rq, RequestDirection::Outbound)));
            }
        }
        // Handle the requests according to the new rule.
        let require_ask = requests
            .into_iter()
            .filter_map(|(request_id, dir)| {
                let rule = match dir {
                    RequestDirection::Inbound => rules.inbound(),
                    RequestDirection::Outbound => rules.outbound(),
                };
                match rule {
                    Some(Rule::Ask) => {
                        // Requests need to await individual approval.
                        let rq = self.get_request_value_ref(&request_id)?;
                        let permissioned = rq.to_permissioned();
                        self.awaiting_approval.push((request_id, dir.clone()));
                        Some((request_id, permissioned, dir))
                    }
                    Some(Rule::Permission(permission)) => {
                        // Checking the individual permissions required for the request type.
                        if let Some(rq) = self.get_request_value_ref(&request_id) {
                            let is_allowed = permission.permits(&rq.permission_value());
                            self.handle_request_approval(request_id, &dir, is_allowed);
                        }
                        None
                    }
                    None => {
                        // Reject request if no rule was provided.
                        self.handle_request_approval(request_id, &dir, false);
                        None
                    }
                }
            })
            .collect();
        // Keep unaffected requests in map.
        if !await_rule.is_empty() {
            self.awaiting_peer_rule.insert(peer, await_rule);
        }
        Some(require_ask)
    }

    // Add failures for pending requests that are awaiting the peer rule.
    pub fn on_no_peer_rule(&mut self, peer: PeerId, direction: RuleDirection) {
        if let Some(mut await_rule) = self.awaiting_peer_rule.remove(&peer) {
            if direction.is_inbound() {
                if let Some(requests) = await_rule.remove(&RequestDirection::Inbound) {
                    for request_id in requests {
                        self.take_stored_request(&request_id, &RequestDirection::Inbound);
                        self.actions.push_back(BehaviourAction::InboundFailure {
                            peer,
                            request_id,
                            reason: InboundFailure::NotPermitted,
                        });
                    }
                }
            }
            if direction.is_outbound() {
                if let Some(requests) = await_rule.remove(&RequestDirection::Outbound) {
                    for request_id in requests {
                        self.take_stored_request(&request_id, &RequestDirection::Outbound);
                        self.actions.push_back(BehaviourAction::OutboundFailure {
                            peer,
                            request_id,
                            reason: OutboundFailure::NotPermitted,
                        });
                    }
                }
            }
            // Keep unaffected requests in map.
            if !await_rule.is_empty() {
                self.awaiting_peer_rule.insert(peer, await_rule);
            }
        }
    }

    // Handle the approval of an individual request.
    pub fn on_request_approval(&mut self, request_id: RequestId, is_allowed: bool) -> Option<()> {
        let index = self
            .awaiting_approval
            .binary_search_by(|(id, _)| id.cmp(&request_id))
            .ok()?;
        let (request_id, direction) = self.awaiting_approval.remove(index);
        self.handle_request_approval(request_id, &direction, is_allowed)
    }

    // Handle Response / Failure for a previously received request.
    // Remove the request from the list of pending responses, add failure if there is one.
    pub fn on_res_for_inbound(
        &mut self,
        peer: PeerId,
        connection: &ConnectionId,
        request_id: RequestId,
        result: Result<(), InboundFailure>,
    ) {
        self.connections
            .remove_request(connection, &request_id, &RequestDirection::Inbound);
        if let Err(reason) = result {
            let action = BehaviourAction::InboundFailure {
                peer,
                request_id,
                reason,
            };
            self.actions.push_back(action)
        }
    }

    // Handle Response / Failure for a previously sent request.
    // Remove the request from the list of pending responses, add failure if there is one.
    pub fn on_res_for_outbound(
        &mut self,
        peer: PeerId,
        connection: &ConnectionId,
        request_id: RequestId,
        result: Result<(), OutboundFailure>,
    ) {
        self.connections
            .remove_request(connection, &request_id, &RequestDirection::Outbound);
        if let Err(reason) = result {
            let action = BehaviourAction::OutboundFailure {
                peer,
                request_id,
                reason,
            };
            self.actions.push_back(action)
        }
    }

    // Check if there are pending requests for a rules for a specific peer.
    pub fn pending_rule_requests(&self, peer: &PeerId) -> Option<RuleDirection> {
        let await_rule = self.awaiting_peer_rule.get(&peer)?;
        let is_inbound_pending = await_rule.contains_key(&RequestDirection::Inbound);
        let is_outbound_pending = await_rule.contains_key(&RequestDirection::Outbound);
        let is_both = is_inbound_pending && is_outbound_pending;
        is_both
            .then(|| RuleDirection::Both)
            .or_else(|| is_inbound_pending.then(|| RuleDirection::Inbound))
            .or_else(|| is_outbound_pending.then(|| RuleDirection::Outbound))
    }

    // Add a placeholder to the map of pending rule requests for the given direction to mark that there is a pending
    // rule request.
    pub fn add_pending_rule_requests(&mut self, peer: PeerId, direction: RuleDirection) {
        let pending = self.awaiting_peer_rule.entry(peer).or_insert_with(HashMap::new);
        if direction.is_inbound() && !pending.contains_key(&RequestDirection::Inbound) {
            pending.insert(RequestDirection::Inbound, SmallVec::new());
        }
        if direction.is_outbound() && !pending.contains_key(&RequestDirection::Outbound) {
            pending.insert(RequestDirection::Outbound, SmallVec::new());
        }
    }

    // Add a [`BehaviourAction::SetProtocolSupport`] to the action queue to inform the `ConnectionHandler` of changed
    // protocol support.
    pub fn set_protocol_support(
        &mut self,
        peer: PeerId,
        connection: Option<ConnectionId>,
        protocol_support: ProtocolSupport,
    ) {
        let connections = connection
            .map(|c| smallvec![c])
            .unwrap_or_else(|| self.connections.get_connections(&peer));
        for conn in connections {
            self.actions.push_back(BehaviourAction::SetProtocolSupport {
                peer,
                connection: conn,
                support: protocol_support.clone(),
            });
        }
    }

    // Remove the next [`BehaviourAction`] from the queue and return it.
    pub fn take_next_action(&mut self) -> Option<BehaviourAction<Rq, Rs>> {
        let next = self.actions.pop_front();
        if self.actions.capacity() > EMPTY_QUEUE_SHRINK_THRESHOLD {
            self.actions.shrink_to_fit();
        }
        next
    }

    // Temporary store a request until it is approved / a connection to the remote was established.
    fn store_request(
        &mut self,
        peer: PeerId,
        request_id: RequestId,
        request: RequestMessage<Rq, Rs>,
        direction: &RequestDirection,
    ) {
        match direction {
            RequestDirection::Inbound => self.inbound_request_store.insert(request_id, (peer, request)),
            RequestDirection::Outbound => self.outbound_request_store.insert(request_id, (peer, request)),
        };
    }

    // Remove a cached request from the store and return it, so it can be used or discarded.
    fn take_stored_request(
        &mut self,
        request_id: &RequestId,
        direction: &RequestDirection,
    ) -> Option<(PeerId, RequestMessage<Rq, Rs>)> {
        match direction {
            RequestDirection::Inbound => self.inbound_request_store.remove(request_id),
            RequestDirection::Outbound => self.outbound_request_store.remove(request_id),
        }
    }

    // Add a [`BehaviourAction::RequireDialAttempt`] to the action queue to demand a dial attempt to the remote.
    fn add_dial_attempt(&mut self, peer: PeerId, request_id: RequestId) {
        let reqs = self.awaiting_connection.entry(peer).or_default();
        reqs.push(request_id);
        self.actions.push_back(BehaviourAction::RequireDialAttempt(peer));
    }

    // Add a [`BehaviourAction::InboundReady`] / [`BehaviourAction::OutboundReady`] to the action queue to forward the
    // request.
    fn add_ready_request(
        &mut self,
        peer: PeerId,
        request_id: RequestId,
        connection: ConnectionId,
        request: RequestMessage<Rq, Rs>,
        direction: &RequestDirection,
    ) {
        let event = match direction {
            RequestDirection::Inbound => BehaviourAction::InboundReady {
                request_id,
                peer,
                request,
            },
            RequestDirection::Outbound => BehaviourAction::OutboundReady {
                request_id,
                peer,
                connection,
                request,
            },
        };
        self.actions.push_back(event)
    }

    // Handle the approval / rejection of a individual request.
    fn handle_request_approval(
        &mut self,
        request_id: RequestId,
        direction: &RequestDirection,
        is_allowed: bool,
    ) -> Option<()> {
        // Emit a failure if the request was rejected.
        if !is_allowed {
            let (peer, req) = self.take_stored_request(&request_id, direction)?;
            drop(req.response_tx);
            let action = match direction {
                RequestDirection::Outbound => BehaviourAction::OutboundFailure {
                    request_id,
                    peer,
                    reason: OutboundFailure::NotPermitted,
                },
                RequestDirection::Inbound => BehaviourAction::InboundFailure {
                    request_id,
                    peer,
                    reason: InboundFailure::NotPermitted,
                },
            };
            self.actions.push_back(action);
            return Some(());
        }

        let peer = *self.get_request_peer_ref(&request_id)?;

        // Assign the request to a connection if the remote is connected.
        // If no connection to the peer exists, add dial attempt (if outbound request) or drop the request and emit a
        // failure.
        if let Some(connection) = self.connections.add_request(&peer, request_id, &direction) {
            let (peer, request) = self.take_stored_request(&request_id, direction)?;
            self.add_ready_request(peer, request_id, connection, request, direction);
            Some(())
        } else {
            match direction {
                RequestDirection::Inbound => {
                    let (_, req) = self.take_stored_request(&request_id, direction)?;
                    drop(req.response_tx);
                    let action = BehaviourAction::InboundFailure {
                        request_id,
                        peer,
                        reason: InboundFailure::ConnectionClosed,
                    };
                    self.actions.push_back(action);
                }
                RequestDirection::Outbound => self.add_dial_attempt(peer, request_id),
            }
            Some(())
        }
    }

    // Get the peer id for a stored request.
    fn get_request_peer_ref(&self, request_id: &RequestId) -> Option<&PeerId> {
        self.inbound_request_store
            .get(request_id)
            .or_else(|| self.outbound_request_store.get(request_id))
            .map(|(peer, _)| peer)
    }

    // Get the request type of a store request.
    fn get_request_value_ref(&self, request_id: &RequestId) -> Option<&Rq> {
        self.inbound_request_store
            .get(request_id)
            .or_else(|| self.outbound_request_store.get(request_id))
            .map(|(_, query)| &query.data)
    }
}
