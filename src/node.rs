use std::io::Write;
use std::sync::Arc;
use parking_lot::RwLock;
use sc_service::{
    new_full_parts, spawn_tasks, build_network,
    Configuration, TaskManager, RpcHandlers, TaskType,
    BuildNetworkParams, SpawnTasksParams, TaskExecutor,
};
use sc_transaction_pool::BasicPool;
use sp_inherents::InherentDataProviders;
use sc_cli::SubstrateCli;
use jsonrpc_core::MetaIoHandler;
use futures::FutureExt;
use sc_executor::NativeExecutionDispatch;
use sp_runtime::traits::Block as BlockT;
use std::marker::PhantomData;

type Module = String;
type Logger = Arc<RwLock<std::collections::HashMap<Module, Vec<String>>>>;

/// this holds a reference to a running node on another thread,
/// we set a port over cli, process is dropped when this struct is dropped
/// holds logs from the process.
pub struct InternalNode<Block, RuntimeApi, Executor> {
	logs: Logger,

	/// rpc handler for communicating with the node over rpc.
	rpc_handlers: Arc<MetaIoHandler<sc_rpc::Metadata>>,

	/// tokio-compat runtime
	_tokio_runtime: tokio_compat::runtime::Runtime,

	/// handle to the running node.
	_task_manager: Option<TaskManager>,
	
	/// phantom type to pin generics
	_phantom: PhantomData<(Block, RuntimeApi, Executor)>
}

impl<Block, RuntimeApi, Executor> InternalNode<Block, RuntimeApi, Executor>
	where
		Block: BlockT,
		Executor: NativeExecutionDispatch,
		RuntimeApi: Send + Sync,
{
    pub fn builder() -> InternalNodeBuilder<Block, RuntimeApi, Executor> {
        InternalNodeBuilder::new()
    }

	pub fn new(logs: Logger, cli: &[String]) -> Self {
        let cli = node_cli::Cli::from_iter(cli.iter());
        let tokio_runtime =  tokio_compat::runtime::Runtime::new().unwrap();
        let runtime_handle = tokio_runtime.handle().clone();

        let task_executor = move |fut, task_type| {
            match task_type {
                TaskType::Async => runtime_handle.spawn(fut).map(drop),
                TaskType::Blocking => {
                    runtime_handle
                        .spawn_blocking(move || futures::executor::block_on(fut))
                        .map(drop)
                }
            }
        };

        let config = cli
            .create_configuration(&cli.run, TaskExecutor::from(task_executor))
            .expect("failed to create node config");
        let (task_manager, rpc_handlers) = build_node::<Block, RuntimeApi, Executor>(config).unwrap();

        Self {
            logs,
            _task_manager: Some(task_manager),
            _tokio_runtime: tokio_runtime,
			rpc_handlers: Arc::new(rpc_handlers.into_handler().into()),
			_phantom: PhantomData,
        }
    }

    pub fn rpc_handler(&self) -> Arc<MetaIoHandler<sc_rpc::Metadata>> {
        self.rpc_handlers.clone()
	}
	
	pub fn tokio_runtime(&mut self) -> &mut tokio_compat::runtime::Runtime {
		&mut self._tokio_runtime
	}

    pub(crate) fn logs(&self) -> &Logger {
        &self.logs
    }
}

impl<Block, RuntimeApi, Executor> Drop for InternalNode<Block, RuntimeApi, Executor> {
    fn drop(&mut self) {
        if let Some(mut task_manager) = self._task_manager.take() {
            task_manager.terminate()
        }
    }
}
#[derive(Debug)]
pub struct InternalNodeBuilder<Block, RuntimeApi, Executor> {
    /// Parameters passed as-is.
    cli: Vec<String>,
	logs: Logger,
	_phantom: PhantomData<(Block, RuntimeApi, Executor)>
}

