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

use databend_common_ast::Dialect;
use databend_common_config::GlobalConfig;
use databend_common_exception::ErrorCode;
use databend_common_exception::Result;
use databend_common_meta_app::principal::UserSettingValue;
use databend_common_users::UserApiProvider;

use crate::settings::Settings;
use crate::settings_default::DefaultSettings;
use crate::ChangeValue;
use crate::ReplaceIntoShuffleStrategy;
use crate::ScopeLevel;
use crate::SettingMode;

#[derive(Clone, Copy)]
pub enum FlightCompression {
    Lz4,
    Zstd,
}

impl Settings {
    // Get u64 value, we don't get from the metasrv.
    fn try_get_u64(&self, key: &str) -> Result<u64> {
        DefaultSettings::check_setting_mode(key, SettingMode::Read)?;

        unsafe { self.unchecked_try_get_u64(key) }
    }

    unsafe fn unchecked_try_get_u64(&self, key: &str) -> Result<u64> {
        match self.changes.get(key) {
            Some(v) => v.value.as_u64(),
            None => DefaultSettings::try_get_u64(key),
        }
    }

    fn try_get_string(&self, key: &str) -> Result<String> {
        DefaultSettings::check_setting_mode(key, SettingMode::Read)?;

        unsafe { self.unchecked_try_get_string(key) }
    }

    unsafe fn unchecked_try_get_string(&self, key: &str) -> Result<String, ErrorCode> {
        match self.changes.get(key) {
            Some(v) => Ok(v.value.as_string()),
            None => DefaultSettings::try_get_string(key),
        }
    }

    fn try_set_u64(&self, key: &str, val: u64) -> Result<()> {
        DefaultSettings::check_setting_mode(key, SettingMode::Write)?;

        unsafe { self.unchecked_try_set_u64(key, val) }
    }

    /// Sets a u64 value for a given key in the settings.
    /// Ensures that the key exists, the setting type is UInt64, and the value is within any defined numeric range.
    unsafe fn unchecked_try_set_u64(&self, key: &str, val: u64) -> Result<()> {
        // Retrieve the instance of default settings
        let default_settings = DefaultSettings::instance()?;

        let setting_value = default_settings
            .settings
            .get(key)
            .ok_or_else(|| ErrorCode::UnknownVariable(format!("Unknown variable: {:?}", key)))?;

        match &setting_value.value {
            UserSettingValue::UInt64(_) => {
                // If a numeric range is defined, validate the value against this range
                if let Some(range) = &setting_value.range {
                    // Check if the value falls within the numeric range
                    range.is_within_numeric_range(val).map_err(|err| {
                        ErrorCode::WrongValueForVariable(format!("{}: {}", key, err.message()))
                    })?;
                }

                // Insert the value into changes with a session scope
                self.changes.insert(key.to_string(), ChangeValue {
                    level: ScopeLevel::Session,
                    value: UserSettingValue::UInt64(val),
                });

                Ok(())
            }
            // If the setting type is not UInt64, return an error
            _ => Err(ErrorCode::BadArguments(format!(
                "Set an integer ({}) into {:?}",
                val, key
            ))),
        }
    }

    pub async fn set_setting(&self, k: String, v: String) -> Result<()> {
        DefaultSettings::check_setting_mode(&k, SettingMode::Write)?;

        unsafe { self.unchecked_set_setting(k, v).await }
    }

    async unsafe fn unchecked_set_setting(&self, k: String, v: String) -> Result<()> {
        let (key, value) = DefaultSettings::convert_value(k.clone(), v)?;

        if key == "sandbox_tenant" {
            let config = GlobalConfig::instance();
            let tenant = value.as_string();
            if config.query.internal_enable_sandbox_tenant && !tenant.is_empty() {
                UserApiProvider::try_create_simple(config.meta.to_meta_grpc_client_conf(), &tenant)
                    .await?;
            }
        }

        self.changes.insert(key, ChangeValue {
            value,
            level: ScopeLevel::Session,
        });
        Ok(())
    }

