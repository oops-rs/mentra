//! The four push-notification-config methods.
//!
//! A push config asks the agent to call a webhook on each change of a task's
//! state, for a caller that cannot hold a stream open. This client only
//! manages the configs; receiving the calls is the host's own server's job.
//! A config's `token` and `authentication` are credentials the agent will
//! present to that webhook, so nothing here logs or renders one, and one is
//! never sent to a plaintext interface that configured headers could not be
//! sent to either.
//!
//! # Debug output
//!
//! [`TaskPushNotificationConfig`] is the `a2a-lf` wire type, and its derived
//! `Debug` prints `token` and `authentication.credentials` as they are. A
//! host must not `{:?}` a config it passes to or receives from these
//! methods — nor a [`ListTaskPushNotificationConfigsResponse`] — into a log.

#[cfg(test)]
mod tests;

use a2a_lf::{
    DeleteTaskPushNotificationConfigRequest, GetTaskPushNotificationConfigRequest,
    ListTaskPushNotificationConfigsRequest, ListTaskPushNotificationConfigsResponse,
    TaskPushNotificationConfig, methods,
};
use reqwest::Method;

use super::binding::{Operation, checked_id};
use super::{A2aClient, segments};
use crate::a2a::error::A2aError;

/// The collection a task's push configs live in, under the task.
const PUSH_CONFIGS: &str = "pushNotificationConfigs";

impl A2aClient {
    /// Asks the agent to call `config.url` on each change of
    /// `config.task_id`'s state, returning the config as the agent stored it
    /// (with the id it assigned, when `config.id` was empty).
    ///
    /// The config's tenant is replaced by the interface's. A config carrying
    /// a `token` or `authentication` is refused as
    /// [`A2aError::PlaintextPushCredentials`] when the interface is plaintext
    /// `http` to a host that is not loopback, unless the configuration sets
    /// `allow_plaintext_credentials`.
    ///
    /// `TaskPushNotificationConfig`'s derived `Debug` prints `token` and
    /// `authentication.credentials` as they are, so neither the config passed
    /// nor the one returned may be logged with `{:?}`.
    pub async fn create_push_config(
        &self,
        mut config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2aError> {
        let method = methods::CREATE_PUSH_CONFIG;
        checked_id(method, &config.task_id)?;
        let carries_credentials = config.token.is_some() || config.authentication.is_some();
        if carries_credentials && self.transport.refuses_credentials_in_clear {
            return Err(A2aError::PlaintextPushCredentials);
        }
        config.tenant = self.transport.tenant.clone();
        let task_id = config.task_id.clone();
        let operation = Operation::new(
            methods::CREATE_PUSH_CONFIG,
            &config,
            Method::POST,
            segments(["tasks", &task_id, PUSH_CONFIGS]),
        )?;
        let stored = self.transport.call(operation).await?;
        expect_config(methods::CREATE_PUSH_CONFIG, &task_id, stored)
    }

    /// Reads one of a task's push configs.
    ///
    /// `TaskPushNotificationConfig`'s derived `Debug` prints `token` and
    /// `authentication.credentials` as they are, so the config returned may
    /// not be logged with `{:?}`.
    pub async fn get_push_config(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<TaskPushNotificationConfig, A2aError> {
        checked_id(methods::GET_PUSH_CONFIG, task_id)?;
        checked_id(methods::GET_PUSH_CONFIG, config_id)?;
        let request = GetTaskPushNotificationConfigRequest {
            task_id: task_id.to_string(),
            id: config_id.to_string(),
            tenant: self.transport.tenant.clone(),
        };
        let operation = Operation::new(
            methods::GET_PUSH_CONFIG,
            &request,
            Method::GET,
            segments(["tasks", task_id, PUSH_CONFIGS, config_id]),
        )?;
        let config = self.transport.call(operation).await?;
        expect_config(methods::GET_PUSH_CONFIG, task_id, config)
    }

    /// Lists a task's push configs, a page at a time.
    ///
    /// The request's tenant is replaced by the interface's.
    ///
    /// `TaskPushNotificationConfig`'s derived `Debug` prints `token` and
    /// `authentication.credentials` as they are, so the response may not be
    /// logged with `{:?}`.
    pub async fn list_push_configs(
        &self,
        mut request: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2aError> {
        checked_id(methods::LIST_PUSH_CONFIGS, &request.task_id)?;
        request.tenant = self.transport.tenant.clone();
        let operation = Operation::new(
            methods::LIST_PUSH_CONFIGS,
            &request,
            Method::GET,
            segments(["tasks", &request.task_id, PUSH_CONFIGS]),
        )?
        .query("pageSize", request.page_size)
        .query("pageToken", request.page_token.as_ref());
        let listed: ListTaskPushNotificationConfigsResponse =
            self.transport.call(operation).await?;
        if listed
            .configs
            .iter()
            .any(|config| !names_task(config, &request.task_id))
        {
            return Err(A2aError::MismatchedTask {
                method: methods::LIST_PUSH_CONFIGS,
            });
        }
        Ok(listed)
    }

    /// Deletes one of a task's push configs.
    ///
    /// Nothing comes back, so nothing here prints a credential; but the ids
    /// passed come from a `TaskPushNotificationConfig`, whose derived `Debug`
    /// prints `token` and `authentication.credentials` as they are — log the
    /// ids, never the config.
    pub async fn delete_push_config(&self, task_id: &str, config_id: &str) -> Result<(), A2aError> {
        checked_id(methods::DELETE_PUSH_CONFIG, task_id)?;
        checked_id(methods::DELETE_PUSH_CONFIG, config_id)?;
        let request = DeleteTaskPushNotificationConfigRequest {
            task_id: task_id.to_string(),
            id: config_id.to_string(),
            tenant: self.transport.tenant.clone(),
        };
        let operation = Operation::new(
            methods::DELETE_PUSH_CONFIG,
            &request,
            Method::DELETE,
            segments(["tasks", task_id, PUSH_CONFIGS, config_id]),
        )?;
        // The answer is empty in both bindings; only its failure matters.
        self.transport.call_value(operation).await.map(drop)
    }
}

/// Whether a config is about `task_id`. An agent may leave the task out of
/// a config it returns under that task's path, so an empty one is not a
/// mismatch.
fn names_task(config: &TaskPushNotificationConfig, task_id: &str) -> bool {
    config.task_id.is_empty() || config.task_id == task_id
}

/// Refuses a config about another task than the one asked about.
fn expect_config(
    method: &'static str,
    task_id: &str,
    config: TaskPushNotificationConfig,
) -> Result<TaskPushNotificationConfig, A2aError> {
    if !names_task(&config, task_id) {
        return Err(A2aError::MismatchedTask { method });
    }
    Ok(config)
}
