use std::{collections::BTreeMap, fmt::Debug, str::FromStr, time::Duration};
use std::ops::DerefMut;
use std::sync::Arc;
use bytesize::ByteSize;
use frame_try_runtime::UpgradeCheckSelect;
use log::Level;
use parity_scale_codec::Encode;
use sc_executor::sp_wasm_interface::HostFunctions;
use sp_core::{hexdisplay::HexDisplay, Hasher, H256};
use sp_runtime::{
    traits::{Block as BlockT, HashingFor, NumberFor},
    DeserializeOwned,
};
use sp_runtime::ExtrinsicInclusionMode;
use sp_core::twox_128;
use parity_scale_codec::Codec;
use sp_state_machine::TestExternalities;
use sp_state_machine::{CompactProof, OverlayedChanges, StorageProof};
use tokio::sync::Mutex;

use crate::{
    common::{
        empty_block::{inherents::providers::ProviderVariant, production::mine_block, production::core_version},
        misc_logging::basti_log,
        state::{build_executor, state_machine_call_with_proof, RuntimeChecks, State},
    },
	commands::on_runtime_upgrade::Command,
    RefTimeInfo, SharedParams, LOG_TARGET,
};

pub struct MbmChecker<Block, HostFns> {
	pub command: Command,
	pub shared: SharedParams,
	pub runtime_checks: RuntimeChecks,
	pub _phantom: core::marker::PhantomData<(Block, HostFns)>
}

impl<Block, HostFns> MbmChecker<Block, HostFns>
where 
	Block: BlockT<Hash = H256> + DeserializeOwned,
	Block::Header: DeserializeOwned,
	<Block::Hash as FromStr>::Err: Debug,
	NumberFor<Block>: FromStr,
	<NumberFor<Block> as FromStr>::Err: Debug,
	HostFns: HostFunctions,
{
	pub async fn check_mbms(&self) -> sc_cli::Result<()> {
		basti_log(
			Level::Info,
			&format!(
				"🔬 Running Multi-Block-Migrations with checks: {:?}",
				self.command.checks
			)
		);

		let executor = build_executor(&self.shared);
		let ext = self.command
			.state
			.to_ext::<Block, HostFns>(&self.shared, &executor, None, self.runtime_checks)
			.await?;

		if core_version::<Block, HostFns>(&ext, &executor)? < 5 {
			return Err("Your runtime does not support Multi-Block-Migrations. Please disable the check with `--mbms false` or update your runtime.".into());
		}

		let inner_ext = Arc::new(Mutex::new(ext.inner_ext));
		let mut parent_header = ext.header.clone();
		let mut parent_block_building_info = None;
		let provider_variant = ProviderVariant::Smart(Duration::from_millis(self.command.blocktime));
		let mut n = 0;

		let mut ext_guard = inner_ext.lock().await;
    	let ext = ext_guard.deref_mut();
		Self::modify_spec_name(ext).await?;
		drop(ext_guard);

		// This actually runs the MBMs block by block:
		loop {
			let (next_block_building_info, next_header, mode) = mine_block::<Block, HostFns>(
				inner_ext.clone(),
				&executor,
				parent_block_building_info,
				parent_header.clone(),
				provider_variant,
				frame_try_runtime::TryStateSelect::None,
			)
			.await?;

			parent_block_building_info = Some(next_block_building_info);
			parent_header = next_header;
			// The first block does not yet have the MBMs enabled.
			let first_is_free = n == 0;

			if first_is_free || Self::poll_mbms_ongoing(mode, inner_ext.clone()).await {
				n += 1;
				log::info!(target: LOG_TARGET, "MBM ongoing for {n} blocks");
			} else if n >= self.command.mbm_max_blocks {
				log::error!(target: LOG_TARGET, "MBM reached its maximum number of allowed blocks after {} blocks", n);
				return Err("MBM max blocks reached".into());
			} else {
				log::info!(target: LOG_TARGET, "MBM finished after {n} blocks");
				break;
			}
		}

		let mut ext_guard = inner_ext.lock().await;
    	let ext = ext_guard.deref_mut();

		let _ = state_machine_call_with_proof::<Block, HostFns>(
			&ext,
			&mut Default::default(),
			&executor,
			"TryRuntime_on_runtime_upgrade",
			self.command.checks.encode().as_ref(),
			Default::default(), // TODO
			None,
		)?;

		Ok(())
	}

	/// Modify up the spec name in storage such that the `was_upgraded` check will always return true because of changes spec name.
	async fn modify_spec_name<H>(ext: &mut TestExternalities<H>) -> sc_cli::Result<()>
	where
		H: Hasher + 'static,
		H::Out: Codec + Ord
	{
		let key = [twox_128(b"System"), twox_128(b"LastRuntimeUpgrade")].concat();
		let version = frame_system::LastRuntimeUpgradeInfo {
			spec_version: 0.into(),
			spec_name: "test".into(),
		};
		//sp_io::storage::set(&key, &version.encode());
		ext.execute_with(|| {
			sp_io::storage::set(&key, &version.encode());
		});
		ext.commit_all().unwrap();

		Ok(())
	}

	/// Are there any Multi-Block-Migrations ongoing?
	async fn poll_mbms_ongoing<H>(mode: Option<ExtrinsicInclusionMode>, ext_mutex: std::sync::Arc<Mutex<TestExternalities<H>>>) -> bool
	where
		H: Hasher + 'static,
		H::Out: Codec + Ord
	{
		if mode == Some(ExtrinsicInclusionMode::OnlyInherents) {
			return true;
		}

		let mut ext_guard = ext_mutex.lock().await;
		let ext = ext_guard.deref_mut();

		ext.execute_with(|| {
			let mbm_in_progress_key = [twox_128(b"MultiBlockMigrations"), twox_128(b"Cursor")].concat();
			let mbm_in_progress = sp_io::storage::get(&mbm_in_progress_key).unwrap_or_default();
			
			mbm_in_progress.len() > 0
		})
	}
}
