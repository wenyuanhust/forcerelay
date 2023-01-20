use ckb_jsonrpc_types::Status;
use ckb_types::H256;
use eth2_types::EthSpec;
use eth_light_client_in_ckb_verification::mmr::{self, HeaderWithCache};
use eth_light_client_in_ckb_verification::types::{
    core::{Client as EthLcClient, Header as EthLcHeader},
    packed::{self, Client as PackedClient, ProofUpdate as PackedProofUpdate},
    prelude::*,
};
use ibc_relayer_storage::{
    error::Error as StorageError,
    prelude::{StorageAsMMRStore, StorageReader, StorageWriter},
    Slot,
};
use ibc_relayer_types::clients::ics07_eth::types::{Header as EthHeader, Update as EthUpdate};
use std::sync::Arc;
use std::time::Duration;
use tendermint_light_client::errors::Error as LightClientError;
use tracing::debug;

use crate::chain::ckb::communication::CkbReader;
use crate::error::Error;

use super::rpc_client::RpcClient;

fn into_height(slot: u64) -> tendermint::block::Height {
    slot.try_into().expect("slot too big")
}

fn into_cached_headers(header_updates: &[EthUpdate]) -> Vec<HeaderWithCache> {
    header_updates
        .iter()
        .map(|update| {
            let EthHeader {
                slot,
                proposer_index,
                parent_root,
                state_root,
                body_root,
            } = update.finalized_header.clone();
            let header = EthLcHeader {
                slot,
                proposer_index,
                parent_root,
                state_root,
                body_root,
            };
            header.calc_cache()
        })
        .collect::<Vec<_>>()
}

fn commit_headers_into_mmr_storage<S, E>(
    finalized_headers: &Vec<HeaderWithCache>,
    storage: &S,
) -> Result<(), Error>
where
    S: StorageWriter<E> + StorageAsMMRStore<E>,
    E: EthSpec,
{
    if finalized_headers.is_empty() {
        return Ok(());
    }

    let mut finalized_headers_iter = finalized_headers.iter();
    let mut last_slot = if storage.is_initialized()? {
        finalized_headers[0].inner.slot - 1
    } else {
        let first = finalized_headers_iter.next().expect("checked");
        storage.initialize_with(first.inner.slot, first.digest())?;
        storage.put_tip_beacon_header_slot(first.inner.slot)?;
        first.inner.slot
    };

    let mut mmr = storage.chain_root_mmr(last_slot)?;
    for header in finalized_headers_iter {
        last_slot = header.inner.slot;
        mmr.push(header.digest()).map_err(StorageError::from)?;
    }
    mmr.commit().map_err(StorageError::from)?;

    storage.put_tip_beacon_header_slot(last_slot)?;
    Ok(())
}

