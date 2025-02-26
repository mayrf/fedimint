use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::convert::{Infallible, TryInto};
#[cfg(not(target_family = "wasm"))]
use std::time::Duration;

use anyhow::{bail, format_err, Context};
use bitcoin::hashes::{sha256, Hash as BitcoinHash, HashEngine, Hmac, HmacEngine};
use bitcoin::policy::DEFAULT_MIN_RELAY_TX_FEE;
use bitcoin::secp256k1::{All, Secp256k1, Verification};
use bitcoin::util::psbt::{Input, PartiallySignedTransaction};
use bitcoin::util::sighash::SighashCache;
use bitcoin::{
    Address, BlockHash, EcdsaSig, EcdsaSighashType, Network, PackedLockTime, Script, Sequence,
    Transaction, TxIn, TxOut, Txid,
};
use common::config::WalletConfigConsensus;
use common::db::{
    BlockCountVoteKey, BlockCountVotePrefix, DbKeyPrefix, FeeRateVoteKey, FeeRateVotePrefix,
    PegOutNonceKey,
};
use common::{
    proprietary_tweak_key, PegOutFees, PegOutSignatureItem, PendingTransaction,
    ProcessPegOutSigError, SpendableUTXO, UnsignedTransaction, WalletCommonInit,
    WalletConsensusItem, WalletCreationError, WalletInput, WalletModuleTypes, WalletOutput,
    WalletOutputOutcome, CONFIRMATION_TARGET,
};
use fedimint_bitcoind::{create_bitcoind, DynBitcoindRpc};
use fedimint_core::config::{
    ConfigGenModuleParams, DkgResult, ServerModuleConfig, ServerModuleConsensusConfig,
    TypedServerModuleConfig, TypedServerModuleConsensusConfig,
};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::{
    Database, DatabaseTransaction, DatabaseVersion, IDatabaseTransactionOpsCoreTyped,
};
use fedimint_core::encoding::Encodable;
use fedimint_core::endpoint_constants::{
    BLOCK_COUNT_ENDPOINT, BLOCK_COUNT_LOCAL_ENDPOINT, PEG_OUT_FEES_ENDPOINT,
};
use fedimint_core::module::audit::Audit;
use fedimint_core::module::{
    api_endpoint, ApiEndpoint, CoreConsensusVersion, InputMeta, ModuleConsensusVersion, ModuleInit,
    PeerHandle, ServerModuleInit, ServerModuleInitArgs, SupportedModuleApiVersions,
    TransactionItemAmount,
};
use fedimint_core::server::DynServerModule;
#[cfg(not(target_family = "wasm"))]
use fedimint_core::task::sleep;
use fedimint_core::task::{TaskGroup, TaskHandle};
use fedimint_core::{
    apply, async_trait_maybe_send, push_db_key_items, push_db_pair_items, Feerate, NumPeers,
    OutPoint, PeerId, ServerModule,
};
use fedimint_server::config::distributedgen::PeerHandleOps;
pub use fedimint_wallet_common as common;
use fedimint_wallet_common::config::{WalletClientConfig, WalletConfig, WalletGenParams};
use fedimint_wallet_common::db::{
    BlockHashKey, BlockHashKeyPrefix, PegOutBitcoinTransaction, PegOutBitcoinTransactionPrefix,
    PegOutTxSignatureCI, PegOutTxSignatureCIPrefix, PendingTransactionKey,
    PendingTransactionPrefixKey, UTXOKey, UTXOPrefixKey, UnsignedTransactionKey,
    UnsignedTransactionPrefixKey,
};
use fedimint_wallet_common::keys::CompressedPublicKey;
use fedimint_wallet_common::tweakable::Tweakable;
use fedimint_wallet_common::{Rbf, WalletInputError, WalletOutputError, WalletOutputV0};
use futures::StreamExt;
use miniscript::psbt::PsbtExt;
use miniscript::{translate_hash_fail, Descriptor, TranslatePk};
use rand::rngs::OsRng;
use secp256k1::{Message, Scalar};
use strum::IntoEnumIterator;
use tracing::{debug, error, info, instrument, trace, warn};

#[derive(Debug, Clone)]
pub struct WalletInit;

#[apply(async_trait_maybe_send!)]
impl ModuleInit for WalletInit {
    type Common = WalletCommonInit;

    async fn dump_database(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        prefix_names: Vec<String>,
    ) -> Box<dyn Iterator<Item = (String, Box<dyn erased_serde::Serialize + Send>)> + '_> {
        let mut wallet: BTreeMap<String, Box<dyn erased_serde::Serialize + Send>> = BTreeMap::new();
        let filtered_prefixes = DbKeyPrefix::iter().filter(|f| {
            prefix_names.is_empty() || prefix_names.contains(&f.to_string().to_lowercase())
        });
        for table in filtered_prefixes {
            match table {
                DbKeyPrefix::BlockHash => {
                    push_db_key_items!(dbtx, BlockHashKeyPrefix, BlockHashKey, wallet, "Blocks");
                }
                DbKeyPrefix::PegOutBitcoinOutPoint => {
                    push_db_pair_items!(
                        dbtx,
                        PegOutBitcoinTransactionPrefix,
                        PegOutBitcoinTransaction,
                        WalletOutputOutcome,
                        wallet,
                        "Peg Out Bitcoin Transaction"
                    );
                }
                DbKeyPrefix::PegOutTxSigCi => {
                    push_db_pair_items!(
                        dbtx,
                        PegOutTxSignatureCIPrefix,
                        PegOutTxSignatureCI,
                        Vec<secp256k1::ecdsa::Signature>,
                        wallet,
                        "Peg Out Transaction Signatures"
                    );
                }
                DbKeyPrefix::PendingTransaction => {
                    push_db_pair_items!(
                        dbtx,
                        PendingTransactionPrefixKey,
                        PendingTransactionKey,
                        PendingTransaction,
                        wallet,
                        "Pending Transactions"
                    );
                }
                DbKeyPrefix::PegOutNonce => {
                    if let Some(nonce) = dbtx.get_value(&PegOutNonceKey).await {
                        wallet.insert("Peg Out Nonce".to_string(), Box::new(nonce));
                    }
                }
                DbKeyPrefix::UnsignedTransaction => {
                    push_db_pair_items!(
                        dbtx,
                        UnsignedTransactionPrefixKey,
                        UnsignedTransactionKey,
                        UnsignedTransaction,
                        wallet,
                        "Unsigned Transactions"
                    );
                }
                DbKeyPrefix::Utxo => {
                    push_db_pair_items!(
                        dbtx,
                        UTXOPrefixKey,
                        UTXOKey,
                        SpendableUTXO,
                        wallet,
                        "UTXOs"
                    );
                }

                DbKeyPrefix::BlockCountVote => {
                    push_db_pair_items!(
                        dbtx,
                        BlockCountVotePrefix,
                        BlockCountVoteKey,
                        u32,
                        wallet,
                        "Block Count Votes"
                    );
                }

                DbKeyPrefix::FeeRateVote => {
                    push_db_pair_items!(
                        dbtx,
                        FeeRateVotePrefix,
                        FeeRateVoteKey,
                        Feerate,
                        wallet,
                        "Fee Rate Votes"
                    );
                }
            }
        }

        Box::new(wallet.into_iter())
    }
}

#[apply(async_trait_maybe_send!)]
impl ServerModuleInit for WalletInit {
    type Params = WalletGenParams;
    const DATABASE_VERSION: DatabaseVersion = DatabaseVersion(0);

    fn versions(&self, _core: CoreConsensusVersion) -> &[ModuleConsensusVersion] {
        const MODULE_CONSENSUS_VERSION: ModuleConsensusVersion = ModuleConsensusVersion::new(0, 0);
        &[MODULE_CONSENSUS_VERSION]
    }

    fn supported_api_versions(&self) -> SupportedModuleApiVersions {
        SupportedModuleApiVersions::from_raw((u32::MAX, 0), (0, 0), &[(0, 0)])
    }

    async fn init(&self, args: &ServerModuleInitArgs<Self>) -> anyhow::Result<DynServerModule> {
        Ok(Wallet::new(
            args.cfg().to_typed()?,
            args.db().clone(),
            &mut args.task_group().clone(),
            args.our_peer_id(),
        )
        .await?
        .into())
    }

