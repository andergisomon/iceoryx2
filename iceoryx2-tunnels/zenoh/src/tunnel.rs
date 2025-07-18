// Copyright (c) 2025 Contributors to the Eclipse Foundation
//
// See the NOTICE file(s) distributed with this work for additional
// information regarding copyright ownership.
//
// This program and the accompanying materials are made available under the
// terms of the Apache Software License 2.0 which is available at
// https://www.apache.org/licenses/LICENSE-2.0, or the MIT license
// which is available at https://opensource.org/licenses/MIT.
//
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::discovery::Discovery;
use crate::discovery::IceoryxDiscovery;
use crate::discovery::ZenohDiscovery;
use crate::BidirectionalEventConnection;
use crate::BidirectionalPublishSubscribeConnection;
use crate::Connection;

use iceoryx2::config::Config as IceoryxConfig;
use iceoryx2::node::Node as IceoryxNode;
use iceoryx2::node::NodeBuilder;
use iceoryx2::service::service_id::ServiceId as IceoryxServiceId;
use iceoryx2::service::static_config::messaging_pattern::MessagingPattern;
use iceoryx2::service::static_config::StaticConfig as IceoryxServiceConfig;
use iceoryx2_bb_log::error;
use iceoryx2_bb_log::info;

use zenoh::Config as ZenohConfig;
use zenoh::Session as ZenohSession;
use zenoh::Wait;

use std::collections::HashMap;

#[derive(Default)]
pub struct TunnelConfig {
    pub discovery_service: Option<String>,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub enum CreationError {
    Error,
}

impl core::fmt::Display for CreationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> std::fmt::Result {
        core::write!(f, "CreationError::{self:?}")
    }
}

impl core::error::Error for CreationError {}

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub enum DiscoveryError {
    Error,
}

impl core::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> std::fmt::Result {
        core::write!(f, "DiscoveryError::{self:?}")
    }
}

impl core::error::Error for DiscoveryError {}

/// Defines the operational scope for tunnel services.
///
/// This enum specifies which environment to use for tunnel operations:
/// - `Iceoryx`: Only operate within the local Iceoryx environment
/// - `Zenoh`: Only operate through the Zenoh network
/// - `Both`: Operate in both Iceoryx and Zenoh environments
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub enum Scope {
    Iceoryx,
    Zenoh,
    Both,
}

impl core::fmt::Display for Scope {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scope::Iceoryx => write!(f, "iceoryx"),
            Scope::Zenoh => write!(f, "zenoh"),
            Scope::Both => write!(f, "both"),
        }
    }
}

/// A tunnel for propagating iceoryx2 payloads across hosts via the Zenoh network middleware.
pub struct Tunnel<'a, ServiceType: iceoryx2::service::Service> {
    z_session: ZenohSession,
    z_discovery: ZenohDiscovery<'a, ServiceType>,
    iox_node: IceoryxNode<ServiceType>,
    iox_discovery: IceoryxDiscovery<ServiceType>,
    publish_subscribe_connectons:
        HashMap<IceoryxServiceId, BidirectionalPublishSubscribeConnection<'a, ServiceType>>,
    event_connections: HashMap<IceoryxServiceId, BidirectionalEventConnection<'a, ServiceType>>,
}

