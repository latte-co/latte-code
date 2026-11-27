use crate::provider::{
    ANTHROPIC_RESERVED_HEADERS, AnthropicProvider, Message, OpenAiProvider, Provider,
    ProviderCapabilities, ProviderContext, ProviderError, ProviderFuture, ProviderRequest,
    RESERVED_HEADERS, SESSION_ID_PLACEHOLDER,
};
use latte_core::{SessionProviderBinding, ToolPresentationStyle};
use latte_engine::ToolDescriptor;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
        #[serde(default = "default_streaming")]
        streaming: bool,
        /// Extra request headers this Provider requires beyond the Chat
        /// Completions protocol. `${session_id}` in a value expands to an
        /// opaque per-Session identifier; other `${...}` names are rejected at
        /// load time. Reserved headers cannot be set here.
        // Empty maps are omitted from serialization so the config fingerprint of
        // a headerless provider is byte-identical to bindings persisted before
        // the `headers` field existed (upgrade compatibility). Configured
        // (non-empty) headers still serialize and bind.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        aliases: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_style: Option<ToolPresentationStyle>,
    },
    AnthropicMessages {
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
        #[serde(default = "default_streaming")]
        streaming: bool,
        #[serde(default = "default_prompt_caching")]
        prompt_caching: bool,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        aliases: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_style: Option<ToolPresentationStyle>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<AnthropicAuthStyle>,
    },
}