    fn trusted_dealer_gen(
        &self,
        peers: &[PeerId],
        params: &ConfigGenModuleParams,
    ) -> BTreeMap<PeerId, ServerModuleConfig> {
        let params = self.parse_params(params).unwrap();
        let secp = secp256k1::Secp256k1::new();

        let btc_pegin_keys = peers
            .iter()
            .map(|&id| (id, secp.generate_keypair(&mut OsRng)))
            .collect::<Vec<_>>();

        let wallet_cfg: BTreeMap<PeerId, WalletConfig> = btc_pegin_keys
            .iter()
            .map(|(id, (sk, _))| {
                let cfg = WalletConfig::new(
                    btc_pegin_keys
                        .iter()
                        .map(|(peer_id, (_, pk))| (*peer_id, CompressedPublicKey { key: *pk }))
                        .collect(),
                    *sk,
                    peers.threshold(),
                    params.consensus.network,
                    params.consensus.finality_delay,
                    params.local.bitcoin_rpc.clone(),
                    params.consensus.client_default_bitcoin_rpc.clone(),
                );
                (*id, cfg)
            })
            .collect();

        wallet_cfg
            .into_iter()
            .map(|(k, v)| (k, v.to_erased()))
            .collect()
    }

    async fn distributed_gen(
        &self,
        peers: &PeerHandle,
        params: &ConfigGenModuleParams,
    ) -> DkgResult<ServerModuleConfig> {
        let params = self.parse_params(params).unwrap();
        let secp = secp256k1::Secp256k1::new();
        let (sk, pk) = secp.generate_keypair(&mut OsRng);
        let our_key = CompressedPublicKey { key: pk };
        let peer_peg_in_keys: BTreeMap<PeerId, CompressedPublicKey> = peers
            .exchange_pubkeys("wallet".to_string(), our_key.key)
            .await?
            .into_iter()
            .map(|(k, key)| (k, CompressedPublicKey { key }))
            .collect();

        let wallet_cfg = WalletConfig::new(
            peer_peg_in_keys,
            sk,
            peers.peer_ids().threshold(),
            params.consensus.network,
            params.consensus.finality_delay,
            params.local.bitcoin_rpc.clone(),
            params.consensus.client_default_bitcoin_rpc.clone(),
        );

        Ok(wallet_cfg.to_erased())
    }

    fn validate_config(&self, identity: &PeerId, config: ServerModuleConfig) -> anyhow::Result<()> {
        let config = config.to_typed::<WalletConfig>()?;
        let pubkey = secp256k1::PublicKey::from_secret_key_global(&config.private.peg_in_key);

        if config
            .consensus
            .peer_peg_in_keys
            .get(identity)
            .ok_or_else(|| format_err!("Secret key doesn't match any public key"))?
            != &CompressedPublicKey::new(pubkey)
        {
            bail!(" Bitcoin wallet private key doesn't match multisig pubkey");
        }

        Ok(())
    }

    fn get_client_config(
        &self,
        config: &ServerModuleConsensusConfig,
    ) -> anyhow::Result<WalletClientConfig> {
        let config = WalletConfigConsensus::from_erased(config)?;
        Ok(WalletClientConfig {
            peg_in_descriptor: config.peg_in_descriptor,
            network: config.network,
            fee_consensus: config.fee_consensus,
            finality_delay: config.finality_delay,
            default_bitcoin_rpc: config.client_default_bitcoin_rpc,
        })
    }
}

#[apply(async_trait_maybe_send!)]
impl ServerModule for Wallet {
    type Common = WalletModuleTypes;
    type Init = WalletInit;