pub fn align_native_and_onchain_updates<S, E>(
    chain_id: &str,
    header_updates: &Vec<EthUpdate>,
    storage: &S,
    onchain_packed_client: &PackedClient,
) -> Result<(), Error>
where
    S: StorageReader<E> + StorageWriter<E> + StorageAsMMRStore<E>,
    E: EthSpec,
{
    if header_updates.is_empty() {
        return Err(Error::empty_upgraded_client_state());
    }

    // prepare minimal and maximal slots from onchain client
    let onchain_minimal_slot = onchain_packed_client.minimal_slot().unpack();
    let onchain_maximal_slot = onchain_packed_client.maximal_slot().unpack();

    // check stored base slot is less than the onchain slot
    if let Some(stored_base_slot) = storage.get_base_beacon_header_slot()? {
        // unrecoverable condition
        if stored_base_slot > onchain_minimal_slot {
            return Err(Error::light_client_verification(
                chain_id.to_owned(),
                LightClientError::target_lower_than_trusted_state(
                    into_height(onchain_minimal_slot),
                    into_height(stored_base_slot),
                ),
            ));
        }
    }

    let finalized_headers = into_cached_headers(header_updates);
    let upcoming_start_slot = header_updates[0].finalized_header.slot;
    let upcoming_last_slot = header_updates.last().unwrap().finalized_header.slot;

    // check stored tip slot is less or greator than the onchain slot
    if let Some(mut stored_tip_slot) = storage.get_tip_beacon_header_slot()? {
        // recoverable condition: need to make native slots chase to onchain maximal slot
        if stored_tip_slot < onchain_maximal_slot {
            if upcoming_start_slot == stored_tip_slot + 1 {
                commit_headers_into_mmr_storage(&finalized_headers, storage)?;
                debug!(
                    "headers from {} to {} are aligned to storage",
                    upcoming_start_slot, upcoming_last_slot
                );
                stored_tip_slot = storage
                    .get_tip_beacon_header_slot()?
                    .expect("reacquire stored tip slot");
            } else {
                return Err(Error::light_client_verification(
                    chain_id.to_owned(),
                    LightClientError::missing_last_block_id(into_height(stored_tip_slot + 1)),
                ));
            }
        }
        // recoverable condition: need to make native tip slot chases to onchain maximal slot
        if stored_tip_slot < onchain_maximal_slot {
            return Err(Error::light_client_verification(
                chain_id.to_owned(),
                LightClientError::missing_last_block_id(into_height(stored_tip_slot + 1)),
            ));
        }
    } else {
        // recoverable condition: empty native slots need to be recovered according to the onchain slots
        if upcoming_start_slot == onchain_minimal_slot {
            commit_headers_into_mmr_storage(&finalized_headers, storage)?;
            debug!(
                "headers from {} to {} are aligned to storage (initialize)",
                upcoming_start_slot, upcoming_last_slot
            );
        } else {
            return Err(Error::light_client_verification(
                chain_id.to_owned(),
                LightClientError::missing_last_block_id(into_height(onchain_minimal_slot)),
            ));
        }
        let stored_tip_slot = storage
            .get_tip_beacon_header_slot()?
            .expect("reaquire stored tip slot");
        // recoverable condition: need to make native tip slot chases to onchain maximal slot
        if stored_tip_slot < onchain_maximal_slot {
            return Err(Error::light_client_verification(
                chain_id.to_owned(),
                LightClientError::missing_last_block_id(into_height(stored_tip_slot + 1)),
            ));
        }
    }
    Ok(())
}

