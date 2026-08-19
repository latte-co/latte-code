use crate::provider::{
    Message, OpenAiProvider, Provider, ProviderCapabilities, ProviderContext, ProviderError,
    ProviderFuture, ProviderRequest,
};
use latte_core::ThreadProviderBindingV2;
use latte_engine::ToolDescriptor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    sync::Arc,
    time::Duration,
};
use thiserror::Error;

const BINDING_VERSION: u32 = 1;
const MAX_ALIAS_BYTES: usize = 64;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderFile {
    pub version: u32,
    #[serde(default)]
    pub default_model: String,
    pub providers: BTreeMap<String, ProviderDefinition>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ProviderDefinition {
    OpenaiChat {
        /// Provider-specific model catalog. Each object key is the model ID
        /// sent to the Provider. A string array is accepted as a shorthand
        /// for models without display names or typed `OpenAI` Chat options.
        models: OpenAiChatModels,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        endpoint: Option<String>,
        api_key: SecretRef,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
        #[serde(default = "default_attempts")]
        max_attempts: u32,
        #[serde(default)]
        temperature: Option<f64>,
        #[serde(default)]
        max_tokens: Option<u32>,
        #[serde(default)]
        compatibility_input_request: bool,
        #[serde(default)]
        streaming: bool,
        #[serde(default)]
        aliases: BTreeMap<String, String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum OpenAiChatModels {
    Ids(Vec<String>),
    Configured(BTreeMap<String, OpenAiChatModelConfig>),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiChatModelConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub options: OpenAiChatModelOptions,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiChatModelOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl ProviderDefinition {
    fn available_models(&self) -> Vec<&str> {
        match self {
            Self::OpenaiChat { models, .. } => models.ids(),
        }
    }

    fn model_options(&self, selected: &str) -> Option<OpenAiChatModelOptions> {
        match self {
            Self::OpenaiChat { models, .. } => models.options(selected),
        }
    }

    fn model_name(&self, selected: &str) -> Option<&str> {
        match self {
            Self::OpenaiChat { models, .. } => models.name(selected),
        }
    }

    fn require_model(&self, provider: &str, selected: &str) -> Result<(), RegistryError> {
        if self.available_models().contains(&selected) {
            Ok(())
        } else {
            Err(RegistryError::Invalid(format!(
                "unknown model {selected} for provider {provider}"
            )))
        }
    }
}

impl OpenAiChatModels {
    fn ids(&self) -> Vec<&str> {
        match self {
            Self::Ids(models) => models.iter().map(String::as_str).collect(),
            Self::Configured(models) => models.keys().map(String::as_str).collect(),
        }
    }

    fn options(&self, selected: &str) -> Option<OpenAiChatModelOptions> {
        match self {
            Self::Ids(models) => models
                .iter()
                .any(|model| model == selected)
                .then(OpenAiChatModelOptions::default),
            Self::Configured(models) => models.get(selected).map(|model| model.options.clone()),
        }
    }

    fn name(&self, selected: &str) -> Option<&str> {
        match self {
            Self::Ids(_) => None,
            Self::Configured(models) => models.get(selected)?.name.as_deref(),
        }
    }

    fn configured(&self) -> Vec<(&str, &OpenAiChatModelConfig)> {
        match self {
            Self::Ids(_) => Vec::new(),
            Self::Configured(models) => models
                .iter()
                .map(|(model, options)| (model.as_str(), options))
                .collect(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged, deny_unknown_fields)]
pub enum SecretRef {
    Literal(String),
    Env { source: SecretSource, name: String },
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecretSource {
    Env,
}

impl std::fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Literal(_) => f.write_str("SecretRef::Literal([REDACTED])"),
            Self::Env { .. } => f.write_str("SecretRef::Env([REDACTED])"),
        }
    }
}

impl SecretRef {
    fn credential_ref_id(&self, provider_name: &str) -> String {
        match self {
            Self::Literal(_) => format!("config:{provider_name}/api_key"),
            Self::Env { name, .. } => format!("env:{name}"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderBinding {
    pub version: u32,
    pub provider_name: String,
    pub provider_type: String,
    pub protocol: String,
    pub model: String,
    pub config_fingerprint: String,
    pub tools_fingerprint: String,
    pub aliases: BTreeMap<String, String>,
}
impl ProviderBinding {
    #[cfg(test)]
    pub(crate) fn direct(tools: &[ToolDescriptor]) -> Self {
        Self {
            version: BINDING_VERSION,
            provider_name: "embedded".into(),
            provider_type: "embedded".into(),
            protocol: "latte-provider-v1".into(),
            model: "embedded".into(),
            config_fingerprint: fingerprint(&serde_json::json!({"type":"embedded"})),
            tools_fingerprint: fingerprint(&serde_json::to_value(tools).unwrap_or_default()),
            aliases: tools
                .iter()
                .map(|tool| (tool.name.clone(), tool.name.clone()))
                .collect(),
        }
    }

    /// Builds the additive v2 binding without exposing any credential value.
    #[must_use]
    pub fn with_thread_scope(
        &self,
        credential_ref_id: String,
        data_scope_id: String,
        credential_generation: u64,
    ) -> ThreadProviderBindingV2 {
        ThreadProviderBindingV2 {
            version: self.version,
            provider_name: self.provider_name.clone(),
            provider_type: self.provider_type.clone(),
            protocol: self.protocol.clone(),
            model: self.model.clone(),
            config_fingerprint: self.config_fingerprint.clone(),
            tools_fingerprint: self.tools_fingerprint.clone(),
            aliases: self.aliases.clone(),
            credential_ref_id,
            data_scope_id,
            credential_generation,
        }
    }
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("invalid provider configuration: {0}")]
    Invalid(String),
    #[error("provider secret environment variable {0} is missing or empty")]
    MissingSecret(String),
    #[error(
        "provider binding mismatch: {0}; start a new run or restore the original provider configuration"
    )]
    BindingMismatch(String),
    #[error(transparent)]
    Provider(#[from] crate::provider::ProviderError),
}

pub struct ResolvedProvider {
    pub provider: Arc<dyn Provider>,
    pub binding: ProviderBinding,
}

/// Secret-free provider/model option exposed to interactive clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderModelEntry {
    pub provider_name: String,
    pub model: String,
    pub name: Option<String>,
    pub is_default: bool,
}

/// A complete binding catalog entry for model discovery.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BindingCatalogEntry {
    pub provider_name: String,
    pub model: String,
    pub name: Option<String>,
    pub is_default: bool,
    pub binding: ThreadProviderBindingV2,
}

#[derive(Clone, Debug)]
pub struct ProviderRegistry {
    config: ProviderFile,
}

impl ProviderRegistry {
    pub fn parse_jsonc(text: &str) -> Result<Self, RegistryError> {
        let mut value: serde_json::Value =
            json5::from_str(text).map_err(|e| RegistryError::Invalid(e.to_string()))?;
        if let Some(object) = value.as_object_mut() {
            object.remove("database");
            object.remove("verification");
            // Application-owned transcript-history limits are validated by
            // `latte_code::AppConfig`; they are not provider semantics. Keep
            // the provider schema strict after removing every documented
            // application-owned top-level section.
            object.remove("thread");
        }
        let config: ProviderFile =
            serde_json::from_value(value).map_err(|e| RegistryError::Invalid(e.to_string()))?;
        let registry = Self { config };
        registry.validate()?;
        Ok(registry)
    }

    #[must_use]
    pub fn default_name(&self) -> Option<&str> {
        self.default_selection().ok().map(|selection| selection.0)
    }

    #[must_use]
    pub fn default_model(&self) -> Option<&str> {
        self.default_selection().ok().map(|selection| selection.1)
    }

    pub fn resolve_default(
        &self,
        tools: &[ToolDescriptor],
    ) -> Result<ResolvedProvider, RegistryError> {
        let (provider, model) = self.default_selection()?;
        self.resolve_model(provider, model, tools)
    }

    /// Returns every configured provider/model pair in deterministic order.
    /// Exactly one pair is the global default.
    #[must_use]
    pub fn model_catalog(&self) -> Vec<ProviderModelEntry> {
        let default = self.default_selection().ok();
        self.config
            .providers
            .iter()
            .flat_map(|(provider_name, definition)| {
                let mut models = definition.available_models();
                if default.is_some_and(|(default_provider, _)| provider_name == default_provider)
                    && let Some(index) = models.iter().position(|model| {
                        default.is_some_and(|(_, default_model)| *model == default_model)
                    })
                {
                    let selected = models.remove(index);
                    models.insert(0, selected);
                }
                models.into_iter().map(move |model| ProviderModelEntry {
                    provider_name: provider_name.clone(),
                    name: definition.model_name(model).map(str::to_owned),
                    is_default: default.is_some_and(|(default_provider, default_model)| {
                        provider_name == default_provider && model == default_model
                    }),
                    model: model.to_owned(),
                })
            })
            .collect()
    }

    pub fn resolve_bound(
        &self,
        binding: &ProviderBinding,
        tools: &[ToolDescriptor],
    ) -> Result<ResolvedProvider, RegistryError> {
        let definition = self
            .config
            .providers
            .get(&binding.provider_name)
            .ok_or_else(|| {
                RegistryError::BindingMismatch("pinned provider is no longer configured".into())
            })?;
        if definition
            .require_model(&binding.provider_name, &binding.model)
            .is_err()
        {
            return Err(RegistryError::BindingMismatch(
                "pinned model is no longer configured for its provider".into(),
            ));
        }
        let resolved = self.resolve_model(&binding.provider_name, &binding.model, tools)?;
        if &resolved.binding != binding {
            return Err(RegistryError::BindingMismatch(
                "semantic provider or tool configuration changed".into(),
            ));
        }
        Ok(resolved)
    }

    /// Computes the complete Thread v2 binding for the single global default.
    pub fn thread_binding_for_default(
        &self,
        tools: &[ToolDescriptor],
    ) -> Result<ThreadProviderBindingV2, RegistryError> {
        let (provider, model) = self.default_selection()?;
        self.thread_binding_for_model(provider, model, tools)
    }

    /// Computes a complete v2 binding for one explicit catalog selection.
    pub fn thread_binding_for_model(
        &self,
        name: &str,
        model: &str,
        tools: &[ToolDescriptor],
    ) -> Result<ThreadProviderBindingV2, RegistryError> {
        let definition = self
            .config
            .providers
            .get(name)
            .ok_or_else(|| RegistryError::Invalid(format!("unknown provider {name}")))?;
        definition.require_model(name, model)?;
        let binding = Self::binding_for_model(name, definition, model, tools)?;
        let ProviderDefinition::OpenaiChat { api_key, .. } = definition;
        let result =
            binding.with_thread_scope(api_key.credential_ref_id(name), "workspace".into(), 1);
        result.validate().map_err(RegistryError::Invalid)?;
        Ok(result)
    }

    /// Returns the complete binding catalog for model discovery. Fails closed:
    /// a model whose binding cannot be constructed is an error, not a silently
    /// dropped entry, so a broken configuration is visible to the client
    /// instead of producing a partial catalog.
    pub fn thread_binding_catalog(
        &self,
        tools: &[ToolDescriptor],
    ) -> Result<Vec<BindingCatalogEntry>, RegistryError> {
        self.model_catalog()
            .into_iter()
            .map(|entry| {
                let binding =
                    self.thread_binding_for_model(&entry.provider_name, &entry.model, tools)?;
                Ok(BindingCatalogEntry {
                    provider_name: entry.provider_name,
                    model: entry.model,
                    name: entry.name,
                    is_default: entry.is_default,
                    binding,
                })
            })
            .collect()
    }

    /// Validates a persisted v2 binding before resolving the configured secret.
    pub fn resolve_thread_bound(
        &self,
        binding: &ThreadProviderBindingV2,
        tools: &[ToolDescriptor],
    ) -> Result<ResolvedProvider, RegistryError> {
        let definition = self
            .config
            .providers
            .get(&binding.provider_name)
            .ok_or_else(|| {
                RegistryError::BindingMismatch("pinned provider is no longer configured".into())
            })?;
        if definition
            .require_model(&binding.provider_name, &binding.model)
            .is_err()
        {
            return Err(RegistryError::BindingMismatch(
                "pinned model is no longer configured for its provider".into(),
            ));
        }
        let proposed =
            self.thread_binding_for_model(&binding.provider_name, &binding.model, tools)?;
        if &proposed != binding {
            return Err(RegistryError::BindingMismatch(
                "provider binding, aliases, credential reference/generation, or data scope changed"
                    .into(),
            ));
        }
        self.resolve_model(&binding.provider_name, &binding.model, tools)
    }

    /// Resolves one explicit configured provider/model pair.
    pub fn resolve_model(
        &self,
        name: &str,
        selected_model: &str,
        tools: &[ToolDescriptor],
    ) -> Result<ResolvedProvider, RegistryError> {
        let definition = self
            .config
            .providers
            .get(name)
            .ok_or_else(|| RegistryError::Invalid(format!("unknown provider {name}")))?;
        definition.require_model(name, selected_model)?;
        match definition {
            ProviderDefinition::OpenaiChat {
                base_url,
                endpoint,
                api_key,
                aliases: _,
                timeout_ms,
                max_attempts,
                temperature,
                max_tokens,
                compatibility_input_request,
                streaming,
                ..
            } => {
                let model_options = definition.model_options(selected_model).ok_or_else(|| {
                    RegistryError::Invalid(format!(
                        "unknown model {selected_model} for provider {name}"
                    ))
                })?;
                let key = match api_key {
                    SecretRef::Literal(value) => {
                        (!value.is_empty()).then(|| value.clone()).ok_or_else(|| {
                            RegistryError::Invalid(
                                "provider inline api_key must not be empty".into(),
                            )
                        })?
                    }
                    SecretRef::Env { name, .. } => env::var(name)
                        .ok()
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| RegistryError::MissingSecret(name.clone()))?,
                };
                let endpoint = endpoint.clone().unwrap_or_else(|| {
                    format!(
                        "{}/chat/completions",
                        base_url.as_deref().unwrap().trim_end_matches('/')
                    )
                });
                let binding = Self::binding_for_model(name, definition, selected_model, tools)?;
                let provider = OpenAiProvider::new(
                    endpoint,
                    selected_model,
                    key,
                    Duration::from_millis(*timeout_ms),
                )?
                .with_max_attempts(*max_attempts)
                .with_sampling_options(
                    model_options.temperature.or(*temperature),
                    model_options.max_tokens.or(*max_tokens),
                )
                .with_reasoning_effort(model_options.reasoning_effort)
                .with_compatibility_input_request(*compatibility_input_request)
                .with_streaming(*streaming);
                let reverse = binding
                    .aliases
                    .iter()
                    .map(|(canonical, wire)| (wire.clone(), canonical.clone()))
                    .collect();
                Ok(ResolvedProvider {
                    provider: Arc::new(AliasedProvider {
                        inner: provider,
                        forward: binding.aliases.clone(),
                        reverse,
                    }),
                    binding,
                })
            }
        }
    }

    fn binding_for_model(
        name: &str,
        definition: &ProviderDefinition,
        selected_model: &str,
        tools: &[ToolDescriptor],
    ) -> Result<ProviderBinding, RegistryError> {
        match definition {
            ProviderDefinition::OpenaiChat { aliases, .. } => {
                let alias_table = ToolAliases::new(tools, aliases)?.canonical_to_wire;
                Ok(ProviderBinding {
                    version: BINDING_VERSION,
                    provider_name: name.into(),
                    provider_type: "openai-chat".into(),
                    protocol: "openai-chat-completions-v1".into(),
                    model: selected_model.into(),
                    config_fingerprint: fingerprint(&semantic_definition_for_model(
                        definition,
                        selected_model,
                    )?),
                    tools_fingerprint: fingerprint(&canonical_tools(tools, &alias_table)?),
                    aliases: alias_table,
                })
            }
        }
    }

    fn validate(&self) -> Result<(), RegistryError> {
        if self.config.version != 1 {
            return Err(RegistryError::Invalid("version must be 1".into()));
        }
        if self.config.default_model.is_empty() && self.config.providers.is_empty() {
            return Ok(());
        }
        if self
            .config
            .providers
            .keys()
            .any(|name| name.trim().is_empty())
        {
            return Err(RegistryError::Invalid(
                "provider names must not be empty".into(),
            ));
        }
        let Some((default_provider, default_model)) = self.config.default_model.split_once('/')
        else {
            return Err(RegistryError::Invalid(
                "default_model must use provider/model format".into(),
            ));
        };
        if default_provider.trim().is_empty()
            || default_model.trim().is_empty()
            || self.config.default_model.len() > 2048
            || self.config.default_model.chars().any(char::is_control)
        {
            return Err(RegistryError::Invalid(
                "default_model provider/model must be non-empty, bounded, and contain no controls"
                    .into(),
            ));
        }
        let Some(default_definition) = self.config.providers.get(default_provider) else {
            return Err(RegistryError::Invalid(
                "default_model provider must name a configured provider".into(),
            ));
        };
        if !default_definition
            .available_models()
            .contains(&default_model)
        {
            return Err(RegistryError::Invalid(
                "default_model model must be configured for its provider".into(),
            ));
        }
        for (name, provider) in &self.config.providers {
            validate_provider(name, provider)?;
        }
        Ok(())
    }

    fn default_selection(&self) -> Result<(&str, &str), RegistryError> {
        self.config.default_model.split_once('/').ok_or_else(|| {
            RegistryError::Invalid(
                "default_model must be configured as provider/model before provider use".into(),
            )
        })
    }
}

fn validate_provider(name: &str, provider: &ProviderDefinition) -> Result<(), RegistryError> {
    let ProviderDefinition::OpenaiChat {
        models,
        base_url,
        endpoint,
        timeout_ms,
        max_attempts,
        temperature,
        ..
    } = provider;
    let model_ids = models.ids();
    if model_ids.is_empty()
        || model_ids.len() > 256
        || model_ids.iter().any(|model| {
            model.trim().is_empty() || model.len() > 1024 || model.chars().any(char::is_control)
        })
        || model_ids.iter().collect::<BTreeSet<_>>().len() != model_ids.len()
    {
        return Err(RegistryError::Invalid(format!(
            "provider {name} models must be unique, non-empty, and bounded"
        )));
    }
    if base_url.is_some() == endpoint.is_some() {
        return Err(RegistryError::Invalid(format!(
            "provider {name} requires exactly one of base_url or endpoint"
        )));
    }
    if *timeout_ms == 0 || *max_attempts == 0 || *max_attempts > 10 {
        return Err(RegistryError::Invalid(format!(
            "provider {name} timeout/attempts are out of range"
        )));
    }
    if temperature.is_some_and(|value| !value.is_finite() || !(0.0..=2.0).contains(&value)) {
        return Err(RegistryError::Invalid(format!(
            "provider {name} temperature must be between 0 and 2"
        )));
    }
    for (model, config) in models.configured() {
        let options = &config.options;
        if options.context_window == Some(0)
            || options.max_tokens == Some(0)
            || options
                .context_window
                .zip(options.max_tokens)
                .is_some_and(|(context_window, max_tokens)| max_tokens >= context_window)
            || options
                .temperature
                .is_some_and(|value| !value.is_finite() || !(0.0..=2.0).contains(&value))
            || options.reasoning_effort.as_ref().is_some_and(|effort| {
                effort.trim().is_empty()
                    || effort.len() > 64
                    || effort.chars().any(char::is_control)
            })
            || config.name.as_ref().is_some_and(|display_name| {
                display_name.trim().is_empty()
                    || display_name.len() > 128
                    || display_name.chars().any(char::is_control)
            })
        {
            return Err(RegistryError::Invalid(format!(
                "provider {name} model {model} options are invalid"
            )));
        }
    }
    Ok(())
}

struct AliasedProvider {
    inner: OpenAiProvider,
    forward: BTreeMap<String, String>,
    reverse: BTreeMap<String, String>,
}
impl Provider for AliasedProvider {
    fn complete(
        &self,
        mut request: ProviderRequest,
        context: ProviderContext,
    ) -> ProviderFuture<'_> {
        let lowered = (|| {
            for tool in &mut request.tools {
                tool.name = self.forward.get(&tool.name).cloned().ok_or_else(|| {
                    ProviderError::Malformed("tool binding does not contain a declaration".into())
                })?;
            }
            for message in &mut request.messages {
                match message {
                    Message::Assistant { tool_calls, .. } => {
                        for call in tool_calls {
                            call.name = self.forward.get(&call.name).cloned().ok_or_else(|| {
                                ProviderError::Malformed(
                                    "historical tool call is not in the pinned binding".into(),
                                )
                            })?;
                        }
                    }
                    Message::Tool {
                        name: Some(name), ..
                    } => {
                        *name = self.forward.get(name).cloned().ok_or_else(|| {
                            ProviderError::Malformed(
                                "historical tool result is not in the pinned binding".into(),
                            )
                        })?
                    }
                    _ => {}
                }
            }
            Ok(request)
        })();
        Box::pin(async move {
            let request = lowered?;
            let mut outcome = self.inner.complete(request, context).await?;
            for call in &mut outcome.tool_calls {
                call.name = self.reverse.get(&call.name).cloned().ok_or_else(|| {
                    ProviderError::Malformed("provider returned an unknown tool alias".into())
                })?;
            }
            Ok(outcome)
        })
    }
    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }
}

struct ToolAliases {
    canonical_to_wire: BTreeMap<String, String>,
}
impl ToolAliases {
    fn new(
        tools: &[ToolDescriptor],
        configured: &BTreeMap<String, String>,
    ) -> Result<Self, RegistryError> {
        let names: BTreeSet<_> = tools.iter().map(|t| t.name.as_str()).collect();
        if configured.keys().any(|n| !names.contains(n.as_str())) {
            return Err(RegistryError::Invalid(
                "alias references an unknown canonical tool".into(),
            ));
        }
        let mut result = BTreeMap::new();
        let mut wire = BTreeSet::new();
        for tool in tools {
            let alias = configured
                .get(&tool.name)
                .cloned()
                .unwrap_or_else(|| deterministic_alias(&tool.name));
            if !valid_wire_name(&alias) || !wire.insert(alias.clone()) {
                return Err(RegistryError::Invalid(format!(
                    "tool alias collision or invalid alias: {alias}"
                )));
            }
            result.insert(tool.name.clone(), alias);
        }
        Ok(Self {
            canonical_to_wire: result,
        })
    }
}

fn deterministic_alias(name: &str) -> String {
    if valid_wire_name(name) {
        return name.into();
    }
    let digest = hex_digest(name.as_bytes());
    format!("tool_{}", &digest[..16])
}
fn valid_wire_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_ALIAS_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
fn default_timeout() -> u64 {
    60_000
}
fn default_attempts() -> u32 {
    1
}
fn fingerprint(value: &serde_json::Value) -> String {
    hex_digest(
        serde_json::to_string(value)
            .expect("canonical JSON")
            .as_bytes(),
    )
}
fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn semantic_definition_for_model(
    value: &ProviderDefinition,
    selected_model: &str,
) -> Result<serde_json::Value, RegistryError> {
    let selected_options = value
        .model_options(selected_model)
        .ok_or_else(|| RegistryError::Invalid(format!("unknown model {selected_model}")))?;
    let mut v = serde_json::to_value(value).map_err(|e| RegistryError::Invalid(e.to_string()))?;
    if let Some(obj) = v.as_object_mut() {
        obj.remove("api_key");
        obj.remove("models");
        obj.insert(
            "model".into(),
            serde_json::Value::String(selected_model.into()),
        );
        obj.insert(
            "model_options".into(),
            serde_json::to_value(selected_options)
                .map_err(|error| RegistryError::Invalid(error.to_string()))?,
        );
    }
    Ok(serde_json::json!({"version":1,"provider":v}))
}
fn canonical_tools(
    tools: &[ToolDescriptor],
    aliases: &BTreeMap<String, String>,
) -> Result<serde_json::Value, RegistryError> {
    let mut tools = tools.to_vec();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    serde_json::to_value(serde_json::json!({"version":1,"tools":tools,"aliases":aliases}))
        .map_err(|e| RegistryError::Invalid(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
        time::Instant,
    };
    fn tool(name: &str) -> ToolDescriptor {
        ToolDescriptor {
            name: name.into(),
            description: "x".into(),
            input_schema: serde_json::json!({"type":"object"}),
            version: 1,
            effect: "read".into(),
        }
    }
    fn capturing_server(response_body: &str) -> (String, mpsc::Receiver<serde_json::Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let response_body = response_body.to_owned();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 16 * 1024];
            let count = socket.read(&mut buffer).unwrap();
            let start = buffer[..count]
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
                + 4;
            tx.send(serde_json::from_slice(&buffer[start..count]).unwrap())
                .unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            socket.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}"), rx)
    }
    #[test]
    fn config_is_strict_and_secret_is_redacted() {
        let r=ProviderRegistry::parse_jsonc(r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],base_url:'https://api.example/v1',api_key:{source:'env',name:'KEY'}}}}").unwrap();
        assert_eq!(r.default_name(), Some("main"));
        assert!(!format!("{:?}", r.config).contains("secret-value"));
        let inline_secret = "inline-secret-must-be-redacted";
        let inline = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],base_url:'https://api.example/v1',api_key:'inline-secret-must-be-redacted'}}}",
        )
        .unwrap();
        assert!(inline.resolve_default(&[]).is_ok());
        let inline_binding = inline.thread_binding_for_default(&[]).unwrap();
        assert_eq!(inline_binding.credential_ref_id, "config:main/api_key");
        assert_eq!(inline_binding.data_scope_id, "workspace");
        assert_eq!(inline_binding.credential_generation, 1);
        assert!(!format!("{:?}", inline.config).contains(inline_secret));
        let empty = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],base_url:'https://api.example/v1',api_key:''}}}",
        )
        .unwrap();
        assert!(matches!(
            empty.resolve_default(&[]),
            Err(RegistryError::Invalid(message)) if message == "provider inline api_key must not be empty"
        ));
        assert!(
            ProviderRegistry::parse_jsonc(r"{version:1,default_model:'x/m',providers:{},wat:1}")
                .is_err()
        );
    }

    #[test]
    fn complete_example_config_parses_but_unknown_provider_fields_do_not() {
        let example = include_str!("../../../latte-code.config.example.jsonc");
        let registry = ProviderRegistry::parse_jsonc(example).unwrap();
        assert_eq!(registry.default_name(), Some("primary"));

        for field in [
            "unsupported_provider_option",
            "credential_ref_id",
            "data_scope_id",
            "credential_generation",
        ] {
            let mut invalid: serde_json::Value = json5::from_str(example).unwrap();
            invalid["providers"]["primary"][field] = serde_json::Value::Bool(true);
            assert!(
                ProviderRegistry::parse_jsonc(&serde_json::to_string(&invalid).unwrap()).is_err(),
                "{field} must not be public provider configuration"
            );
        }
    }
    #[test]
    fn aliases_are_deterministic_bijective_and_byte_bounded() {
        let tools = vec![tool("合法/名字")];
        let a = ToolAliases::new(&tools, &BTreeMap::new()).unwrap();
        let b = ToolAliases::new(&tools, &BTreeMap::new()).unwrap();
        assert_eq!(a.canonical_to_wire, b.canonical_to_wire);
        assert!(
            a.canonical_to_wire
                .values()
                .all(|v| v.len() <= 64 && valid_wire_name(v))
        );
    }
    #[test]
    fn configured_aliases_reject_unknown_invalid_and_duplicate_values() {
        let tools = vec![tool("read_file"), tool("search")];
        for configured in [
            BTreeMap::from([("missing".into(), "wire".into())]),
            BTreeMap::from([("read_file".into(), "not valid!".into())]),
            BTreeMap::from([
                ("read_file".into(), "same".into()),
                ("search".into(), "same".into()),
            ]),
        ] {
            assert!(ToolAliases::new(&tools, &configured).is_err());
        }
    }

    #[test]
    fn thread_bindings_are_scoped_pinned_and_validated_before_secret_lookup() {
        let tools = vec![tool("read_file"), tool("search")];
        let registry = ProviderRegistry::parse_jsonc(
            r"{
                version: 1,
                default_model: 'main/gpt-test',
                providers: {
                    main: {
                        type: 'openai-chat', models: ['gpt-test'],
                        endpoint: 'https://example.invalid/v1/chat/completions',
                        api_key: {source: 'env', name: 'NEVER_LOOK_UP_THIS_KEY'},
                        streaming: true,
                        aliases: {read_file: 'rf'},
                    }
                }
            }",
        )
        .unwrap();
        let binding = registry.thread_binding_for_default(&tools).unwrap();
        assert_eq!(binding.provider_name, "main");
        assert_eq!(binding.aliases["read_file"], "rf");
        assert_eq!(binding.credential_ref_id, "env:NEVER_LOOK_UP_THIS_KEY");
        assert_eq!(binding.data_scope_id, "workspace");
        assert_eq!(binding.credential_generation, 1);
        assert!(matches!(
            registry.resolve_thread_bound(&binding, &tools),
            Err(RegistryError::MissingSecret(name)) if name == "NEVER_LOOK_UP_THIS_KEY"
        ));

        let mut changed = binding.clone();
        changed.credential_generation += 1;
        assert!(matches!(
            registry.resolve_thread_bound(&changed, &tools),
            Err(RegistryError::BindingMismatch(_))
        ));
        assert!(matches!(
            registry.thread_binding_for_model("unknown", "gpt-test", &tools),
            Err(RegistryError::Invalid(message)) if message.contains("unknown provider")
        ));
    }

    #[test]
    fn registry_rejects_invalid_semantics() {
        let invalid = [
            r"{version:2,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_model:'missing/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_model:'',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',base_url:'https://y',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:{source:'env',name:'K'},timeout_ms:0}}}",
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:{source:'env',name:'K'},max_attempts:11}}}",
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:{source:'env',name:'K'},temperature:3}}}",
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:[''],endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_model:'main/missing',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m','m'],endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_model:'main/bad\nmodel',providers:{main:{type:'openai-chat',models:['bad\nmodel'],endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:{m:{options:{context_window:0}}},endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:{m:{options:{context_window:10,max_tokens:10}}},endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:{m:{options:{reasoning_effort:''}}},endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:{m:{options:{anthropic_thinking_budget:10}}},endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:{m:{name:' '}}},endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
        ];
        for source in invalid {
            assert!(ProviderRegistry::parse_jsonc(source).is_err(), "{source}");
        }
    }

    #[test]
    fn bindings_and_aliases_are_stable_when_tool_order_changes() {
        let definition = ProviderDefinition::OpenaiChat {
            models: OpenAiChatModels::Ids(vec!["m".into()]),
            base_url: Some("https://example.invalid/v1".into()),
            endpoint: None,
            api_key: SecretRef::Env {
                source: SecretSource::Env,
                name: "K".into(),
            },
            timeout_ms: default_timeout(),
            max_attempts: default_attempts(),
            temperature: None,
            max_tokens: None,
            compatibility_input_request: false,
            streaming: false,
            aliases: BTreeMap::default(),
        };
        let forward = ProviderRegistry::binding_for_model(
            "main",
            &definition,
            "m",
            &[tool("search"), tool("read_file")],
        )
        .unwrap();
        let reverse = ProviderRegistry::binding_for_model(
            "main",
            &definition,
            "m",
            &[tool("read_file"), tool("search")],
        )
        .unwrap();
        assert_eq!(forward, reverse);
        assert_eq!(deterministic_alias("valid_name"), "valid_name");
        assert!(deterministic_alias("not valid/name").starts_with("tool_"));
        assert!(valid_wire_name("letters-123_"));
        assert!(!valid_wire_name(""));
        assert!(!valid_wire_name(&"x".repeat(MAX_ALIAS_BYTES + 1)));
        let semantic = semantic_definition_for_model(&definition, "m").unwrap();
        assert!(semantic["provider"].get("api_key").is_none());
        assert_eq!(fingerprint(&semantic), fingerprint(&semantic));
    }

    #[test]
    fn resolved_binding_is_exact_and_uses_nonsecret_semantics_only() {
        // PATH is guaranteed by the cross-platform test runners and is used
        // only to construct a provider object; this test never opens a network
        // connection. It proves binding equality is checked after the normal
        // resolution path.
        let registry = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'main/m',providers:{main:{
                type:'openai-chat',models:['m'],base_url:'https://example.invalid/v1',
                api_key:{source:'env',name:'PATH'}
            }}}",
        )
        .unwrap();
        let tools = vec![tool("read_file")];
        let binding = registry.resolve_default(&tools).unwrap().binding;
        assert!(registry.resolve_bound(&binding, &tools).is_ok());
        let mut changed = binding;
        changed.model = "other".into();
        assert!(matches!(
            registry.resolve_bound(&changed, &tools),
            Err(RegistryError::BindingMismatch(_))
        ));
        let mut changed = registry.resolve_default(&tools).unwrap().binding;
        changed.config_fingerprint = "changed".into();
        assert!(matches!(
            registry.resolve_bound(&changed, &tools),
            Err(RegistryError::BindingMismatch(message)) if message.contains("semantic")
        ));
    }

    #[test]
    fn model_catalog_groups_provider_models_and_resolves_explicit_selection() {
        let registry = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'alpha/family/a-default',providers:{
                alpha:{type:'openai-chat',models:{
                    'family/a-default':{name:'Alpha Default',options:{context_window:128000,reasoning_effort:'high',max_tokens:4096}},
                    'a-fast':{},'a-large':{}
                },endpoint:'https://a',api_key:{source:'env',name:'PATH'}},
                beta:{type:'openai-chat',models:['b-default','b-reasoning'],endpoint:'https://b',api_key:{source:'env',name:'PATH'}}
            }}",
        )
        .unwrap();
        assert_eq!(registry.default_name(), Some("alpha"));
        assert_eq!(registry.default_model(), Some("family/a-default"));
        assert_eq!(
            registry
                .model_catalog()
                .into_iter()
                .map(|entry| (entry.provider_name, entry.model, entry.is_default))
                .collect::<Vec<_>>(),
            vec![
                ("alpha".into(), "family/a-default".into(), true),
                ("alpha".into(), "a-fast".into(), false),
                ("alpha".into(), "a-large".into(), false),
                ("beta".into(), "b-default".into(), false),
                ("beta".into(), "b-reasoning".into(), false),
            ]
        );
        let catalog = registry.model_catalog();
        assert_eq!(catalog[0].name.as_deref(), Some("Alpha Default"));
        assert!(catalog[1..].iter().all(|entry| entry.name.is_none()));
        let semantic = semantic_definition_for_model(
            registry.config.providers.get("alpha").unwrap(),
            "family/a-default",
        )
        .unwrap();
        assert!(!semantic.to_string().contains("Alpha Default"));
        let selected = registry
            .thread_binding_for_model("beta", "b-reasoning", &[])
            .unwrap();
        assert_eq!(selected.provider_name, "beta");
        assert_eq!(selected.model, "b-reasoning");
        assert!(registry.resolve_thread_bound(&selected, &[]).is_ok());
        assert!(matches!(
            registry.thread_binding_for_model("beta", "missing", &[]),
            Err(RegistryError::Invalid(message)) if message.contains("unknown model")
        ));
        let mut missing_provider = selected.clone();
        missing_provider.provider_name = "missing".into();
        assert!(matches!(
            registry.resolve_thread_bound(&missing_provider, &[]),
            Err(RegistryError::BindingMismatch(message)) if message.contains("provider")
        ));
        let mut missing_model = selected;
        missing_model.model = "missing".into();
        assert!(matches!(
            registry.resolve_thread_bound(&missing_model, &[]),
            Err(RegistryError::BindingMismatch(message)) if message.contains("model")
        ));

        let resolved = registry.resolve_model("beta", "b-reasoning", &[]).unwrap();
        let mut legacy_missing_provider = resolved.binding.clone();
        legacy_missing_provider.provider_name = "missing".into();
        assert!(matches!(
            registry.resolve_bound(&legacy_missing_provider, &[]),
            Err(RegistryError::BindingMismatch(message)) if message.contains("provider")
        ));
        let mut legacy_missing_model = resolved.binding;
        legacy_missing_model.model = "missing".into();
        assert!(matches!(
            registry.resolve_bound(&legacy_missing_model, &[]),
            Err(RegistryError::BindingMismatch(message)) if message.contains("model")
        ));
    }

    #[tokio::test]
    async fn aliased_provider_rejects_unpinned_history_before_network_and_exposes_capabilities() {
        let provider = AliasedProvider {
            inner: OpenAiProvider::new(
                "https://example.invalid/v1/chat/completions",
                "m",
                "test-key",
                Duration::from_secs(1),
            )
            .unwrap(),
            forward: BTreeMap::new(),
            reverse: BTreeMap::new(),
        };
        assert!(provider.capabilities().tools);
        let context = || ProviderContext {
            deadline: Instant::now() + Duration::from_millis(10),
            cancellation: latte_engine::CancellationToken::new(),
            events: None,
        };
        let tool_request = ProviderRequest {
            messages: vec![],
            tools: vec![tool("read_file")],
        };
        assert!(matches!(
            provider.complete(tool_request, context()).await,
            Err(ProviderError::Malformed(message)) if message.contains("declaration")
        ));
        let assistant_request = ProviderRequest {
            messages: vec![Message::Assistant {
                content: None,
                tool_calls: vec![crate::provider::ToolCall {
                    id: "call".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({}),
                }],
            }],
            tools: vec![],
        };
        assert!(matches!(
            provider.complete(assistant_request, context()).await,
            Err(ProviderError::Malformed(message)) if message.contains("historical tool call")
        ));
        let tool_result_request = ProviderRequest {
            messages: vec![Message::Tool {
                tool_call_id: "call".into(),
                name: Some("read_file".into()),
                content: "result".into(),
            }],
            tools: vec![],
        };
        assert!(matches!(
            provider.complete(tool_result_request, context()).await,
            Err(ProviderError::Malformed(message)) if message.contains("historical tool result")
        ));
    }

    #[tokio::test]
    async fn aliased_provider_rejects_unknown_response_alias_after_real_transport() {
        let response = r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"call","function":{"name":"unknown","arguments":"{}"}}]}}]}"#;
        let (endpoint, _captured) = capturing_server(response);
        let provider = AliasedProvider {
            inner: OpenAiProvider::new(endpoint, "m", "test-key", Duration::from_secs(1)).unwrap(),
            forward: BTreeMap::new(),
            reverse: BTreeMap::new(),
        };
        assert!(matches!(
            provider
                .complete(
                    ProviderRequest {
                        messages: vec![],
                        tools: vec![],
                    },
                    ProviderContext {
                        deadline: Instant::now() + Duration::from_secs(1),
                        cancellation: latte_engine::CancellationToken::new(),
                        events: None,
                    },
                )
                .await,
            Err(ProviderError::Malformed(message)) if message.contains("unknown tool alias")
        ));
    }

    #[test]
    fn registry_rejects_empty_provider_name() {
        assert!(ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'/m',providers:{'':{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:{source:'env',name:'K'}}}}"
        )
        .is_err());
    }

    #[tokio::test]
    async fn aliased_provider_lowers_history_and_reverse_maps_response() {
        let tools = vec![tool("read_file")];
        let response = r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"new","function":{"name":"rf","arguments":"{}"}}]}}]}"#;
        let (endpoint, captured) = capturing_server(response);
        let forward = BTreeMap::from([("read_file".into(), "rf".into())]);
        let provider = AliasedProvider {
            inner: OpenAiProvider::new(endpoint, "m", "test-key", Duration::from_secs(1))
                .unwrap()
                .with_sampling_options(Some(0.4), Some(77)),
            reverse: BTreeMap::from([("rf".into(), "read_file".into())]),
            forward,
        };
        let outcome = provider
            .complete(
                ProviderRequest {
                    messages: vec![Message::Assistant {
                        content: None,
                        tool_calls: vec![crate::provider::ToolCall {
                            id: "old".into(),
                            name: "read_file".into(),
                            input: serde_json::json!({}),
                        }],
                    }],
                    tools,
                },
                ProviderContext {
                    deadline: Instant::now() + Duration::from_secs(2),
                    cancellation: latte_engine::CancellationToken::new(),
                    events: None,
                },
            )
            .await
            .unwrap();
        let outbound = captured.recv().unwrap();
        assert_eq!(outbound["temperature"], 0.4);
        assert_eq!(outbound["max_tokens"], 77);
        assert_eq!(outbound["tools"][0]["function"]["name"], "rf");
        assert_eq!(
            outbound["messages"][0]["tool_calls"][0]["function"]["name"],
            "rf"
        );
        assert_eq!(outcome.tool_calls[0].name, "read_file");
    }

    #[test]
    fn thread_binding_catalog_returns_every_configured_model() {
        // The happy path: a well-formed config produces a complete catalog with
        // one entry per configured model, default flagged.
        let registry = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'alpha/a-default',providers:{
                alpha:{type:'openai-chat',models:['a-default','a-fast'],endpoint:'https://a',api_key:{source:'env',name:'PATH'}},
                beta:{type:'openai-chat',models:['b-default'],endpoint:'https://b',api_key:{source:'env',name:'PATH'}}
            }}",
        )
        .unwrap();
        let catalog = registry
            .thread_binding_catalog(&[tool("read_file")])
            .unwrap();
        assert_eq!(
            catalog
                .iter()
                .map(|entry| (
                    entry.provider_name.as_str(),
                    entry.model.as_str(),
                    entry.is_default
                ))
                .collect::<Vec<_>>(),
            vec![
                ("alpha", "a-default", true),
                ("alpha", "a-fast", false),
                ("beta", "b-default", false),
            ]
        );
        // Every entry carries a fully-constructed, valid binding.
        assert!(catalog.iter().all(|entry| entry.binding.validate().is_ok()));
    }

    #[test]
    fn thread_binding_catalog_fails_closed_on_a_broken_model() {
        // A model whose binding cannot be constructed (here: an alias that
        // references a tool absent from the descriptor set) must surface as an
        // error, not be silently dropped from the catalog. A client that asked
        // for the model list must see the broken configuration rather than a
        // catalog that looks complete but is missing an entry.
        let registry = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'alpha/a-default',providers:{
                alpha:{type:'openai-chat',models:['a-default'],endpoint:'https://a',api_key:{source:'env',name:'PATH'},aliases:{nonexistent_tool:'x'}}
            }}",
        )
        .unwrap();
        // The model is listed in the catalog...
        assert_eq!(registry.model_catalog().len(), 1);
        // ...but its binding cannot be built with these tools, so the catalog
        // fails closed instead of returning an empty (silently partial) list.
        assert!(matches!(
            registry.thread_binding_catalog(&[tool("read_file")]),
            Err(RegistryError::Invalid(message)) if message.contains("unknown canonical tool")
        ));
    }
}
