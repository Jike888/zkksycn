//! API testing helpers.

// Built-in uses

// External uses
use actix_web::{web, App, Scope};

// Workspace uses
use zksync_config::ConfigurationOptions;
use zksync_crypto::rand::{SeedableRng, XorShiftRng};
use zksync_storage::test_data::{
    dummy_ethereum_tx_hash, gen_acc_random_updates, gen_unique_operation,
    gen_unique_operation_with_txs, BLOCK_SIZE_CHUNKS,
};
use zksync_storage::ConnectionPool;
use zksync_test_account::ZkSyncAccount;
use zksync_types::{ethereum::OperationType, helpers::apply_updates, AccountMap, Action};
use zksync_types::{
    operations::{ChangePubKeyOp, TransferToNewOp},
    ExecutedOperations, ExecutedTx, ZkSyncOp, ZkSyncTx,
};

// Local uses
use super::client::Client;

#[derive(Debug, Clone)]
pub struct TestServerConfig {
    pub env_options: ConfigurationOptions,
    pub pool: ConnectionPool,
}

impl Default for TestServerConfig {
    fn default() -> Self {
        Self {
            env_options: ConfigurationOptions::from_env(),
            pool: ConnectionPool::new(Some(1)),
        }
    }
}

impl TestServerConfig {
    pub fn start_server<F>(&self, scope_factory: F) -> (Client, actix_web::test::TestServer)
    where
        F: Fn(&TestServerConfig) -> Scope + Clone + Send + 'static,
    {
        let this = self.clone();
        let server = actix_web::test::start(move || {
            App::new().service(web::scope("/api/v1").service(scope_factory(&this)))
        });

        let mut url = server.url("");
        url.pop(); // Pop last '/' symbol.

        let client = Client::new(url);
        (client, server)
    }

    /// Creates several transactions and the corresponding executed operations.
    fn gen_zk_txs() -> Vec<(ZkSyncTx, ExecutedOperations)> {
        let from = ZkSyncAccount::rand();
        from.set_account_id(Some(0xdead));

        let to = ZkSyncAccount::rand();
        to.set_account_id(Some(0xf00d));

        let mut txs = Vec::new();

        // Sign change pubkey tx pair
        {
            let tx = from.sign_change_pubkey_tx(None, false, 0, 0_u64.into(), false);

            let zksync_op = ZkSyncOp::ChangePubKeyOffchain(Box::new(ChangePubKeyOp {
                tx: tx.clone(),
                account_id: from.get_account_id().unwrap(),
            }));

            let executed_tx = ExecutedTx {
                signed_tx: zksync_op.try_get_tx().unwrap().into(),
                success: true,
                op: Some(zksync_op),
                fail_reason: None,
                block_index: None,
                created_at: chrono::Utc::now(),
                batch_id: None,
            };

            txs.push((
                ZkSyncTx::ChangePubKey(Box::new(tx)),
                ExecutedOperations::Tx(Box::new(executed_tx)),
            ));
        }
        // Transfer tx pair
        {
            let tx = from
                .sign_transfer(
                    0,
                    "ETH",
                    1_u64.into(),
                    0_u64.into(),
                    &to.address,
                    None,
                    false,
                )
                .0;

            let zksync_op = ZkSyncOp::TransferToNew(Box::new(TransferToNewOp {
                tx: tx.clone(),
                from: from.get_account_id().unwrap(),
                to: to.get_account_id().unwrap(),
            }));

            let executed_tx = ExecutedTx {
                signed_tx: zksync_op.try_get_tx().unwrap().into(),
                success: true,
                op: Some(zksync_op),
                fail_reason: None,
                block_index: None,
                created_at: chrono::Utc::now(),
                batch_id: None,
            };

            txs.push((
                ZkSyncTx::Transfer(Box::new(tx)),
                ExecutedOperations::Tx(Box::new(executed_tx)),
            ));
        }

        txs
    }

    pub async fn fill_database(&self) -> anyhow::Result<()> {
        let mut storage = self.pool.access_storage().await?;

        // Check if database is been already inited.
        if storage.chain().block_schema().get_block(1).await?.is_some() {
            return Ok(());
        }

        // Below lies the initialization of the data for the test.
        let mut rng = XorShiftRng::from_seed([0, 1, 2, 3]);

        // Required since we use `EthereumSchema` in this test.
        storage.ethereum_schema().initialize_eth_data().await?;

        let mut accounts = AccountMap::default();
        let n_committed = 5;
        let n_verified = n_committed - 2;

        // Create and apply several blocks to work with.
        for block_number in 1..=n_committed {
            let updates = (0..3)
                .map(|_| gen_acc_random_updates(&mut rng))
                .flatten()
                .collect::<Vec<_>>();
            apply_updates(&mut accounts, updates.clone());

            // Add transactions to every odd block.
            let txs = if block_number % 2 == 1 {
                Self::gen_zk_txs().into_iter().map(|(_tx, op)| op).collect()
            } else {
                vec![]
            };

            // Store the operation in the block schema.
            let operation = storage
                .chain()
                .block_schema()
                .execute_operation(gen_unique_operation_with_txs(
                    block_number,
                    Action::Commit,
                    BLOCK_SIZE_CHUNKS,
                    txs,
                ))
                .await?;
            storage
                .chain()
                .state_schema()
                .commit_state_update(block_number, &updates, 0)
                .await?;

            // Store & confirm the operation in the ethereum schema, as it's used for obtaining
            // commit/verify hashes.
            let ethereum_op_id = operation.id.unwrap() as i64;
            let eth_tx_hash = dummy_ethereum_tx_hash(ethereum_op_id);
            let response = storage
                .ethereum_schema()
                .save_new_eth_tx(
                    OperationType::Commit,
                    Some(ethereum_op_id),
                    100,
                    100u32.into(),
                    Default::default(),
                )
                .await?;
            storage
                .ethereum_schema()
                .add_hash_entry(response.id, &eth_tx_hash)
                .await?;
            storage
                .ethereum_schema()
                .confirm_eth_tx(&eth_tx_hash)
                .await?;

            // Add verification for the block if required.
            if block_number <= n_verified {
                storage
                    .prover_schema()
                    .store_proof(block_number, &Default::default())
                    .await?;
                let operation = storage
                    .chain()
                    .block_schema()
                    .execute_operation(gen_unique_operation(
                        block_number,
                        Action::Verify {
                            proof: Default::default(),
                        },
                        BLOCK_SIZE_CHUNKS,
                    ))
                    .await?;

                let ethereum_op_id = operation.id.unwrap() as i64;
                let eth_tx_hash = dummy_ethereum_tx_hash(ethereum_op_id);
                let response = storage
                    .ethereum_schema()
                    .save_new_eth_tx(
                        OperationType::Verify,
                        Some(ethereum_op_id),
                        100,
                        100u32.into(),
                        Default::default(),
                    )
                    .await?;
                storage
                    .ethereum_schema()
                    .add_hash_entry(response.id, &eth_tx_hash)
                    .await?;
                storage
                    .ethereum_schema()
                    .confirm_eth_tx(&eth_tx_hash)
                    .await?;
            }
        }
        Ok(())
    }
}