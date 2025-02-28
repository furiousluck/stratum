//! Configurable Sv2 it support extended and group channel
//! Upstream means another proxy or a pool
//! Downstream means another proxy or a mining device
//!
//! UpstreamMining is the trait that a proxy must implement in order to
//! understant Downstream mining messages.
//!
//! DownstreamMining is the trait that a proxy must implement in order to
//! understand Upstream mining messages
//!
//! Same thing for DownstreamCommon and UpstreamCommon but for common messages
//!
//! DownstreamMiningNode implement both UpstreamMining and UpstreamCommon
//!
//! UpstreamMiningNode implement both DownstreamMining and DownstreamCommon
//!
//! A Downstream that signal the capacity to handle group channels can open more than one channel.
//! A Downstream that signal the incapacity to handle group channels can open only one channel.
//!
#![allow(special_module_name)]
mod lib;
use async_channel::bounded;
use lib::{
    job_negotiator::JobNegotiator, template_receiver::TemplateRx,
    upstream_mining::UpstreamMiningNode,
};
use once_cell::sync::OnceCell;
use roles_logic_sv2::{
    routing_logic::{CommonRoutingLogic, MiningProxyRoutingLogic, MiningRoutingLogic},
    selectors::GeneralMiningSelector,
    utils::{GroupId, Id, Mutex},
};
use serde::Deserialize;
use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::Arc,
};
use tracing::{error, info};
type RLogic = MiningProxyRoutingLogic<
    crate::lib::downstream_mining::DownstreamMiningNode,
    crate::lib::upstream_mining::UpstreamMiningNode,
    crate::lib::upstream_mining::ProxyRemoteSelector,
>;

/// Panic whene we are looking one of this 2 global mutex would force the proxy to go down as every
/// part of the program depend on them.
/// SAFTEY note: we use global mutable memory instead of a dedicated struct that use a dedicated
/// task to change the mutable state and communicate with the other parts of the program via
/// messages cause it is impossible for a task to panic while is using one of the two below Mutex.
/// So it make sense to use shared mutable memory to lower the complexity of the codebase and to
/// have some performance gain.
static ROUTING_LOGIC: OnceCell<Mutex<RLogic>> = OnceCell::new();
static MIN_EXTRANONCE_SIZE: u16 = 6;
static EXTRANONCE_RANGE_1_LENGTH: usize = 4;

async fn initialize_upstreams(min_version: u16, max_version: u16) {
    let upstreams = ROUTING_LOGIC
        .get()
        .expect("BUG: ROUTING_LOGIC has not been set yet")
        .safe_lock(|r_logic| r_logic.upstream_selector.upstreams.clone())
        .unwrap();
    let available_upstreams =
        crate::lib::upstream_mining::scan(upstreams, min_version, max_version).await;
    ROUTING_LOGIC
        .get()
        .unwrap()
        .safe_lock(|rl| rl.upstream_selector.update_upstreams(available_upstreams))
        .unwrap();
}

fn remove_upstream(id: u32) {
    let upstreams = ROUTING_LOGIC
        .get()
        .expect("BUG: ROUTING_LOGIC has not been set yet")
        .safe_lock(|r_logic| r_logic.upstream_selector.upstreams.clone())
        .unwrap();
    let mut updated_upstreams = vec![];
    for upstream in upstreams {
        if upstream.safe_lock(|s| s.get_id()).unwrap() != id {
            updated_upstreams.push(upstream)
        }
    }
    ROUTING_LOGIC
        .get()
        .unwrap()
        .safe_lock(|rl| rl.upstream_selector.update_upstreams(updated_upstreams))
        .unwrap();
}

pub fn get_routing_logic() -> MiningRoutingLogic<
    crate::lib::downstream_mining::DownstreamMiningNode,
    crate::lib::upstream_mining::UpstreamMiningNode,
    crate::lib::upstream_mining::ProxyRemoteSelector,
    RLogic,