    async fn consensus_proposal<'a>(
        &'a self,
        dbtx: &mut DatabaseTransaction<'_>,
    ) -> Vec<WalletConsensusItem> {
        let mut items = dbtx
            .find_by_prefix(&PegOutTxSignatureCIPrefix)
            .await
            .map(|(key, val)| {
                WalletConsensusItem::PegOutSignature(PegOutSignatureItem {
                    txid: key.0,
                    signature: val,
                })
            })
            .collect::<Vec<WalletConsensusItem>>()
            .await;

        // If we are unable to get a block count from the node we skip adding a block
        // count vote to consensus items.
        //
        // The potential impact of not including the latest block count from our peer's
        // node is delayed processing of change outputs for the federation, which is an
        // acceptable risk since subsequent rounds of consensus will reattempt to fetch
        // the latest block count.
        if let Ok(block_count) = self.get_block_count().await {
            let block_count_vote = block_count.saturating_sub(self.cfg.consensus.finality_delay);

            let current_vote = dbtx
                .get_value(&BlockCountVoteKey(self.our_peer_id))
                .await
                .unwrap_or(0);

            debug!(
                ?current_vote,
                ?block_count_vote,
                ?block_count,
                "Proposing block count"
            );

            items.push(WalletConsensusItem::BlockCount(block_count_vote));
        }

        // If there's an error getting the fee rate from the node we default to the most
        // recent fee rate vote. Using an alternative fee rate may cause unwanted
        // jitter.
        let fee_rate_proposal = match self.get_fee_rate_opt().await {
            Ok(fee_rate_opt) => fee_rate_opt.unwrap_or(self.cfg.consensus.default_fee),
            Err(err) => {
                error!(
                    "Error while calling get_free_rate_opt, using most recent fee rate vote: {:?}",
                    err
                );

                dbtx.get_value(&FeeRateVoteKey(self.our_peer_id))
                    .await
                    .unwrap_or(self.cfg.consensus.default_fee)
            }
        };

        items.push(WalletConsensusItem::Feerate(fee_rate_proposal));

        items
    }

    async fn process_consensus_item<'a, 'b>(
        &'a self,
        dbtx: &mut DatabaseTransaction<'b>,
        consensus_item: WalletConsensusItem,
        peer_id: PeerId,
    ) -> anyhow::Result<()> {
        trace!(?consensus_item, "Received consensus proposals");

        match consensus_item {
            WalletConsensusItem::BlockCount(block_count) => {
                let current_vote = dbtx
                    .get_value(&BlockCountVoteKey(peer_id))
                    .await
                    .unwrap_or(0);

                if block_count < current_vote {
                    debug!(?peer_id, ?block_count, "Received outdated block count vote");
                    bail!("Block count vote decreased");
                }

                if block_count == current_vote {
                    debug!(
                        ?peer_id,
                        ?block_count,
                        "Received redundant block count vote"
                    );
                    bail!("Block count vote is redundant");
                }

                let old_consensus_block_count = self.consensus_block_count(dbtx).await;

                dbtx.insert_entry(&BlockCountVoteKey(peer_id), &block_count)
                    .await;

                let new_consensus_block_count = self.consensus_block_count(dbtx).await;

                debug!(
                    ?peer_id,
                    ?current_vote,
                    ?block_count,
                    ?old_consensus_block_count,
                    ?new_consensus_block_count,
                    "Received block count vote"
                );

                // only sync when we have a consensus block count
                match (old_consensus_block_count, new_consensus_block_count) {
                    (Some(old), Some(new)) if new > old => {
                        if old > 0 {
                            let new_height = new - 1;
                            let old_height = old - 1;
                            self.sync_up_to_consensus_height(dbtx, old_height, new_height)
                                .await;
                        } else {
                            info!(
                                ?new,
                                ?old,
                                "Not syncing up to consensus block count because we are at block 0"
                            );
                        }
                    }
                    _ => {}
                }
            }
            WalletConsensusItem::Feerate(feerate) => {
                if Some(feerate) == dbtx.insert_entry(&FeeRateVoteKey(peer_id), &feerate).await {
                    bail!("Fee rate vote is redundant");
                }
            }
            WalletConsensusItem::PegOutSignature(peg_out_signature) => {
                let txid = peg_out_signature.txid;

                if dbtx.get_value(&PendingTransactionKey(txid)).await.is_some() {
                    bail!("Already received a threshold of valid signatures");
                }

                let mut unsigned = dbtx
                    .get_value(&UnsignedTransactionKey(txid))
                    .await
                    .context("Unsigned transaction does not exist")?;

                self.sign_peg_out_psbt(&mut unsigned.psbt, &peer_id, &peg_out_signature)
                    .context("Peg out signature is invalid")?;

                dbtx.insert_entry(&UnsignedTransactionKey(txid), &unsigned)
                    .await;

                if let Ok(pending_tx) = self.finalize_peg_out_psbt(unsigned) {
                    // We were able to finalize the transaction, so we will delete the
                    // PSBT and instead keep the extracted tx for periodic transmission
                    // as well as to accept the change into our wallet eventually once
                    // it confirms.
                    dbtx.insert_new_entry(&PendingTransactionKey(txid), &pending_tx)
                        .await;

                    dbtx.remove_entry(&PegOutTxSignatureCI(txid)).await;
                    dbtx.remove_entry(&UnsignedTransactionKey(txid)).await;
                }
            }
            WalletConsensusItem::Default { variant, .. } => {
                bail!("Received wallet consensus item with unknown variant {variant}");
            }
        }

        Ok(())
    }

    async fn process_input<'a, 'b, 'c>(
        &'a self,
        dbtx: &mut DatabaseTransaction<'c>,
        input: &'b WalletInput,
    ) -> Result<InputMeta, WalletInputError> {
        let input = input.ensure_v0_ref()?;

        if !self.block_is_known(dbtx, input.proof_block()).await {
            return Err(WalletInputError::UnknownPegInProofBlock(
                input.proof_block(),
            ));
        }

        input.verify(&self.secp, &self.cfg.consensus.peg_in_descriptor)?;

        debug!(outpoint = %input.outpoint(), "Claiming peg-in");

        if dbtx
            .insert_entry(
                &UTXOKey(input.outpoint()),
                &SpendableUTXO {
                    tweak: input.tweak_contract_key().serialize(),
                    amount: bitcoin::Amount::from_sat(input.tx_output().value),
                },
            )
            .await
            .is_some()
        {
            return Err(WalletInputError::PegInAlreadyClaimed);
        }

        Ok(InputMeta {
            amount: TransactionItemAmount {
                amount: fedimint_core::Amount::from_sats(input.tx_output().value),
                fee: self.cfg.consensus.fee_consensus.peg_in_abs,
            },
            pub_key: *input.tweak_contract_key(),
        })
    }

    async fn process_output<'a, 'b>(
        &'a self,
        dbtx: &mut DatabaseTransaction<'b>,
        output: &'a WalletOutput,
        out_point: OutPoint,
    ) -> Result<TransactionItemAmount, WalletOutputError> {
        let output = output.ensure_v0_ref()?;

        let change_tweak = self.consensus_nonce(dbtx).await;

        let mut tx = self.create_peg_out_tx(dbtx, output, &change_tweak).await?;

        let fee_rate = self.consensus_fee_rate(dbtx).await;

        self.offline_wallet()
            .validate_tx(&tx, output, fee_rate, self.cfg.consensus.network)?;

        self.offline_wallet().sign_psbt(&mut tx.psbt);

        let txid = tx.psbt.unsigned_tx.txid();

        info!(
            %txid,
            "Signing peg out",
        );

        let sigs = tx
            .psbt
            .inputs
            .iter_mut()
            .map(|input| {
                assert_eq!(
                    input.partial_sigs.len(),
                    1,
                    "There was already more than one (our) or no signatures in input"
                );

                // TODO: don't put sig into PSBT in the first place
                // We actually take out our own signature so everyone finalizes the tx in the
                // same epoch.
                let sig = std::mem::take(&mut input.partial_sigs)
                    .into_values()
                    .next()
                    .expect("asserted previously");

                // We drop SIGHASH_ALL, because we always use that and it is only present in the
                // PSBT for compatibility with other tools.
                secp256k1::ecdsa::Signature::from_der(&sig.to_vec()[..sig.to_vec().len() - 1])
                    .expect("we serialized it ourselves that way")
            })
            .collect::<Vec<_>>();

        // Delete used UTXOs
        for input in tx.psbt.unsigned_tx.input.iter() {
            dbtx.remove_entry(&UTXOKey(input.previous_output)).await;
        }

        dbtx.insert_new_entry(&UnsignedTransactionKey(txid), &tx)
            .await;

        dbtx.insert_new_entry(&PegOutTxSignatureCI(txid), &sigs)
            .await;

        dbtx.insert_new_entry(
            &PegOutBitcoinTransaction(out_point),
            &WalletOutputOutcome::new_v0(txid),
        )
        .await;

        Ok(TransactionItemAmount {
            amount: output.amount().into(),
            fee: self.cfg.consensus.fee_consensus.peg_out_abs,
        })
    }

    async fn output_status(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        out_point: OutPoint,
    ) -> Option<WalletOutputOutcome> {
        dbtx.get_value(&PegOutBitcoinTransaction(out_point)).await
    }

    async fn audit(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        audit: &mut Audit,
        module_instance_id: ModuleInstanceId,
    ) {
        audit
            .add_items(dbtx, module_instance_id, &UTXOPrefixKey, |_, v| {
                v.amount.to_sat() as i64 * 1000
            })
            .await;
        audit
            .add_items(
                dbtx,
                module_instance_id,
                &UnsignedTransactionPrefixKey,
                |_, v| match v.rbf {
                    None => v.change.to_sat() as i64 * 1000,
                    Some(rbf) => rbf.fees.amount().to_sat() as i64 * -1000,
                },
            )
            .await;
        audit
            .add_items(
                dbtx,
                module_instance_id,
                &PendingTransactionPrefixKey,
                |_, v| match v.rbf {
                    None => v.change.to_sat() as i64 * 1000,
                    Some(rbf) => rbf.fees.amount().to_sat() as i64 * -1000,
                },
            )
            .await;
    }

    fn api_endpoints(&self) -> Vec<ApiEndpoint<Self>> {
        vec![
            api_endpoint! {
                BLOCK_COUNT_ENDPOINT,
                async |module: &Wallet, context, _params: ()| -> u32 {
                    // TODO: perhaps change this to an Option
                    Ok(module.consensus_block_count(&mut context.dbtx().into_nc()).await.unwrap_or_default())
                }
            },
            api_endpoint! {
                BLOCK_COUNT_LOCAL_ENDPOINT,
                async |module: &Wallet, _context, _params: ()| -> Option<u32> {
                    Ok(*module.block_count_local.lock().expect("Locking failed"))
                }
            },
            api_endpoint! {
                PEG_OUT_FEES_ENDPOINT,
                async |module: &Wallet, context, params: (Address, u64)| -> Option<PegOutFees> {
                    let (address, sats) = params;
                    let feerate = module.consensus_fee_rate(&mut context.dbtx().into_nc()).await;

                    // Since we are only calculating the tx size we can use an arbitrary dummy nonce.
                    let dummy_tweak = [0; 33];

                    let tx = module.offline_wallet().create_tx(
                        bitcoin::Amount::from_sat(sats),
                        address.script_pubkey(),
                        vec![],
                        module.available_utxos(&mut context.dbtx().into_nc()).await,
                        feerate,
                        &dummy_tweak,
                        None
                    );

                    match tx {
                        Err(error) => {
                            // Usually from not enough spendable UTXOs
                            warn!("Error returning peg-out fees {error}");
                            Ok(None)
                        }
                        Ok(tx) => Ok(Some(tx.fees))
                    }
                }
            },
        ]
    }
}

#[derive(Debug)]
pub struct Wallet {
    cfg: WalletConfig,
    secp: Secp256k1<All>,
    btc_rpc: DynBitcoindRpc,
    /// The result of last successful get_block_count
    block_count_local: std::sync::Mutex<Option<u32>>,
    our_peer_id: PeerId,
}

impl Wallet {
    pub async fn new(
        cfg: WalletConfig,
        db: Database,
        task_group: &mut TaskGroup,
        our_peer_id: PeerId,
    ) -> anyhow::Result<Wallet> {
        let btc_rpc = create_bitcoind(&cfg.local.bitcoin_rpc, task_group.make_handle())?;
        Ok(Self::new_with_bitcoind(cfg, db, btc_rpc, task_group, our_peer_id).await?)
    }

    pub async fn new_with_bitcoind(
        cfg: WalletConfig,
        db: Database,
        bitcoind: DynBitcoindRpc,
        task_group: &mut TaskGroup,
        our_peer_id: PeerId,
    ) -> Result<Wallet, WalletCreationError> {
        let broadcaster_bitcoind_rpc = bitcoind.clone();
        let broadcaster_db = db.clone();
        task_group
            .spawn("broadcast pending", |handle| async move {
                run_broadcast_pending_tx(broadcaster_db, broadcaster_bitcoind_rpc, &handle).await;
            })
            .await;

        let bitcoind_rpc = bitcoind;

        let bitcoind_net = bitcoind_rpc
            .get_network()
            .await
            .map_err(|e| WalletCreationError::RpcError(e.to_string()))?;
        if bitcoind_net != cfg.consensus.network {
            return Err(WalletCreationError::WrongNetwork(
                cfg.consensus.network,
                bitcoind_net,
            ));
        }

        let wallet = Wallet {
            cfg,
            secp: Default::default(),
            block_count_local: Default::default(),
            btc_rpc: bitcoind_rpc,
            our_peer_id,
        };

        match wallet.get_block_count().await {
            Ok(height) => info!(height, "Connected to bitcoind"),
            Err(err) => warn!("Bitcoin node is not ready or configured properly. Modules relying on it may not function correctly: {:?}", err),
        }

        match wallet.get_fee_rate_opt().await {
            Ok(feerate) => {
                match feerate {
                    Some(fr) => info!(feerate = fr.sats_per_kvb, "Bitcoind feerate available"),
                    None => info!(feerate = 0, "Bitcoind feerate not available. Using defaults."),
                }
            },
            Err(err) => warn!("Bitcoin fee estimation failed. Please configure your nodes to enable fee estimation: {:?}", err),
        }

        Ok(wallet)
    }