    pub fn get_enable_clickhouse_handler(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_clickhouse_handler")? != 0)
    }
    // Get max_block_size.
    pub fn get_max_block_size(&self) -> Result<u64> {
        self.try_get_u64("max_block_size")
    }

    // Max block size for parquet reader
    pub fn get_parquet_max_block_size(&self) -> Result<u64> {
        self.try_get_u64("parquet_max_block_size")
    }

    // Get max_threads.
    pub fn get_max_threads(&self) -> Result<u64> {
        match self.try_get_u64("max_threads")? {
            0 => Ok(16),
            value => Ok(value),
        }
    }

    // Set max_threads.
    pub fn set_max_threads(&self, val: u64) -> Result<()> {
        self.try_set_u64("max_threads", val)
    }

    // Get storage_fetch_part_num.
    pub fn get_storage_fetch_part_num(&self) -> Result<u64> {
        match self.try_get_u64("storage_fetch_part_num")? {
            0 => Ok(16),
            value => Ok(value),
        }
    }

    // Get parquet_uncompressed_buffer_size.
    pub fn get_parquet_uncompressed_buffer_size(&self) -> Result<u64> {
        self.try_get_u64("parquet_uncompressed_buffer_size")
    }

    pub fn get_max_memory_usage(&self) -> Result<u64> {
        self.try_get_u64("max_memory_usage")
    }

    pub fn set_max_memory_usage(&self, val: u64) -> Result<()> {
        self.try_set_u64("max_memory_usage", val)
    }

    pub fn set_retention_period(&self, hours: u64) -> Result<()> {
        self.try_set_u64("retention_period", hours)
    }

    pub fn get_retention_period(&self) -> Result<u64> {
        self.try_get_u64("retention_period")
    }

    pub fn set_data_retention_time_in_days(&self, days: u64) -> Result<()> {
        self.try_set_u64("data_retention_time_in_days", days)
    }

    pub fn get_data_retention_time_in_days(&self) -> Result<u64> {
        self.try_get_u64("data_retention_time_in_days")
    }

    pub fn get_max_storage_io_requests(&self) -> Result<u64> {
        self.try_get_u64("max_storage_io_requests")
    }

    pub fn set_max_storage_io_requests(&self, val: u64) -> Result<()> {
        if val > 0 {
            self.try_set_u64("max_storage_io_requests", val)
        } else {
            Err(ErrorCode::BadArguments(
                "max_storage_io_requests must be greater than 0",
            ))
        }
    }

    pub fn get_storage_io_min_bytes_for_seek(&self) -> Result<u64> {
        self.try_get_u64("storage_io_min_bytes_for_seek")
    }

    pub fn get_storage_io_max_page_bytes_for_read(&self) -> Result<u64> {
        self.try_get_u64("storage_io_max_page_bytes_for_read")
    }

    // Get max_execute_time_in_seconds.
    pub fn get_max_execute_time_in_seconds(&self) -> Result<u64> {
        self.try_get_u64("max_execute_time_in_seconds")
    }

    // Get flight client timeout.
    pub fn get_flight_client_timeout(&self) -> Result<u64> {
        self.try_get_u64("flight_client_timeout")
    }

    // Get storage read buffer size.
    pub fn get_storage_read_buffer_size(&self) -> Result<u64> {
        self.try_get_u64("storage_read_buffer_size")
    }

    pub fn get_input_read_buffer_size(&self) -> Result<u64> {
        self.try_get_u64("input_read_buffer_size")
    }

    pub fn get_enable_bushy_join(&self) -> Result<u64> {
        self.try_get_u64("enable_bushy_join")
    }

    pub fn get_timezone(&self) -> Result<String> {
        self.try_get_string("timezone")
    }

    // Get group by two level threshold
    pub fn get_group_by_two_level_threshold(&self) -> Result<u64> {
        self.try_get_u64("group_by_two_level_threshold")
    }

