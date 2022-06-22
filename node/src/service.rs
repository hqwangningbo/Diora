//! Service and ServiceFactory implementation. Specialized wrapper over substrate service.

// std
use std::{sync::Arc, time::Duration};

// Local Runtime Types
use diora_runtime::{
	opaque::Block, AccountId, Balance, Hash, Index as Nonce, RuntimeApi,
};

use nimbus_consensus::{
	BuildNimbusConsensusParams, NimbusConsensus, NimbusManualSealConsensusDataProvider,
};

// Cumulus Imports
use cumulus_client_cli::CollatorOptions;
use cumulus_client_consensus_common::ParachainConsensus;
use cumulus_client_network::BlockAnnounceValidator;
use cumulus_client_service::{
	prepare_node_config, start_collator, start_full_node, StartCollatorParams, StartFullNodeParams,
};
use cumulus_primitives_core::ParaId;
use cumulus_primitives_parachain_inherent::{
	MockValidationDataInherentDataProvider, MockXcmConfig,
};
use cumulus_relay_chain_inprocess_interface::build_inprocess_relay_chain;
use cumulus_relay_chain_interface::{RelayChainError, RelayChainInterface, RelayChainResult};
use cumulus_relay_chain_rpc_interface::RelayChainRPCInterface;

use polkadot_service::CollatorPair;

// Substrate Imports
use sc_consensus_manual_seal::{run_instant_seal, InstantSealParams};
use sc_executor::NativeElseWasmExecutor;
use sc_network::NetworkService;
use sc_service::{error::Error as ServiceError, Configuration, PartialComponents, Role, TFullBackend, TFullClient, TaskManager};
use sc_telemetry::{Telemetry, TelemetryHandle, TelemetryWorker, TelemetryWorkerHandle};
use sp_api::ConstructRuntimeApi;
use sp_keystore::SyncCryptoStorePtr;
use sp_runtime::traits::BlakeTwo256;
use substrate_prometheus_endpoint::Registry;

// EVM
use fc_db::DatabaseSource;
use fc_consensus::FrontierBlockImport;
use fc_mapping_sync::{MappingSyncWorker, SyncStrategy::Normal};
use fc_rpc::EthTask;
use fc_rpc_core::types::{FeeHistoryCache, FilterPool};
use futures::StreamExt;
use maplit::hashmap;
use sc_client_api::BlockchainEvents;
use sc_service::config::PrometheusConfig;
use sc_service::BasePath;
use std::{collections::BTreeMap, sync::Mutex};

/// Native executor instance.
pub struct TemplateRuntimeExecutor;

impl sc_executor::NativeExecutionDispatch for TemplateRuntimeExecutor {
	type ExtendHostFunctions = frame_benchmarking::benchmarking::HostFunctions;

	fn dispatch(method: &str, data: &[u8]) -> Option<Vec<u8>> {
		diora_runtime::api::dispatch(method, data)
	}

	fn native_version() -> sc_executor::NativeVersion {
		diora_runtime::native_version()
	}
}

type FullClient =
TFullClient<Block, RuntimeApi, NativeElseWasmExecutor<TemplateRuntimeExecutor>>;
type FullBackend = sc_service::TFullBackend<Block>;
type FullSelectChain = sc_consensus::LongestChain<FullBackend, Block>;

pub fn frontier_database_dir(config: &Configuration, path: &str) -> std::path::PathBuf {
	let config_dir = config
		.base_path
		.as_ref()
		.map(|base_path| base_path.config_dir(config.chain_spec.id()))
		.unwrap_or_else(|| {
			BasePath::from_project("", "", "diora").config_dir(config.chain_spec.id())
		});
	config_dir.join("frontier").join(path)
}

pub fn open_frontier_backend(config: &Configuration) -> Result<Arc<fc_db::Backend<Block>>, String> {
	Ok(Arc::new(fc_db::Backend::<Block>::new(
		&fc_db::DatabaseSettings {
			source: match config.database {
				DatabaseSource::RocksDb { .. } => DatabaseSource::RocksDb {
					path: frontier_database_dir(config, "db"),
					cache_size: 0,
				},
				DatabaseSource::ParityDb { .. } => DatabaseSource::ParityDb {
					path: frontier_database_dir(config, "paritydb"),
				},
				DatabaseSource::Auto { .. } => DatabaseSource::Auto {
					rocksdb_path: frontier_database_dir(config, "db"),
					paritydb_path: frontier_database_dir(config, "paritydb"),
					cache_size: 0,
				},
				_ => {
					return Err("Supported db sources: `rocksdb` | `paritydb` | `auto`".to_string())
				}
			},
		},
	)?))
}


