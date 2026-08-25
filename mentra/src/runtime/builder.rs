use std::{any::Any, path::Path, sync::Arc};

use crate::{
    compaction::CompactionEngine,
    mcp::{McpManager, McpServerConfig, McpSseServerConfig, McpStreamableHttpServerConfig},
    provider::{Provider, ProviderRegistry, ResponsesTransport},
    runtime::{
        RuntimeExecutor, RuntimeHandle, RuntimeHook, RuntimeHooks, RuntimePolicy, RuntimeStore,
        control::PreExecutionHook, error::RuntimeError, skill::SkillLoadError,
    },
    tool::{ExecutableTool, FileToolProfile, ToolAuthorizer},
};
use mentra_provider::BuiltinProvider;

use super::skill::SkillLoader;
use super::{McpServerSummary, Runtime};

/// An MCP server to connect to during build, and how to reach it.
///
/// This is internal so that the two public registration methods keep taking
/// their own configuration types: [`McpServerConfig`] stays the stdio
/// configuration and callers never gain a transport field to fill in.
enum McpRegistration {
    Stdio(Box<McpServerConfig>),
    Sse(Box<McpSseServerConfig>),
    StreamableHttp(Box<McpStreamableHttpServerConfig>),
}

impl McpRegistration {
    /// The configured server name, used for diagnostics.
    fn name(&self) -> &str {
        match self {
            Self::Stdio(config) => &config.name,
            Self::Sse(config) => &config.name,
            Self::StreamableHttp(config) => &config.name,
        }
    }
}

/// Builder for constructing a [`Runtime`] with providers, tools, and policies.
pub struct RuntimeBuilder {
    handle: RuntimeHandle,
    provider_registry: ProviderRegistry,
    mcp_configs: Vec<McpRegistration>,
}

impl RuntimeBuilder {
    /// Creates a builder with Mentra's builtin tools enabled.
    pub fn new(runtime_intrinsics_enabled: bool) -> Self {
        Self {
            handle: RuntimeHandle::new(runtime_intrinsics_enabled),
            provider_registry: ProviderRegistry::default(),
            mcp_configs: Vec::new(),
        }
    }

    /// Registers a custom tool.
    pub fn with_tool<T>(self, tool: T) -> Self
    where
        T: ExecutableTool + 'static,
    {
        self.handle.register_tool(tool);
        self
    }

    /// Reconfigures the eagerly registered builtin file-tool surface.
    ///
    /// The default is [`FileToolProfile::Batched`], preserving the historical
    /// `files` tool. This method also works with [`Runtime::empty_builder`] to
    /// opt into only the selected file tools.
    pub fn with_file_tools(self, profile: FileToolProfile) -> Self {
        self.handle.configure_file_tools(profile);
        self
    }

    /// Registers typed application state that tools can retrieve from their context.
    pub fn with_context(self, context: Arc<dyn Any + Send + Sync>) -> Self {
        self.handle.register_app_context(context);
        self
    }

    /// Registers a runtime intrinsic tool.
    pub fn with_intrinsic<T>(self, tool: T) -> Self
    where
        T: ExecutableTool + 'static,
    {
        self.with_tool(tool)
    }

    /// Replaces the runtime store implementation.
    ///
    /// The default store is not opened on the way here. Recovery runs at build
    /// time against whichever store the builder ends with, so a caller that
    /// supplies its own never has the machine-wide default database created
    /// underneath it.
    pub fn with_store(self, store: impl RuntimeStore + 'static) -> Self {
        self.with_shared_store(std::sync::Arc::new(store))
    }

    /// Crate-internal twin of [`with_store`](Self::with_store) for a store
    /// handed over already type-erased.
    pub(crate) fn with_shared_store(self, store: std::sync::Arc<dyn RuntimeStore>) -> Self {
        Self {
            handle: self.handle.rebind_store(store),
            provider_registry: self.provider_registry,
            mcp_configs: self.mcp_configs,
        }
    }