> {
    MiningRoutingLogic::Proxy(
        ROUTING_LOGIC
            .get()
            .expect("BUG: ROUTING_LOGIC was not set yet"),
    )
}
pub fn get_common_routing_logic() -> CommonRoutingLogic<RLogic> {
    CommonRoutingLogic::Proxy(
        ROUTING_LOGIC
            .get()
            .expect("BUG: ROUTING_LOGIC was not set yet"),
    )
}

#[derive(Debug, Deserialize, Clone)]
pub struct UpstreamMiningValues {
    address: String,
    port: u16,
    pub_key: codec_sv2::noise_sv2::formats::EncodedEd25519PublicKey,
    channel_kind: ChannelKind,
    jn_values: Option<UpstreamJNValues>,
}
#[derive(Debug, Deserialize, Clone)]
pub struct UpstreamJNValues {
    address: String,
    port: u16,
    pub_key: codec_sv2::noise_sv2::formats::EncodedEd25519PublicKey,
}

#[derive(Debug, Deserialize, Clone, Copy)]
pub enum ChannelKind {
    Group,
    Extended,
    ExtendedWithNegotiator,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    upstreams: Vec<UpstreamMiningValues>,
    tp_address: Option<String>,
    listen_address: String,
    listen_mining_port: u16,
    max_supported_version: u16,
    min_supported_version: u16,
    downstream_share_per_minute: f32,
}
pub async fn initialize_r_logic(
    upstreams: &[UpstreamMiningValues],
    group_id: Arc<Mutex<GroupId>>,
    config: Config,
) -> RLogic {
    let request_ids = Arc::new(Mutex::new(Id::new()));
    let channel_ids = Arc::new(Mutex::new(Id::new()));
    let mut upstream_mining_nodes = Vec::with_capacity(upstreams.len());
    for (index, upstream_) in upstreams.iter().enumerate() {
        let socket = SocketAddr::new(upstream_.address.parse().unwrap(), upstream_.port);

        // channel for template
        let (send_tp, recv_tp) = bounded(10);
        // channel for prev hash
        let (send_ph, recv_ph) = bounded(10);
        // channel to send coinbase_output_max_additional_size
        let (send_comas, recv_comas) = bounded(10);

        let upstream = Arc::new(Mutex::new(UpstreamMiningNode::new(
            index as u32,
            socket,
            upstream_.pub_key.clone().into_inner().to_bytes(),
            upstream_.channel_kind,
            group_id.clone(),
            Some(recv_tp),
            Some(recv_ph),
            request_ids.clone(),
            channel_ids.clone(),
            config.downstream_share_per_minute,
            None,
            None,
        )));

        match upstream_.channel_kind {
            ChannelKind::Group => (),
            ChannelKind::Extended => (),
            ChannelKind::ExtendedWithNegotiator => {
                let (send_solution, recv_solution) = bounded(10);
                let (send_coinbase_out_script, recv_coinbase_out_script) = bounded(10);
                upstream
                    .safe_lock(|s| {
                        s.solution_sender = Some(send_solution);
                        s.recv_coinbase_out = Some(recv_coinbase_out_script);
                    })
                    .unwrap();
                tokio::join!(
                    TemplateRx::connect(
                        config
                            .tp_address
                            .clone()
                            .expect("Template provider address not provided in config.toml")
                            .parse()
                            .unwrap(),
                        send_tp,
                        send_ph,
                        recv_comas,
                        recv_solution,
                    ),
                    JobNegotiator::new(
                        SocketAddr::new(
                            IpAddr::from_str(
                                &upstream_
                                    .jn_values
                                    .clone()
                                    .expect("JN values not provided")
                                    .address
                            )
                            .unwrap(),
                            upstream_
                                .jn_values
                                .clone()
                                .expect("JN values not provided")
                                .port,
                        ),
                        upstream_
                            .jn_values
                            .clone()
                            .expect("JN values not provided")
                            .clone()
                            .pub_key
                            .into_inner()
                            .as_bytes()
                            .to_owned(),
                        send_comas,
                        send_coinbase_out_script,
                        config.clone(),
                    )
                );
                UpstreamMiningNode::start_receiving_pool_coinbase_outs(upstream.clone());
                UpstreamMiningNode::start_receiving_new_template(upstream.clone());
                UpstreamMiningNode::start_receiving_new_prev_hash(upstream.clone());
            }
        }

        upstream_mining_nodes.push(upstream);
    }
    let upstream_selector = GeneralMiningSelector::new(upstream_mining_nodes);
    MiningProxyRoutingLogic {
        upstream_selector,
        downstream_id_generator: Id::new(),
        downstream_to_upstream_map: std::collections::HashMap::new(),
    }
}

