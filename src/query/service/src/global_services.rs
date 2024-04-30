// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::BTreeMap;
use std::sync::Arc;

use databend_common_base::base::GlobalInstance;
use databend_common_base::runtime::GlobalIORuntime;
use databend_common_base::runtime::GlobalQueryRuntime;
use databend_common_catalog::catalog::CatalogCreator;
use databend_common_catalog::catalog::CatalogManager;
use databend_common_cloud_control::cloud_api::CloudControlApiProvider;
use databend_common_config::GlobalConfig;
use databend_common_config::InnerConfig;
use databend_common_exception::Result;
use databend_common_meta_app::schema::CatalogType;
use databend_common_sharing::ShareEndpointManager;
use databend_common_storage::DataOperator;
use databend_common_storage::ShareTableConfig;
use databend_common_storages_hive::HiveCreator;
use databend_common_storages_iceberg::IcebergCreator;
use databend_common_tracing::GlobalLogger;
use databend_common_users::RoleCacheManager;
use databend_common_users::UserApiProvider;
use databend_storages_common_cache_manager::CacheManager;

use crate::api::DataExchangeManager;
use crate::auth::AuthMgr;
use crate::catalogs::DatabaseCatalog;
use crate::clusters::ClusterDiscovery;
use crate::locks::LockManager;
use crate::servers::http::v1::HttpQueryManager;
use crate::sessions::SessionManager;

pub struct GlobalServices;

impl GlobalServices {
    #[async_backtrace::framed]
    pub async fn init(config: &InnerConfig) -> Result<()> {
        GlobalInstance::init_production();
        GlobalServices::init_with(config).await
    }

    #[async_backtrace::framed]
    pub async fn init_with(config: &InnerConfig) -> Result<()> {
        // app name format: node_id[0..7]@cluster_id
        let app_name_shuffle = format!("databend-query-{}", config.query.cluster_id);

        // The order of initialization is very important
        // 1. global config init.
        GlobalConfig::init(config)?;

        // 2. log init.
        let mut log_labels = BTreeMap::new();
        log_labels.insert("service".to_string(), "databend-query".to_string());
        log_labels.insert("tenant_id".to_string(), config.query.tenant_id.clone());
        log_labels.insert("cluster_id".to_string(), config.query.cluster_id.clone());
        log_labels.insert("node_id".to_string(), config.query.node_id.clone());
        GlobalLogger::init(&app_name_shuffle, &config.log, log_labels);

        // 3. runtime init.
        GlobalIORuntime::init(config.storage.num_cpus as usize)?;
        GlobalQueryRuntime::init(config.storage.num_cpus as usize)?;

        // 4. cluster discovery init.
        ClusterDiscovery::init(config).await?;

        // TODO(xuanwo):
        //
        // This part is a bit complex because catalog are used widely in different
        // crates that we don't have a good place for different kinds of creators.
        //
        // Maybe we can do some refactor to simplify the logic here.
        {
            // Init default catalog.

            let default_catalog = DatabaseCatalog::try_create_with_config(config.clone()).await?;

            let catalog_creator: Vec<(CatalogType, Arc<dyn CatalogCreator>)> = vec![
                (CatalogType::Iceberg, Arc::new(IcebergCreator)),
                (CatalogType::Hive, Arc::new(HiveCreator)),
            ];

            CatalogManager::init(config, Arc::new(default_catalog), catalog_creator).await?;
        }

        HttpQueryManager::init(config).await?;
        DataExchangeManager::init()?;
        SessionManager::init(config)?;
        LockManager::init()?;
        AuthMgr::init(config)?;
        UserApiProvider::init(
            config.meta.to_meta_grpc_client_conf(),
            config.query.idm.clone(),
            config.query.tenant_id.as_str(),
            config.query.tenant_quota.clone(),
        )
        .await?;
        RoleCacheManager::init()?;
        ShareEndpointManager::init()?;

        DataOperator::init(&config.storage).await?;
        ShareTableConfig::init(
            &config.query.share_endpoint_address,
            &config.query.share_endpoint_auth_token_file,
            config.query.tenant_id.clone(),
        )?;
        CacheManager::init(&config.cache, &config.query.tenant_id)?;

        if let Some(addr) = config.query.cloud_control_grpc_server_address.clone() {
            CloudControlApiProvider::init(addr).await?;
        }

        Ok(())
    }
}