/// Validates one provider's configured headers before any request is built.
/// Failing at load time keeps a typo from silently dropping the header a
/// Provider requires — some reject the whole request without it.
fn validate_headers(
    headers: &BTreeMap<String, String>,
    reserved: &[&str],
) -> Result<(), RegistryError> {
    for (name, value) in headers {
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err(RegistryError::Invalid(format!(
                "header name {name:?} must be non-empty and use only ASCII letters, digits, `-`, or `_`"
            )));
        }
        if reserved.contains(&name.to_ascii_lowercase().as_str()) {
            return Err(RegistryError::Invalid(format!(
                "header {name:?} is derived by the provider and cannot be configured"
            )));
        }
        if value.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(RegistryError::Invalid(format!(
                "header {name:?} value must not contain control characters"
            )));
        }
        let mut rest = value.as_str();
        while let Some(start) = rest.find("${") {
            let end = rest[start..].find('}').map(|offset| start + offset + 1);
            let Some(end) = end else {
                return Err(RegistryError::Invalid(format!(
                    "header {name:?} has an unterminated placeholder"
                )));
            };
            let placeholder = &rest[start..end];
            if placeholder != SESSION_ID_PLACEHOLDER {
                return Err(RegistryError::Invalid(format!(
                    "header {name:?} uses unknown placeholder {placeholder}; only {SESSION_ID_PLACEHOLDER} is supported"
                )));
            }
            rest = &rest[end..];
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AnthropicAuthStyle {
    #[default]
    #[serde(alias = "api_key", alias = "x_api_key")]
    XApiKey,
    Bearer,
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
    fn models_catalog(&self) -> &OpenAiChatModels {
        match self {
            Self::OpenaiChat { models, .. } | Self::AnthropicMessages { models, .. } => models,
        }
    }

    fn api_key_ref(&self) -> &SecretRef {
        match self {
            Self::OpenaiChat { api_key, .. } | Self::AnthropicMessages { api_key, .. } => api_key,
        }
    }

    fn effective_tool_style(&self) -> ToolPresentationStyle {
        match self {
            Self::OpenaiChat { tool_style, .. } => {
                tool_style.unwrap_or(ToolPresentationStyle::Canonical)
            }
            Self::AnthropicMessages { tool_style, .. } => {
                tool_style.unwrap_or(ToolPresentationStyle::ClaudeCode)
            }
        }
    }

    fn available_models(&self) -> Vec<&str> {
        self.models_catalog().ids()
    }

    fn model_options(&self, selected: &str) -> Option<OpenAiChatModelOptions> {
        self.models_catalog().options(selected)
    }

    fn model_name(&self, selected: &str) -> Option<&str> {
        self.models_catalog().name(selected)
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
pub struct ProviderModelBinding {
    #[serde(alias = "version")]
    pub binding_version: u32,
    pub provider_name: String,
    pub provider_type: String,
    pub protocol: String,
    pub model: String,
    pub config_fingerprint: String,
    pub tools_fingerprint: String,
    pub aliases: BTreeMap<String, String>,
}
impl ProviderModelBinding {
    #[cfg(test)]
    pub(crate) fn direct(tools: &[ToolDescriptor]) -> Self {
        Self {
            binding_version: BINDING_VERSION,
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
    pub fn with_session_scope(
        &self,
        credential_ref_id: String,
        data_scope_id: String,
        credential_generation: u64,
    ) -> SessionProviderBinding {
        SessionProviderBinding {
            binding_version: self.binding_version,
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
    pub binding: ProviderModelBinding,
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
    pub binding: SessionProviderBinding,
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
            object.remove("$schema");
            object.remove("database");
            object.remove("verification");
            // Application-owned transcript-history limits are validated by
            // `latte_code::AppConfig`; they are not provider semantics. Keep
            // the provider schema strict after removing every documented
            // application-owned top-level section.
            object.remove("session");
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
        binding: &ProviderModelBinding,
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

    /// Computes the complete Session v2 binding for the single global default.
    pub fn session_binding_for_default(
        &self,
        tools: &[ToolDescriptor],
    ) -> Result<SessionProviderBinding, RegistryError> {
        let (provider, model) = self.default_selection()?;
        self.session_binding_for_model(provider, model, tools)
    }

    /// Computes a complete v2 binding for one explicit catalog selection.
    pub fn session_binding_for_model(
        &self,
        name: &str,
        model: &str,
        tools: &[ToolDescriptor],
    ) -> Result<SessionProviderBinding, RegistryError> {
        let definition = self
            .config
            .providers
            .get(name)
            .ok_or_else(|| RegistryError::Invalid(format!("unknown provider {name}")))?;
        definition.require_model(name, model)?;
        let binding = Self::binding_for_model(name, definition, model, tools)?;
        let result = binding.with_session_scope(
            definition.api_key_ref().credential_ref_id(name),
            "workspace".into(),
            1,
        );
        result.validate().map_err(RegistryError::Invalid)?;
        Ok(result)
    }

    /// Returns the complete binding catalog for model discovery. Fails closed:
    /// a model whose binding cannot be constructed is an error, not a silently
    /// dropped entry, so a broken configuration is visible to the client
    /// instead of producing a partial catalog.
    pub fn session_binding_catalog(
        &self,
        tools: &[ToolDescriptor],
    ) -> Result<Vec<BindingCatalogEntry>, RegistryError> {
        self.model_catalog()
            .into_iter()
            .map(|entry| {
                let binding =
                    self.session_binding_for_model(&entry.provider_name, &entry.model, tools)?;
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

    /// Returns the declared context window (in tokens) of one configured
    /// model, when the configuration declares one. Profile resolution uses
    /// this to tighten per-model context budgets; the value never widens a
    /// configured bound.
    #[must_use]
    pub fn model_context_window(&self, provider: &str, model: &str) -> Option<u32> {
        self.config
            .providers
            .get(provider)?
            .model_options(model)
            .and_then(|options| options.context_window)
    }

    #[must_use]
    pub fn provider_tool_presentation(&self, provider: &str) -> Option<ToolPresentationStyle> {
        self.config
            .providers
            .get(provider)
            .map(ProviderDefinition::effective_tool_style)
    }

    /// Validates a persisted v2 binding before resolving the configured secret.
    pub fn resolve_session_bound(
        &self,
        binding: &SessionProviderBinding,
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
            self.session_binding_for_model(&binding.provider_name, &binding.model, tools)?;
        // Upgrade compatibility: accept a pre-upgrade binding whose only drift
        // is the built-in tool description prose (the security-identity fields,
        // including tool effects/schemas via the legacy recomputation, still
        // match). See `legacy_placeholder_tools_fingerprint`.
        let legacy_tools_fingerprint =
            legacy_placeholder_tools_fingerprint(tools, &proposed.aliases)?;
        if !binding_matches_with_legacy_tools(binding, &proposed, &legacy_tools_fingerprint) {
            return Err(RegistryError::BindingMismatch(
                "provider binding, aliases, credential reference/generation, or data scope changed"
                    .into(),
            ));
        }
        self.resolve_model(&binding.provider_name, &binding.model, tools)
    }

    /// `#[doc(hidden)]` upgrade-compat helper for integration fixtures:
    /// reproduces the pre-upgrade `tools_fingerprint` (legacy placeholder tool
    /// documentation) for a configured model against the current tool set, so a
    /// fixture can persist real base-version binding bytes.
    #[doc(hidden)]
    pub fn legacy_tools_fingerprint_for_model(
        &self,
        name: &str,
        model: &str,
        tools: &[ToolDescriptor],
    ) -> Result<String, RegistryError> {
        let proposed = self.session_binding_for_model(name, model, tools)?;
        legacy_placeholder_tools_fingerprint(tools, &proposed.aliases)
    }

    /// Resolves one explicit configured provider/model pair.
    #[allow(clippy::too_many_lines)]
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
        let model_options = definition.model_options(selected_model).ok_or_else(|| {
            RegistryError::Invalid(format!(
                "unknown model {selected_model} for provider {name}"
            ))
        })?;
        let key = resolve_secret(definition.api_key_ref())?;
        let binding = Self::binding_for_model(name, definition, selected_model, tools)?;
        let reverse: BTreeMap<String, String> = binding
            .aliases
            .iter()
            .map(|(canonical, wire)| (wire.clone(), canonical.clone()))
            .collect();
        let style = definition.effective_tool_style();
        match definition {
            ProviderDefinition::OpenaiChat {
                base_url,
                endpoint,
                timeout_ms,
                max_attempts,
                temperature,
                max_tokens,
                compatibility_input_request,
                streaming,
                headers,
                ..
            } => {
                let endpoint = endpoint.clone().unwrap_or_else(|| {
                    format!(
                        "{}/chat/completions",
                        base_url.as_deref().unwrap().trim_end_matches('/')
                    )
                });
                validate_headers(headers, &RESERVED_HEADERS)?;
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
                .with_streaming(*streaming)
                .with_headers(headers.clone());
                Ok(ResolvedProvider {
                    provider: Arc::new(AliasedProvider {
                        inner: provider,
                        forward: binding.aliases.clone(),
                        reverse,
                        style,
                    }),
                    binding,
                })
            }
            ProviderDefinition::AnthropicMessages {
                base_url,
                endpoint,
                timeout_ms,
                max_attempts,
                temperature,
                max_tokens,
                streaming,
                prompt_caching,
                headers,
                auth,
                ..
            } => {
                let endpoint = endpoint
                    .clone()
                    .unwrap_or_else(|| derive_anthropic_endpoint(base_url.as_deref().unwrap()));
                validate_headers(headers, &ANTHROPIC_RESERVED_HEADERS)?;
                let provider = AnthropicProvider::new(
                    endpoint,
                    selected_model,
                    key,
                    Duration::from_millis(*timeout_ms),
                )?
                .with_bearer_auth(*auth == Some(AnthropicAuthStyle::Bearer))
                .with_max_attempts(*max_attempts)
                .with_sampling_options(
                    model_options.temperature.or(*temperature),
                    model_options.max_tokens.or(*max_tokens),
                )
                .with_reasoning_effort(model_options.reasoning_effort)
                .with_streaming(*streaming)
                .with_prompt_caching(*prompt_caching)
                .with_headers(headers.clone());
                Ok(ResolvedProvider {
                    provider: Arc::new(AliasedProvider {
                        inner: provider,
                        forward: binding.aliases.clone(),
                        reverse,
                        style,
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
    ) -> Result<ProviderModelBinding, RegistryError> {
        let style = definition.effective_tool_style();
        let (provider_type, protocol, aliases) = match definition {
            ProviderDefinition::OpenaiChat { aliases, .. } => {
                ("openai-chat", "openai-chat-completions-v1", aliases)
            }
            ProviderDefinition::AnthropicMessages { aliases, .. } => {
                ("anthropic-messages", "anthropic-messages-v1", aliases)
            }
        };
        let alias_table = ToolAliases::new(tools, aliases, style)?.canonical_to_wire;
        Ok(ProviderModelBinding {
            binding_version: BINDING_VERSION,
            provider_name: name.into(),
            provider_type: provider_type.into(),
            protocol: protocol.into(),
            model: selected_model.into(),
            config_fingerprint: fingerprint(&semantic_definition_for_model(
                definition,
                selected_model,
            )?),
            tools_fingerprint: fingerprint(&canonical_tools_with_style(
                tools,
                &alias_table,
                style,
            )?),
            aliases: alias_table,
        })
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

fn resolve_secret(api_key: &SecretRef) -> Result<String, RegistryError> {
    match api_key {
        SecretRef::Literal(value) => (!value.is_empty()).then(|| value.clone()).ok_or_else(|| {
            RegistryError::Invalid("provider inline api_key must not be empty".into())
        }),
        SecretRef::Env { name, .. } => env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| RegistryError::MissingSecret(name.clone())),
    }
}

fn derive_anthropic_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        format!("{trimmed}/messages")
    } else {
        format!("{trimmed}/v1/messages")
    }
}

fn validate_provider(name: &str, provider: &ProviderDefinition) -> Result<(), RegistryError> {
    let (models, base_url, endpoint, timeout_ms, max_attempts, temperature, max_temp) =
        match provider {
            ProviderDefinition::OpenaiChat {
                models,
                base_url,
                endpoint,
                timeout_ms,
                max_attempts,
                temperature,
                ..
            } => (
                models,
                base_url,
                endpoint,
                timeout_ms,
                max_attempts,
                temperature,
                2.0_f64,
            ),
            ProviderDefinition::AnthropicMessages {
                models,
                base_url,
                endpoint,
                timeout_ms,
                max_attempts,
                temperature,
                ..
            } => (
                models,
                base_url,
                endpoint,
                timeout_ms,
                max_attempts,
                temperature,
                1.0_f64,
            ),
        };
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
    if temperature.is_some_and(|value| !value.is_finite() || !(0.0..=max_temp).contains(&value)) {
        return Err(RegistryError::Invalid(format!(
            "provider {name} temperature must be between 0 and {max_temp}"
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
                .is_some_and(|value| !value.is_finite() || !(0.0..=max_temp).contains(&value))
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

struct AliasedProvider<P> {
    inner: P,
    forward: BTreeMap<String, String>,
    reverse: BTreeMap<String, String>,
    style: ToolPresentationStyle,
}

impl<P: Provider> Provider for AliasedProvider<P> {
    fn complete(
        &self,
        mut request: ProviderRequest,
        context: ProviderContext,
    ) -> ProviderFuture<'_> {
        let style = self.style;
        let lowered = (|| {
            for tool in &mut request.tools {
                let canonical = tool.name.clone();
                let wire = self.forward.get(&canonical).cloned().ok_or_else(|| {
                    ProviderError::Malformed("tool binding does not contain a declaration".into())
                })?;
                present_tool_descriptor(tool, &canonical, wire, style);
            }
            for message in &mut request.messages {
                match message {
                    Message::Assistant { tool_calls, .. } => {
                        for call in tool_calls {
                            let canonical = call.name.clone();
                            call.name = self.forward.get(&canonical).cloned().ok_or_else(|| {
                                ProviderError::Malformed(
                                    "historical tool call is not in the pinned binding".into(),
                                )
                            })?;
                            if style == ToolPresentationStyle::ClaudeCode {
                                call.input =
                                    canonical_input_to_claude_wire(&canonical, &call.input);
                            }
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
                let canonical = self.reverse.get(&call.name).cloned().ok_or_else(|| {
                    ProviderError::Malformed("provider returned an unknown tool alias".into())
                })?;
                if style == ToolPresentationStyle::ClaudeCode {
                    call.input = claude_wire_input_to_canonical(&canonical, &call.input);
                }
                call.name = canonical;
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
        style: ToolPresentationStyle,
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
                .unwrap_or_else(|| default_wire_name(&tool.name, style));
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

fn default_wire_name(canonical: &str, style: ToolPresentationStyle) -> String {
    if style == ToolPresentationStyle::ClaudeCode {
        match canonical {
            "read_file" => return "Read".into(),
            "edit_file" => return "Edit".into(),
            "write_file" => return "Write".into(),
            "list_directory" => return "Glob".into(),
            "search" => return "Grep".into(),
            "process" => return "Bash".into(),
            "git_diff" => return "GitDiff".into(),
            "read_project_manifest" => return "ReadManifest".into(),
            _ => {}
        }
    }
    deterministic_alias(canonical)
}

#[allow(clippy::too_many_lines)]
fn present_tool_descriptor(
    tool: &mut ToolDescriptor,
    canonical: &str,
    wire_name: String,
    style: ToolPresentationStyle,
) {
    tool.name = wire_name;
    if style != ToolPresentationStyle::ClaudeCode {
        return;
    }
    let path_prop = serde_json::json!({"type": "string", "minLength": 1, "maxLength": 4096});
    match canonical {
        "read_file" => {
            tool.description = "Read a UTF-8 file inside the workspace. Supports optional 1-based line pagination via `offset` and `limit`. Reading a file records its snapshot so `Edit` and `Write` can mutate it safely.".into();
            tool.input_schema = serde_json::json!({
                "type": "object",
                "required": ["file_path"],
                "properties": {
                    "file_path": path_prop,
                    "offset": {"type": "integer", "minimum": 1, "maximum": 1_000_000},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 10_000}
                },
                "additionalProperties": false
            });
        }
        "edit_file" => {
            tool.description = "Perform an exact string replacement in an existing file inside the workspace. Call `Read` on the file before editing. `old_string` must match uniquely unless `replace_all` is true.".into();
            tool.input_schema = serde_json::json!({
                "type": "object",
                "required": ["file_path", "old_string", "new_string"],
                "properties": {
                    "file_path": path_prop,
                    "old_string": {"type": "string", "minLength": 1},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "additionalProperties": false
            });
        }
        "write_file" => {
            tool.description = "Create a new file (missing parent directories are created automatically) or replace an entire existing file inside the workspace. Call `Read` first before overwriting an existing file.".into();
            tool.input_schema = serde_json::json!({
                "type": "object",
                "required": ["file_path", "content"],
                "properties": {
                    "file_path": path_prop,
                    "content": {"type": "string"}
                },
                "additionalProperties": false
            });
        }
        "list_directory" => {
            tool.description = "Find files and directories inside the workspace matching a glob `pattern` (for example `**/*.rs` or `src/**`).".into();
            tool.input_schema = serde_json::json!({
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": {"type": "string", "minLength": 1, "maxLength": 1024},
                    "path": path_prop
                },
                "additionalProperties": false
            });
        }
        "search" => {
            tool.description = "Search file contents inside the workspace by regex or literal `pattern`, returning `path:line:content` matches.".into();
            tool.input_schema = serde_json::json!({
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": {"type": "string", "minLength": 1, "maxLength": 4096},
                    "path": path_prop,
                    "glob": {"type": "string", "minLength": 1, "maxLength": 1024},
                    "literal": {"type": "boolean"}
                },
                "additionalProperties": false
            });
        }
        "process" => {
            tool.description = "Execute a command inside the workspace with bounded output and process-group supervision.".into();
            tool.input_schema = serde_json::json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": {"type": "string", "minLength": 1, "maxLength": 16_384},
                    "cwd": path_prop,
                    "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 600_000}
                },
                "additionalProperties": false
            });
        }
        "git_diff" => {
            tool.description = "Show the current `git diff HEAD` inside the workspace.".into();
        }
        "read_project_manifest" => {
            tool.description = "Read top-level project manifests (`Cargo.toml`, `package.json`, `pyproject.toml`, etc.) in the workspace.".into();
        }
        _ => {}
    }
}

fn canonical_input_to_claude_wire(canonical: &str, input: &Value) -> Value {
    let Some(obj) = input.as_object() else {
        return input.clone();
    };
    let mut out = serde_json::Map::new();
    match canonical {
        "read_file" => {
            if let Some(p) = obj.get("path").or_else(|| obj.get("file_path")) {
                out.insert("file_path".into(), p.clone());
            }
            for key in ["offset", "limit"] {
                if let Some(v) = obj.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
        }
        "edit_file" => {
            if let Some(p) = obj.get("path").or_else(|| obj.get("file_path")) {
                out.insert("file_path".into(), p.clone());
            }
            if let Some(b) = obj
                .get("before")
                .or_else(|| obj.get("anchor"))
                .or_else(|| obj.get("old_string"))
            {
                out.insert("old_string".into(), b.clone());
            }
            if let Some(a) = obj.get("after").or_else(|| obj.get("new_string")) {
                out.insert("new_string".into(), a.clone());
            }
            if let Some(r) = obj.get("replace_all") {
                out.insert("replace_all".into(), r.clone());
            }
        }
        "write_file" => {
            if let Some(p) = obj.get("path").or_else(|| obj.get("file_path")) {
                out.insert("file_path".into(), p.clone());
            }
            if let Some(c) = obj.get("content") {
                out.insert("content".into(), c.clone());
            }
        }
        "list_directory" => {
            let pattern = obj
                .get("glob")
                .or_else(|| obj.get("pattern"))
                .cloned()
                .unwrap_or_else(|| Value::String("*".into()));
            out.insert("pattern".into(), pattern);
            if let Some(p) = obj.get("path") {
                out.insert("path".into(), p.clone());
            }
        }
        "search" => {
            if let Some(q) = obj.get("query").or_else(|| obj.get("pattern")) {
                out.insert("pattern".into(), q.clone());
            }
            for key in ["path", "glob"] {
                if let Some(v) = obj.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
            if obj.get("regex").and_then(Value::as_bool) == Some(false) {
                out.insert("literal".into(), Value::Bool(true));
            }
        }
        "process" => {
            if let Some(shell) = obj.get("shell").and_then(Value::as_str) {
                out.insert("command".into(), Value::String(shell.to_owned()));
            } else if let Some(argv) = obj.get("argv").and_then(Value::as_array) {
                let joined = argv
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                out.insert("command".into(), Value::String(joined));
            }
            for key in ["cwd", "timeout_ms"] {
                if let Some(v) = obj.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
        }
        _ => return input.clone(),
    }
    Value::Object(out)
}

#[allow(clippy::too_many_lines)]
fn claude_wire_input_to_canonical(canonical: &str, input: &Value) -> Value {
    let Some(obj) = input.as_object() else {
        return input.clone();
    };
    let mut out = serde_json::Map::new();
    match canonical {
        "read_file" => {
            if let Some(p) = obj.get("file_path").or_else(|| obj.get("path")) {
                out.insert("path".into(), p.clone());
            }
            for key in ["offset", "limit", "max_output"] {
                if let Some(v) = obj.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
        }
        "edit_file" => {
            if let Some(p) = obj.get("file_path").or_else(|| obj.get("path")) {
                out.insert("path".into(), p.clone());
            }
            if let Some(b) = obj.get("old_string").or_else(|| obj.get("before")) {
                out.insert("before".into(), b.clone());
            }
            if let Some(a) = obj.get("new_string").or_else(|| obj.get("after")) {
                out.insert("after".into(), a.clone());
            }
            for key in ["replace_all", "anchor", "precondition"] {
                if let Some(v) = obj.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
        }
        "write_file" => {
            if let Some(p) = obj.get("file_path").or_else(|| obj.get("path")) {
                out.insert("path".into(), p.clone());
            }
            if let Some(c) = obj.get("content") {
                out.insert("content".into(), c.clone());
            }
            for key in ["create_intent", "precondition"] {
                if let Some(v) = obj.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
        }
        "list_directory" => {
            if let Some(p) = obj.get("path") {
                out.insert("path".into(), p.clone());
            }
            let pattern = obj.get("pattern").or_else(|| obj.get("glob"));
            if let Some(pat) = pattern {
                out.insert("glob".into(), pat.clone());
            }
            out.insert(
                "max_depth".into(),
                obj.get("max_depth")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!(8)),
            );
        }
        "search" => {
            let query_val = obj.get("pattern").or_else(|| obj.get("query"));
            if let Some(q) = query_val {
                out.insert("query".into(), q.clone());
            }
            for key in ["path", "glob", "max_results", "max_output"] {
                if let Some(v) = obj.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
            let regex = if obj.get("literal").and_then(Value::as_bool) == Some(true) {
                false
            } else if let Some(explicit) = obj.get("regex").and_then(Value::as_bool) {
                explicit
            } else {
                query_val
                    .and_then(Value::as_str)
                    .is_some_and(looks_like_valid_regex)
            };
            out.insert("regex".into(), Value::Bool(regex));
        }
        "process" => {
            if let Some(cmd) = obj.get("command").and_then(Value::as_str) {
                out.insert("shell".into(), Value::String(cmd.to_owned()));
            } else {
                for key in ["argv", "shell"] {
                    if let Some(v) = obj.get(key) {
                        out.insert(key.into(), v.clone());
                    }
                }
            }
            for key in ["cwd", "timeout_ms", "env"] {
                if let Some(v) = obj.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
        }
        _ => return input.clone(),
    }
    Value::Object(out)
}

fn looks_like_valid_regex(pattern: &str) -> bool {
    let mut escaped = false;
    let mut in_class = false;
    let mut paren_depth = 0_usize;
    for ch in pattern.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if in_class {
            if ch == ']' {
                in_class = false;
            }
            continue;
        }
        match ch {
            '[' => in_class = true,
            '(' => paren_depth += 1,
            ')' => {
                let Some(next) = paren_depth.checked_sub(1) else {
                    return false;
                };
                paren_depth = next;
            }
            _ => {}
        }
    }
    !escaped && !in_class && paren_depth == 0
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
const fn default_streaming() -> bool {
    true
}
const fn default_prompt_caching() -> bool {
    true
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
fn canonical_tools_with_style(
    tools: &[ToolDescriptor],
    aliases: &BTreeMap<String, String>,
    style: ToolPresentationStyle,
) -> Result<serde_json::Value, RegistryError> {
    let mut tools = tools.to_vec();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    if style == ToolPresentationStyle::Canonical {
        serde_json::to_value(serde_json::json!({"version":1,"tools":tools,"aliases":aliases}))
            .map_err(|e| RegistryError::Invalid(e.to_string()))
    } else {
        serde_json::to_value(
            serde_json::json!({"version":1,"tools":tools,"aliases":aliases,"style":style}),
        )
        .map_err(|e| RegistryError::Invalid(e.to_string()))
    }
}
fn canonical_tools(
    tools: &[ToolDescriptor],
    aliases: &BTreeMap<String, String>,
) -> Result<serde_json::Value, RegistryError> {
    canonical_tools_with_style(tools, aliases, ToolPresentationStyle::Canonical)
}

/// The tools fingerprint a pre-upgrade binary computed, reproduced over the
/// *current* built-in tool set but with the legacy placeholder descriptions.
///
/// This is an upgrade-compat shim, not a relaxation: it recomputes the hash
/// from the current tools, so any change to tool names, `input_schema`,
/// `effect`, or `version` makes it differ and the stored binding is rejected.
/// The only drift it accepts is the human-facing description prose moving from
/// the `"Engine-owned <name> operation"` placeholder to real documentation — a
/// model-guidance field, never a permission boundary (`effect`/`input_schema`
/// remain authoritative and are still compared).
fn legacy_placeholder_tools_fingerprint(
    tools: &[ToolDescriptor],
    aliases: &BTreeMap<String, String>,
) -> Result<String, RegistryError> {
    let legacy: Vec<ToolDescriptor> = tools
        .iter()
        .map(|tool| ToolDescriptor {
            description: format!("Engine-owned {} operation", tool.name.replace('_', " ")),
            ..tool.clone()
        })
        .collect();
    Ok(fingerprint(&canonical_tools(&legacy, aliases)?))
}

/// Whether a persisted (pre-upgrade) binding is still valid against the binding
/// the current binary would mint. Every security-identity field must match
/// exactly; only `tools_fingerprint` is allowed to be the legacy placeholder
/// description hash for the identical tool set (see
/// [`legacy_placeholder_tools_fingerprint`]).
fn binding_matches_with_legacy_tools(
    stored: &SessionProviderBinding,
    proposed: &SessionProviderBinding,
    legacy_tools_fingerprint: &str,
) -> bool {
    let identity_matches = stored.binding_version == proposed.binding_version
        && stored.provider_name == proposed.provider_name
        && stored.provider_type == proposed.provider_type
        && stored.protocol == proposed.protocol
        && stored.model == proposed.model
        && stored.config_fingerprint == proposed.config_fingerprint
        && stored.credential_ref_id == proposed.credential_ref_id
        && stored.data_scope_id == proposed.data_scope_id
        && stored.credential_generation == proposed.credential_generation
        && stored.aliases == proposed.aliases;
    let tools_matches = stored.tools_fingerprint == proposed.tools_fingerprint
        || stored.tools_fingerprint == legacy_tools_fingerprint;
    identity_matches && tools_matches
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
        let inline_binding = inline.session_binding_for_default(&[]).unwrap();
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
        let a =
            ToolAliases::new(&tools, &BTreeMap::new(), ToolPresentationStyle::Canonical).unwrap();
        let b =
            ToolAliases::new(&tools, &BTreeMap::new(), ToolPresentationStyle::Canonical).unwrap();
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
            assert!(
                ToolAliases::new(&tools, &configured, ToolPresentationStyle::Canonical).is_err()
            );
        }
    }

    #[test]
    fn session_bindings_are_scoped_pinned_and_validated_before_secret_lookup() {
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
        let binding = registry.session_binding_for_default(&tools).unwrap();
        assert_eq!(binding.provider_name, "main");
        assert_eq!(binding.aliases["read_file"], "rf");
        assert_eq!(binding.credential_ref_id, "env:NEVER_LOOK_UP_THIS_KEY");
        assert_eq!(binding.data_scope_id, "workspace");
        assert_eq!(binding.credential_generation, 1);
        assert!(matches!(
            registry.resolve_session_bound(&binding, &tools),
            Err(RegistryError::MissingSecret(name)) if name == "NEVER_LOOK_UP_THIS_KEY"
        ));

        let mut changed = binding.clone();
        changed.credential_generation += 1;
        assert!(matches!(
            registry.resolve_session_bound(&changed, &tools),
            Err(RegistryError::BindingMismatch(_))
        ));
        assert!(matches!(
            registry.session_binding_for_model("unknown", "gpt-test", &tools),
            Err(RegistryError::Invalid(message)) if message.contains("unknown provider")
        ));
    }

    #[test]
    fn empty_headers_are_absent_from_the_config_fingerprint_payload() {
        let registry = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
        )
        .unwrap();
        let definition = registry
            .config
            .providers
            .get("main")
            .expect("provider exists");
        let semantic = semantic_definition_for_model(definition, "m").unwrap();
        // A headerless provider must not carry a `headers` key at all: its
        // config fingerprint must be byte-identical to a binding persisted
        // before the `headers` field existed (upgrade compatibility).
        assert!(
            semantic["provider"].get("headers").is_none(),
            "empty headers must be omitted from the fingerprint payload: {semantic}"
        );
    }

    #[test]
    fn resolve_accepts_a_pre_upgrade_binding_with_placeholder_tool_docs() {
        // Current tools carry real documentation rather than the legacy
        // placeholder text.
        let tools = {
            let mut read = tool("read_file");
            read.description =
                "Reads a UTF-8 file under the workspace root with a bounded size.".into();
            vec![read]
        };
        let registry = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
        )
        .unwrap();
        let proposed = registry.session_binding_for_default(&tools).unwrap();
        let legacy_fp = legacy_placeholder_tools_fingerprint(&tools, &proposed.aliases).unwrap();
        // The real-doc fingerprint genuinely differs from the placeholder one,
        // otherwise this test would not be exercising the compat shim.
        assert_ne!(legacy_fp, proposed.tools_fingerprint);

        // A binding persisted by the pre-upgrade binary (placeholder tool docs)
        // still resolves. Passing the binding check proceeds to secret lookup,
        // which fails with MissingSecret — the sentinel that the binding was
        // accepted rather than rejected as BindingMismatch.
        let mut legacy = proposed.clone();
        legacy.tools_fingerprint = legacy_fp;
        assert!(matches!(
            registry.resolve_session_bound(&legacy, &tools),
            Err(RegistryError::MissingSecret(name)) if name == "K"
        ));

        // The shim is description-only. A real tool-effect change must still be
        // rejected even when the stored binding carries a placeholder hash: the
        // legacy value is recomputed over the *current* tool set.
        let changed_tools = {
            let mut modified = tools[0].clone();
            modified.effect = "modify".into();
            vec![modified]
        };
        // `legacy` holds the placeholder hash of the ORIGINAL read tool; against
        // the changed tool set neither the current nor the recomputed legacy
        // hash matches, so it must fail closed.
        assert!(matches!(
            registry.resolve_session_bound(&legacy, &changed_tools),
            Err(RegistryError::BindingMismatch(_))
        ));

        // Any other security-identity drift is still a hard mismatch.
        let mut rebind = proposed;
        rebind.model = "other".into();
        assert!(matches!(
            registry.resolve_session_bound(&rebind, &tools),
            Err(RegistryError::BindingMismatch(_))
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
            headers: BTreeMap::new(),
            aliases: BTreeMap::default(),
            tool_style: None,
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
            .session_binding_for_model("beta", "b-reasoning", &[])
            .unwrap();
        assert_eq!(selected.provider_name, "beta");
        assert_eq!(selected.model, "b-reasoning");
        assert!(registry.resolve_session_bound(&selected, &[]).is_ok());
        assert!(matches!(
            registry.session_binding_for_model("beta", "missing", &[]),
            Err(RegistryError::Invalid(message)) if message.contains("unknown model")
        ));
        let mut missing_provider = selected.clone();
        missing_provider.provider_name = "missing".into();
        assert!(matches!(
            registry.resolve_session_bound(&missing_provider, &[]),
            Err(RegistryError::BindingMismatch(message)) if message.contains("provider")
        ));
        let mut missing_model = selected;
        missing_model.model = "missing".into();
        assert!(matches!(
            registry.resolve_session_bound(&missing_model, &[]),
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
            style: ToolPresentationStyle::Canonical,
        };
        assert!(provider.capabilities().tools);
        let context = || ProviderContext {
            deadline: Instant::now() + Duration::from_millis(10),
            cancellation: latte_engine::CancellationToken::new(),
            events: None,
        };
        let tool_request = ProviderRequest {
            session_ref: "session-ref".into(),
            messages: vec![],
            tools: vec![tool("read_file")],
        };
        assert!(matches!(
            provider.complete(tool_request, context()).await,
            Err(ProviderError::Malformed(message)) if message.contains("declaration")
        ));
        let assistant_request = ProviderRequest {
            session_ref: "session-ref".into(),
            messages: vec![Message::Assistant {
                content: None,
                reasoning_content: None,
                thinking_blocks: vec![],
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
            session_ref: "session-ref".into(),
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
            style: ToolPresentationStyle::Canonical,
        };
        assert!(matches!(
            provider
                .complete(
                    ProviderRequest {
                        session_ref: "session-ref".into(),
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
            style: ToolPresentationStyle::Canonical,
        };
        let outcome = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![Message::Assistant {
                        content: None,
                        reasoning_content: None,
                        thinking_blocks: vec![],
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
    fn session_binding_catalog_returns_every_configured_model() {
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
            .session_binding_catalog(&[tool("read_file")])
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
    fn session_binding_catalog_fails_closed_on_a_broken_model() {
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
            registry.session_binding_catalog(&[tool("read_file")]),
            Err(RegistryError::Invalid(message)) if message.contains("unknown canonical tool")
        ));
    }

    // -- additional branch coverage -----------------------------------------

    #[test]
    fn empty_config_validates_and_rejects_provider_use() {
        let registry = ProviderRegistry::parse_jsonc(r"{version:1,providers:{}}").unwrap();
        assert!(registry.default_name().is_none());
        assert!(registry.resolve_default(&[]).is_err());
        assert!(registry.session_binding_for_default(&[]).is_err());
    }

    #[test]
    fn parse_jsonc_strips_owned_session_section() {
        let registry = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'main/m',session:{max_request_bytes:4096},providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:{source:'env',name:'PATH'}}}}",
        )
        .unwrap();
        assert_eq!(registry.default_name(), Some("main"));
    }

    #[test]
    fn direct_binding_with_tools_builds_aliases() {
        let binding = ProviderModelBinding::direct(&[tool("read_file")]);
        assert!(!binding.aliases.is_empty());
        assert_eq!(binding.aliases["read_file"], "read_file");
    }

    #[test]
    fn configured_model_display_name_resolves_and_missing_returns_none() {
        let definition = ProviderDefinition::OpenaiChat {
            models: OpenAiChatModels::Configured(BTreeMap::from([(
                "m".into(),
                OpenAiChatModelConfig {
                    name: Some("Display".into()),
                    options: OpenAiChatModelOptions::default(),
                },
            )])),
            base_url: Some("https://x".into()),
            endpoint: None,
            api_key: SecretRef::Literal("k".into()),
            timeout_ms: default_timeout(),
            max_attempts: default_attempts(),
            temperature: None,
            max_tokens: None,
            compatibility_input_request: false,
            streaming: false,
            headers: BTreeMap::new(),
            aliases: BTreeMap::default(),
            tool_style: None,
        };
        assert_eq!(definition.model_name("m"), Some("Display"));
        assert_eq!(definition.model_name("missing"), None);
    }

    #[test]
    fn resolve_model_rejects_unknown_provider_and_model() {
        let registry = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:{source:'env',name:'PATH'}}}}",
        )
        .unwrap();
        assert!(registry.resolve_model("unknown", "m", &[]).is_err());
        assert!(registry.resolve_model("main", "unknown", &[]).is_err());
    }

    #[test]
    fn resolve_model_rejects_alias_referencing_unknown_tool() {
        let registry = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:{source:'env',name:'PATH'},aliases:{unknown_tool:'wire'}}}}",
        )
        .unwrap();
        assert!(registry.resolve_model("main", "m", &[]).is_err());
    }

    #[test]
    fn configured_headers_are_accepted_and_reach_the_resolved_provider() {
        let registry = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:'k',headers:{'x-session-id':'${session_id}','x-tenant':'acme'}}}}",
        )
        .unwrap();
        assert!(registry.resolve_model("main", "m", &[]).is_ok());
    }

    #[test]
    fn a_header_typo_is_rejected_at_load_rather_than_silently_dropped() {
        // A Provider that requires the header rejects the whole request without
        // it. Failing here names the offending header; failing at request time
        // would surface as an opaque 400 from the Provider.
        for (headers, expected) in [
            (r"{'x-session':'${sesion_id}'}", "unknown placeholder"),
            (r"{'x-session':'${session_id'}", "unterminated placeholder"),
            (r"{'x session':'v'}", "must be non-empty"),
            (r"{'':'v'}", "must be non-empty"),
            (r"{'authorization':'Bearer x'}", "cannot be configured"),
            (r"{'Content-Type':'text/plain'}", "cannot be configured"),
        ] {
            let registry = ProviderRegistry::parse_jsonc(&format!(
                r"{{version:1,default_model:'main/m',providers:{{main:{{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:'k',headers:{headers}}}}}}}"
            ))
            .unwrap();
            let Err(error) = registry.resolve_model("main", "m", &[]) else {
                panic!("headers {headers} must be rejected");
            };
            assert!(
                error.to_string().contains(expected),
                "{headers} produced {error}"
            );
        }
    }

    #[test]
    fn a_header_value_carrying_a_control_character_is_rejected() {
        let registry = ProviderRegistry::parse_jsonc(
            "{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:'k',headers:{'x-a':'a\\nb'}}}}",
        )
        .unwrap();
        let Err(error) = registry.resolve_model("main", "m", &[]) else {
            panic!("a control character in a header value must be rejected");
        };
        assert!(error.to_string().contains("control characters"), "{error}");
    }

    #[test]
    fn headers_take_part_in_the_configuration_fingerprint() {
        // Two deployments that differ only by a required header are not the
        // same configuration: a binding pinned under one must not be reused
        // under the other.
        let without = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:'k'}}}",
        )
        .unwrap()
        .resolve_model("main", "m", &[])
        .unwrap();
        let with = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:'k',headers:{'x-session-id':'${session_id}'}}}}",
        )
        .unwrap()
        .resolve_model("main", "m", &[])
        .unwrap();
        assert_ne!(
            without.binding.config_fingerprint,
            with.binding.config_fingerprint
        );
    }

    #[test]
    fn model_options_temperature_is_validated() {
        assert!(ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:{m:{options:{temperature:3}}},endpoint:'https://x',api_key:{source:'env',name:'PATH'}}}}",
        )
        .is_err());
    }

    fn test_provider_context() -> ProviderContext {
        ProviderContext {
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(2),
            cancellation: latte_engine::CancellationToken::new(),
            events: None,
        }
    }

    #[tokio::test]
    async fn aliased_provider_rejects_unknown_tool_in_request() {
        let registry = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://127.0.0.1:1',api_key:{source:'env',name:'PATH'},aliases:{read_file:'rf'}}}}",
        )
        .unwrap();
        let resolved = registry
            .resolve_model("main", "m", &[tool("read_file")])
            .unwrap();
        // A Tool message whose name is not in the forward alias map.
        let request = ProviderRequest {
            session_ref: "session-ref".into(),
            messages: vec![Message::Tool {
                tool_call_id: "t1".into(),
                name: Some("unknown".into()),
                content: "result".into(),
            }],
            tools: vec![],
        };
        let err = resolved
            .provider
            .complete(request, test_provider_context())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Malformed(_)));
    }

    #[tokio::test]
    async fn aliased_provider_rejects_unknown_alias_in_response() {
        // The inner provider returns a tool call with a name not in the reverse
        // alias map.
        let body = r#"{"choices":[{"message":{"tool_calls":[{"id":"c1","function":{"name":"unknown_alias","arguments":"{}"}}]}}]}"#;
        let (endpoint, _rx) = capturing_server(body);
        let registry = ProviderRegistry::parse_jsonc(&format!(
            "{{version:1,default_model:'main/m',providers:{{main:{{type:'openai-chat',models:['m'],endpoint:'{endpoint}',api_key:{{source:'env',name:'PATH'}},aliases:{{read_file:'rf'}}}}}}}}"
        ))
        .unwrap();
        let resolved = registry
            .resolve_model("main", "m", &[tool("read_file")])
            .unwrap();
        let request = ProviderRequest {
            session_ref: "session-ref".into(),
            messages: vec![],
            tools: vec![],
        };
        let err = resolved
            .provider
            .complete(request, test_provider_context())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Malformed(_)));
    }

    #[tokio::test]
    async fn aliased_provider_propagates_inner_error() {
        // Bind-and-drop gives a port that refuses connections on every
        // platform (port 1 is handled inconsistently by Windows http.sys).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let registry = ProviderRegistry::parse_jsonc(&format!(
            r"{{version:1,default_model:'main/m',providers:{{main:{{type:'openai-chat',models:['m'],endpoint:'https://127.0.0.1:{port}',api_key:{{source:'env',name:'PATH'}},aliases:{{read_file:'rf'}}}}}}}}",
        ))
        .unwrap();
        let resolved = registry
            .resolve_model("main", "m", &[tool("read_file")])
            .unwrap();
        let request = ProviderRequest {
            session_ref: "session-ref".into(),
            messages: vec![],
            tools: vec![],
        };
        // The inner provider cannot connect → Transport or Timeout (depending
        // on how the OS reports the refused connection) propagated.
        let err = resolved
            .provider
            .complete(request, test_provider_context())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Transport(_) | ProviderError::Timeout),
            "expected transport or timeout error, got {err:?}"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn anthropic_messages_registry_and_claude_code_tool_presentation_roundtrip() {
        assert_eq!(
            derive_anthropic_endpoint("http://10.37.20.168:8317/"),
            "http://10.37.20.168:8317/v1/messages"
        );
        assert_eq!(
            derive_anthropic_endpoint("https://api.anthropic.com/v1/"),
            "https://api.anthropic.com/v1/messages"
        );
        // Anthropic rejects temperature > 1.0 at config validation time.
        assert!(ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'c/m',providers:{c:{type:'anthropic-messages',models:['m'],base_url:'http://127.0.0.1:1',api_key:'k',temperature:1.5}}}",
        )
        .is_err());
        // Anthropic reserved headers (`x-api-key`, `anthropic-version`) are rejected.
        let reserved_reg = ProviderRegistry::parse_jsonc(
            r"{version:1,default_model:'c/m',providers:{c:{type:'anthropic-messages',models:['m'],base_url:'http://127.0.0.1:1',api_key:'k',headers:{'x-api-key':'bad'}}}}",
        )
        .unwrap();
        assert!(reserved_reg.resolve_model("c", "m", &[]).is_err());

        let tools = vec![
            tool("read_file"),
            tool("edit_file"),
            tool("write_file"),
            tool("list_directory"),
            tool("search"),
            tool("process"),
            tool("git_diff"),
            tool("read_project_manifest"),
        ];
        let response_json = r#"{"type":"message","content":[
            {"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"src/lib.rs","offset":1,"limit":50}},
            {"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"src/lib.rs","old_string":"old","new_string":"new","replace_all":true}},
            {"type":"tool_use","id":"t3","name":"Write","input":{"file_path":"src/new.rs","content":"fn a() {}\n"}},
            {"type":"tool_use","id":"t4","name":"Glob","input":{"pattern":"**/*.rs","path":"src"}},
            {"type":"tool_use","id":"t5","name":"Grep","input":{"pattern":"fn main(","path":"src"}},
            {"type":"tool_use","id":"t6","name":"Bash","input":{"command":"git status --short"}},
            {"type":"tool_use","id":"t7","name":"Bash","input":{"command":"cargo test 2>&1 | head -n 5","cwd":"."}}
        ],"stop_reason":"tool_use","usage":{"input_tokens":10,"output_tokens":20}}"#;
        let (endpoint, captured) = capturing_server(response_json);
        let registry = ProviderRegistry::parse_jsonc(&format!(
            r"{{version:1,default_model:'claude/seed',providers:{{claude:{{type:'anthropic-messages',models:{{seed:{{options:{{context_window:200000,reasoning_effort:'high',max_tokens:16384}}}}}},endpoint:{endpoint:?},api_key:'cpk-key',streaming:false}}}}}}"
        ))
        .unwrap();
        assert_eq!(
            registry.model_context_window("claude", "seed"),
            Some(200_000)
        );
        assert_eq!(
            registry.provider_tool_presentation("claude"),
            Some(ToolPresentationStyle::ClaudeCode)
        );

        let session_binding = registry.session_binding_for_default(&tools).unwrap();
        assert_eq!(session_binding.provider_type, "anthropic-messages");
        assert_eq!(session_binding.protocol, "anthropic-messages-v1");
        assert_eq!(session_binding.aliases["read_file"], "Read");
        assert_eq!(session_binding.aliases["edit_file"], "Edit");
        assert_eq!(session_binding.aliases["process"], "Bash");

        let resolved = registry
            .resolve_session_bound(&session_binding, &tools)
            .unwrap();
        let outcome = resolved
            .provider
            .complete(
                ProviderRequest {
                    session_ref: "s1".into(),
                    tools,
                    messages: vec![
                        Message::User {
                            content: "go".into(),
                        },
                        Message::Assistant {
                            content: None,
                            reasoning_content: None,
                            thinking_blocks: vec![],
                            tool_calls: vec![
                                crate::provider::ToolCall {
                                    id: "h1".into(),
                                    name: "read_file".into(),
                                    input: serde_json::json!({"path": "a.rs", "offset": 2}),
                                },
                                crate::provider::ToolCall {
                                    id: "h2".into(),
                                    name: "edit_file".into(),
                                    input: serde_json::json!({"path": "a.rs", "before": "x", "after": "y", "replace_all": true}),
                                },
                                crate::provider::ToolCall {
                                    id: "h3".into(),
                                    name: "write_file".into(),
                                    input: serde_json::json!({"path": "b.rs", "content": "c"}),
                                },
                                crate::provider::ToolCall {
                                    id: "h4".into(),
                                    name: "list_directory".into(),
                                    input: serde_json::json!({"path": "src", "glob": "*.rs"}),
                                },
                                crate::provider::ToolCall {
                                    id: "h5".into(),
                                    name: "search".into(),
                                    input: serde_json::json!({"query": "foo", "regex": false}),
                                },
                                crate::provider::ToolCall {
                                    id: "h6".into(),
                                    name: "process".into(),
                                    input: serde_json::json!({"argv": ["git", "status"]}),
                                },
                            ],
                        },
                        Message::Tool {
                            tool_call_id: "h1".into(),
                            name: Some("read_file".into()),
                            content: "ok".into(),
                        },
                    ],
                },
                test_provider_context(),
            )
            .await
            .unwrap();

        let outbound = captured.recv().unwrap();
        let tool_names: Vec<&str> = outbound["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(tool_names.contains(&"Read") && tool_names.contains(&"Edit"));
        assert!(tool_names.contains(&"Bash") && tool_names.contains(&"Grep"));
        let hist_blocks = outbound["messages"][1]["content"].as_array().unwrap();
        assert_eq!(hist_blocks[0]["name"], "Read");
        assert_eq!(hist_blocks[0]["input"]["file_path"], "a.rs");
        assert_eq!(hist_blocks[1]["name"], "Edit");
        assert_eq!(hist_blocks[1]["input"]["old_string"], "x");
        assert_eq!(hist_blocks[5]["name"], "Bash");
        assert_eq!(hist_blocks[5]["input"]["command"], "git status");

        // Inbound tool calls are mapped back to canonical engine names & inputs.
        assert_eq!(outcome.tool_calls.len(), 7);
        assert_eq!(outcome.tool_calls[0].name, "read_file");
        assert_eq!(outcome.tool_calls[0].input["path"], "src/lib.rs");
        assert_eq!(outcome.tool_calls[1].name, "edit_file");
        assert_eq!(outcome.tool_calls[1].input["before"], "old");
        assert_eq!(outcome.tool_calls[1].input["after"], "new");
        assert_eq!(outcome.tool_calls[1].input["replace_all"], true);
        assert_eq!(outcome.tool_calls[2].name, "write_file");
        assert_eq!(outcome.tool_calls[2].input["path"], "src/new.rs");
        assert_eq!(outcome.tool_calls[3].name, "list_directory");
        assert_eq!(outcome.tool_calls[3].input["glob"], "**/*.rs");
        assert_eq!(outcome.tool_calls[3].input["max_depth"], 8);
        assert_eq!(outcome.tool_calls[4].name, "search");
        assert_eq!(outcome.tool_calls[4].input["query"], "fn main(");
        assert_eq!(
            outcome.tool_calls[4].input["regex"], false,
            "unclosed paren auto-falls back to literal search"
        );
        assert_eq!(outcome.tool_calls[5].name, "process");
        assert!(outcome.tool_calls[5].input.get("argv").is_none());
        assert_eq!(outcome.tool_calls[5].input["shell"], "git status --short");
        assert_eq!(outcome.tool_calls[6].name, "process");
        assert_eq!(
            outcome.tool_calls[6].input["shell"],
            "cargo test 2>&1 | head -n 5"
        );
        // Shell builtins (`cd`, `export`, `source`) stay on `shell` rather than breaking under `execve` argv!
        let builtin_mapped = claude_wire_input_to_canonical(
            "process",
            &serde_json::json!({"command": "export FOO=1"}),
        );
        assert_eq!(builtin_mapped["shell"], "export FOO=1");
        assert!(builtin_mapped.get("argv").is_none());
    }
}