mod args {
    use std::path::PathBuf;

    #[derive(Debug)]
    pub struct Args {
        pub config_path: PathBuf,
    }

    enum ArgsState {
        Next,
        ExpectPath,
        Done,
    }

    enum ArgsResult {
        Config(PathBuf),
        None,
        Help(String),
    }

    impl Args {
        const DEFAULT_CONFIG_PATH: &'static str = "proxy-config.toml";

        pub fn from_args() -> Result<Self, String> {
            let cli_args = std::env::args();

            let config_path = cli_args
                .scan(ArgsState::Next, |state, item| {
                    match std::mem::replace(state, ArgsState::Done) {
                        ArgsState::Next => match item.as_str() {
                            "-c" | "--config" => {
                                *state = ArgsState::ExpectPath;
                                Some(ArgsResult::None)
                            }
                            "-h" | "--help" => Some(ArgsResult::Help(format!(
                                "Usage: -h/--help, -c/--config <path|default {}>",
                                Self::DEFAULT_CONFIG_PATH
                            ))),
                            _ => {
                                *state = ArgsState::Next;

                                Some(ArgsResult::None)
                            }
                        },
                        ArgsState::ExpectPath => Some(ArgsResult::Config(PathBuf::from(item))),
                        ArgsState::Done => None,
                    }
                })
                .last();
            let config_path = match config_path {
                Some(ArgsResult::Config(p)) => p,
                Some(ArgsResult::Help(h)) => return Err(h),
                _ => PathBuf::from(Self::DEFAULT_CONFIG_PATH),
            };
            Ok(Self { config_path })
        }
    }
}

/// 1. the proxy scan all the upstreams and map them
/// 2. donwstream open a connetcion with proxy
/// 3. downstream send SetupConnection
/// 4. a mining_channle::Upstream is created
/// 5. upstream_mining::UpstreamMiningNodes is used to pair this downstream with the most suitable
///    upstream
/// 6. mining_channle::Upstream create a new downstream_mining::DownstreamMiningNode embedding
///    itself in it
/// 7. normal operation between the paired downstream_mining::DownstreamMiningNode and
///    upstream_mining::UpstreamMiningNode begin
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = match args::Args::from_args() {
        Ok(cfg) => cfg,
        Err(help) => {
            error!("{}", help);
            return;
        }
    };

    // Scan all the upstreams and map them
    let config_file = std::fs::read_to_string(args.config_path.clone())
        .unwrap_or_else(|_| panic!("Can not open {:?}", args.config_path));
    let config = match toml::from_str::<Config>(&config_file) {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Failed to parse config file: {}", e);
            return;
        }
    };

    let group_id = Arc::new(Mutex::new(GroupId::new()));
    ROUTING_LOGIC
        .set(Mutex::new(
            initialize_r_logic(&config.upstreams, group_id, config.clone()).await,
        ))
        .expect("BUG: Failed to set ROUTING_LOGIC");
    info!("PROXY INITIALIZING");
    initialize_upstreams(config.min_supported_version, config.max_supported_version).await;

    // Wait for downstream connection
    let socket = SocketAddr::new(
        config.listen_address.parse().unwrap(),
        config.listen_mining_port,
    );

    info!("PROXY INITIALIZED");
    crate::lib::downstream_mining::listen_for_downstream_mining(socket).await
}