    /// Try to attach signatures to a pending peg-out tx.
    fn sign_peg_out_psbt(
        &self,
        psbt: &mut PartiallySignedTransaction,
        peer: &PeerId,
        signature: &PegOutSignatureItem,
    ) -> Result<(), ProcessPegOutSigError> {
        let peer_key = self
            .cfg
            .consensus
            .peer_peg_in_keys
            .get(peer)
            .expect("always called with valid peer id");

        if psbt.inputs.len() != signature.signature.len() {
            return Err(ProcessPegOutSigError::WrongSignatureCount(
                psbt.inputs.len(),
                signature.signature.len(),
            ));
        }

        let mut tx_hasher = SighashCache::new(&psbt.unsigned_tx);
        for (idx, (input, signature)) in psbt
            .inputs
            .iter_mut()
            .zip(signature.signature.iter())
            .enumerate()
        {
            let tx_hash = tx_hasher
                .segwit_signature_hash(
                    idx,
                    input
                        .witness_script
                        .as_ref()
                        .expect("Missing witness script"),
                    input.witness_utxo.as_ref().expect("Missing UTXO").value,
                    EcdsaSighashType::All,
                )
                .map_err(|_| ProcessPegOutSigError::SighashError)?;

            let tweak = input
                .proprietary
                .get(&proprietary_tweak_key())
                .expect("we saved it with a tweak");

            let tweaked_peer_key = peer_key.tweak(tweak, &self.secp);
            self.secp
                .verify_ecdsa(
                    &Message::from_slice(&tx_hash[..]).unwrap(),
                    signature,
                    &tweaked_peer_key.key,
                )
                .map_err(|_| ProcessPegOutSigError::InvalidSignature)?;

            if input
                .partial_sigs
                .insert(tweaked_peer_key.into(), EcdsaSig::sighash_all(*signature))
                .is_some()
            {
                // Should never happen since peers only sign a PSBT once
                return Err(ProcessPegOutSigError::DuplicateSignature);
            }
        }
        Ok(())
    }

    fn finalize_peg_out_psbt(
        &self,
        mut unsigned: UnsignedTransaction,
    ) -> Result<PendingTransaction, ProcessPegOutSigError> {
        // We need to save the change output's tweak key to be able to access the funds
        // later on. The tweak is extracted here because the psbt is moved next
        // and not available anymore when the tweak is actually needed in the
        // end to be put into the batch on success.
        let change_tweak: [u8; 33] = unsigned
            .psbt
            .outputs
            .iter()
            .flat_map(|output| output.proprietary.get(&proprietary_tweak_key()).cloned())
            .next()
            .ok_or(ProcessPegOutSigError::MissingOrMalformedChangeTweak)?
            .try_into()
            .map_err(|_| ProcessPegOutSigError::MissingOrMalformedChangeTweak)?;

        if let Err(error) = unsigned.psbt.finalize_mut(&self.secp) {
            return Err(ProcessPegOutSigError::ErrorFinalizingPsbt(error));
        }

        let tx = unsigned.psbt.clone().extract_tx();

        Ok(PendingTransaction {
            tx,
            tweak: change_tweak,
            change: unsigned.change,
            destination: unsigned.destination,
            fees: unsigned.fees,
            selected_utxos: unsigned.selected_utxos,
            peg_out_amount: unsigned.peg_out_amount,
            rbf: unsigned.rbf,
        })
    }

    /// Wrapper around `self.btc_rpc` that keeps track of the last successful
    /// result
    async fn get_block_count(&self) -> anyhow::Result<u32> {
        let res = self
            .btc_rpc
            .get_block_count()
            .await
            .and_then(|count| Ok(u32::try_from(count)?));

        match res {
            Ok(count) => *self.block_count_local.lock().expect("Failed to lock") = Some(count),
            Err(ref err) => error!("Error while calling get_block_count: {:?}", err),
        }

        res
    }

    pub async fn get_fee_rate_opt(&self) -> anyhow::Result<Option<Feerate>> {
        self.btc_rpc.get_fee_rate(CONFIRMATION_TARGET).await
    }

    pub async fn consensus_block_count(&self, dbtx: &mut DatabaseTransaction<'_>) -> Option<u32> {
        let peer_count = self.cfg.consensus.peer_peg_in_keys.total();

        let mut counts = dbtx
            .find_by_prefix(&BlockCountVotePrefix)
            .await
            .map(|(.., count)| Some(count))
            .collect::<Vec<_>>()
            .await;

        assert!(counts.len() <= peer_count);
        while counts.len() < peer_count {
            counts.push(None);
        }

        counts.sort_unstable();

        counts[peer_count / 2]
    }

    pub async fn consensus_fee_rate(&self, dbtx: &mut DatabaseTransaction<'_>) -> Feerate {
        let peer_count = self.cfg.consensus.peer_peg_in_keys.total();

        let mut rates = dbtx
            .find_by_prefix(&FeeRateVotePrefix)
            .await
            .map(|(.., rate)| rate)
            .collect::<Vec<_>>()
            .await;

        assert!(rates.len() <= peer_count);

        while rates.len() < peer_count {
            rates.push(self.cfg.consensus.default_fee);
        }

        rates.sort_unstable();

        rates[peer_count / 2]
    }

    pub async fn consensus_nonce(&self, dbtx: &mut DatabaseTransaction<'_>) -> [u8; 33] {
        let nonce_idx = dbtx.get_value(&PegOutNonceKey).await.unwrap_or(0);
        dbtx.insert_entry(&PegOutNonceKey, &(nonce_idx + 1)).await;

        nonce_from_idx(nonce_idx)
    }