    /// Replaces the command executor used by builtin tools.
    pub fn with_executor<E>(self, executor: E) -> Self
    where
        E: RuntimeExecutor + 'static,
    {
        Self {
            handle: self.handle.with_executor(Arc::new(executor)),
            provider_registry: self.provider_registry,
            mcp_configs: self.mcp_configs,
        }
    }

    /// Replaces the compaction engine used for transcript summarization.
    pub fn with_compaction_engine<C>(self, engine: C) -> Self
    where
        C: CompactionEngine + 'static,
    {
        Self {
            handle: self.handle.with_compaction_engine(Arc::new(engine)),
            provider_registry: self.provider_registry,
            mcp_configs: self.mcp_configs,
        }
    }

    /// Sets the runtime policy used to authorize file and process access.
    pub fn with_policy(self, policy: RuntimePolicy) -> Self {
        Self {
            handle: self.handle.with_policy(policy),
            provider_registry: self.provider_registry,
            mcp_configs: self.mcp_configs,
        }
    }

    /// Installs a pre-tool authorization service for runtime tool calls.
    pub fn with_tool_authorizer<A>(self, tool_authorizer: A) -> Self
    where
        A: ToolAuthorizer + 'static,
    {
        Self {
            handle: self.handle.with_tool_authorizer(Arc::new(tool_authorizer)),
            provider_registry: self.provider_registry,
            mcp_configs: self.mcp_configs,
        }
    }

    /// Sets the persisted runtime identifier used to group resumable agents.
    pub fn with_runtime_identifier(self, runtime_identifier: impl Into<Arc<str>>) -> Self {
        Self {
            handle: self.handle.with_runtime_identifier(runtime_identifier),
            provider_registry: self.provider_registry,
            mcp_configs: self.mcp_configs,
        }
    }

    /// Appends a single runtime hook, keeping any already registered.
    pub fn with_hook<H>(self, hook: H) -> Self
    where
        H: RuntimeHook + 'static,
    {
        let hooks = self.handle.hooks().clone().with_hook(hook);
        Self {
            handle: self.handle.with_hooks(hooks),
            provider_registry: self.provider_registry,
            mcp_configs: self.mcp_configs,
        }
    }

    /// Appends a single pre-execution hook, keeping any already registered.
    pub fn with_pre_hook<H>(self, hook: H) -> Self
    where
        H: PreExecutionHook + 'static,
    {
        let pre_hooks = self.handle.pre_hooks().clone().with_hook(hook);
        Self {
            handle: self.handle.with_pre_hooks(pre_hooks),
            provider_registry: self.provider_registry,
            mcp_configs: self.mcp_configs,
        }
    }

    /// Appends a single post-execution hook, keeping any already registered.
    ///
    /// Post-execution hooks run in reverse registration order, so a hook
    /// registered before another wraps it: first in on the way to the tool,
    /// last out on the way back.
    pub fn with_post_hook<H>(self, hook: H) -> Self
    where
        H: crate::runtime::PostExecutionHook + 'static,
    {
        let post_hooks = self.handle.post_hooks().clone().with_hook(hook);
        Self {
            handle: self.handle.with_post_hooks(post_hooks),
            provider_registry: self.provider_registry,
            mcp_configs: self.mcp_configs,
        }
    }

    /// Replaces hooks with the provided collection.
    pub fn with_hooks<I>(self, hooks: I) -> Self
    where
        I: IntoIterator<Item = Arc<dyn RuntimeHook>>,
    {
        Self {
            handle: self.handle.with_hooks(RuntimeHooks::new().extend(hooks)),
            provider_registry: self.provider_registry,
            mcp_configs: self.mcp_configs,
        }
    }

    /// Registers a skills directory and enables the builtin `load_skill` tool.
    pub fn with_skills_dir(self, path: impl AsRef<Path>) -> Result<Self, SkillLoadError> {
        self.handle
            .register_skill_loader(SkillLoader::from_dir(path)?);
        Ok(self)
    }

