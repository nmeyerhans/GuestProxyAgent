// Copyright (c) Microsoft Corporation
// SPDX-License-Identifier: MIT

//! The key keeper module is responsible for polling the secure channel status from the WireServer endpoint.
//! It polls the secure channel status at a specified interval and update the secure channel state, key details, access control rule details.
//! This module will be launched when the GPA service is started.
//! It start the redirector/eBPF module when the key keeper task is running.
//! Example:
//! ```rust
//! use proxy_agent::key_keeper;
//! use proxy_agent::shared_state::SharedState;
//! use std::sync::{Arc, Mutex};
//! use hyper::Uri;
//! use std::path::PathBuf;
//! use std::time::Duration;
//!
//! let shared_state = SharedState::start_all();
//! let base_url = "http://127:0.0.1:8081/";
//! let key_dir = PathBuf::from("path");
//! let interval = Duration::from_secs(10);
//! let config_start_redirector = false;
//! let key_keeper = key_keeper::KeyKeeper::new(base_url.parse().unwrap(), key_dir, interval, config_start_redirector, &shared_state);
//! tokio::spawn(key_keeper.poll_secure_channel_status());
//! ```

pub mod key;

use self::key::Key;
use crate::common::error::Error;
use crate::common::result::Result;
use crate::common::{constants, helpers, logger};
use crate::provision;
use crate::proxy::authorization_rules::{AuthorizationRulesForLogging, ComputedAuthorizationRules};
use crate::redirector::Redirector;
use crate::shared_state::agent_status_wrapper::{AgentStatusModule, AgentStatusSharedState};
use crate::shared_state::key_keeper_wrapper::KeyKeeperSharedState;
use crate::shared_state::provision_wrapper::ProvisionSharedState;
use crate::shared_state::redirector_wrapper::RedirectorSharedState;
use crate::shared_state::telemetry_wrapper::TelemetrySharedState;
use crate::shared_state::SharedState;
use crate::{acl, redirector};
use hyper::Uri;
use proxy_agent_shared::misc_helpers;
use proxy_agent_shared::proxy_agent_aggregate_status::ModuleState;
use proxy_agent_shared::telemetry::event_logger;
use std::path::Path;
use std::time::Instant;
use std::{path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;

//pub const RUNNING_STATE: &str = "running";
pub const DISABLE_STATE: &str = "disabled";
pub const MUST_SIG_WIRESERVER: &str = "wireserver";
pub const MUST_SIG_WIRESERVER_IMDS: &str = "wireserverandimds";
pub const UNKNOWN_STATE: &str = "Unknown";
static FREQUENT_PULL_INTERVAL: Duration = Duration::from_secs(1); // 1 second
const FREQUENT_PULL_TIMEOUT_IN_MILLISECONDS: u128 = 300000; // 5 minutes
const PROVISION_TIMEUP_IN_MILLISECONDS: u128 = 120000; // 2 minute
const DELAY_START_EVENT_THREADS_IN_MILLISECONDS: u128 = 60000; // 1 minute

#[derive(Clone)]
pub struct KeyKeeper {
    /// base_url: the WireServer endpoint to poll the secure channel status
    base_url: Uri,
    /// key_dir: the folder to save the key details
    key_dir: PathBuf,
    /// log_dir: the folder to log the access control rule details
    log_dir: PathBuf,
    /// interval: the interval to poll the secure channel status
    interval: Duration,
    /// cancellation_token: the cancellation token to cancel the key keeper task
    cancellation_token: CancellationToken,
    /// key_keeper_shared_state: the sender for the key details, secure channel state, access control rule
    key_keeper_shared_state: KeyKeeperSharedState,
    /// telemetry_shared_state: the sender for the telemetry events
    telemetry_shared_state: TelemetrySharedState,
    /// redirector_shared_state: the sender for the redirector/eBPF module
    redirector_shared_state: RedirectorSharedState,
    /// provision_shared_state: the sender for the provision state
    provision_shared_state: ProvisionSharedState,
    /// agent_status_shared_state: the sender for the agent status
    agent_status_shared_state: AgentStatusSharedState,
}

impl KeyKeeper {
    pub fn new(
        base_url: Uri,
        key_dir: PathBuf,
        log_dir: PathBuf,
        interval: Duration,
        shared_state: &SharedState,
    ) -> Self {
        KeyKeeper {
            base_url,
            key_dir,
            log_dir,
            interval,
            cancellation_token: shared_state.get_cancellation_token(),
            key_keeper_shared_state: shared_state.get_key_keeper_shared_state(),
            telemetry_shared_state: shared_state.get_telemetry_shared_state(),
            redirector_shared_state: shared_state.get_redirector_shared_state(),
            provision_shared_state: shared_state.get_provision_shared_state(),
            agent_status_shared_state: shared_state.get_agent_status_shared_state(),
        }
    }

    /// poll secure channel status at interval from the WireServer endpoint
    pub async fn poll_secure_channel_status(&self) {
        self.update_status_message("poll secure channel status task started.".to_string(), true)
            .await;

        // launch redirector initialization when the key keeper task is running
        tokio::spawn({
            let cancellation_token = self.cancellation_token.clone();
            let key_keeper_shared_state = self.key_keeper_shared_state.clone();
            let telemetry_shared_state = self.telemetry_shared_state.clone();
            let provision_shared_state = self.provision_shared_state.clone();
            let agent_status_shared_state = self.agent_status_shared_state.clone();

            let redirector = Redirector::new(
                constants::PROXY_AGENT_PORT,
                self.redirector_shared_state.clone(),
                self.key_keeper_shared_state.clone(),
                agent_status_shared_state.clone(),
            );
            async move {
                redirector.start().await;
                if redirector.is_started().await {
                    provision::redirector_ready(
                        cancellation_token.clone(),
                        key_keeper_shared_state.clone(),
                        telemetry_shared_state.clone(),
                        provision_shared_state.clone(),
                        agent_status_shared_state.clone(),
                    )
                    .await;
                }
            }
        });

        if let Err(e) = misc_helpers::try_create_folder(&self.key_dir) {
            logger::write_warning(format!(
                "key folder {} created failed with error {}.",
                misc_helpers::path_to_string(&self.key_dir),
                e
            ));
        } else {
            logger::write(format!(
                "key folder {} created if not exists before.",
                misc_helpers::path_to_string(&self.key_dir)
            ));
        }

        match acl::acl_directory(self.key_dir.to_path_buf()) {
            Ok(()) => {
                logger::write(format!(
                    "key folder {} ACLed if has not before.",
                    misc_helpers::path_to_string(&self.key_dir)
                ));
            }
            Err(e) => {
                logger::write_warning(format!(
                    "key folder {} ACLed failed with error {}.",
                    misc_helpers::path_to_string(&self.key_dir),
                    e
                ));
            }
        }

        tokio::select! {
            _ = self.loop_poll() => {
                self.update_status_message("poll_secure_channel_status task exited.".to_string(), true).await;

            },
            _ = self.cancellation_token.cancelled() => {
                self.update_status_message("poll_secure_channel_status task cancelled.".to_string(), true).await;
                self.stop().await;
            }
        }
    }

    /// Loop to poll the secure channel status from the WireServer endpoint
    async fn loop_poll(&self) {
        let mut first_iteration: bool = true;
        let mut started_event_threads: bool = false;
        let mut provision_timeup: bool = false;
        let notify = match self.key_keeper_shared_state.get_notify().await {
            Ok(notify) => notify,
            Err(e) => {
                logger::write_error(format!("Failed to get notify: {}", e));
                return;
            }
        };

        // set the key keeper task state to running
        if let Err(e) = self
            .agent_status_shared_state
            .set_module_state(ModuleState::RUNNING, AgentStatusModule::KeyKeeper)
            .await
        {
            logger::write_error(format!(
                "Failed to set key_keeper module state to 'Running' with error: {} ",
                e
            ));
        }

        let mut start = Instant::now();
        loop {
            if !first_iteration {
                // skip the sleep for the first loop

                let current_state = match self
                    .key_keeper_shared_state
                    .get_current_secure_channel_state()
                    .await
                {
                    Ok(state) => state,
                    Err(e) => {
                        logger::write_warning(format!(
                            "Failed to get current secure channel state: {}",
                            e
                        ));
                        UNKNOWN_STATE.to_string()
                    }
                };

                let sleep = if current_state == UNKNOWN_STATE
                    && helpers::get_elapsed_time_in_millisec()
                        < FREQUENT_PULL_TIMEOUT_IN_MILLISECONDS
                {
                    // frequent poll the secure channel status every second for the first 5 minutes
                    // until the secure channel state is known
                    FREQUENT_PULL_INTERVAL
                } else {
                    self.interval
                };

                let time = Instant::now();
                tokio::select! {
                    // notify to query the secure channel status immediately when the secure channel state is unknown or disabled
                    // this is to handle quicker response to the secure channel state change during VM provisioning.
                    _ = notify.notified() => {
                        if  current_state == DISABLE_STATE || current_state == UNKNOWN_STATE {
                            logger::write_warning(format!("poll_secure_channel_status task notified and secure channel state is '{}', start poll status now.", current_state));
                            provision::key_latch_ready_state_reset(self.provision_shared_state.clone()).await;

                            if start.elapsed().as_millis() > PROVISION_TIMEUP_IN_MILLISECONDS {
                                // already timeup, reset the start timer
                                start = Instant::now();
                            }
                        } else {
                            let slept_time_in_millisec = time.elapsed().as_millis();
                            let continue_sleep = sleep.as_millis() - slept_time_in_millisec;
                            if continue_sleep > 0 {
                                let continue_sleep = Duration::from_millis(continue_sleep as u64);
                                let message = format!("poll_secure_channel_status task notified but secure channel state is '{}', continue with sleep wait for {:?}.", current_state, continue_sleep);
                                logger::write_warning(message);
                                tokio::time::sleep(continue_sleep).await;
                            }
                        }
                    },
                    _ = tokio::time::sleep(sleep) => {}
                }
            }
            first_iteration = false;

            if !provision_timeup && start.elapsed().as_millis() > PROVISION_TIMEUP_IN_MILLISECONDS {
                provision::provision_timeup(
                    None,
                    self.provision_shared_state.clone(),
                    self.agent_status_shared_state.clone(),
                )
                .await;
                provision_timeup = true;
            }

            if !started_event_threads
                && helpers::get_elapsed_time_in_millisec()
                    > DELAY_START_EVENT_THREADS_IN_MILLISECONDS
            {
                provision::start_event_threads(
                    self.cancellation_token.clone(),
                    self.key_keeper_shared_state.clone(),
                    self.telemetry_shared_state.clone(),
                    self.provision_shared_state.clone(),
                    self.agent_status_shared_state.clone(),
                )
                .await;
                started_event_threads = true;
            }

            let status = match key::get_status(&self.base_url).await {
                Ok(s) => s,
                Err(e) => {
                    self.update_status_message(format!("Failed to get key status - {}", e), true)
                        .await;
                    continue;
                }
            };
            self.update_status_message(format!("Got key status successfully: {}.", status), true)
                .await;

            let mut access_control_rules_changed = false;
            let wireserver_rule_id = status.get_wireserver_rule_id();
            let imds_rule_id: String = status.get_imds_rule_id();
            match self
                .key_keeper_shared_state
                .update_wireserver_rule_id(wireserver_rule_id.to_string())
                .await
            {
                Ok((updated, old_wire_server_rule_id)) => {
                    if updated {
                        logger::write_warning(format!(
                            "Wireserver rule id changed from '{}' to '{}'.",
                            old_wire_server_rule_id, wireserver_rule_id
                        ));
                        if let Err(e) = self
                            .key_keeper_shared_state
                            .set_wireserver_rules(status.get_wireserver_rules())
                            .await
                        {
                            logger::write_error(format!("Failed to set wireserver rules: {}", e));
                        }
                        access_control_rules_changed = true;
                    }
                }
                Err(e) => {
                    logger::write_warning(format!("Failed to update wireserver rule id: {}", e));
                }
            }

            match self
                .key_keeper_shared_state
                .update_imds_rule_id(imds_rule_id.to_string())
                .await
            {
                Ok((updated, old_imds_rule_id)) => {
                    if updated {
                        logger::write_warning(format!(
                            "IMDS rule id changed from '{}' to '{}'.",
                            old_imds_rule_id, imds_rule_id
                        ));
                        if let Err(e) = self
                            .key_keeper_shared_state
                            .set_imds_rules(status.get_imds_rules())
                            .await
                        {
                            logger::write_error(format!("Failed to set imds rules: {}", e));
                        }
                        access_control_rules_changed = true;
                    }
                }
                Err(e) => {
                    logger::write_warning(format!("Failed to update imds rule id: {}", e));
                }
            }

            if access_control_rules_changed {
                if let (Ok(wireserver_rules), Ok(imds_rules)) = (
                    self.key_keeper_shared_state.get_wireserver_rules().await,
                    self.key_keeper_shared_state.get_imds_rules().await,
                ) {
                    let rules = AuthorizationRulesForLogging::new(
                        status.authorizationRules.clone(),
                        ComputedAuthorizationRules {
                            wireserver: wireserver_rules,
                            imds: imds_rules,
                        },
                    );
                    rules.write_all(&self.log_dir, constants::MAX_LOG_FILE_COUNT);
                }
            }

            let state = status.get_secure_channel_state();
            // check if need fetch the key
            if state != DISABLE_STATE
                && (status.keyGuid.is_none()  // key has not latched yet
                || status.keyGuid != self.key_keeper_shared_state.get_current_key_guid().await.unwrap_or(None))
            // key changed
            {
                let mut key_found = false;
                if let Some(guid) = &status.keyGuid {
                    // key latched before and search the key locally first
                    let mut key_file = self.key_dir.to_path_buf().join(guid);
                    key_file.set_extension("key");
                    // the key already latched before
                    if key_file.exists() {
                        // read the key details locally and update
                        match misc_helpers::json_read_from_file::<Key>(&key_file) {
                            Ok(key) => {
                                if let Err(e) =
                                    self.key_keeper_shared_state.update_key(key.clone()).await
                                {
                                    logger::write_warning(format!("Failed to update key: {}", e));
                                }

                                let message = helpers::write_startup_event(
                                    "Found key details from local and ready to use.",
                                    "poll_secure_channel_status",
                                    "key_keeper",
                                    logger::AGENT_LOGGER_KEY,
                                );
                                self.update_status_message(message, false).await;
                                key_found = true;

                                provision::key_latched(
                                    self.cancellation_token.clone(),
                                    self.key_keeper_shared_state.clone(),
                                    self.telemetry_shared_state.clone(),
                                    self.provision_shared_state.clone(),
                                    self.agent_status_shared_state.clone(),
                                )
                                .await;
                            }
                            Err(e) => {
                                let message = format!("Failed to read latched key details from file: {:?}. Will try acquire the key details from Server.",
                                e);
                                event_logger::write_event(
                                    event_logger::WARN_LEVEL,
                                    message.to_string(),
                                    "poll_secure_channel_status",
                                    "key_keeper",
                                    logger::AGENT_LOGGER_KEY,
                                );
                            }
                        };
                    } else {
                        let message = "The latched key file does not exist locally. Will try acquire the key details from Server.".to_string();
                        event_logger::write_event(
                            event_logger::WARN_LEVEL,
                            message.to_string(),
                            "poll_secure_channel_status",
                            "key_keeper",
                            logger::AGENT_LOGGER_KEY,
                        );
                    }
                }

                // if key has not latched before,
                // or not found
                // or could not read locally,
                // try fetch from server
                if !key_found {
                    let key = match key::acquire_key(&self.base_url).await {
                        Ok(k) => k,
                        Err(e) => {
                            self.update_status_message(
                                format!("Failed to acquire key details: {:?}", e),
                                true,
                            )
                            .await;
                            continue;
                        }
                    };

                    // key has not latched before,
                    // set the key_file full path from key details
                    let guid = key.guid.to_string();
                    let mut key_file = self.key_dir.to_path_buf().join(&guid);
                    key_file.set_extension("key");
                    match misc_helpers::json_write_to_file(&key, &key_file) {
                        Ok(()) => {
                            logger::write_information(format!(
                        "Successfully acquired the key '{}' details from server and saved locally.", guid));
                        }
                        Err(e) => {
                            self.update_status_message(
                                format!("Failed to save key details to file: {:?}", e),
                                true,
                            )
                            .await;
                            continue;
                        }
                    }

                    // double check the key details saved correctly to local disk
                    if let Err(e) = Self::check_local_key(&self.key_dir, &key) {
                        self.update_status_message(
                            format!(
                                "Failed to check the key '{}' details saved locally: {:?}.",
                                guid, e
                            ),
                            true,
                        )
                        .await;
                        continue;
                    } else {
                        match key::attest_key(&self.base_url, &key).await {
                            Ok(()) => {
                                // update in memory
                                if let Err(e) =
                                    self.key_keeper_shared_state.update_key(key.clone()).await
                                {
                                    logger::write_warning(format!("Failed to update key: {}", e));
                                }

                                let message = helpers::write_startup_event(
                                    "Successfully attest the key and ready to use.",
                                    "poll_secure_channel_status",
                                    "key_keeper",
                                    logger::AGENT_LOGGER_KEY,
                                );
                                self.update_status_message(message, false).await;
                                provision::key_latched(
                                    self.cancellation_token.clone(),
                                    self.key_keeper_shared_state.clone(),
                                    self.telemetry_shared_state.clone(),
                                    self.provision_shared_state.clone(),
                                    self.agent_status_shared_state.clone(),
                                )
                                .await;
                            }
                            Err(e) => {
                                logger::write_warning(format!("Failed to attest the key: {:?}", e));
                                continue;
                            }
                        }
                    }
                }
            }

            // update the current secure channel state if different
            match self
                .key_keeper_shared_state
                .update_current_secure_channel_state(state.to_string())
                .await
            {
                Ok(updated) => {
                    if updated {
                        // update the redirector policy map
                        redirector::update_wire_server_redirect_policy(
                            status.get_wire_server_mode() != DISABLE_STATE,
                            self.redirector_shared_state.clone(),
                        )
                        .await;
                        redirector::update_imds_redirect_policy(
                            status.get_imds_mode() != DISABLE_STATE,
                            self.redirector_shared_state.clone(),
                        )
                        .await;

                        // customer has not enforce the secure channel state
                        if state == DISABLE_STATE {
                            let message = helpers::write_startup_event(
                                "Customer has not enforce the secure channel state.",
                                "poll_secure_channel_status",
                                "key_keeper",
                                logger::AGENT_LOGGER_KEY,
                            );
                            // Update the status message and let the provision to continue
                            self.update_status_message(message, false).await;

                            // clear key in memory for disabled state
                            if let Err(e) = self.key_keeper_shared_state.clear_key().await {
                                logger::write_warning(format!("Failed to clear key: {}", e));
                            }
                            provision::key_latched(
                                self.cancellation_token.clone(),
                                self.key_keeper_shared_state.clone(),
                                self.telemetry_shared_state.clone(),
                                self.provision_shared_state.clone(),
                                self.agent_status_shared_state.clone(),
                            )
                            .await;
                        }
                    }
                }
                Err(e) => {
                    logger::write_warning(format!("Failed to update secure channel state: {}", e));
                }
            }
        }
    }

    async fn update_status_message(&self, message: String, log_to_file: bool) {
        match self
            .agent_status_shared_state
            .set_module_status_message(message.clone(), AgentStatusModule::KeyKeeper)
            .await
        {
            Ok(updated) => {
                if log_to_file {
                    if updated {
                        logger::write_information(message);
                    } else {
                        // not updated, log at verbose level
                        logger::write(message);
                    }
                }
            }
            Err(e) => {
                logger::write_warning(format!("Failed to set module status message: {}", e));
            }
        }
    }

    // key was saved locally correctly before
    // check the key file found and its guid and key value are corrected
    fn check_local_key(key_dir: &Path, key: &Key) -> Result<()> {
        let guid = key.guid.to_string();
        let mut key_file = key_dir.join(guid);
        key_file.set_extension("key");
        if !key_file.exists() {
            // guid.key file does not exist locally
            return Err(Error::Key(
                crate::common::error::KeyErrorType::CheckLocalKey(format!(
                    "Key file '{}' does not exist locally.",
                    key_file.display()
                )),
            ));
        }

        match misc_helpers::json_read_from_file::<Key>(&key_file) {
            Ok(local_key) => {
                if local_key.guid == key.guid && local_key.key == key.key {
                    Ok(())
                } else {
                    // guid.key file found but guid or key value is not matched
                    Err(Error::Key(
                        crate::common::error::KeyErrorType::CheckLocalKey(format!(
                            "Key file '{}' found but guid or key value is not matched.",
                            key_file.display()
                        )),
                    ))
                }
            }
            Err(e) => {
                // failed to parse guid.key file
                Err(Error::Key(
                    crate::common::error::KeyErrorType::CheckLocalKey(format!(
                        "Parse key file '{}' with error: {}",
                        key_file.display(),
                        e
                    )),
                ))
            }
        }
    }

    /// Stop the key keeper task
    async fn stop(&self) {
        if let Err(e) = self
            .agent_status_shared_state
            .set_module_state(ModuleState::STOPPED, AgentStatusModule::KeyKeeper)
            .await
        {
            logger::write_warning(format!(
                "Failed to set key_keeper module state to 'Stopped' with error: {} ",
                e
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::key::Key;
    use crate::common::logger;
    use crate::key_keeper;
    use crate::key_keeper::KeyKeeper;
    use crate::test_mock::server_mock;
    use proxy_agent_shared::{logger_manager, misc_helpers};
    use std::env;
    use std::fs;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn check_local_key_test() {
        let mut temp_test_path = env::temp_dir();
        let logger_key = "check_local_key_test";
        temp_test_path.push(logger_key);
        // clean up and ignore the clean up errors
        _ = fs::remove_dir_all(&temp_test_path);
        logger_manager::init_logger(
            logger_key.to_string(),
            temp_test_path.clone(),
            logger_key.to_string(),
            200,
            6,
        )
        .await;
        _ = misc_helpers::try_create_folder(&temp_test_path);

        let key_str = r#"{
            "authorizationScheme": "Azure-HMAC-SHA256",        
            "guid": "9cf81e97-0316-4ad3-94a7-8ccbdee8ccbf",        
            "issued": "2021-05-05T 12:00:00Z",        
            "key": "4A404E635266556A586E3272357538782F413F4428472B4B6250645367566B59"        
        }"#;
        let key: Key = serde_json::from_str(key_str).unwrap();
        let mut key_file = temp_test_path.to_path_buf().join(key.guid.clone());
        key_file.set_extension("key");
        _ = misc_helpers::json_write_to_file(&key, &key_file);

        assert!(KeyKeeper::check_local_key(&temp_test_path, &key).is_ok());

        _ = fs::remove_dir_all(&temp_test_path);
    }

    #[tokio::test]
    async fn poll_secure_channel_status_tests() {
        let mut temp_test_path = env::temp_dir();
        temp_test_path.push("poll_secure_channel_status_tests");
        let mut log_dir = temp_test_path.to_path_buf();
        log_dir.push("Logs");
        let mut keys_dir = temp_test_path.to_path_buf();
        keys_dir.push("Keys");

        // clean up and ignore the clean up errors
        match fs::remove_dir_all(&temp_test_path) {
            Ok(_) => {}
            Err(e) => {
                print!("Failed to remove_dir_all with error {}.", e);
            }
        }

        // init main logger
        logger_manager::init_logger(
            logger::AGENT_LOGGER_KEY.to_string(), // production code uses 'Agent_Log' to write.
            log_dir.clone(),
            "logger_key".to_string(),
            10 * 1024 * 1024,
            20,
        )
        .await;

        let cancellation_token = CancellationToken::new();
        // start wire_server listener
        let ip = "127.0.0.1";
        let port = 8081u16;
        tokio::spawn(server_mock::start(
            ip.to_string(),
            port,
            cancellation_token.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;

        // start with disabled secure channel state
        server_mock::set_secure_channel_state(false);

        // start poll_secure_channel_status
        let cloned_keys_dir = keys_dir.to_path_buf();
        let key_keeper = KeyKeeper {
            base_url: (format!("http://{}:{}/", ip, port)).parse().unwrap(),
            key_dir: cloned_keys_dir.clone(),
            log_dir: cloned_keys_dir.clone(),
            interval: Duration::from_millis(10),
            cancellation_token: cancellation_token.clone(),
            key_keeper_shared_state: key_keeper::KeyKeeperSharedState::start_new(),
            telemetry_shared_state: key_keeper::TelemetrySharedState::start_new(),
            redirector_shared_state: key_keeper::RedirectorSharedState::start_new(),
            provision_shared_state: key_keeper::ProvisionSharedState::start_new(),
            agent_status_shared_state: key_keeper::AgentStatusSharedState::start_new(),
        };

        tokio::spawn({
            let key_keeper = key_keeper.clone();
            async move {
                key_keeper.poll_secure_channel_status().await;
            }
        });

        for _ in [0; 5] {
            // wait poll_secure_channel_status run at least one loop
            tokio::time::sleep(Duration::from_millis(100)).await;
            if keys_dir.exists() {
                break;
            }
        }

        let key_files: Vec<std::path::PathBuf> = misc_helpers::get_files(&keys_dir).unwrap();
        assert!(
            key_files.is_empty(),
            "Should not write key file at disable secure channel state"
        );

        // set secure channel state to running
        server_mock::set_secure_channel_state(true);
        // wait poll_secure_channel_status run at least one loop
        tokio::time::sleep(Duration::from_millis(100)).await;
        let key_files = misc_helpers::get_files(&keys_dir).unwrap();
        assert_eq!(
            1,
            key_files.len(),
            "Should write key file at running secure channel state"
        );

        // stop poll
        cancellation_token.cancel();

        // clean up and ignore the clean up errors
        _ = fs::remove_dir_all(&temp_test_path);
    }
}
