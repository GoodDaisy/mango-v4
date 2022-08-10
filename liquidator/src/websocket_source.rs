use jsonrpc_core::futures::StreamExt;
use jsonrpc_core_client::transports::ws;

use solana_account_decoder::UiAccountEncoding;
use solana_client::{
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
    rpc_response::{Response, RpcKeyedAccount, RpcResponseContext},
};
use solana_rpc::rpc_pubsub::RpcSolPubSubClient;
use solana_sdk::{account::AccountSharedData, commitment_config::CommitmentConfig, pubkey::Pubkey};

use anyhow::Context;
use log::*;
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio_stream::StreamMap;

use client::chain_data;

use crate::AnyhowWrap;

#[derive(Clone)]
pub struct AccountUpdate {
    pub pubkey: Pubkey,
    pub slot: u64,
    pub account: AccountSharedData,
}

impl AccountUpdate {
    pub fn from_rpc(rpc: Response<RpcKeyedAccount>) -> anyhow::Result<Self> {
        let pubkey = Pubkey::from_str(&rpc.value.pubkey)?;
        let account = rpc
            .value
            .account
            .decode()
            .ok_or_else(|| anyhow::anyhow!("could not decode account"))?;
        Ok(AccountUpdate {
            pubkey,
            slot: rpc.context.slot,
            account,
        })
    }
}

#[derive(Clone)]
pub enum Message {
    Account(AccountUpdate),
    Slot(Arc<solana_client::rpc_response::SlotUpdate>),
}

pub struct Config {
    pub rpc_ws_url: String,
    pub mango_program: Pubkey,
    pub serum_program: Pubkey,
    pub open_orders_authority: Pubkey,
}

async fn feed_data(
    config: &Config,
    mango_oracles: Vec<Pubkey>,
    sender: async_channel::Sender<Message>,
) -> anyhow::Result<()> {
    let connect = ws::try_connect::<RpcSolPubSubClient>(&config.rpc_ws_url).map_err_anyhow()?;
    let client = connect.await.map_err_anyhow()?;

    let account_info_config = RpcAccountInfoConfig {
        encoding: Some(UiAccountEncoding::Base64),
        commitment: Some(CommitmentConfig::processed()),
        data_slice: None,
        min_context_slot: None,
    };
    let all_accounts_config = RpcProgramAccountsConfig {
        filters: None,
        with_context: Some(true),
        account_config: account_info_config.clone(),
    };
    let open_orders_accounts_config = RpcProgramAccountsConfig {
        // filter for only OpenOrders with v4 authority
        filters: Some(vec![
            RpcFilterType::DataSize(3228), // open orders size
            RpcFilterType::Memcmp(Memcmp {
                offset: 0,
                // "serum" + u64 that is Initialized (1) + OpenOrders (4)
                bytes: MemcmpEncodedBytes::Base58("AcUQf4PGf6fCHGwmpB".into()),
                encoding: None,
            }),
            RpcFilterType::Memcmp(Memcmp {
                offset: 45, // owner is the 4th field, after "serum" (header), account_flags: u64 and market: Pubkey
                bytes: MemcmpEncodedBytes::Bytes(config.open_orders_authority.to_bytes().into()),
                encoding: None,
            }),
        ]),
        with_context: Some(true),
        account_config: account_info_config.clone(),
    };
    let mut mango_sub = client
        .program_subscribe(
            config.mango_program.to_string(),
            Some(all_accounts_config.clone()),
        )
        .map_err_anyhow()?;
    let mut mango_oracles_sub_map = StreamMap::new();
    for oracle in mango_oracles.into_iter() {
        mango_oracles_sub_map.insert(
            oracle,
            client
                .account_subscribe(
                    oracle.to_string(),
                    Some(RpcAccountInfoConfig {
                        encoding: Some(UiAccountEncoding::Base64),
                        commitment: Some(CommitmentConfig::processed()),
                        data_slice: None,
                        min_context_slot: None,
                    }),
                )
                .map_err_anyhow()?,
        );
    }
    let mut open_orders_sub = client
        .program_subscribe(
            config.serum_program.to_string(),
            Some(open_orders_accounts_config.clone()),
        )
        .map_err_anyhow()?;
    let mut slot_sub = client.slots_updates_subscribe().map_err_anyhow()?;

    loop {
        tokio::select! {
            message = mango_sub.next() => {
                if let Some(data) = message {
                    let response = data.map_err_anyhow()?;
                    sender.send(Message::Account(AccountUpdate::from_rpc(response)?)).await.expect("sending must succeed");
                } else {
                    warn!("mango stream closed");
                    return Ok(());
                }
            },
            message = mango_oracles_sub_map.next() => {
                if let Some(data) = message {
                    let response = data.1.map_err_anyhow()?;
                    let response = solana_client::rpc_response::Response{ context: RpcResponseContext{ slot: response.context.slot, api_version: None }, value: RpcKeyedAccount{ pubkey: data.0.to_string(), account:  response.value} } ;
                    sender.send(Message::Account(AccountUpdate::from_rpc(response)?)).await.expect("sending must succeed");
                } else {
                    warn!("oracle stream closed");
                    return Ok(());
                }
            },
            message = open_orders_sub.next() => {
                if let Some(data) = message {
                    let response = data.map_err_anyhow()?;
                    sender.send(Message::Account(AccountUpdate::from_rpc(response)?)).await.expect("sending must succeed");
                } else {
                    warn!("serum stream closed");
                    return Ok(());
                }
            },
            message = slot_sub.next() => {
                if let Some(data) = message {
                    sender.send(Message::Slot(data.map_err_anyhow()?)).await.expect("sending must succeed");
                } else {
                    warn!("slot update stream closed");
                    return Ok(());
                }
            },
            _ = tokio::time::sleep(Duration::from_secs(60)) => {
                warn!("websocket timeout");
                return Ok(())
            }
        }
    }
}