pub fn get_verified_packed_client_and_proof_update<S, E>(
    chain_id: &String,
    header_updates: &Vec<EthUpdate>,
    storage: &S,
    onchain_packed_client_opt: Option<PackedClient>,
) -> Result<(Option<Slot>, PackedClient, PackedProofUpdate), Error>
where
    S: StorageReader<E> + StorageWriter<E> + StorageAsMMRStore<E>,
    E: EthSpec,
{
    let mut prev_tip_slot = None;

    if header_updates.is_empty() {
        return Err(Error::empty_upgraded_client_state());
    }

    // make sure the upcoming headers' slot are continuous
    let start_slot = header_updates[0].finalized_header.slot;
    for (i, item) in header_updates.iter().enumerate() {
        if item.finalized_header.slot != i as u64 + start_slot {
            return Err(Error::send_tx("uncontinuous header slot".to_owned()));
        }
    }

    // make sure the upcoming start slot is continuous with the onchain tip slot
    if let Some(ref client) = onchain_packed_client_opt {
        let onchain_tip_slot: u64 = client.maximal_slot().unpack();
        if start_slot != onchain_tip_slot + 1 {
            return Err(Error::light_client_verification(
                chain_id.to_string(),
                LightClientError::missing_last_block_id(into_height(onchain_tip_slot + 1)),
            ));
        }
        prev_tip_slot = Some(onchain_tip_slot);
    }

    // make sure the upcoming start slot is continuous with the stored tip slot
    if let Some(mut stored_tip_slot) = storage.get_tip_beacon_header_slot()? {
        // trim exceesive slots from storage
        if start_slot <= stored_tip_slot {
            debug!(
                "rollback stored tip slot from {} to {}",
                stored_tip_slot, start_slot
            );
            storage.rollback_to(Some(start_slot - 1))?;
            stored_tip_slot = storage
                .get_tip_beacon_header_slot()?
                .expect("reaquire stored tip slot");
        }
        assert_eq!(start_slot, stored_tip_slot + 1);
    }

    let finalized_headers = into_cached_headers(header_updates);
    let minimal_slot = storage.get_base_beacon_header_slot()?.unwrap_or(start_slot);
    let last_finalized_header = &finalized_headers[finalized_headers.len() - 1];
    let maximal_slot = last_finalized_header.inner.slot;

    // save all header digests into storage for MMR.
    commit_headers_into_mmr_storage(&finalized_headers, storage)?;

    // get the new root and a proof for all new headers.
    let (packed_headers_mmr_root, packed_headers_mmr_proof) = {
        let positions = (start_slot..=maximal_slot)
            .into_iter()
            .map(|slot| mmr::lib::leaf_index_to_pos(slot - minimal_slot))
            .collect::<Vec<_>>();

        let mmr = storage.chain_root_mmr(maximal_slot)?;

        let headers_mmr_root = mmr.get_root().map_err(StorageError::from)?;
        let headers_mmr_proof_items = mmr
            .gen_proof(positions)
            .map_err(StorageError::from)?
            .proof_items()
            .iter()
            .map(Clone::clone)
            .collect::<Vec<_>>();
        let headers_mmr_proof = packed::MmrProof::new_builder()
            .set(headers_mmr_proof_items)
            .build();

        (headers_mmr_root, headers_mmr_proof)
    };

    // build the packed proof update.
    let packed_proof_update = {
        let updates_items = finalized_headers
            .iter()
            .map(|header| {
                packed::FinalityUpdate::new_builder()
                    .finalized_header(header.inner.pack())
                    .build()
            })
            .collect::<Vec<_>>();
        let updates = packed::FinalityUpdateVec::new_builder()
            .set(updates_items)
            .build();
        packed::ProofUpdate::new_builder()
            .new_headers_mmr_root(packed_headers_mmr_root)
            .new_headers_mmr_proof(packed_headers_mmr_proof)
            .updates(updates)
            .build()
    };

    // invoke verification from core::Client on packed_proof_update
    let client = if let Some(client) = onchain_packed_client_opt {
        client
            .unpack()
            .try_apply_packed_proof_update(packed_proof_update.as_reader())
            .map_err(|_| Error::send_tx("failed to update header".to_owned()))?
    } else {
        EthLcClient::new_from_packed_proof_update(packed_proof_update.as_reader())
            .map_err(|_| Error::send_tx("failed to create header".to_owned()))?
    };

    Ok((prev_tip_slot, client.pack(), packed_proof_update))
}

