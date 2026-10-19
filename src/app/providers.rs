//! The language-model providers the app offers, with their endpoints.
//!
//! Every provider except Anthropic speaks the OpenAI chat-completions
//! protocol at a different base URL; Anthropic speaks its Messages API.
//! Model names are never hard-coded: after the key is entered the app asks
//! the provider for its model list and falls back to a text field.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// `POST {base}/chat/completions`, `Authorization: Bearer <key>`, `GET {base}/models`.
    OpenAi,
    /// `POST {base}/messages`, `x-api-key` + `anthropic-version`, `GET {base}/models`.
    Anthropic,
}

#[derive(Debug, Clone, Copy)]
pub struct Provider {
    pub id: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    pub protocol: Protocol,
    /// Where to get a key (shown next to the field).
    pub key_url: &'static str,
    /// True for a local server that needs no key.
    pub local: bool,
}

pub const PROVIDERS: &[Provider] = &[
    Provider { id: "anthropic", name: "Anthropic", base_url: "https://api.anthropic.com/v1", protocol: Protocol::Anthropic, key_url: "https://console.anthropic.com/settings/keys", local: false },
    Provider { id: "openai", name: "OpenAI", base_url: "https://api.openai.com/v1", protocol: Protocol::OpenAi, key_url: "https://platform.openai.com/api-keys", local: false },
    Provider { id: "deepseek", name: "DeepSeek", base_url: "https://api.deepseek.com/v1", protocol: Protocol::OpenAi, key_url: "https://platform.deepseek.com/api_keys", local: false },
    Provider { id: "kimi", name: "Kimi (Moonshot)", base_url: "https://api.moonshot.ai/v1", protocol: Protocol::OpenAi, key_url: "https://platform.moonshot.ai/console/api-keys", local: false },
    Provider { id: "mistral", name: "Mistral", base_url: "https://api.mistral.ai/v1", protocol: Protocol::OpenAi, key_url: "https://console.mistral.ai/api-keys", local: false },
    Provider { id: "zai", name: "Z.ai (GLM)", base_url: "https://api.z.ai/api/paas/v4", protocol: Protocol::OpenAi, key_url: "https://z.ai/manage-apikey/apikey-list", local: false },
    Provider { id: "ollama-cloud", name: "Ollama Cloud", base_url: "https://ollama.com/v1", protocol: Protocol::OpenAi, key_url: "https://ollama.com/settings/keys", local: false },
    Provider { id: "openrouter", name: "OpenRouter", base_url: "https://openrouter.ai/api/v1", protocol: Protocol::OpenAi, key_url: "https://openrouter.ai/keys", local: false },
    Provider { id: "nvidia-nim", name: "NVIDIA NIM", base_url: "https://integrate.api.nvidia.com/v1", protocol: Protocol::OpenAi, key_url: "https://build.nvidia.com/", local: false },
    Provider { id: "ollama", name: "Ollama (this computer)", base_url: "http://127.0.0.1:11434/v1", protocol: Protocol::OpenAi, key_url: "", local: true },
    Provider { id: "custom", name: "Other OpenAI-compatible endpoint", base_url: "", protocol: Protocol::OpenAi, key_url: "", local: false },
];

pub fn by_id(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_has_a_usable_base_url_or_is_custom() {
        for p in PROVIDERS {
            if p.id == "custom" {
                assert!(p.base_url.is_empty());
            } else {
                assert!(p.base_url.starts_with("http"), "{}", p.id);
                assert!(!p.base_url.ends_with('/'), "{}", p.id);
            }
        }
        assert_eq!(by_id("anthropic").unwrap().protocol, Protocol::Anthropic);
        assert_eq!(PROVIDERS.len(), 11);
    }
}