pub fn start(config: Config, mango_oracles: Vec<Pubkey>, sender: async_channel::Sender<Message>) {
    tokio::spawn(async move {
        // if the websocket disconnects, we get no data in a while etc, reconnect and try again
        loop {
            info!("connecting to solana websocket streams");
            let out = feed_data(&config, mango_oracles.clone(), sender.clone());
            let _ = out.await;
        }
    });
}

pub fn update_chain_data(chain: &mut chain_data::ChainData, message: Message) {
    use chain_data::*;
    match message {
        Message::Account(account_write) => {
            trace!("websocket account message");
            chain.update_account(
                account_write.pubkey,
                AccountAndSlot {
                    slot: account_write.slot,
                    account: account_write.account,
                },
            );
        }
        Message::Slot(slot_update) => {
            trace!("websocket slot message");
            let slot_update = match *slot_update {
                solana_client::rpc_response::SlotUpdate::CreatedBank { slot, parent, .. } => {
                    Some(SlotData {
                        slot,
                        parent: Some(parent),
                        status: SlotStatus::Processed,
                        chain: 0,
                    })
                }
                solana_client::rpc_response::SlotUpdate::OptimisticConfirmation {
                    slot, ..
                } => Some(SlotData {
                    slot,
                    parent: None,
                    status: SlotStatus::Confirmed,
                    chain: 0,
                }),
                solana_client::rpc_response::SlotUpdate::Root { slot, .. } => Some(SlotData {
                    slot,
                    parent: None,
                    status: SlotStatus::Rooted,
                    chain: 0,
                }),
                _ => None,
            };
            if let Some(update) = slot_update {
                chain.update_slot(update);
            }
        }
    }
}

pub async fn get_next_create_bank_slot(
    receiver: async_channel::Receiver<Message>,
    timeout: Duration,
) -> anyhow::Result<u64> {
    let start = std::time::Instant::now();
    loop {
        let elapsed = start.elapsed();
        if elapsed > timeout {
            anyhow::bail!(
                "did not receive a slot from the websocket connection in {}s",
                timeout.as_secs()
            );
        }
        let remaining_timeout = timeout - elapsed;

        let msg = match tokio::time::timeout(remaining_timeout, receiver.recv()).await {
            // timeout
            Err(_) => continue,
            // channel close
            Ok(Err(err)) => {
                return Err(err).context("while waiting for first slot from websocket connection");
            }
            // success
            Ok(Ok(msg)) => msg,
        };

        match msg {
            Message::Slot(slot_update) => {
                if let solana_client::rpc_response::SlotUpdate::CreatedBank { slot, .. } =
                    *slot_update
                {
                    return Ok(slot);
                }
            }
            _ => {}
        }
    }
}
