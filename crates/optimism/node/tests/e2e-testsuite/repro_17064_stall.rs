use std::{collections::HashSet, sync::Arc, time::Duration};

use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::Genesis;
use alloy_primitives::{hex, Address, Bytes, B256, TxKind, U256};
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_signer::Signer;

use reth_tracing::tracing;
use reth_e2e_test_utils::{
    transaction::TransactionTestContext, wallet::Wallet, E2ETestSetupBuilder,
};
use reth_node_core::args::TxPoolArgs;
use reth_optimism_chainspec::OpChainSpecBuilder;
use reth_optimism_node::{OpNode, OpPayloadBuilderAttributes};
use reth_payload_builder::EthPayloadBuilderAttributes;

fn op_payload_attributes<T>(timestamp: u64, gas_limit: u64) -> OpPayloadBuilderAttributes<T> {
    // Copy of reth_optimism_node::utils::optimism_payload_attributes but with param gas_limit.
    // The shape is shown in the utils source.
    let attributes = PayloadAttributes {
        timestamp,
        prev_randao: B256::ZERO,
        suggested_fee_recipient: Address::ZERO,
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(B256::ZERO),
    };

    OpPayloadBuilderAttributes {
        payload_attributes: EthPayloadBuilderAttributes::new(B256::ZERO, attributes),
        transactions: vec![],
        no_tx_pool: false,
        gas_limit: Some(gas_limit),
        eip_1559_params: None,
        min_base_fee: None,
    }
}

// Minimal "transfer-like" tx builder mirroring reth_e2e_test_utils::transaction::tx()
fn tx_request(chain_id: u64, nonce: u64, max_fee_per_gas: u128, tip: u128) -> TransactionRequest {
    TransactionRequest {
        nonce: Some(nonce),
        value: Some(U256::from(1)),
        to: Some(TxKind::Call(Address::random())),
        gas: Some(21_000),
        max_fee_per_gas: Some(max_fee_per_gas),
        max_priority_fee_per_gas: Some(tip),
        chain_id: Some(chain_id),
        input: TransactionInput { input: None, data: None },
        ..Default::default()
    }
}

fn l1_info_tx_request(
    chain_id: u64,
    nonce: u64,
    max_fee_per_gas: u128,
    tip: u128,
) -> TransactionRequest {
    let l1_block_info = Bytes::from_static(&hex!(
        "7ef9015aa044bae9d41b8380d781187b426c6fe43df5fb2fb57bd4466ef6a701e1f01e015694deaddeaddeaddeaddeaddeaddeaddeaddead000194420000000000000000000000000000000000001580808408f0d18001b90104015d8eb900000000000000000000000000000000000000000000000000000000008057650000000000000000000000000000000000000000000000000000000063d96d10000000000000000000000000000000000000000000000000000000000009f35273d89754a1e0387b89520d989d3be9c37c1f32495a88faf1ea05c61121ab0d1900000000000000000000000000000000000000000000000000000000000000010000000000000000000000002d679b567db6187c0c8323fa982cfb88b74dbcc7000000000000000000000000000000000000000000000000000000000000083400000000000000000000000000000000000000000000000000000000000f4240"
    ));
    TransactionRequest {
        nonce: Some(nonce),
        value: Some(U256::from(100)),
        to: Some(TxKind::Call(Address::random())),
        gas: Some(210_000),
        max_fee_per_gas: Some(max_fee_per_gas),
        max_priority_fee_per_gas: Some(tip),
        chain_id: Some(chain_id),
        input: TransactionInput { input: None, data: Some(l1_block_info) },
        ..Default::default()
    }
}