    /// Registers an MCP server, reached over stdio, to connect to during build.
    pub fn with_mcp_server(mut self, config: McpServerConfig) -> Self {
        self.mcp_configs
            .push(McpRegistration::Stdio(Box::new(config)));
        self
    }

    /// Registers multiple stdio MCP servers to connect to during build.
    pub fn with_mcp_servers(mut self, configs: impl IntoIterator<Item = McpServerConfig>) -> Self {
        self.mcp_configs.extend(
            configs
                .into_iter()
                .map(|config| McpRegistration::Stdio(Box::new(config))),
        );
        self
    }

    /// Registers an MCP server reached over the legacy HTTP+SSE transport.
    ///
    /// Every tool the server advertises is bridged into the runtime under a
    /// namespaced name. Use [`McpSseClient`](crate::mcp::McpSseClient) directly
    /// when a host needs to apply its own allowlist before anything is
    /// registered.
    ///
    /// ```rust,no_run
    /// use mentra::{BuiltinProvider, McpSseServerConfig, Runtime};
    /// # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// let runtime = Runtime::builder()
    ///     .with_provider(BuiltinProvider::Anthropic, "sk-...")
    ///     .with_mcp_sse_server(
    ///         McpSseServerConfig::new("observability", "https://mcp.example.com/sse")
    ///             .with_bearer_token("<token>"),
    ///     )
    ///     .build_async()
    ///     .await?;
    /// # let _ = runtime;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_mcp_sse_server(mut self, config: McpSseServerConfig) -> Self {
        self.mcp_configs
            .push(McpRegistration::Sse(Box::new(config)));
        self
    }

    /// Registers multiple HTTP+SSE MCP servers to connect to during build.
    pub fn with_mcp_sse_servers(
        mut self,
        configs: impl IntoIterator<Item = McpSseServerConfig>,
    ) -> Self {
        self.mcp_configs.extend(
            configs
                .into_iter()
                .map(|config| McpRegistration::Sse(Box::new(config))),
        );
        self
    }

    /// Registers an MCP server reached over the Streamable HTTP transport.
    ///
    /// This is the transport current MCP servers ship, and the one to reach for
    /// unless a server is known to serve only the legacy `/sse` path. Every tool
    /// the server advertises is bridged into the runtime under a namespaced
    /// name; use
    /// [`McpStreamableHttpClient`](crate::mcp::McpStreamableHttpClient) directly
    /// when a host needs to apply its own allowlist before anything is
    /// registered.
    ///
    /// ```rust,no_run
    /// use mentra::{BuiltinProvider, McpStreamableHttpServerConfig, Runtime};
    /// # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// let runtime = Runtime::builder()
    ///     .with_provider(BuiltinProvider::Anthropic, "sk-...")
    ///     .with_mcp_streamable_http_server(
    ///         McpStreamableHttpServerConfig::new("observability", "https://mcp.example.com/mcp")
    ///             .with_bearer_token("<token>"),
    ///     )
    ///     .build_async()
    ///     .await?;
    /// # let _ = runtime;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_mcp_streamable_http_server(
        mut self,
        config: McpStreamableHttpServerConfig,
    ) -> Self {
        self.mcp_configs
            .push(McpRegistration::StreamableHttp(Box::new(config)));
        self
    }

    /// Registers multiple Streamable HTTP MCP servers to connect to during build.
    pub fn with_mcp_streamable_http_servers(
        mut self,
        configs: impl IntoIterator<Item = McpStreamableHttpServerConfig>,
    ) -> Self {
        self.mcp_configs.extend(
            configs
                .into_iter()
                .map(|config| McpRegistration::StreamableHttp(Box::new(config))),
        );
        self
    }

    /// Registers a builtin provider when an API key is present.
    pub fn with_optional_provider(
        mut self,
        id: BuiltinProvider,
        api_key: Option<impl Into<String>>,
    ) -> Self {
        if let Some(api_key) = api_key {
            let _ = self
                .provider_registry
                .register_builtin_provider(id, api_key.into());
        }
        self
    }

    /// Registers a builtin provider from an API key.
    pub fn with_provider(mut self, id: BuiltinProvider, api_key: impl Into<String>) -> Self {
        let _ = self
            .provider_registry
            .register_builtin_provider(id, api_key);
        self
    }

    /// Chooses the transport this runtime's Responses-family requests stream
    /// over.
    ///
    /// Runtime scope, because a transport is a property of the connection to a
    /// provider rather than of one run: an HTTP+SSE turn and a websocket turn
    /// against the same endpoint are two different conversations with it, and a
    /// per-run switch would mean the runtime holding two live opinions about
    /// one socket. Left unset, each request's own
    /// [`ResponsesRequestOptions::transport`](crate::provider::ResponsesRequestOptions)
    /// stands — which is HTTP+SSE unless a host set otherwise, exactly as
    /// before this method existed.
    ///
    /// A provider that does not serve websockets — anthropic and gemini, whose
    /// definitions report `supports_websockets: false` — refuses an explicit
    /// [`ResponsesTransport::WebSocket`](crate::provider::ResponsesTransport)
    /// at its first request, naming itself, rather than answering over
    /// HTTP+SSE. Selecting a transport is an explicit act, and a silent
    /// fallback would hand back a stream nobody asked for.
    ///
    /// ```rust,no_run
    /// use mentra::{BuiltinProvider, Runtime};
    /// use mentra::provider::ResponsesTransport;
    /// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// let runtime = Runtime::builder()
    ///     .with_provider(BuiltinProvider::OpenAI, "sk-...")
    ///     .with_responses_transport(ResponsesTransport::WebSocket)
    ///     .build()?;
    /// # let _ = runtime;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_responses_transport(mut self, transport: ResponsesTransport) -> Self {
        self.provider_registry.set_responses_transport(transport);
        self
    }

    /// Registers the local Ollama provider using its default OpenAI-compatible endpoint.
    pub fn with_ollama(mut self) -> Self {
        self.provider_registry.register_ollama();
        self
    }

    /// Registers the local LM Studio provider using its default OpenAI-compatible endpoint.
    pub fn with_lmstudio(mut self) -> Self {
        self.provider_registry.register_lmstudio();
        self
    }

    /// Registers a custom runtime provider implementation.
    ///
    /// This is the supported seam for test-time provider injection when you
    /// want to script model responses without live API calls.
    ///
    /// ```rust,no_run
    /// use async_trait::async_trait;
    /// use mentra::{BuiltinProvider, ModelInfo, ProviderDescriptor, Runtime};
    /// use mentra::error::{ProviderError, RuntimeError};
    /// use mentra::provider::{Provider, ProviderEventStream, Request};
    /// use tokio::sync::mpsc;
    ///
    /// struct TestProvider;
    ///
    /// #[async_trait]
    /// impl Provider for TestProvider {
    ///     fn descriptor(&self) -> ProviderDescriptor {
    ///         ProviderDescriptor::new(BuiltinProvider::Anthropic)
    ///     }
    ///
    ///     async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
    ///         Ok(vec![ModelInfo::new("test-model", BuiltinProvider::Anthropic)])
    ///     }
    ///
    ///     async fn stream(
    ///         &self,
    ///         _request: Request<'_>,
    ///     ) -> Result<ProviderEventStream, ProviderError> {
    ///         let (_tx, rx) = mpsc::unbounded_channel();
    ///         Ok(rx)
    ///     }
    /// }
    ///
    /// let runtime = Runtime::empty_builder()
    ///     .with_provider_instance(TestProvider)
    ///     .build()?;
    /// # Ok::<(), RuntimeError>(())
    /// ```
    pub fn with_provider_instance<P>(mut self, provider: P) -> Self
    where
        P: Provider + 'static,
    {
        self.provider_registry.register_provider_instance(provider);
        self
    }

    /// Registers an endpoint speaking the OpenAI `chat/completions` wire,
    /// before the runtime exists.
    ///
    /// The builder-time counterpart to
    /// [`Runtime::register_openai_compatible`](crate::Runtime::register_openai_compatible),
    /// for a host that must settle its provider before it has a runtime to
    /// register one on. `api_key` is `None` for an endpoint that wants no
    /// credentials — a local vLLM or llama.cpp server — which is otherwise
    /// awkward to express, since a static credential source has no way to say
    /// "none".
    ///
    /// ```rust,no_run
    /// # use mentra::Runtime;
    /// let runtime = Runtime::empty_builder()
    ///     .with_openai_compatible("deepseek", "https://api.deepseek.com/", Some("key".into()))
    ///     .with_openai_compatible("local", "http://127.0.0.1:8000/", None)
    ///     .build();
    /// ```
    pub fn with_openai_compatible(
        mut self,
        id: impl Into<crate::provider::ProviderId>,
        base_url: impl AsRef<str>,
        api_key: Option<String>,
    ) -> Self {
        let provider = match api_key {
            Some(api_key) => crate::provider::openai_compatible::OpenAiCompatibleProvider::new(
                id, base_url, api_key,
            ),
            None => {
                crate::provider::openai_compatible::OpenAiCompatibleProvider::without_credentials(
                    id, base_url,
                )
            }
        };
        self.provider_registry.register_provider_instance(provider);
        self
    }

    /// Registers a provider-core instance built from `mentra::provider_core`.
    ///
    /// Use this when you want Mentra's runtime with a customized provider
    /// definition, such as a custom OpenAI-compatible or Anthropic-compatible
    /// base URL.
    pub fn with_registered_provider<P>(mut self, provider: P) -> Self
    where
        P: mentra_provider::Provider + 'static,
    {
        self.provider_registry
            .register_registered_provider(provider);
        self
    }

    /// Builds the runtime, connects to MCP servers, and validates providers.
    ///
    /// This is an async method because MCP server connections require spawning
    /// processes and performing the initialize handshake.
    pub async fn build_async(self) -> Result<Runtime, RuntimeError> {
        if self.provider_registry.is_empty() {
            return Err(RuntimeError::ProviderNotFound(None));
        }

        // Connect to MCP servers and register their tools.
        let mut outcomes = Vec::new();
        if !self.mcp_configs.is_empty() {
            let mut manager = McpManager::new();
            for config in &self.mcp_configs {
                let connected = match config {
                    McpRegistration::Stdio(config) => manager
                        .connect(config)
                        .await
                        .map_err(|error| error.to_string()),
                    McpRegistration::Sse(config) => manager
                        .connect_sse(config)
                        .await
                        .map_err(|error| error.to_string()),
                    McpRegistration::StreamableHttp(config) => manager
                        .connect_streamable_http(config)
                        .await
                        .map_err(|error| error.to_string()),
                };

                match connected {
                    Ok(bridged_tools) => {
                        let tools = bridged_tools.len();
                        for tool in bridged_tools {
                            self.handle.register_tool(tool);
                        }
                        outcomes.push(McpServerSummary {
                            name: config.name().to_string(),
                            tools,
                            error: None,
                        });
                    }
                    Err(error) => {
                        // Degraded mode: one unreachable server must not sink a
                        // session. Recorded rather than only printed, so a host
                        // can say which servers are live instead of a user
                        // wondering why a tool is missing.
                        eprintln!(
                            "Warning: MCP server '{}' failed to connect: {error}",
                            config.name()
                        );
                        outcomes.push(McpServerSummary {
                            name: config.name().to_string(),
                            tools: 0,
                            error: Some(error),
                        });
                    }
                }
            }
            // Store the manager in the app context for later use.
            self.handle
                .register_app_context(Arc::new(tokio::sync::Mutex::new(manager)));
        }

        let provider_registry = Arc::new(std::sync::RwLock::new(self.provider_registry));
        let handle = self
            .handle
            .with_provider_registry(provider_registry.clone());
        handle.prepare_recovery();
        Ok(Runtime {
            handle,
            provider_registry,
            mcp_servers: outcomes,
        })
    }

    /// Builds the runtime synchronously.
    ///
    /// Connecting to an MCP server means spawning a process and completing a
    /// handshake, which cannot happen here — so registering one and then
    /// calling this is refused rather than silently honored halfway. Use
    /// [`build_async`](Self::build_async) when MCP servers are configured.
    pub fn build(self) -> Result<Runtime, RuntimeError> {
        if self.provider_registry.is_empty() {
            return Err(RuntimeError::ProviderNotFound(None));
        }

        if !self.mcp_configs.is_empty() {
            let names: Vec<&str> = self.mcp_configs.iter().map(McpRegistration::name).collect();
            return Err(RuntimeError::OperationDenied(format!(
                "MCP servers are registered ({}) but `build` cannot connect them; \
                 use `build_async`",
                names.join(", ")
            )));
        }

        let provider_registry = Arc::new(std::sync::RwLock::new(self.provider_registry));
        let handle = self
            .handle
            .with_provider_registry(provider_registry.clone());
        handle.prepare_recovery();
        Ok(Runtime {
            handle,
            provider_registry,
            mcp_servers: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::VolatileRuntimeStore;
    use crate::runtime::control::{HookDecision, PreExecutionContext};
    use crate::runtime::store::default_store_paths_on_this_thread;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The least a builder will accept: a provider must exist before any
    /// other check runs.
    struct StubProvider;

    #[async_trait]
    impl crate::provider::Provider for StubProvider {
        fn descriptor(&self) -> crate::provider::ProviderDescriptor {
            crate::provider::ProviderDescriptor::new(BuiltinProvider::OpenAI)
        }

        async fn list_models(
            &self,
        ) -> Result<Vec<crate::ModelInfo>, crate::provider::ProviderError> {
            Ok(Vec::new())
        }

        async fn stream(
            &self,
            _request: crate::provider::Request<'_>,
        ) -> Result<crate::provider::ProviderEventStream, crate::provider::ProviderError> {
            unreachable!("no turn is run in these tests")
        }
    }

    /// Counts how many times it was consulted, so a hook that was silently
    /// dropped during registration shows up as a count that never moves.
    struct Counting(Arc<AtomicUsize>);

    #[async_trait]
    impl PreExecutionHook for Counting {
        async fn pre_tool_execution(
            &self,
            _context: &PreExecutionContext,
        ) -> Result<HookDecision, RuntimeError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(HookDecision::Allow)
        }
    }

    #[tokio::test]
    async fn registering_a_second_pre_hook_keeps_the_first() {
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));

        let builder = RuntimeBuilder::new(false)
            .with_pre_hook(Counting(Arc::clone(&first)))
            .with_pre_hook(Counting(Arc::clone(&second)));

        let context = PreExecutionContext {
            agent_id: "a1".to_string(),
            tool_name: "shell".to_string(),
            tool_call_id: "tc-1".to_string(),
            input_json: "{}".to_string(),
            working_directory: std::path::PathBuf::from("/repo"),
        };
        builder
            .handle
            .pre_hooks()
            .run(&context)
            .await
            .expect("hooks run");

        // The first registration used to be discarded by the second, which is
        // a security-relevant silent failure for a veto seam.
        assert_eq!(
            first.load(Ordering::SeqCst),
            1,
            "the first hook must still run"
        );
        assert_eq!(second.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn build_refuses_to_discard_registered_mcp_servers() {
        let error = RuntimeBuilder::new(false)
            .with_provider_instance(StubProvider)
            .with_mcp_server(McpServerConfig {
                name: "github".to_string(),
                command: "npx".to_string(),
                args: Vec::new(),
                env: Default::default(),
                cwd: None,
            })
            .build()
            .err()
            .expect("a sync build cannot connect a server, so it must say so");

        // The old behavior was to build cleanly and drop the server, which a
        // caller only discovered when a tool it had configured was missing.
        let message = error.to_string();
        assert!(
            message.contains("github") && message.contains("build_async"),
            "the refusal must name the server and the way forward: {message}"
        );
    }

    #[tokio::test]
    async fn a_runtime_with_no_mcp_servers_reports_none() {
        let runtime = RuntimeBuilder::new(false)
            .with_provider_instance(StubProvider)
            .build_async()
            .await
            .expect("builds");

        assert!(runtime.mcp_servers().is_empty());
    }

    /// A caller that supplies its own store has opted out of the machine-wide
    /// default. Constructing the handle used to open it anyway — creating
    /// `runtime.sqlite` on a pristine machine — before `with_store` replaced
    /// the store it had just prepared.
    #[test]
    fn a_build_with_a_caller_store_leaves_the_default_database_alone() {
        let store = VolatileRuntimeStore::new();
        let probe = store.clone();

        let runtime = RuntimeBuilder::new(false)
            .with_store(store)
            .with_provider_instance(StubProvider)
            .build()
            .expect("builds");

        let default_paths = default_store_paths_on_this_thread();
        assert!(
            !default_paths.is_empty(),
            "the handle still constructs a default store, so this test has something to check"
        );
        for path in default_paths {
            assert!(
                !path.exists(),
                "a discarded default store must never be opened: {}",
                path.display()
            );
        }
        assert_eq!(
            probe.recovery_preparations(),
            1,
            "recovery must run once, on the store the caller kept"
        );
        drop(runtime);
    }

    /// The async build boundary carries the same guarantee as the sync one.
    #[tokio::test]
    async fn an_async_build_prepares_recovery_once_on_the_caller_store() {
        let store = VolatileRuntimeStore::new();
        let probe = store.clone();

        let runtime = RuntimeBuilder::new(false)
            .with_store(store)
            .with_provider_instance(StubProvider)
            .build_async()
            .await
            .expect("builds");

        assert_eq!(probe.recovery_preparations(), 1);
        drop(runtime);
    }

    /// Deferring recovery must not skip it: a build that keeps the default
    /// store still reconciles interrupted state, which for SQLite means the
    /// database is opened and its schema created.
    #[test]
    fn a_default_build_still_prepares_recovery() {
        let runtime = RuntimeBuilder::new(false)
            .with_provider_instance(StubProvider)
            .build()
            .expect("builds");

        let default_paths = default_store_paths_on_this_thread();
        assert!(!default_paths.is_empty());
        for path in default_paths {
            assert!(
                path.exists(),
                "the store a runtime actually kept must be prepared: {}",
                path.display()
            );
        }
        drop(runtime);
    }

    /// Recovery belongs to the build boundary, not to assembly: until `build`
    /// settles which store survives, nothing may be prepared.
    #[test]
    fn assembling_a_builder_prepares_nothing() {
        let store = VolatileRuntimeStore::new();
        let probe = store.clone();

        let builder = RuntimeBuilder::new(false)
            .with_store(store)
            .with_provider_instance(StubProvider);

        assert_eq!(
            probe.recovery_preparations(),
            0,
            "a store is only prepared once the builder is done being reconfigured"
        );
        drop(builder);
    }

    /// A store that is swapped out again must never be prepared: `with_store`
    /// used to prepare eagerly, which made every intermediate store pay for a
    /// choice the caller went on to revise.
    #[test]
    fn a_replaced_store_is_never_prepared() {
        let discarded = VolatileRuntimeStore::new();
        let discarded_probe = discarded.clone();
        let kept = VolatileRuntimeStore::new();
        let kept_probe = kept.clone();

        let runtime = RuntimeBuilder::new(false)
            .with_store(discarded)
            .with_store(kept)
            .with_provider_instance(StubProvider)
            .build()
            .expect("builds");

        assert_eq!(
            discarded_probe.recovery_preparations(),
            0,
            "a store the builder threw away must not have been opened"
        );
        assert_eq!(kept_probe.recovery_preparations(), 1);
        drop(runtime);
    }
}