    pub fn get_max_inlist_to_or(&self) -> Result<u64> {
        self.try_get_u64("max_inlist_to_or")
    }

    pub fn get_unquoted_ident_case_sensitive(&self) -> Result<bool> {
        Ok(self.try_get_u64("unquoted_ident_case_sensitive")? != 0)
    }

    pub fn get_quoted_ident_case_sensitive(&self) -> Result<bool> {
        Ok(self.try_get_u64("quoted_ident_case_sensitive")? != 0)
    }

    pub fn get_max_result_rows(&self) -> Result<u64> {
        self.try_get_u64("max_result_rows")
    }

    pub fn get_enable_dphyp(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_dphyp")? != 0)
    }

    pub fn get_enable_cbo(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_cbo")? != 0)
    }

    /// # Safety
    pub unsafe fn get_disable_join_reorder(&self) -> Result<bool> {
        Ok(self.unchecked_try_get_u64("disable_join_reorder")? != 0)
    }

    pub fn get_join_spilling_threshold(&self) -> Result<usize> {
        Ok(self.try_get_u64("join_spilling_threshold")? as usize)
    }

    pub fn get_bloom_runtime_filter(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_bloom_runtime_filter")? != 0)
    }

    pub fn get_prefer_broadcast_join(&self) -> Result<bool> {
        Ok(self.try_get_u64("prefer_broadcast_join")? != 0)
    }

    pub fn get_enforce_broadcast_join(&self) -> Result<bool> {
        Ok(self.try_get_u64("enforce_broadcast_join")? != 0)
    }

    pub fn get_sql_dialect(&self) -> Result<Dialect> {
        match self.try_get_string("sql_dialect")?.to_lowercase().as_str() {
            "hive" => Ok(Dialect::Hive),
            "mysql" => Ok(Dialect::MySQL),
            "experimental" => Ok(Dialect::Experimental),
            _ => Ok(Dialect::PostgreSQL),
        }
    }

    pub fn get_collation(&self) -> Result<&str> {
        match self.try_get_string("collation")?.to_lowercase().as_str() {
            "utf8" => Ok("utf8"),
            _ => Ok("binary"),
        }
    }

    pub fn get_enable_hive_parquet_predict_pushdown(&self) -> Result<u64> {
        self.try_get_u64("enable_hive_parquet_predict_pushdown")
    }

    pub fn get_hive_parquet_chunk_size(&self) -> Result<u64> {
        self.try_get_u64("hive_parquet_chunk_size")
    }

    pub fn get_load_file_metadata_expire_hours(&self) -> Result<u64> {
        self.try_get_u64("load_file_metadata_expire_hours")
    }

    pub fn get_sandbox_tenant(&self) -> Result<String> {
        self.try_get_string("sandbox_tenant")
    }

    pub fn get_hide_options_in_show_create_table(&self) -> Result<bool> {
        Ok(self.try_get_u64("hide_options_in_show_create_table")? != 0)
    }

    pub fn get_enable_query_result_cache(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_query_result_cache")? != 0)
    }

    pub fn get_query_result_cache_max_bytes(&self) -> Result<usize> {
        Ok(self.try_get_u64("query_result_cache_max_bytes")? as usize)
    }

    pub fn get_http_handler_result_timeout_secs(&self) -> Result<u64> {
        self.try_get_u64("http_handler_result_timeout_secs")
    }

    pub fn get_query_result_cache_ttl_secs(&self) -> Result<u64> {
        self.try_get_u64("query_result_cache_ttl_secs")
    }

    pub fn get_query_result_cache_allow_inconsistent(&self) -> Result<bool> {
        Ok(self.try_get_u64("query_result_cache_allow_inconsistent")? != 0)
    }

    pub fn get_aggregate_spilling_bytes_threshold_per_proc(&self) -> Result<usize> {
        Ok(self.try_get_u64("aggregate_spilling_bytes_threshold_per_proc")? as usize)
    }