fn funding_tx_request(
    chain_id: u64,
    nonce: u64,
    to: Address,
    value: U256,
    max_fee_per_gas: u128,
    tip: u128,
) -> TransactionRequest {
    TransactionRequest {
        nonce: Some(nonce),
        value: Some(value),
        to: Some(TxKind::Call(to)),
        gas: Some(21_000),
        max_fee_per_gas: Some(max_fee_per_gas),
        max_priority_fee_per_gas: Some(tip),
        chain_id: Some(chain_id),
        input: TransactionInput { input: None, data: None },
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "stress repro for #17064; run manually"]
async fn repro_17064_op_stall_stress() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // --- Setup OP chainspec like the existing OP utils do. ---
    let genesis: Genesis = serde_json::from_str(include_str!("../assets/genesis.json"))?;
    let funded: HashSet<Address> = genesis.alloc.keys().copied().collect();
    let chain_spec = Arc::new(
        OpChainSpecBuilder::base_mainnet()
            .genesis(genesis)
            .ecotone_activated()
            .build(),
    );

    let gas_limit = 200_000_000u64;

    let (mut nodes, _tasks, mut wallet) = E2ETestSetupBuilder::<OpNode, _>::new(
        1,
        chain_spec,
        move |ts| op_payload_attributes(ts, gas_limit),
    )
    .with_node_config_modifier(|config| {
        let mut txpool = TxPoolArgs::default();
        txpool.pending_max_count = 200_000;
        txpool.pending_max_size = 1024;
        txpool.basefee_max_count = 200_000;
        txpool.basefee_max_size = 1024;
        txpool.queued_max_count = 200_000;
        txpool.queued_max_size = 1024;
        txpool.max_account_slots = 4096;
        txpool.additional_validation_tasks = 8;
        config.set_dev(false).with_txpool(txpool)
    })
    .build()
    .await?;

    let node = &mut nodes[0];

    // Generate many funded test accounts from the standard test mnemonic.
    // Wallet::wallet_gen() is designed for this.
    let chain_id = wallet.chain_id;
    let mut senders = Wallet::new(256).with_chain_id(chain_id).wallet_gen();
    let inner_address = wallet.inner.address();
    senders.retain(|signer| signer.address() != inner_address);
    let mut funded_senders = Vec::new();
    let mut unfunded_senders = Vec::new();
    for signer in senders {
        if funded.contains(&signer.address()) {
            funded_senders.push(signer);
        } else {
            unfunded_senders.push(signer);
        }
    }

    // Tuning knobs (env overrides are handy for lab runs)
    let target_senders: usize = 256;
    let blocks: u64 = 50_000;
    let txs_per_block: usize = 10_000;
    let warmup_txs: usize = 200_000;
    let fund_batch: usize = 50;
    let fund_value: U256 = U256::from(1_000_000_000_000_000_000u128);

    let mut senders = funded_senders;
    let mut to_fund = Vec::new();
    while senders.len() < target_senders {
        let Some(signer) = unfunded_senders.pop() else { break };
        to_fund.push(signer.address());
        senders.push(signer);
    }

    if senders.is_empty() {
        eyre::bail!("no funded test accounts found in genesis alloc besides L1 info sender");
    }

    let mut nonces = vec![0u64; senders.len()];

    // Keep L1 info tx "very expensive" so it sorts first vs our spam txs
    let spam_max_fee: u128 = 200_000_000_000;
    let spam_tip: u128 = 1_000_000_000;
    let l1_info_max_fee: u128 = 400_000_000_000;
    let l1_info_tip: u128 = 2_000_000_000;
    let funding_max_fee: u128 = 400_000_000_000;
    let funding_tip: u128 = 5_000_000_000;

    let per_rpc_timeout = Duration::from_secs(2);
    let per_block_timeout = Duration::from_secs(15);

    if !to_fund.is_empty() {
        for chunk in to_fund.chunks(fund_batch) {
            let l1_info_tx = {
                let tx = l1_info_tx_request(
                    chain_id,
                    wallet.inner_nonce,
                    l1_info_max_fee,
                    l1_info_tip,
                );
                let signed = TransactionTestContext::sign_tx(wallet.inner.clone(), tx).await;
                let raw: Bytes = signed.encoded_2718().into();
                raw
            };
            wallet.inner_nonce += 1;

            tokio::time::timeout(per_rpc_timeout, node.rpc.inject_tx(l1_info_tx)).await??;

            for address in chunk {
                let tx = funding_tx_request(
                    chain_id,
                    wallet.inner_nonce,
                    *address,
                    fund_value,
                    funding_max_fee,
                    funding_tip,
                );
                wallet.inner_nonce += 1;

                let signed = TransactionTestContext::sign_tx(wallet.inner.clone(), tx).await;
                let raw: Bytes = signed.encoded_2718().into();

                match tokio::time::timeout(per_rpc_timeout, node.rpc.inject_tx(raw)).await {
                    Ok(_) => {
                        // Ignore funding inject errors; only fail on timeout.
                    }
                    Err(_) => {
                        panic!("funding inject_tx timed out after {:?}", per_rpc_timeout);
                    }
                }
            }

            tokio::time::timeout(per_block_timeout, node.advance_block()).await??;
        }
    }

    if warmup_txs > 0 {
        for i in 0..warmup_txs {
            let idx = i % senders.len();
            let signer = senders[idx].clone();

            let nonce = nonces[idx];
            let tx = tx_request(chain_id, nonce, spam_max_fee, spam_tip);
            let signed = TransactionTestContext::sign_tx(signer, tx).await;
            let raw: Bytes = signed.encoded_2718().into();

            match tokio::time::timeout(per_rpc_timeout, node.rpc.inject_tx(raw)).await {
                Ok(Ok(_)) => {
                    nonces[idx] += 1;
                }
                Ok(Err(err)) => {
                    if err.to_string().contains("txpool is full") {
                        break;
                    }
                }
                Err(_) => {
                    panic!("inject_tx timed out after {:?}", per_rpc_timeout);
                }
            }
        }
    }

    for height in 0..blocks {
        // 1) Inject the OP "L1 block info" tx (required by OP execution paths).
        let l1_info_tx = {
            let tx =
                l1_info_tx_request(chain_id, wallet.inner_nonce, l1_info_max_fee, l1_info_tip);
            let signed = TransactionTestContext::sign_tx(wallet.inner.clone(), tx).await;
            let raw: Bytes = signed.encoded_2718().into();
            raw
        };
        wallet.inner_nonce += 1;

        tokio::time::timeout(per_rpc_timeout, node.rpc.inject_tx(l1_info_tx)).await??;

        // 2) Inject a big batch of low-fee txs.
        // NOTE: keep this sequential first; you can parallelize later once it compiles & runs.
        for i in 0..txs_per_block {
            let idx = i % senders.len();
            let signer = senders[idx].clone();

            let nonce = nonces[idx];
            let tx = tx_request(chain_id, nonce, spam_max_fee, spam_tip);
            let signed = TransactionTestContext::sign_tx(signer, tx).await;
            let raw: Bytes = signed.encoded_2718().into();

            match tokio::time::timeout(per_rpc_timeout, node.rpc.inject_tx(raw)).await {
                Ok(Ok(_)) => {
                    nonces[idx] += 1;
                }
                Ok(Err(err)) => {
                    if err.to_string().contains("txpool is full") {
                        break;
                    }
                }
                Err(_) => {
                    panic!("inject_tx timed out after {:?}", per_rpc_timeout);
                }
            }
        }

        // 3) Build + submit + FCU (engine flow). This is where "stall" will show up.
        tokio::time::timeout(per_block_timeout, node.advance_block()).await??;

        if height % 100 == 0 {
            tracing::info!(height, "repro_17064 still making progress");
        }
    }

    Ok(())
}