impl<Service: iceoryx2::service::Service> Tunnel<'_, Service> {
    /// Creates a new tunnel with the provided configuration.
    ///
    /// # Arguments
    ///
    /// * `tunnel_config` - Tunnel configuration
    /// * `iox_config` - Iceoryx configuration to be used
    /// * `z_config` - Zenoh configuration to be used
    ///
    /// # Returns
    ///
    /// * `Ok(Self)` - A new tunnel instance if creation was successful
    /// * `Err(CreationError)` - If any part of the tunnel creation failed
    pub fn create(
        tunnel_config: &TunnelConfig,
        iox_config: &IceoryxConfig,
        z_config: &ZenohConfig,
    ) -> Result<Self, CreationError> {
        info!("STARTING Zenoh Tunnel");

        let z_session = zenoh::open(z_config.clone())
            .wait()
            .map_err(|_e| CreationError::Error)?;
        let z_discovery = ZenohDiscovery::create(&z_session).map_err(|_e| CreationError::Error)?;

        let iox_node = NodeBuilder::new()
            .config(iox_config)
            .create::<Service>()
            .map_err(|_e| CreationError::Error)?;
        let iox_discovery =
            IceoryxDiscovery::create(iox_config, &iox_node, &tunnel_config.discovery_service)
                .map_err(|_e| CreationError::Error)?;

        let publish_subscribe_connectons: HashMap<
            IceoryxServiceId,
            BidirectionalPublishSubscribeConnection<Service>,
        > = HashMap::new();
        let event_connections: HashMap<IceoryxServiceId, BidirectionalEventConnection<Service>> =
            HashMap::new();

        Ok(Self {
            z_session,
            z_discovery,
            iox_node,
            iox_discovery,
            publish_subscribe_connectons,
            event_connections,
        })
    }

    /// Discover iceoryx services across all connected hosts.
    ///
    /// # Arguments
    ///
    /// * `scope` - Determines the discovery scope
    ///
    /// # Returns
    ///
    /// * `Ok(())` - If discovery was successful
    /// * `Err(DiscoveryError)` - If discovery failed
    pub fn discover(&mut self, scope: Scope) -> Result<(), DiscoveryError> {
        if scope == Scope::Iceoryx || scope == Scope::Both {
            self.iox_discovery
                .discover(&mut |iox_service_config| {
                    on_discovery(
                        Scope::Iceoryx,
                        iox_service_config,
                        &self.iox_node,
                        &self.z_session,
                        &mut self.publish_subscribe_connectons,
                        &mut self.event_connections,
                    )
                })
                .map_err(|_e| DiscoveryError::Error)?;
        }

        if scope == Scope::Zenoh || scope == Scope::Both {
            self.z_discovery
                .discover(&mut |iox_service_config| {
                    on_discovery(
                        Scope::Zenoh,
                        iox_service_config,
                        &self.iox_node,
                        &self.z_session,
                        &mut self.publish_subscribe_connectons,
                        &mut self.event_connections,
                    )
                })
                .map_err(|_e| DiscoveryError::Error)?;
        }

        Ok(())
    }

    /// Propagates payloads between all connected hosts.
    pub fn propagate(&self) {
        // TODO(correctioness): consolidate and forward errors
        for (id, connection) in &self.publish_subscribe_connectons {
            if let Err(e) = connection.propagate() {
                error!("Failed to propagate ({:?}): {}", id, e);
            }
        }

        for (id, connection) in &self.event_connections {
            if let Err(e) = connection.propagate() {
                error!("Failed to propagate ({:?}): {}", id, e);
            }
        }
    }

    /// Returns a list of all service IDs that are currently being tunneled.
    ///
    /// # Returns
    ///
    /// * `Vec<String>` - A vector containing the string representation of all service IDs
    ///   that are currently being tunneled through this tunnel instance.
    pub fn tunneled_services(&self) -> Vec<String> {
        self.publish_subscribe_connectons
            .keys()
            .chain(self.event_connections.keys())
            .map(|id| id.as_str().to_string())
            .collect()
    }
}

/// Process a discovered service and create appropriate connections.
///
/// # Arguments
///
/// * `source` - The scope from which the service was discovered (Iceoryx, Zenoh, or Both)
/// * `iox_service_config` - Configuration of the discovered Iceoryx service
/// * `iox_node` - The Iceoryx node instance to use for creating connections
/// * `z_session` - The Zenoh session to use for creating connections
/// * `publish_subscribe_connections` - Map to store created publish-subscribe connections
/// * `event_connections` - Map to store created event connections
///
/// # Returns
///
/// This function doesn't return a value. It updates the connection maps in-place.
fn on_discovery<'a, ServiceType: iceoryx2::service::Service>(
    source: Scope,
    iox_service_config: &IceoryxServiceConfig,
    iox_node: &IceoryxNode<ServiceType>,
    z_session: &ZenohSession,
    publish_subscribe_connections: &mut HashMap<
        IceoryxServiceId,
        BidirectionalPublishSubscribeConnection<'a, ServiceType>,
    >,
    event_connections: &mut HashMap<
        IceoryxServiceId,
        BidirectionalEventConnection<'a, ServiceType>,
    >,
) {
    let iox_service_id = iox_service_config.service_id();
    match iox_service_config.messaging_pattern() {
        MessagingPattern::PublishSubscribe(_) => {
            if !publish_subscribe_connections.contains_key(iox_service_id) {
                info!(
                    "DISCOVERED({}): PublishSubscribe {} [{}]",
                    source,
                    iox_service_id.as_str(),
                    iox_service_config.name()
                );

                let connection = BidirectionalPublishSubscribeConnection::create(
                    iox_node,
                    z_session,
                    iox_service_config,
                )
                .unwrap();

                publish_subscribe_connections.insert(iox_service_id.clone(), connection);
            }
        }
        MessagingPattern::Event(_) => {
            if !event_connections.contains_key(iox_service_id) {
                info!(
                    "DISCOVERED({}): Event {} [{}]",
                    source,
                    iox_service_id.as_str(),
                    iox_service_config.name()
                );

                let connection =
                    BidirectionalEventConnection::create(iox_node, z_session, iox_service_config)
                        .unwrap();

                event_connections.insert(iox_service_id.clone(), connection);
            }
        }
        _ => { /* Not supported. Nothing to do. */ }
    }
}
