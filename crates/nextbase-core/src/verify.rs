use crate::config::Provider;

#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub ok: bool,
    pub message: String,
}

impl VerifyResult {
    fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
        }
    }
}

pub async fn verify_provider_key(provider: Provider, key: &str) -> VerifyResult {
    if key.trim().is_empty() {
        return VerifyResult::failed("No key entered.");
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
    {
        Ok(client) => client,
        Err(error) => return VerifyResult::failed(format!("Verification error: {error}")),
    };

    let request = match provider {
        Provider::Groq => client
            .get("https://api.groq.com/openai/v1/models")
            .bearer_auth(key),
        Provider::ElevenLabs => client
            .get("https://api.elevenlabs.io/v1/user")
            .header("xi-api-key", key),
        Provider::Sarvam => {
            // Sarvam exposes no lightweight verification endpoint; `/models`
            // returns 404 even for valid keys and used to block setup. The first
            // transcription call is the real check.
            return VerifyResult::ok(
                "Sarvam key saved. It will be validated on first transcription.",
            );
        }
        Provider::NextbaseCodex => {
            if !key.starts_with("nbmg_") {
                return VerifyResult::failed("Nextbase key must start with nbmg_.");
            }
            client
                .get("https://nextbase-model-gateway.infinitycorp.tech/v1/token/check")
                .bearer_auth(key)
        }
    };

    match request.send().await {
        Ok(response) if response.status().is_success() => {
            VerifyResult::ok(format!("{provider} key verified."))
        }
        Ok(response) => VerifyResult::failed(format!(
            "{provider} verification failed: HTTP {}",
            response.status().as_u16()
        )),
        Err(error) => VerifyResult::failed(format!("Verification error: {error}")),
    }
}