    async fn sync_up_to_consensus_height<'a>(
        &self,
        dbtx: &mut DatabaseTransaction<'a>,
        old_height: u32,
        new_height: u32,
    ) {
        info!(
            new_height,
            blocks_to_go = new_height - old_height,
            "New consensus height, syncing up",
        );

        for height in (old_height + 1)..=(new_height) {
            if height % 100 == 0 {
                debug!("Caught up to block {height}");
            }

            // TODO: use batching for mainnet syncing
            trace!(block = height, "Fetching block hash");
            let block_hash = self
                .btc_rpc
                .get_block_hash(height as u64)
                .await
                .expect("bitcoind rpc backend failed"); // TODO: use u64 for height everywhere

            let pending_transactions = dbtx
                .find_by_prefix(&PendingTransactionPrefixKey)
                .await
                .map(|(key, transaction)| (key.0, transaction))
                .collect::<HashMap<Txid, PendingTransaction>>()
                .await;
            let pending_transactions_len = pending_transactions.len();

            debug!(
                ?height,
                ?pending_transactions_len,
                "Recognizing change UTXOs"
            );
            for (txid, tx) in &pending_transactions {
                if let Ok(Some(tx_height)) = self
                    .btc_rpc
                    .get_tx_block_height(txid)
                    .await
                    .map(|r| r.filter(|tx_height| *tx_height == height as u64))
                {
                    if tx_height == height as u64 {
                        debug!(?txid, ?tx_height, "Recognizing change UTXO");
                        self.recognize_change_utxo(dbtx, tx).await;
                    } else {
                        debug!(
                            ?txid,
                            ?tx_height,
                            ?height,
                            "Pending transaction not yet confirmed in this block"
                        );
                    }
                } else {
                    debug!(?txid, ?height, "Pending transaction not yet confirmed");
                }
            }

            dbtx.insert_new_entry(
                &BlockHashKey(BlockHash::from_inner(block_hash.into_inner())),
                &(),
            )
            .await;
        }
    }

    /// Add a change UTXO to our spendable UTXO database after it was included
    /// in a block that we got consensus on.
    async fn recognize_change_utxo<'a>(
        &self,
        dbtx: &mut DatabaseTransaction<'a>,
        pending_tx: &PendingTransaction,
    ) {
        self.remove_rbf_transactions(dbtx, pending_tx).await;

        let script_pk = self
            .cfg
            .consensus
            .peg_in_descriptor
            .tweak(&pending_tx.tweak, &self.secp)
            .script_pubkey();
        for (idx, output) in pending_tx.tx.output.iter().enumerate() {
            if output.script_pubkey == script_pk {
                dbtx.insert_entry(
                    &UTXOKey(bitcoin::OutPoint {
                        txid: pending_tx.tx.txid(),
                        vout: idx as u32,
                    }),
                    &SpendableUTXO {
                        tweak: pending_tx.tweak,
                        amount: bitcoin::Amount::from_sat(output.value),
                    },
                )
                .await;
            }
        }
    }

    /// Removes the `PendingTransaction` and any transactions tied to it via RBF
    async fn remove_rbf_transactions<'a>(
        &self,
        dbtx: &mut DatabaseTransaction<'a>,
        pending_tx: &PendingTransaction,
    ) {
        let mut all_transactions: BTreeMap<Txid, PendingTransaction> = dbtx
            .find_by_prefix(&PendingTransactionPrefixKey)
            .await
            .map(|(key, val)| (key.0, val))
            .collect::<BTreeMap<Txid, PendingTransaction>>()
            .await;

        // We need to search and remove all `PendingTransactions` invalidated by RBF
        let mut pending_to_remove = vec![pending_tx.clone()];
        while let Some(removed) = pending_to_remove.pop() {
            all_transactions.remove(&removed.tx.txid());
            dbtx.remove_entry(&PendingTransactionKey(removed.tx.txid()))
                .await;

            // Search for tx that this `removed` has as RBF
            if let Some(rbf) = &removed.rbf {
                if let Some(tx) = all_transactions.get(&rbf.txid) {
                    pending_to_remove.push(tx.clone());
                }
            }

            // Search for tx that wanted to RBF the `removed` one
            for tx in all_transactions.values() {
                if let Some(rbf) = &tx.rbf {
                    if rbf.txid == removed.tx.txid() {
                        pending_to_remove.push(tx.clone());
                    }
                }
            }
        }
    }

    async fn block_is_known(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        block_hash: BlockHash,
    ) -> bool {
        dbtx.get_value(&BlockHashKey(block_hash)).await.is_some()
    }

    async fn create_peg_out_tx(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        output: &WalletOutputV0,
        change_tweak: &[u8; 33],
    ) -> Result<UnsignedTransaction, WalletOutputError> {
        match output {
            WalletOutputV0::PegOut(peg_out) => self.offline_wallet().create_tx(
                peg_out.amount,
                peg_out.recipient.script_pubkey(),
                vec![],
                self.available_utxos(dbtx).await,
                peg_out.fees.fee_rate,
                change_tweak,
                None,
            ),
            WalletOutputV0::Rbf(rbf) => {
                let tx = dbtx
                    .get_value(&PendingTransactionKey(rbf.txid))
                    .await
                    .ok_or(WalletOutputError::RbfTransactionIdNotFound)?;

                self.offline_wallet().create_tx(
                    tx.peg_out_amount,
                    tx.destination,
                    tx.selected_utxos,
                    self.available_utxos(dbtx).await,
                    tx.fees.fee_rate,
                    change_tweak,
                    Some(rbf.clone()),
                )
            }
        }
    }

    async fn available_utxos(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
    ) -> Vec<(UTXOKey, SpendableUTXO)> {
        dbtx.find_by_prefix(&UTXOPrefixKey)
            .await
            .collect::<Vec<(UTXOKey, SpendableUTXO)>>()
            .await
    }

    pub async fn get_wallet_value(&self, dbtx: &mut DatabaseTransaction<'_>) -> bitcoin::Amount {
        let sat_sum = self
            .available_utxos(dbtx)
            .await
            .into_iter()
            .map(|(_, utxo)| utxo.amount.to_sat())
            .sum();
        bitcoin::Amount::from_sat(sat_sum)
    }

    fn offline_wallet(&self) -> StatelessWallet {
        StatelessWallet {
            descriptor: &self.cfg.consensus.peg_in_descriptor,
            secret_key: &self.cfg.private.peg_in_key,
            secp: &self.secp,
        }
    }
}

#[instrument(level = "debug", skip_all)]
pub async fn run_broadcast_pending_tx(db: Database, rpc: DynBitcoindRpc, tg_handle: &TaskHandle) {
    while !tg_handle.is_shutting_down() {
        broadcast_pending_tx(db.begin_transaction().await.into_nc(), &rpc).await;
        sleep(Duration::from_secs(1)).await;
    }
}

pub async fn broadcast_pending_tx(mut dbtx: DatabaseTransaction<'_>, rpc: &DynBitcoindRpc) {
    let pending_tx: Vec<PendingTransaction> = dbtx
        .find_by_prefix(&PendingTransactionPrefixKey)
        .await
        .map(|(_, val)| val)
        .collect::<Vec<_>>()
        .await;
    let rbf_txids: BTreeSet<Txid> = pending_tx
        .iter()
        .filter_map(|tx| tx.rbf.clone().map(|rbf| rbf.txid))
        .collect();
    debug!(
        "Broadcasting pending transactions (total={}, rbf={})",
        pending_tx.len(),
        rbf_txids.len()
    );

    for PendingTransaction { tx, .. } in pending_tx {
        if !rbf_txids.contains(&tx.txid()) {
            debug!(
                tx = %tx.txid(),
                weight = tx.weight(),
                output = ?tx.output,
                "Broadcasting peg-out",
            );
            trace!(transaction = ?tx);
            rpc.submit_transaction(tx).await;
        }
    }
}

struct StatelessWallet<'a> {
    descriptor: &'a Descriptor<CompressedPublicKey>,
    secret_key: &'a secp256k1::SecretKey,
    secp: &'a secp256k1::Secp256k1<secp256k1::All>,
}

impl<'a> StatelessWallet<'a> {
    /// Given a tx created from an `WalletOutput`, validate there will be no
    /// issues submitting the transaction to the Bitcoin network
    fn validate_tx(
        &self,
        tx: &UnsignedTransaction,
        output: &WalletOutputV0,
        consensus_fee_rate: Feerate,
        network: Network,
    ) -> Result<(), WalletOutputError> {
        if let WalletOutputV0::PegOut(peg_out) = output {
            if !peg_out.recipient.is_valid_for_network(network) {
                return Err(WalletOutputError::WrongNetwork(
                    network,
                    peg_out.recipient.network,
                ));
            }
        }

        // Validate the tx amount is over the dust limit
        if tx.peg_out_amount < tx.destination.dust_value() {
            return Err(WalletOutputError::PegOutUnderDustLimit);
        }

        // Validate tx fee rate is above the consensus fee rate
        if tx.fees.fee_rate < consensus_fee_rate {
            return Err(WalletOutputError::PegOutFeeBelowConsensus(
                tx.fees.fee_rate,
                consensus_fee_rate,
            ));
        }

        // Validate added fees are above the min relay tx fee
        // BIP-0125 requires 1 sat/vb for RBF by default (same as normal txs)
        let fees = match output {
            WalletOutputV0::PegOut(pegout) => pegout.fees,
            WalletOutputV0::Rbf(rbf) => rbf.fees,
        };
        if fees.fee_rate.sats_per_kvb < DEFAULT_MIN_RELAY_TX_FEE as u64 {
            return Err(WalletOutputError::BelowMinRelayFee);
        }

        // Validate fees weight matches the actual weight
        if fees.total_weight != tx.fees.total_weight {
            return Err(WalletOutputError::TxWeightIncorrect(
                fees.total_weight,
                tx.fees.total_weight,
            ));
        }

        Ok(())
    }