// If we're using prometheus, use a registry with a prefix of `moonbeam`.
fn set_prometheus_registry(config: &mut Configuration) -> Result<(), ServiceError> {
	if let Some(PrometheusConfig { registry, .. }) = config.prometheus_config.as_mut() {
		let labels = hashmap! {
            "chain".into() => config.chain_spec.id().into(),
        };
		*registry = Registry::new_custom(Some("frontier".into()), Some(labels))?;
	}

	Ok(())
}


/// Starts a `ServiceBuilder` for a full service.
///
/// Use this macro if you don't actually need the full service, but just the builder in order to
/// be able to perform chain operations.
#[allow(clippy::type_complexity)]
pub fn new_partial(
	config: &Configuration,
	parachain: bool,
) -> Result<
	PartialComponents<
		FullClient,
		FullBackend,
		FullSelectChain,
		sc_consensus::DefaultImportQueue<Block, FullClient>,
		sc_transaction_pool::FullPool<Block, FullClient>,
		(
			Option<FilterPool>,
			Arc<fc_db::Backend<Block>>,
			Option<Telemetry>,
			Option<TelemetryWorkerHandle>,
			FeeHistoryCache,
		),
	>,
	sc_service::Error,
>

{
	let telemetry = config
		.telemetry_endpoints
		.clone()
		.filter(|x| !x.is_empty())
		.map(|endpoints| -> Result<_, sc_telemetry::Error> {
			let worker = TelemetryWorker::new(16)?;
			let telemetry = worker.handle().new_telemetry(endpoints);
			Ok((worker, telemetry))
		})
		.transpose()?;

	let executor = sc_executor::NativeElseWasmExecutor::<TemplateRuntimeExecutor>::new(
		config.wasm_method,
		config.default_heap_pages,
		config.max_runtime_instances,
		config.runtime_cache_size,
	);

	let (client, backend, keystore_container, task_manager) =
		sc_service::new_full_parts::<Block, RuntimeApi, _>(
			config,
			telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
			executor,
		)?;
	let client = Arc::new(client);

	let telemetry_worker_handle = telemetry.as_ref().map(|(worker, _)| worker.handle());

	let telemetry = telemetry.map(|(worker, telemetry)| {
		task_manager
			.spawn_handle()
			.spawn("telemetry", None, worker.run());
		telemetry
	});

	// Although this will not be used by the parachain collator, it will be used by the instant seal
	// And sovereign nodes, so we create it anyway.
	let select_chain = sc_consensus::LongestChain::new(backend.clone());

	let transaction_pool = sc_transaction_pool::BasicPool::new_full(
		config.transaction_pool.clone(),
		config.role.is_authority().into(),
		config.prometheus_registry(),
		task_manager.spawn_essential_handle(),
		client.clone(),
	);

	let filter_pool: Option<FilterPool> = Some(Arc::new(Mutex::new(BTreeMap::new())));
	let fee_history_cache: FeeHistoryCache = Arc::new(Mutex::new(BTreeMap::new()));
	let frontier_backend = open_frontier_backend(config)?;

	let frontier_block_import =
		FrontierBlockImport::new(client.clone(), client.clone(), frontier_backend.clone());

	let import_queue = nimbus_consensus::import_queue(
		client.clone(),
		frontier_block_import.clone(),
		move |_, _| async move {
			let time = sp_timestamp::InherentDataProvider::from_system_time();

			Ok((time,))
		},
		&task_manager.spawn_essential_handle(),
		config.prometheus_registry().clone(),
		parachain,
	)?;

	Ok(sc_service::PartialComponents {
		client,
		backend,
		task_manager,
		import_queue,
		keystore_container,
		select_chain,
		transaction_pool,
		other: (
			filter_pool,
			frontier_backend,
			telemetry,
			telemetry_worker_handle,
			fee_history_cache,
		),
	})
}