pub async fn wait_ckb_transaction_committed(
    rpc: &Arc<RpcClient>,
    hash: H256,
    interval: Duration,
    confirms: u8,
) -> Result<(), Error> {
    let mut block_number = 0u64;
    loop {
        tokio::time::sleep(interval).await;
        let tx = rpc
            .get_transaction(&hash)
            .await?
            .expect("wait transaction response");
        if tx.tx_status.status == Status::Rejected {
            return Err(Error::send_tx(format!(
                "transaction {} had been rejected",
                hex::encode(hash)
            )));
        }
        if tx.tx_status.status != Status::Committed {
            continue;
        }
        if block_number == 0 {
            if let Some(block_hash) = tx.tx_status.block_hash {
                let block = rpc.get_block(&block_hash).await?;
                block_number = block.header.inner.number.into();
            }
        } else {
            let tip = rpc.get_tip_header().await?;
            let tip_number: u64 = tip.inner.number.into();
            if tip_number >= block_number + confirms as u64 {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::align_native_and_onchain_updates;
    use super::commit_headers_into_mmr_storage;
    use super::get_verified_packed_client_and_proof_update;
    use super::into_cached_headers;
    use super::EthHeader;
    use super::EthUpdate;
    use eth2_types::MainnetEthSpec;
    use eyre::Result;
    use ibc_relayer_storage::{prelude::StorageAsMMRStore, Storage};
    use std::fs;
    use tempfile::TempDir;

    use crate::error::ErrorDetail::LightClientVerification;
    use tendermint_light_client::errors::ErrorDetail::MissingLastBlockId;

    fn load_updates_from_file(path: &str) -> Result<Vec<EthUpdate>> {
        let headers_json = fs::read_to_string(path)?;
        let headers: Vec<EthHeader> = serde_json::from_str(&headers_json)?;
        Ok(headers
            .into_iter()
            .map(EthUpdate::from_finalized_header)
            .collect::<Vec<_>>())
    }

    fn prepare_essentials() -> (
        String,
        Vec<EthUpdate>,
        Vec<EthUpdate>,
        Storage<MainnetEthSpec>,
    ) {
        let chain_id = "chain_id".to_string();
        let updates_part_1 =
            load_updates_from_file("src/testdata/test_update_eth_client/headers-part-1.json")
                .expect("part_1");
        let updates_part_2 =
            load_updates_from_file("src/testdata/test_update_eth_client/headers-part-2.json")
                .expect("part_2");
        let path = TempDir::new().unwrap();
        let storage: Storage<MainnetEthSpec> = Storage::new(path).unwrap();

        (chain_id, updates_part_1, updates_part_2, storage)
    }

    #[test]
    fn test_verify_and_align_updates_with_empty_storage() {
        let (chain_id, updates_part_1, updates_part_2, storage) = prepare_essentials();

        // generate onchain packed client for later use
        let onchain_packed_client = {
            let (_, onchain_packed_client, _) = get_verified_packed_client_and_proof_update(
                &chain_id,
                &updates_part_1,
                &storage,
                None,
            )
            .expect("verify part_1");

            let (_, onchain_packed_client, _) = get_verified_packed_client_and_proof_update(
                &chain_id,
                &updates_part_2,
                &storage,
                Some(onchain_packed_client),
            )
            .expect("verify part_2");

            onchain_packed_client
        };

        // empty storage
        storage.rollback_to(None).expect("rollback");

        // test first updates alignment
        let result = align_native_and_onchain_updates(
            &chain_id,
            &updates_part_1,
            &storage,
            &onchain_packed_client,
        );
        if let Err(error) = result {
            match error.detail() {
                LightClientVerification(error) => match &error.source {
                    MissingLastBlockId(block) => {
                        let missing_slot = updates_part_1.last().unwrap().finalized_header.slot + 1;
                        assert_eq!(block.height, missing_slot.try_into().unwrap());
                    }
                    _ => panic!("unexpected error"),
                },
                _ => panic!("unexpected error"),
            }
        }

        // test next updates alignment
        align_native_and_onchain_updates(
            &chain_id,
            &updates_part_2,
            &storage,
            &onchain_packed_client,
        )
        .expect("align part_2");
    }

    #[test]
    fn test_verify_and_align_updates_with_exceesive_storage() {
        let (chain_id, updates_part_1, updates_part_2, storage) = prepare_essentials();

        // generate onchain packed client
        let (_, onchain_packed_client, _) =
            get_verified_packed_client_and_proof_update(&chain_id, &updates_part_1, &storage, None)
                .expect("verify part_1");

        // prepare exceesive full-filled storage
        let headers_part_2 = into_cached_headers(&updates_part_2);
        commit_headers_into_mmr_storage(&headers_part_2, &storage).expect("commit part_2");

        // make new update beyond the last slot from updates_part_2
        let next_update = EthUpdate {
            finalized_header: EthHeader {
                slot: updates_part_2.last().unwrap().finalized_header.slot + 1,
                ..Default::default()
            },
            ..Default::default()
        };

        align_native_and_onchain_updates(
            &chain_id,
            &vec![next_update],
            &storage,
            &onchain_packed_client,
        )
        .expect("align next_update");
    }
}
