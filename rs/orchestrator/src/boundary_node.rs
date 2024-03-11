use crate::{
    error::{OrchestratorError, OrchestratorResult},
    metrics::OrchestratorMetrics,
    process_manager::{Process, ProcessManager},
    registry_helper::RegistryHelper,
};
use ic_logger::{info, warn, ReplicaLogger};
use ic_types::{NodeId, ReplicaVersion};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

struct BoundaryNodeProcess {
    version: ReplicaVersion,
    binary: String,
    args: Vec<String>,
}

impl Process for BoundaryNodeProcess {
    const NAME: &'static str = "Boundary Node";

    type Version = ReplicaVersion;

    fn get_version(&self) -> &Self::Version {
        &self.version
    }

    fn get_binary(&self) -> &str {
        &self.binary
    }

    fn get_args(&self) -> &[String] {
        &self.args
    }
}

pub(crate) struct BoundaryNodeManager {
    registry: Arc<RegistryHelper>,
    _metrics: Arc<OrchestratorMetrics>,
    process: Arc<Mutex<ProcessManager<BoundaryNodeProcess>>>,
    ic_binary_dir: PathBuf,
    version: ReplicaVersion,
    logger: ReplicaLogger,
    node_id: NodeId,
}

impl BoundaryNodeManager {
    pub(crate) fn new(
        registry: Arc<RegistryHelper>,
        metrics: Arc<OrchestratorMetrics>,
        version: ReplicaVersion,
        node_id: NodeId,
        ic_binary_dir: PathBuf,
        logger: ReplicaLogger,
    ) -> Self {
        Self {
            registry,
            _metrics: metrics,
            process: Arc::new(Mutex::new(ProcessManager::new(
                logger.clone().inner_logger.root,
            ))),
            ic_binary_dir,
            version,
            logger,
            node_id,
        }
    }

    pub(crate) async fn check(&mut self) {
        let registry_version = self.registry.get_latest_version();

        match self
            .registry
            .get_api_boundary_node_version(self.node_id, registry_version)
        {
            Ok(replica_version) => {
                // BN manager is waiting for Upgrade to be performed
                if replica_version != self.version {
                    warn!(
                        every_n_seconds => 60,
                        self.logger, "Boundary node runs outdated version ({:?}), expecting upgrade to {:?}", self.version, replica_version
                    );
                    // NOTE: We could also shutdown the boundary node here. However, it makes sense to continue
                    // serving requests while the orchestrator is downloading the new image in most cases.
                } else if let Err(err) = self.ensure_boundary_node_running(&self.version) {
                    warn!(self.logger, "Failed to start Boundary Node: {}", err);
                }
            }
            // BN should not be active
            Err(OrchestratorError::ApiBoundaryNodeMissingError(_, _)) => {
                if let Err(err) = self.ensure_boundary_node_stopped() {
                    warn!(self.logger, "Failed to stop Boundary Node: {}", err);
                }
            }
            // Failing to read the registry
            Err(err) => warn!(
                self.logger,
                "Failed to fetch Boundary Node version: {}", err
            ),
        }
    }

    /// Start the current boundary node process
    fn ensure_boundary_node_running(&self, version: &ReplicaVersion) -> OrchestratorResult<()> {
        let mut process = self.process.lock().unwrap();

        if process.is_running() {
            return Ok(());
        }
        info!(self.logger, "Starting new boundary node process");

        let binary = self
            .ic_binary_dir
            .join("ic-boundary")
            .as_path()
            .display()
            .to_string();

        // TODO: Should these values be settable via config?
        // TODO: Add --hostname argument for prod usage
        let args = vec![
            format!("--http-port=80"),
            format!("--https-port=443"),
            format!("--tls-cert-path=/var/lib/ic/data/ic-boundary-tls.crt"),
            format!("--tls-pkey-path=/var/lib/ic/data/ic-boundary-tls.key"),
            format!("--acme-credentials-path=/var/lib/ic/data/ic-boundary-acme.json"),
            format!("--disable-registry-replicator"),
            format!("--local-store-path=/var/lib/ic/data/ic_registry_local_store"),
            format!("--metrics-addr=[::]:9324"),
        ];

        process
            .start(BoundaryNodeProcess {
                version: version.clone(),
                binary,
                args,
            })
            .map_err(|e| {
                OrchestratorError::IoError(
                    "Error when attempting to start new boundary node".into(),
                    e,
                )
            })
    }

    /// Stop the current boundary node process.
    fn ensure_boundary_node_stopped(&self) -> OrchestratorResult<()> {
        let mut process = self.process.lock().unwrap();
        if process.is_running() {
            return process.stop().map_err(|e| {
                OrchestratorError::IoError("Error when attempting to stop boundary node".into(), e)
            });
        }

        Ok(())
    }
}