    /// Attempts to create a tx ready to be signed from available UTXOs.
    //
    // * `peg_out_amount`: How much the peg-out should be
    // * `destination`: The address the user is pegging-out to
    // * `included_utxos`: UXTOs that must be included (for RBF)
    // * `remaining_utxos`: All other spendable UXTOs
    // * `fee_rate`: How much needs to be spent on fees
    // * `change_tweak`: How the federation can recognize it's change UTXO
    // * `rbf`: If this is an RBF transaction
    #[allow(clippy::too_many_arguments)]
    fn create_tx(
        &self,
        peg_out_amount: bitcoin::Amount,
        destination: Script,
        mut included_utxos: Vec<(UTXOKey, SpendableUTXO)>,
        mut remaining_utxos: Vec<(UTXOKey, SpendableUTXO)>,
        mut fee_rate: Feerate,
        change_tweak: &[u8; 33],
        rbf: Option<Rbf>,
    ) -> Result<UnsignedTransaction, WalletOutputError> {
        // Add the rbf fees to the existing tx fees
        if let Some(rbf) = &rbf {
            fee_rate.sats_per_kvb += rbf.fees.fee_rate.sats_per_kvb;
        }

        // When building a transaction we need to take care of two things:
        //  * We need enough input amount to fund all outputs
        //  * We need to keep an eye on the tx weight so we can factor the fees into out
        //    calculation
        // We then go on to calculate the base size of the transaction `total_weight`
        // and the maximum weight per added input which we will add every time
        // we select an input.
        let change_script = self.derive_script(change_tweak);
        let out_weight = (destination.len() * 4 + 1 + 32
            // Add change script weight, it's very likely to be needed if not we just overpay in fees
            + 1 // script len varint, 1 byte for all addresses we accept
            + change_script.len() * 4 // script len
            + 32) as u64; // value
        let mut total_weight = 16 + // version
            12 + // up to 2**16-1 inputs
            12 + // up to 2**16-1 outputs
            out_weight + // weight of all outputs
            16; // lock time
        let max_input_weight = (self
            .descriptor
            .max_satisfaction_weight()
            .expect("is satisfyable") +
            128 + // TxOutHash
            16 + // TxOutIndex
            16) as u64; // sequence

        // Ensure deterministic ordering of UTXOs for all peers
        included_utxos.sort_by_key(|(_, utxo)| utxo.amount);
        remaining_utxos.sort_by_key(|(_, utxo)| utxo.amount);
        included_utxos.extend(remaining_utxos);

        // Finally we initialize our accumulator for selected input amounts
        let mut total_selected_value = bitcoin::Amount::from_sat(0);
        let mut selected_utxos: Vec<(UTXOKey, SpendableUTXO)> = vec![];
        let mut fees = fee_rate.calculate_fee(total_weight);

        while total_selected_value < peg_out_amount + change_script.dust_value() + fees {
            match included_utxos.pop() {
                Some((utxo_key, utxo)) => {
                    total_selected_value += utxo.amount;
                    total_weight += max_input_weight;
                    fees = fee_rate.calculate_fee(total_weight);
                    selected_utxos.push((utxo_key, utxo));
                }
                _ => return Err(WalletOutputError::NotEnoughSpendableUTXO), // Not enough UTXOs
            }
        }

        // We always pay ourselves change back to ensure that we don't lose anything due
        // to dust
        let change = total_selected_value - fees - peg_out_amount;
        let output: Vec<TxOut> = vec![
            TxOut {
                value: peg_out_amount.to_sat(),
                script_pubkey: destination.clone(),
            },
            TxOut {
                value: change.to_sat(),
                script_pubkey: change_script,
            },
        ];
        let mut change_out = bitcoin::util::psbt::Output::default();
        change_out
            .proprietary
            .insert(proprietary_tweak_key(), change_tweak.to_vec());

        info!(
            inputs = selected_utxos.len(),
            input_sats = total_selected_value.to_sat(),
            peg_out_sats = peg_out_amount.to_sat(),
            ?total_weight,
            fees_sats = fees.to_sat(),
            fee_rate = fee_rate.sats_per_kvb,
            change_sats = change.to_sat(),
            "Creating peg-out tx",
        );

        let transaction = Transaction {
            version: 2,
            lock_time: PackedLockTime::ZERO,
            input: selected_utxos
                .iter()
                .map(|(utxo_key, _utxo)| TxIn {
                    previous_output: utxo_key.0,
                    script_sig: Default::default(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: bitcoin::Witness::new(),
                })
                .collect(),
            output,
        };
        info!(txid = %transaction.txid(), "Creating peg-out tx");

        // FIXME: use custom data structure that guarantees more invariants and only
        // convert to PSBT for finalization
        let psbt = PartiallySignedTransaction {
            unsigned_tx: transaction,
            version: 0,
            xpub: Default::default(),
            proprietary: Default::default(),
            unknown: Default::default(),
            inputs: selected_utxos
                .iter()
                .map(|(_utxo_key, utxo)| {
                    let script_pubkey = self
                        .descriptor
                        .tweak(&utxo.tweak, self.secp)
                        .script_pubkey();
                    Input {
                        non_witness_utxo: None,
                        witness_utxo: Some(TxOut {
                            value: utxo.amount.to_sat(),
                            script_pubkey,
                        }),
                        partial_sigs: Default::default(),
                        sighash_type: None,
                        redeem_script: None,
                        witness_script: Some(
                            self.descriptor
                                .tweak(&utxo.tweak, self.secp)
                                .script_code()
                                .expect("Failed to tweak descriptor"),
                        ),
                        bip32_derivation: Default::default(),
                        final_script_sig: None,
                        final_script_witness: None,
                        ripemd160_preimages: Default::default(),
                        sha256_preimages: Default::default(),
                        hash160_preimages: Default::default(),
                        hash256_preimages: Default::default(),
                        proprietary: vec![(proprietary_tweak_key(), utxo.tweak.to_vec())]
                            .into_iter()
                            .collect(),
                        tap_key_sig: Default::default(),
                        tap_script_sigs: Default::default(),
                        tap_scripts: Default::default(),
                        tap_key_origins: Default::default(),
                        tap_internal_key: Default::default(),
                        tap_merkle_root: Default::default(),
                        unknown: Default::default(),
                    }
                })
                .collect(),
            outputs: vec![Default::default(), change_out],
        };

        Ok(UnsignedTransaction {
            psbt,
            signatures: vec![],
            change,
            fees: PegOutFees {
                fee_rate,
                total_weight,
            },
            destination,
            selected_utxos,
            peg_out_amount,
            rbf,
        })
    }

    fn sign_psbt(&self, psbt: &mut PartiallySignedTransaction) {
        let mut tx_hasher = SighashCache::new(&psbt.unsigned_tx);

        for (idx, (psbt_input, _tx_input)) in psbt
            .inputs
            .iter_mut()
            .zip(psbt.unsigned_tx.input.iter())
            .enumerate()
        {
            let tweaked_secret = {
                let tweak = psbt_input
                    .proprietary
                    .get(&proprietary_tweak_key())
                    .expect("Malformed PSBT: expected tweak");

                self.secret_key.tweak(tweak, self.secp)
            };

            let tx_hash = tx_hasher
                .segwit_signature_hash(
                    idx,
                    psbt_input
                        .witness_script
                        .as_ref()
                        .expect("Missing witness script"),
                    psbt_input
                        .witness_utxo
                        .as_ref()
                        .expect("Missing UTXO")
                        .value,
                    EcdsaSighashType::All,
                )
                .expect("Failed to create segwit sighash");

            let signature = self
                .secp
                .sign_ecdsa(&Message::from_slice(&tx_hash[..]).unwrap(), &tweaked_secret);

            psbt_input.partial_sigs.insert(
                bitcoin::PublicKey {
                    compressed: true,
                    inner: secp256k1::PublicKey::from_secret_key(self.secp, &tweaked_secret),
                },
                EcdsaSig::sighash_all(signature),
            );
        }
    }

    fn derive_script(&self, tweak: &[u8]) -> Script {
        struct CompressedPublicKeyTranslator<'t, 's, Ctx: Verification> {
            tweak: &'t [u8],
            secp: &'s Secp256k1<Ctx>,
        }

        impl<'t, 's, Ctx: Verification>
            miniscript::Translator<CompressedPublicKey, CompressedPublicKey, Infallible>
            for CompressedPublicKeyTranslator<'t, 's, Ctx>
        {
            fn pk(&mut self, pk: &CompressedPublicKey) -> Result<CompressedPublicKey, Infallible> {
                let hashed_tweak = {
                    let mut hasher = HmacEngine::<sha256::Hash>::new(&pk.key.serialize()[..]);
                    hasher.input(self.tweak);
                    Hmac::from_engine(hasher).into_inner()
                };

                Ok(CompressedPublicKey {
                    key: pk
                        .key
                        .add_exp_tweak(
                            self.secp,
                            &Scalar::from_be_bytes(hashed_tweak).expect("can't fail"),
                        )
                        .expect("tweaking failed"),
                })
            }
            translate_hash_fail!(CompressedPublicKey, CompressedPublicKey, Infallible);
        }

        let descriptor = self
            .descriptor
            .translate_pk(&mut CompressedPublicKeyTranslator {
                tweak,
                secp: self.secp,
            })
            .expect("can't fail");

        descriptor.script_pubkey()
    }
}

pub fn nonce_from_idx(nonce_idx: u64) -> [u8; 33] {
    let mut nonce: [u8; 33] = [0; 33];
    // Make it look like a compressed pubkey, has to be either 0x02 or 0x03
    nonce[0] = 0x02;
    nonce[1..].copy_from_slice(&nonce_idx.consensus_hash::<sha256::Hash>()[..]);

    nonce
}

#[cfg(test)]
mod tests {