    pub fn get_aggregate_spilling_memory_ratio(&self) -> Result<usize> {
        Ok(self.try_get_u64("aggregate_spilling_memory_ratio")? as usize)
    }

    pub fn get_sort_spilling_bytes_threshold_per_proc(&self) -> Result<usize> {
        Ok(self.try_get_u64("sort_spilling_bytes_threshold_per_proc")? as usize)
    }

    pub fn get_sort_spilling_memory_ratio(&self) -> Result<usize> {
        Ok(self.try_get_u64("sort_spilling_memory_ratio")? as usize)
    }

    pub fn get_group_by_shuffle_mode(&self) -> Result<String> {
        self.try_get_string("group_by_shuffle_mode")
    }

    pub fn get_efficiently_memory_group_by(&self) -> Result<bool> {
        Ok(self.try_get_u64("efficiently_memory_group_by")? == 1)
    }

    pub fn get_enable_experimental_aggregate_hashtable(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_experimental_aggregate_hashtable")? == 1)
    }

    pub fn get_lazy_read_threshold(&self) -> Result<u64> {
        self.try_get_u64("lazy_read_threshold")
    }

    pub fn set_parquet_fast_read_bytes(&self, value: u64) -> Result<()> {
        self.try_set_u64("parquet_fast_read_bytes", value)
    }

    pub fn get_parquet_fast_read_bytes(&self) -> Result<u64> {
        self.try_get_u64("parquet_fast_read_bytes")
    }

    pub fn get_enable_table_lock(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_table_lock")? != 0)
    }

    pub fn get_enable_experimental_rbac_check(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_experimental_rbac_check")? != 0)
    }

    pub fn get_table_lock_expire_secs(&self) -> Result<u64> {
        self.try_get_u64("table_lock_expire_secs")
    }

    pub fn get_acquire_lock_timeout(&self) -> Result<u64> {
        self.try_get_u64("acquire_lock_timeout")
    }

    /// # Safety
    pub unsafe fn get_enterprise_license(&self) -> Result<String> {
        self.unchecked_try_get_string("enterprise_license")
    }

    /// # Safety
    pub async unsafe fn set_enterprise_license(&self, val: String) -> Result<()> {
        self.unchecked_set_setting("enterprise_license".to_string(), val)
            .await
    }

    /// # Safety
    pub unsafe fn get_deduplicate_label(&self) -> Result<Option<String>> {
        let deduplicate_label = self.unchecked_try_get_string("deduplicate_label")?;
        if deduplicate_label.is_empty() {
            Ok(None)
        } else {
            Ok(Some(deduplicate_label))
        }
    }

    /// # Safety
    pub async unsafe fn set_deduplicate_label(&self, val: String) -> Result<()> {
        self.unchecked_set_setting("deduplicate_label".to_string(), val)
            .await
    }

    pub fn get_enable_distributed_copy(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_distributed_copy_into")? != 0)
    }

    pub fn get_enable_experimental_merge_into(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_experimental_merge_into")? != 0)
    }

    pub fn get_enable_distributed_merge_into(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_distributed_merge_into")? != 0)
    }

    pub fn get_enable_distributed_replace(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_distributed_replace_into")? != 0)
    }

    pub fn get_enable_distributed_compact(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_distributed_compact")? != 0)
    }

    pub fn get_enable_aggregating_index_scan(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_aggregating_index_scan")? != 0)
    }

    pub fn get_enable_compact_after_write(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_compact_after_write")? != 0)
    }

    pub fn get_auto_compaction_imperfect_blocks_threshold(&self) -> Result<u64> {
        self.try_get_u64("auto_compaction_imperfect_blocks_threshold")
    }

    pub fn set_auto_compaction_imperfect_blocks_threshold(&self, val: u64) -> Result<()> {
        self.try_set_u64("auto_compaction_imperfect_blocks_threshold", val)
    }