async fn build_relay_chain_interface(
	polkadot_config: Configuration,
	parachain_config: &Configuration,
	telemetry_worker_handle: Option<TelemetryWorkerHandle>,
	task_manager: &mut TaskManager,
	collator_options: CollatorOptions,
) -> RelayChainResult<(
	Arc<(dyn RelayChainInterface + 'static)>,
	Option<CollatorPair>,
)> {
	match collator_options.relay_chain_rpc_url {
		Some(relay_chain_url) => Ok((
			Arc::new(RelayChainRPCInterface::new(relay_chain_url).await?) as Arc<_>,
			None,
		)),
		None => build_inprocess_relay_chain(
			polkadot_config,
			parachain_config,
			telemetry_worker_handle,
			task_manager,
		),
	}
}

/// Start a node with the given parachain `Configuration` and relay chain `Configuration`.
///
/// This is the actual implementation that is abstract over the executor and the runtime api.
#[sc_tracing::logging::prefix_logs_with("Parachain")]
async fn start_node_impl<RB, BIC>(
	parachain_config: Configuration,
	polkadot_config: Configuration,
	collator_options: CollatorOptions,
	id: ParaId,
	_rpc_ext_builder: RB,
	build_consensus: BIC,
) -> sc_service::error::Result<(
	TaskManager,
	Arc<FullClient>,
)>
where

	sc_client_api::StateBackendFor<TFullBackend<Block>, Block>: sp_api::StateBackend<BlakeTwo256>,
	TemplateRuntimeExecutor: sc_executor::NativeExecutionDispatch + 'static,
	RB: Fn(
			Arc<FullClient>,
		) -> Result<jsonrpc_core::IoHandler<sc_rpc::Metadata>, sc_service::Error>
		+ Send
		+ 'static,
	BIC: FnOnce(
		Arc<FullClient>,
		Option<&Registry>,
		Option<TelemetryHandle>,
		&TaskManager,
		Arc<dyn RelayChainInterface>,
		Arc<
			sc_transaction_pool::FullPool<
				Block,
				FullClient,
			>,
		>,
		Arc<NetworkService<Block, Hash>>,
		SyncCryptoStorePtr,
		bool,
	) -> Result<Box<dyn ParachainConsensus<Block>>, sc_service::Error>,
{
	if matches!(parachain_config.role, Role::Light) {
		return Err("Light client not supported!".into());
	}

	let parachain_config = prepare_node_config(parachain_config);

	let sc_service::PartialComponents {
		client,
		backend,
		mut task_manager,
		import_queue,
		mut keystore_container,
		select_chain,
		transaction_pool,
		other: (filter_pool, frontier_backend, mut telemetry,telemetry_worker_handle, fee_history_cache),
	} = new_partial(&parachain_config,true)?;

	let (relay_chain_interface, collator_key) = build_relay_chain_interface(
		polkadot_config,
		&parachain_config,
		telemetry_worker_handle,
		&mut task_manager,
		collator_options.clone(),
	)
	.await
	.map_err(|e| match e {
		RelayChainError::ServiceError(polkadot_service::Error::Sub(x)) => x,
		s => s.to_string().into(),
	})?;

	let block_announce_validator = BlockAnnounceValidator::new(relay_chain_interface.clone(), id);

	let force_authoring = parachain_config.force_authoring;
	let validator = parachain_config.role.is_authority();
	let prometheus_registry = parachain_config.prometheus_registry().cloned();
	let transaction_pool = transaction_pool.clone();
	let import_queue = cumulus_client_service::SharedImportQueue::new(import_queue);
	let (network, system_rpc_tx, start_network) =
		sc_service::build_network(sc_service::BuildNetworkParams {
			config: &parachain_config,
			client: client.clone(),
			transaction_pool: transaction_pool.clone(),
			spawn_handle: task_manager.spawn_handle(),
			import_queue: import_queue.clone(),
			block_announce_validator_builder: Some(Box::new(|_| {
				Box::new(block_announce_validator)
			})),
			warp_sync: None,
		})?;

	let subscription_task_executor =
		sc_rpc::SubscriptionTaskExecutor::new(task_manager.spawn_handle());
	let overrides = crate::rpc::overrides_handle(client.clone());
	let fee_history_limit = 2048;

	let block_data_cache = Arc::new(fc_rpc::EthBlockDataCacheTask::new(
		task_manager.spawn_handle(),
		overrides.clone(),
		50,
		50,
		prometheus_registry.clone(),
	));

	let rpc_extensions_builder = {
		let client = client.clone();
		let pool = transaction_pool.clone();
		let network = network.clone();
		let filter_pool = filter_pool.clone();
		let frontier_backend = frontier_backend.clone();
		let overrides = overrides.clone();
		let fee_history_cache = fee_history_cache.clone();
		let is_authority = false;
		let max_past_logs = 10000;

		Box::new(move |deny_unsafe, _| {
			let deps = crate::rpc::FullDeps {
				client: client.clone(),
				pool: pool.clone(),
				graph: pool.pool().clone(),
				deny_unsafe,
				is_authority,
				network: network.clone(),
				filter_pool: filter_pool.clone(),
				backend: frontier_backend.clone(),
				max_past_logs,
				fee_history_limit,
				fee_history_cache: fee_history_cache.clone(),
				overrides: overrides.clone(),
				block_data_cache: block_data_cache.clone(),
			};

			Ok(crate::rpc::create_full(
				deps,
				subscription_task_executor.clone(),
			))
		})
	};

	sc_service::spawn_tasks(sc_service::SpawnTasksParams {
		rpc_extensions_builder,
		client: client.clone(),
		transaction_pool: transaction_pool.clone(),
		task_manager: &mut task_manager,
		config: parachain_config,
		keystore: keystore_container.sync_keystore(),
		backend: backend.clone(),
		network: network.clone(),
		system_rpc_tx,
		telemetry: telemetry.as_mut(),
	})?;

	let announce_block = {
		let network = network.clone();
		Arc::new(move |hash, data| network.announce_block(hash, data))
	};

	let relay_chain_slot_duration = Duration::from_secs(6);

	if validator {
		let parachain_consensus = build_consensus(
			client.clone(),
			prometheus_registry.as_ref(),
			telemetry.as_ref().map(|t| t.handle()),
			&task_manager,
			relay_chain_interface.clone(),
			transaction_pool,
			network,
			keystore_container.sync_keystore(),
			force_authoring,
		)?;

		let spawner = task_manager.spawn_handle();

		let params = StartCollatorParams {
			para_id: id,
			block_status: client.clone(),
			announce_block,
			client: client.clone(),
			task_manager: &mut task_manager,
			relay_chain_interface,
			spawner,
			parachain_consensus,
			import_queue,
			collator_key: collator_key.expect("Command line arguments do not allow this. qed"),
			relay_chain_slot_duration,
		};

		start_collator(params).await?;
	} else {
		let params = StartFullNodeParams {
			client: client.clone(),
			announce_block,
			task_manager: &mut task_manager,
			para_id: id,
			relay_chain_interface,
			relay_chain_slot_duration,
			import_queue,
			collator_options,
		};

		start_full_node(params)?;
	}

	start_network.start_network();

	Ok((task_manager, client))
}

/// Start a parachain node.
pub async fn start_parachain_node(
	parachain_config: Configuration,
	polkadot_config: Configuration,
	collator_options: CollatorOptions,
	id: ParaId,
) -> sc_service::error::Result<(
	TaskManager,
	Arc<TFullClient<Block, RuntimeApi, NativeElseWasmExecutor<TemplateRuntimeExecutor>>>,
)> {
	start_node_impl::<_, _>(
		parachain_config,
		polkadot_config,
		collator_options,
		id,
		|_| Ok(Default::default()),
		|client,
		 prometheus_registry,
		 telemetry,
		 task_manager,
		 relay_chain_interface,
		 transaction_pool,
		 _sync_oracle,
		 keystore,
		 force_authoring| {
			let proposer_factory = sc_basic_authorship::ProposerFactory::with_proof_recording(
				task_manager.spawn_handle(),
				client.clone(),
				transaction_pool,
				prometheus_registry,
				telemetry.clone(),
			);

			Ok(NimbusConsensus::build(BuildNimbusConsensusParams {
				para_id: id,
				proposer_factory,
				block_import: client.clone(),
				parachain_client: client.clone(),
				keystore,
				skip_prediction: force_authoring,
				create_inherent_data_providers: move |_,
				                                      (
					relay_parent,
					validation_data,
					_author_id,
				)| {
					let relay_chain_interface = relay_chain_interface.clone();
					async move {
						let parachain_inherent =
							cumulus_primitives_parachain_inherent::ParachainInherentData::create_at(
								relay_parent,
								&relay_chain_interface,
								&validation_data,
								id,
							).await;

						let time = sp_timestamp::InherentDataProvider::from_system_time();

						let parachain_inherent = parachain_inherent.ok_or_else(|| {
							Box::<dyn std::error::Error + Send + Sync>::from(
								"Failed to create parachain inherent",
							)
						})?;

						let nimbus_inherent = nimbus_primitives::InherentDataProvider;

						Ok((time, parachain_inherent, nimbus_inherent))
					}
				},
			}))
		},
	)
	.await
}

/// Builds a new service for a full client.
pub fn start_instant_seal_node(config: Configuration) -> Result<TaskManager, sc_service::Error> {
	let sc_service::PartialComponents {
		client,
		backend,
		mut task_manager,
		import_queue,
		mut keystore_container,
		select_chain,
		transaction_pool,
		other: (filter_pool, frontier_backend, mut telemetry,telemetry_worker_handle, fee_history_cache),
	} = new_partial(&config, false)?;

	let (network, system_rpc_tx, network_starter) =
		sc_service::build_network(sc_service::BuildNetworkParams {
			config: &config,
			client: client.clone(),
			transaction_pool: transaction_pool.clone(),
			spawn_handle: task_manager.spawn_handle(),
			import_queue,
			block_announce_validator_builder: None,
			warp_sync: None,
		})?;

	if config.offchain_worker.enabled {
		sc_service::build_offchain_workers(
			&config,
			task_manager.spawn_handle(),
			client.clone(),
			network.clone(),
		);
	}

	let is_authority = config.role.is_authority();
	let prometheus_registry = config.prometheus_registry().cloned();

	let subscription_task_executor =
		sc_rpc::SubscriptionTaskExecutor::new(task_manager.spawn_handle());
	let overrides = crate::rpc::overrides_handle(client.clone());
	let fee_history_limit = 2048;

	let block_data_cache = Arc::new(fc_rpc::EthBlockDataCacheTask::new(
		task_manager.spawn_handle(),
		overrides.clone(),
		50,
		50,
		prometheus_registry.clone(),
	));

	let rpc_extensions_builder = {
		let client = client.clone();
		let pool = transaction_pool.clone();
		let network = network.clone();
		let filter_pool = filter_pool.clone();
		let frontier_backend = frontier_backend.clone();
		let overrides = overrides.clone();
		let fee_history_cache = fee_history_cache.clone();
		let is_authority = false;
		let max_past_logs = 10000;

		Box::new(move |deny_unsafe, _| {
			let deps = crate::rpc::FullDeps {
				client: client.clone(),
				pool: pool.clone(),
				graph: pool.pool().clone(),
				deny_unsafe,
				is_authority,
				network: network.clone(),
				filter_pool: filter_pool.clone(),
				backend: frontier_backend.clone(),
				max_past_logs,
				fee_history_limit,
				fee_history_cache: fee_history_cache.clone(),
				overrides: overrides.clone(),
				block_data_cache: block_data_cache.clone(),
			};

			Ok(crate::rpc::create_full(
				deps,
				subscription_task_executor.clone(),
			))
		})
	};

	sc_service::spawn_tasks(sc_service::SpawnTasksParams {
		network,
		client: client.clone(),
		keystore: keystore_container.sync_keystore(),
		task_manager: &mut task_manager,
		transaction_pool: transaction_pool.clone(),
		rpc_extensions_builder,
		backend,
		system_rpc_tx,
		config,
		telemetry: telemetry.as_mut(),
	})?;

	if is_authority {
		let proposer = sc_basic_authorship::ProposerFactory::new(
			task_manager.spawn_handle(),
			client.clone(),
			transaction_pool.clone(),
			prometheus_registry.as_ref(),
			telemetry.as_ref().map(|t| t.handle()),
		);

		let client_set_aside_for_cidp = client.clone();

		// Create channels for mocked XCM messages.
		let (_downward_xcm_sender, downward_xcm_receiver) = flume::bounded::<Vec<u8>>(100);
		let (_hrmp_xcm_sender, hrmp_xcm_receiver) = flume::bounded::<(ParaId, Vec<u8>)>(100);

		let authorship_future = run_instant_seal(InstantSealParams {
			block_import: client.clone(),
			env: proposer,
			client: client.clone(),
			pool: transaction_pool.clone(),
			select_chain,
			consensus_data_provider: Some(Box::new(NimbusManualSealConsensusDataProvider {
				keystore: keystore_container.sync_keystore(),
				client: client.clone(),
			})),
			create_inherent_data_providers: move |block, _extra_args| {
				let downward_xcm_receiver = downward_xcm_receiver.clone();
				let hrmp_xcm_receiver = hrmp_xcm_receiver.clone();

				let client_for_xcm = client_set_aside_for_cidp.clone();

				async move {
					let time = sp_timestamp::InherentDataProvider::from_system_time();

					// The nimbus runtime is shared among all nodes including the parachain node.
					// Because this is not a parachain context, we need to mock the parachain inherent data provider.
					//TODO might need to go back and get the block number like how I do in Moonbeam
					let mocked_parachain = MockValidationDataInherentDataProvider {
						current_para_block: 0,
						relay_offset: 0,
						relay_blocks_per_para_block: 0,
						xcm_config: MockXcmConfig::new(
							&*client_for_xcm,
							block,
							Default::default(),
							Default::default(),
						),
						raw_downward_messages: downward_xcm_receiver.drain().collect(),
						raw_horizontal_messages: hrmp_xcm_receiver.drain().collect(),
					};

					Ok((time, mocked_parachain))
				}
			},
		});

		task_manager.spawn_essential_handle().spawn_blocking(
			"instant-seal",
			None,
			authorship_future,
		);
	};

	network_starter.start_network();
	Ok(task_manager)
}