    use std::str::FromStr;

    use bitcoin::Network::{Bitcoin, Testnet};
    use bitcoin::{Address, Amount, Network, OutPoint, Txid};
    use fedimint_core::{BitcoinHash, Feerate};
    use fedimint_wallet_common::{PegOut, PegOutFees, Rbf, WalletOutputV0};
    use miniscript::descriptor::Wsh;

    use crate::common::PegInDescriptor;
    use crate::{
        CompressedPublicKey, OsRng, SpendableUTXO, StatelessWallet, UTXOKey, WalletOutputError,
    };

    #[test]
    fn create_tx_should_validate_amounts() {
        let secp = secp256k1::Secp256k1::new();

        let descriptor = PegInDescriptor::Wsh(
            Wsh::new_sortedmulti(
                3,
                (0..4)
                    .map(|_| secp.generate_keypair(&mut OsRng))
                    .map(|(_, key)| CompressedPublicKey { key })
                    .collect(),
            )
            .unwrap(),
        );

        let (secret_key, _) = secp.generate_keypair(&mut OsRng);

        let wallet = StatelessWallet {
            descriptor: &descriptor,
            secret_key: &secret_key,
            secp: &secp,
        };

        let spendable = SpendableUTXO {
            tweak: [0; 33],
            amount: Amount::from_sat(3000),
        };

        let recipient = Address::from_str("32iVBEu4dxkUQk9dJbZUiBiQdmypcEyJRf").unwrap();

        let fee = Feerate { sats_per_kvb: 1000 };
        let weight = 875;

        // not enough SpendableUTXO
        // tx fee = ceil(875 / 4) * 1 sat/vb = 219
        // change script dust = 330
        // spendable sats = 3000 - 219 - 330 = 2451
        let tx = wallet.create_tx(
            Amount::from_sat(2452),
            recipient.script_pubkey(),
            vec![],
            vec![(UTXOKey(OutPoint::null()), spendable.clone())],
            fee,
            &[0; 33],
            None,
        );
        assert_eq!(tx, Err(WalletOutputError::NotEnoughSpendableUTXO));

        // successful tx creation
        let mut tx = wallet
            .create_tx(
                Amount::from_sat(1000),
                recipient.script_pubkey(),
                vec![],
                vec![(UTXOKey(OutPoint::null()), spendable)],
                fee,
                &[0; 33],
                None,
            )
            .expect("is ok");

        // peg out weight is incorrectly set to 0
        let res = wallet.validate_tx(&tx, &rbf(fee.sats_per_kvb, 0), fee, Network::Bitcoin);
        assert_eq!(res, Err(WalletOutputError::TxWeightIncorrect(0, weight)));

        // fee rate set below min relay fee to 0
        let res = wallet.validate_tx(&tx, &rbf(0, weight), fee, Bitcoin);
        assert_eq!(res, Err(WalletOutputError::BelowMinRelayFee));

        // fees are okay
        let res = wallet.validate_tx(&tx, &rbf(fee.sats_per_kvb, weight), fee, Bitcoin);
        assert_eq!(res, Ok(()));

        // tx has fee below consensus
        tx.fees = PegOutFees::new(0, weight);
        let res = wallet.validate_tx(&tx, &rbf(fee.sats_per_kvb, weight), fee, Bitcoin);
        assert_eq!(
            res,
            Err(WalletOutputError::PegOutFeeBelowConsensus(
                Feerate { sats_per_kvb: 0 },
                fee
            ))
        );

        // tx has peg-out amount under dust limit
        tx.peg_out_amount = Amount::ZERO;
        let res = wallet.validate_tx(&tx, &rbf(fee.sats_per_kvb, weight), fee, Bitcoin);
        assert_eq!(res, Err(WalletOutputError::PegOutUnderDustLimit));

        // tx is invalid for network
        let output = WalletOutputV0::PegOut(PegOut {
            recipient,
            amount: Amount::from_sat(1000),
            fees: PegOutFees::new(100, weight),
        });
        let res = wallet.validate_tx(&tx, &output, fee, Testnet);
        assert_eq!(res, Err(WalletOutputError::WrongNetwork(Testnet, Bitcoin)));
    }

    fn rbf(sats_per_kvb: u64, total_weight: u64) -> WalletOutputV0 {
        WalletOutputV0::Rbf(Rbf {
            fees: PegOutFees::new(sats_per_kvb, total_weight),
            txid: Txid::all_zeros(),
        })
    }
}

#[cfg(test)]
mod fedimint_migration_tests {
    use anyhow::{ensure, Context};
    use bitcoin::psbt::{Input, PartiallySignedTransaction};
    use bitcoin::{
        Amount, BlockHash, PackedLockTime, Script, Sequence, Transaction, TxIn, TxOut, Txid,
        WPubkeyHash,
    };
    use fedimint_core::core::LEGACY_HARDCODED_INSTANCE_ID_WALLET;
    use fedimint_core::db::{
        apply_migrations, DatabaseTransaction, IDatabaseTransactionOpsCoreTyped,
    };
    use fedimint_core::module::registry::ModuleDecoderRegistry;
    use fedimint_core::module::{CommonModuleInit, DynServerModuleInit};
    use fedimint_core::{BitcoinHash, Feerate, OutPoint, PeerId, ServerModule, TransactionId};
    use fedimint_testing::db::{
        prepare_db_migration_snapshot, validate_migrations, BYTE_20, BYTE_32, BYTE_33,
    };
    use fedimint_wallet_common::db::{
        BlockCountVoteKey, BlockCountVotePrefix, BlockHashKey, BlockHashKeyPrefix, DbKeyPrefix,
        FeeRateVoteKey, FeeRateVotePrefix, PegOutBitcoinTransaction,
        PegOutBitcoinTransactionPrefix, PegOutNonceKey, PegOutTxSignatureCI,
        PegOutTxSignatureCIPrefix, PendingTransactionKey, PendingTransactionPrefixKey, UTXOKey,
        UTXOPrefixKey, UnsignedTransactionKey, UnsignedTransactionPrefixKey,
    };
    use fedimint_wallet_common::{
        PegOutFees, PendingTransaction, Rbf, SpendableUTXO, UnsignedTransaction, WalletCommonInit,
        WalletOutputOutcome,
    };
    use futures::StreamExt;
    use rand::rngs::OsRng;
    use secp256k1::Message;
    use strum::IntoEnumIterator;

    use crate::{Wallet, WalletInit};