    pub fn get_use_parquet2(&self) -> Result<bool> {
        Ok(self.try_get_u64("use_parquet2")? != 0)
    }

    pub fn set_use_parquet2(&self, val: bool) -> Result<()> {
        self.try_set_u64("use_parquet2", u64::from(val))
    }

    pub fn get_enable_replace_into_partitioning(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_replace_into_partitioning")? != 0)
    }

    pub fn get_enable_replace_into_bloom_pruning(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_replace_into_bloom_pruning")? != 0)
    }

    pub fn get_replace_into_bloom_pruning_max_column_number(&self) -> Result<u64> {
        self.try_get_u64("replace_into_bloom_pruning_max_column_number")
    }

    pub fn get_replace_into_shuffle_strategy(&self) -> Result<ReplaceIntoShuffleStrategy> {
        let v = self.try_get_u64("replace_into_shuffle_strategy")?;
        ReplaceIntoShuffleStrategy::try_from(v)
    }

    pub fn get_recluster_timeout_secs(&self) -> Result<u64> {
        self.try_get_u64("recluster_timeout_secs")
    }

    pub fn set_recluster_block_size(&self, val: u64) -> Result<()> {
        self.try_set_u64("recluster_block_size", val)
    }

    pub fn get_recluster_block_size(&self) -> Result<u64> {
        self.try_get_u64("recluster_block_size")
    }

    pub fn get_enable_distributed_recluster(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_distributed_recluster")? != 0)
    }

    pub fn get_ddl_column_type_nullable(&self) -> Result<bool> {
        Ok(self.try_get_u64("ddl_column_type_nullable")? != 0)
    }

    pub fn get_enable_query_profiling(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_query_profiling")? != 0)
    }

    pub fn get_enable_parquet_page_index(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_parquet_page_index")? != 0)
    }

    pub fn get_enable_parquet_rowgroup_pruning(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_parquet_rowgroup_pruning")? != 0)
    }

    pub fn get_enable_parquet_prewhere(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_parquet_prewhere")? != 0)
    }

    pub fn get_numeric_cast_option(&self) -> Result<String> {
        self.try_get_string("numeric_cast_option")
    }

    pub fn get_external_server_connect_timeout_secs(&self) -> Result<u64> {
        self.try_get_u64("external_server_connect_timeout_secs")
    }

    pub fn get_external_server_request_timeout_secs(&self) -> Result<u64> {
        self.try_get_u64("external_server_request_timeout_secs")
    }

    pub fn get_create_query_flight_client_with_current_rt(&self) -> Result<bool> {
        Ok(self.try_get_u64("create_query_flight_client_with_current_rt")? != 0)
    }

    pub fn get_query_flight_compression(&self) -> Result<Option<FlightCompression>> {
        match self
            .try_get_string("query_flight_compression")?
            .to_uppercase()
            .as_str()
        {
            "NONE" => Ok(None),
            "LZ4" => Ok(Some(FlightCompression::Lz4)),
            "ZSTD" => Ok(Some(FlightCompression::Zstd)),
            _ => unreachable!("check possible_values in set variable"),
        }
    }

    pub fn get_enable_refresh_virtual_column_after_write(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_refresh_virtual_column_after_write")? != 0)
    }

    pub fn set_enable_refresh_virtual_column_after_write(&self, val: bool) -> Result<()> {
        self.try_set_u64("enable_refresh_virtual_column_after_write", u64::from(val))
    }

    pub fn get_enable_refresh_aggregating_index_after_write(&self) -> Result<bool> {
        Ok(self.try_get_u64("enable_refresh_aggregating_index_after_write")? != 0)
    }

    pub fn set_enable_refresh_aggregating_index_after_write(&self, val: bool) -> Result<()> {
        self.try_set_u64(
            "enable_refresh_aggregating_index_after_write",
            u64::from(val),
        )
    }
}