impl<Block, RuntimeApi, Executor> InternalNodeBuilder<Block, RuntimeApi, Executor> {
    pub fn new() -> Self {
        let ignore = [
            "yamux", "multistream_select", "libp2p", "jsonrpc_client_transports",
            "sc_network", "tokio_reactor", "sub-libp2p", "sync", "peerset",
            "ws", "sc_network", "sc_service", "sc_peerset", "rpc"
        ];
        let logs = Logger::default();
        {
            let logs = logs.clone();
            let mut builder = env_logger::builder();
            builder.format(move |buf: &mut env_logger::fmt::Formatter, record: &log::Record| {
                let entry = format!("{} {} {}", record.level(), record.target(), record.args());
                let res = writeln!(buf, "{}", entry);
                logs.write()
                    .entry(record.target().to_string())
                    .or_default()
                    .push(entry);
                res
            });
            builder.filter_level(log::LevelFilter::Debug);
            builder.filter_module("runtime", log::LevelFilter::Trace);
            for module in &ignore {
                builder.filter_module(module, log::LevelFilter::Info);
            }

            let _ = builder
                .is_test(true)
                .try_init();
        }

        // create random directory for database
        let random_path = {
            let dir: String = rand::Rng::sample_iter(
                    rand::thread_rng(),
                    &rand::distributions::Alphanumeric
                )
                .take(15)
                .collect();
            let path = format!("/tmp/substrate-test-runner/{}", dir);
            std::fs::create_dir_all(&path).unwrap();
            path
        };

        Self {
            cli: vec![
                "--no-mdns".into(),
                "--no-prometheus".into(),
                "--no-telemetry".into(),
                format!("--base-path={}", random_path),
                "--dev".into(),
            ],
            logs,
            _phantom: PhantomData,
        }
    }

    pub fn cli_param(mut self, param: &str) -> Self {
        self.cli.push(param.into());
        self
    }

    pub fn start(self) -> InternalNode<Block, RuntimeApi, Executor> {
        InternalNode::new(self.logs, &self.cli)
    }
}

/// starts a manual seal authorship task.
pub fn build_node<Block, RuntimeApi, Executor>(config: Configuration) -> Result<(TaskManager, RpcHandlers), sc_service::Error>
	where
		Block: BlockT,
		Executor: NativeExecutionDispatch,
		RuntimeApi: frame_system::Trait + Send + Sync,
{
    // Channel for the rpc handler to communicate with the authorship task.
    let (command_sink, commands_stream) = futures::channel::mpsc::channel(10);

    let (
        client,
        backend,
        keystore,
        mut task_manager
    ) = new_full_parts::<Block, RuntimeApi, Executor>(&config)?;
    let client = Arc::new(client);
    let import_queue = manual_seal::import_queue(
        Box::new(client.clone()),
        &task_manager.spawn_handle(),
        None
    );

    let transaction_pool = BasicPool::new_full(
        config.transaction_pool.clone(),
        config.prometheus_registry(),
        task_manager.spawn_handle(),
        client.clone(),
	);
	
    let (network, network_status_sinks, system_rpc_tx) = {
		let params = BuildNetworkParams {
    	    config: &config,
    	    client: client.clone(),
    	    transaction_pool: transaction_pool.clone(),
    	    spawn_handle: task_manager.spawn_handle(),
    	    import_queue,
    	    on_demand: None,
    	    block_announce_validator_builder: None,
    	    finality_proof_request_builder: None,
    	    finality_proof_provider: None,
    	};
		build_network(params)?
	};

    // Proposer object for block authorship.
    let proposer = sc_basic_authorship::ProposerFactory::new(
        client.clone(),
        transaction_pool.clone(),
        config.prometheus_registry(),
    );

    let rpc_handlers = {
		let params = SpawnTasksParams {
            config,
            client: client.clone(),
            backend: backend.clone(),
            task_manager: &mut task_manager,
            keystore,
            on_demand: None,
            transaction_pool: transaction_pool.clone(),
            rpc_extensions_builder: Box::new(move |_| {
                use manual_seal::rpc;
                let mut io = jsonrpc_core::IoHandler::default();
                io.extend_with(
                    // We provide the rpc handler with the sending end of the channel to allow the rpc
                    // send EngineCommands to the background block authorship task.
                    rpc::ManualSealApi::to_delegate(rpc::ManualSeal::<RuntimeApi::Hash>::new(command_sink.clone())),
                );
                io
            }),
            remote_blockchain: None,
            network,
            network_status_sinks,
            system_rpc_tx,
            telemetry_connection_sinks: Default::default()
        };
		spawn_tasks(params)?
	};
	
	let inherent_data_providers = InherentDataProviders::new();
    inherent_data_providers
        .register_provider(sp_timestamp::InherentDataProvider)?;
	
	let select_chain = sc_consensus::LongestChain::new(backend.clone());

    // Background authorship future.
    let authorship_future = manual_seal::run_manual_seal(
        Box::new(client.clone()),
        proposer,
        client,
        transaction_pool.pool().clone(),
        commands_stream,
        select_chain,
        inherent_data_providers,
    );

    // spawn the authorship task as an essential task.
    task_manager.spawn_essential_handle()
        .spawn("manual-seal", authorship_future);

    // we really only care about the rpc interface.
    Ok((task_manager, rpc_handlers))
}