    /// Create a database with version 0 data. The database produced is not
    /// intended to be real data or semantically correct. It is only
    /// intended to provide coverage when reading the database
    /// in future code versions. This function should not be updated when
    /// database keys/values change - instead a new function should be added
    /// that creates a new database backup that can be tested.
    async fn create_db_with_v0_data(mut dbtx: DatabaseTransaction<'_>) {
        dbtx.insert_new_entry(&BlockHashKey(BlockHash::from_slice(&BYTE_32).unwrap()), &())
            .await;

        let utxo = UTXOKey(bitcoin::OutPoint {
            txid: Txid::from_slice(&BYTE_32).unwrap(),
            vout: 0,
        });
        let spendable_utxo = SpendableUTXO {
            tweak: BYTE_33,
            amount: Amount::from_sat(10000),
        };

        dbtx.insert_new_entry(&utxo, &spendable_utxo).await;

        dbtx.insert_new_entry(&PegOutNonceKey, &1).await;

        dbtx.insert_new_entry(&BlockCountVoteKey(PeerId::from(0)), &1)
            .await;

        dbtx.insert_new_entry(
            &FeeRateVoteKey(PeerId::from(0)),
            &Feerate { sats_per_kvb: 10 },
        )
        .await;

        let unsigned_transaction_key = UnsignedTransactionKey(Txid::from_slice(&BYTE_32).unwrap());

        let selected_utxos: Vec<(UTXOKey, SpendableUTXO)> = vec![(utxo.clone(), spendable_utxo)];

        let destination = Script::new_v0_p2wpkh(&WPubkeyHash::from_slice(&BYTE_20).unwrap());
        let output: Vec<TxOut> = vec![TxOut {
            value: 10000,
            script_pubkey: destination.clone(),
        }];

        let transaction = Transaction {
            version: 2,
            lock_time: PackedLockTime::ZERO,
            input: vec![TxIn {
                previous_output: utxo.0,
                script_sig: Default::default(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: bitcoin::Witness::new(),
            }],
            output,
        };

        let inputs = vec![Input {
            non_witness_utxo: None,
            witness_utxo: Some(TxOut {
                value: 10000,
                script_pubkey: destination.clone(),
            }),
            partial_sigs: Default::default(),
            sighash_type: None,
            redeem_script: None,
            witness_script: Some(destination.clone()),
            bip32_derivation: Default::default(),
            final_script_sig: None,
            final_script_witness: None,
            ripemd160_preimages: Default::default(),
            sha256_preimages: Default::default(),
            hash160_preimages: Default::default(),
            hash256_preimages: Default::default(),
            proprietary: Default::default(),
            tap_key_sig: Default::default(),
            tap_script_sigs: Default::default(),
            tap_scripts: Default::default(),
            tap_key_origins: Default::default(),
            tap_internal_key: Default::default(),
            tap_merkle_root: Default::default(),
            unknown: Default::default(),
        }];

        let psbt = PartiallySignedTransaction {
            unsigned_tx: transaction.clone(),
            version: 0,
            xpub: Default::default(),
            proprietary: Default::default(),
            unknown: Default::default(),
            inputs,
            outputs: vec![Default::default()],
        };

        let unsigned_transaction = UnsignedTransaction {
            psbt,
            signatures: vec![],
            change: Amount::from_sat(0),
            fees: PegOutFees {
                fee_rate: Feerate { sats_per_kvb: 1000 },
                total_weight: 40000,
            },
            destination: destination.clone(),
            selected_utxos: selected_utxos.clone(),
            peg_out_amount: Amount::from_sat(10000),
            rbf: None,
        };

        dbtx.insert_new_entry(&unsigned_transaction_key, &unsigned_transaction)
            .await;

        let pending_transaction_key = PendingTransactionKey(Txid::from_slice(&BYTE_32).unwrap());

        let pending_tx = PendingTransaction {
            tx: transaction,
            tweak: BYTE_33,
            change: Amount::from_sat(0),
            destination,
            fees: PegOutFees {
                fee_rate: Feerate { sats_per_kvb: 1000 },
                total_weight: 40000,
            },
            selected_utxos: selected_utxos.clone(),
            peg_out_amount: Amount::from_sat(10000),
            rbf: Some(Rbf {
                fees: PegOutFees {
                    fee_rate: Feerate { sats_per_kvb: 1000 },
                    total_weight: 40000,
                },
                txid: Txid::from_slice(&BYTE_32).unwrap(),
            }),
        };
        dbtx.insert_new_entry(&pending_transaction_key, &pending_tx)
            .await;

        let (sk, _) = secp256k1::generate_keypair(&mut OsRng);
        let secp = secp256k1::Secp256k1::new();
        let signature = secp.sign_ecdsa(&Message::from_slice(&BYTE_32).unwrap(), &sk);
        dbtx.insert_new_entry(
            &PegOutTxSignatureCI(Txid::from_slice(&BYTE_32).unwrap()),
            &vec![signature],
        )
        .await;

        let peg_out_bitcoin_tx = PegOutBitcoinTransaction(OutPoint {
            txid: TransactionId::from_slice(&BYTE_32).unwrap(),
            out_idx: 0,
        });

        dbtx.insert_new_entry(
            &peg_out_bitcoin_tx,
            &WalletOutputOutcome::new_v0(Txid::from_slice(&BYTE_32).unwrap()),
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prepare_db_migration_snapshots() -> anyhow::Result<()> {
        prepare_db_migration_snapshot(
            "wallet-v0",
            |dbtx| {
                Box::pin(async move {
                    create_db_with_v0_data(dbtx).await;
                })
            },
            ModuleDecoderRegistry::from_iter([(
                LEGACY_HARDCODED_INSTANCE_ID_WALLET,
                WalletCommonInit::KIND,
                <Wallet as ServerModule>::decoder(),
            )]),
        )
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_migrations() -> anyhow::Result<()> {
        validate_migrations(
            "wallet",
            |db| async move {
                let module = DynServerModuleInit::from(WalletInit);
                apply_migrations(
                    &db,
                    module.module_kind().to_string(),
                    module.database_version(),
                    module.get_database_migrations(),
                )
                .await
                .context("Error applying migrations to temp database")?;

                // Verify that all of the data from the wallet namespace can be read. If a
                // database migration failed or was not properly supplied,
                // the struct will fail to be read.
                let mut dbtx = db.begin_transaction().await;

                for prefix in DbKeyPrefix::iter() {
                    match prefix {
                        DbKeyPrefix::BlockHash => {
                            let blocks = dbtx
                                .find_by_prefix(&BlockHashKeyPrefix)
                                .await
                                .collect::<Vec<_>>()
                                .await;
                            let num_blocks = blocks.len();
                            ensure!(
                                num_blocks > 0,
                                "validate_migrations was not able to read any BlockHashes"
                            );
                        }
                        DbKeyPrefix::PegOutBitcoinOutPoint => {
                            let outpoints = dbtx
                                .find_by_prefix(&PegOutBitcoinTransactionPrefix)
                                .await
                                .collect::<Vec<_>>()
                                .await;
                            let num_outpoints = outpoints.len();
                            ensure!(
                                num_outpoints > 0,
                                "validate_migrations was not able to read any PegOutBitcoinTransactions"
                            );
                        }
                        DbKeyPrefix::PegOutTxSigCi => {
                            let sigs = dbtx
                                .find_by_prefix(&PegOutTxSignatureCIPrefix)
                                .await
                                .collect::<Vec<_>>()
                                .await;
                            let num_sigs = sigs.len();
                            ensure!(
                                num_sigs > 0,
                                "validate_migrations was not able to read any PegOutTxSigCi"
                            );
                        }
                        DbKeyPrefix::PendingTransaction => {
                            let pending_txs = dbtx
                                .find_by_prefix(&PendingTransactionPrefixKey)
                                .await
                                .collect::<Vec<_>>()
                                .await;
                            let num_txs = pending_txs.len();
                            ensure!(
                                num_txs > 0,
                                "validate_migrations was not able to read any PendingTransactions"
                            );
                        }
                        DbKeyPrefix::PegOutNonce => {
                            ensure!(dbtx
                                .get_value(&PegOutNonceKey)
                                .await
                                .is_some());
                        }
                        DbKeyPrefix::UnsignedTransaction => {
                            let unsigned_txs = dbtx
                                .find_by_prefix(&UnsignedTransactionPrefixKey)
                                .await
                                .collect::<Vec<_>>()
                                .await;
                            let num_txs = unsigned_txs.len();
                            ensure!(
                                num_txs > 0,
                                "validate_migrations was not able to read any UnsignedTransactions"
                            );
                        }
                        DbKeyPrefix::Utxo => {
                            let utxos = dbtx
                                .find_by_prefix(&UTXOPrefixKey)
                                .await
                                .collect::<Vec<_>>()
                                .await;
                            let num_utxos = utxos.len();
                            ensure!(
                                num_utxos > 0,
                                "validate_migrations was not able to read any UTXOs"
                            );
                        }
                        DbKeyPrefix::BlockCountVote => {
                            let heights = dbtx
                                .find_by_prefix(&BlockCountVotePrefix)
                                .await
                                .collect::<Vec<_>>()
                                .await;
                            let num_heights = heights.len();
                            ensure!(
                                num_heights > 0,
                                "validate_migrations was not able to read any block height votes"
                            );
                        }
                        DbKeyPrefix::FeeRateVote => {
                            let rates = dbtx
                                .find_by_prefix(&FeeRateVotePrefix)
                                .await
                                .collect::<Vec<_>>()
                                .await;
                            let num_rates = rates.len();
                            ensure!(
                                num_rates > 0,
                                "validate_migrations was not able to read any fee rate votes"
                            );
                        }
                    }
                }
                Ok(())
            },
            ModuleDecoderRegistry::from_iter([(
                LEGACY_HARDCODED_INSTANCE_ID_WALLET,
                WalletCommonInit::KIND,
                <Wallet as ServerModule>::decoder(),
            )]),
        )
        .await
    }
}
