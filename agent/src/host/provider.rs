//! **Which provider, and how it is configured.**
//!
//! There is more than one way to reach a model now, and they are not
//! variations on a wire format — they are different ones. Chat
//! completions takes `messages` and streams `choices[].delta`;
//! Responses takes typed `input` items and streams named events. A
//! single client with branches inside it would be two implementations
//! wearing one name, which is the mistake `docs/23_ONE_AGENT.md`
//! records from the last time this codebase kept two paths.
//!
//! So each is its own [`LlmClient`], and this module is the only place
//! that chooses. Everything above it holds an `Arc<dyn LlmClient>` and
//! cannot tell which it has.
//!
//! **The endpoint decides, not a flag.** `AGENT2_PROVIDER` exists to
//! override, but the default is inferred from the base URL, because
//! which API an endpoint speaks is a fact about that endpoint rather
//! than a preference anyone holds. Getting it wrong is a 404 or a
//! schema error on the first request, not a silent wrong answer.
//!
//! **The `DEEPSEEK_*` variables still work**, and are read as the
//! second choice behind `AGENT2_*`. They are the spelling every eval
//! script, `run.sh` and recorded arm on disk uses, and renaming them
//! would invalidate the fingerprint on every result file already
//! written. The prefix is a fossil — these keys have pointed at
//! opencode, OpenRouter and a local llama-server far more often than
//! at DeepSeek.

use std::sync::Arc;

use super::llm::LlmClient;

/// The wire format an endpoint speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    /// `POST /chat/completions` — `messages` in, `choices[].delta` out.
    /// DeepSeek, opencode, OpenRouter, llama-server, and OpenAI's
    /// non-reasoning models.
    Completions,
    /// `POST /responses` — typed `input` items in, named events out.
    /// The only way to reach OpenAI's `*-codex` models.
    Responses,
}

impl Api {
    /// What this base URL almost certainly speaks.
    ///
    /// Only two things are ever true here: an endpoint whose path ends
    /// in `/responses`, or one of the hosts that serve that API and
    /// nothing else. Everything else in this project's history —
    /// opencode, OpenRouter, llama-server, DeepSeek — is completions.
    pub fn infer(base_url: &str) -> Api {
        let u = base_url.trim_end_matches('/');
        if u.ends_with("/responses") || u.contains("chatgpt.com/backend-api") {
            Api::Responses
        } else {
            Api::Completions
        }
    }
}

/// **What the model can actually hold, in tokens.**
///
/// The compaction trigger wants a window and the harness had none, so
/// it fell back to a flat 64 KB of *bytes* — about 14k tokens once the
/// headroom is taken. Against `deepseek-v4-flash` that is 1.3% of a
/// 1,048,576-token window, and a session was spending a completion on
/// compaction every few turns with the document ninety-eight percent
/// empty. Five of them in one exploratory run on 2026-09-22, each
/// removing rows and none getting under the line, because the line had
/// nothing to do with the model.
///
/// Matched on a prefix, so a dated or suffixed name (`deepseek-v4-flash-0423`,
/// `gpt-5.6-luna-preview`) resolves to the same window as its family.
/// Longest match wins, so a more specific entry can override a family.
///
/// `AGENT2_CONTEXT_TOKENS` still overrides everything; an unknown model
/// gets `None` and the byte budget, which is the old behaviour and the
/// right default for something nobody has measured.
const CONTEXT_WINDOWS: &[(&str, usize)] = &[
    // 1,048,576 — deepseek.com and the V4 paper.
    ("deepseek-v4", 1_048_576),
    ("deepseek-flash", 131_072),
    // 1.05M, with a pricing step at 272k that is not a limit.
    ("gpt-5.6", 1_050_000),
    ("gpt-5", 400_000),
    // The LAN box's llama-server preset (`evals/local.sh`).
    ("Qwen3.8-27B", 65_536),
];

/// The window for `model`, by longest matching prefix.
pub fn context_window(model: &str) -> Option<usize> {
    CONTEXT_WINDOWS
        .iter()
        .filter(|(name, _)| model.starts_with(name))
        .max_by_key(|(name, _)| name.len())
        .map(|(_, window)| *window)
}

/// Everything a provider needs, read once.
#[derive(Clone, Debug)]
pub struct Config {
    pub api: Api,
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub thinking: bool,
    pub effort: Option<String>,
    pub max_tokens: Option<u32>,
}

/// `AGENT2_<name>`, else `DEEPSEEK_<name>` — see the module note on why
/// the old spelling is still honoured.
pub fn var(name: &str) -> Option<String> {
    std::env::var(format!("AGENT2_{name}"))
        .or_else(|_| std::env::var(format!("DEEPSEEK_{name}")))
        .ok()
        .filter(|v| !v.is_empty())
}

/// The key, read from the file `AGENT2_API_KEY_FILE` names.
///
/// **A path is not a secret.** Without this the only way in is an
/// environment variable, so starting a session means putting the key
/// itself on a command line or into a shell — in front of whoever is
/// reading over your shoulder, into the shell's history, and into the
/// process table. Naming a file instead keeps it on disk, where its
/// permissions already say who may read it, and leaves nothing to
/// clean up afterwards. `curl --netrc` and `ssh -i` have the same
/// shape for the same reason.
///
/// Trailing whitespace goes: a key file written by an editor almost
/// always ends in a newline, and a newline in an `Authorization`
/// header fails as a puzzling 401 rather than as "your key has a
/// newline in it".
fn key_file() -> Option<String> {
    let path = var("API_KEY_FILE")?;
    match std::fs::read_to_string(&path) {
        Ok(text) => Some(text.trim().to_owned()).filter(|k| !k.is_empty()),
        // Silent would be wrong — a misspelt path would read as "no
        // key set" and send you looking at the wrong thing.
        Err(e) => {
            eprintln!("AGENT2_API_KEY_FILE={path}: {e}");
            None
        }
    }
}

/// **Is this endpoint on this machine?** Used for one thing only: an
/// endpoint that cannot bill has no reason to demand a credential, and
/// requiring one would make the free default unusable without a
/// placeholder nobody reads.
///
/// **Parsed as an address, not matched as a prefix.** The first version
/// tested `starts_with("192.168.")`, which is true of
/// `192.168.1.216.example.com` — a name anybody can register, pointing
/// anywhere, and treated as free. `Ipv4Addr` decides it instead, which
/// also gets `172.16.0.0/12` right, and a hostname is remote unless it
/// is literally `localhost`.
pub fn is_local(base_url: &str) -> bool {
    let after_scheme = base_url.split_once("://").map_or(base_url, |(_, r)| r);
    let host = after_scheme.split('/').next().unwrap_or("");
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.split(']').next())
        .unwrap_or_else(|| host.rsplit_once(':').map_or(host, |(h, _)| h));
    if host == "localhost" {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private(),
        Ok(std::net::IpAddr::V6(ip)) => ip.is_loopback(),
        Err(_) => false,
    }
}

impl Config {
    pub fn from_env(
        default_base_url: &str,
        default_model: &str,
        default_effort: &str,
    ) -> Result<Config, String> {
        let base_url = var("BASE_URL").unwrap_or_else(|| default_base_url.to_owned());
        let model = var("MODEL").unwrap_or_else(|| default_model.to_owned());
        // **The ChatGPT backend takes a subscription token, not a
        // key.** Asked for after the explicit key, so an env var still
        // wins — which is what lets one of these endpoints be reached
        // with a pasted token when debugging.
        let api_key = match var("API_KEY").or_else(|| key_file()) {
            Some(key) => key,
            None if is_local(&base_url) => "local".to_owned(),
            None if base_url.contains("chatgpt.com/backend-api") => {
                super::openai_oauth::access_token()?
            }
            None => {
                return Err(format!(
                    "AGENT2_API_KEY is not set, and {base_url} is not on this machine"
                ));
            }
        };
        let api = match var("PROVIDER").as_deref() {
            Some("completions") => Api::Completions,
            Some("responses") => Api::Responses,
            Some(other) => {
                return Err(format!(
                    "AGENT2_PROVIDER is `{other}` — expected `completions` or `responses`"
                ));
            }
            None => Api::infer(&base_url),
        };
        Ok(Config {
            api,
            api_key,
            base_url,
            model,
            thinking: var("NO_THINKING").is_none(),
            effort: Some(var("REASONING_EFFORT").unwrap_or_else(|| default_effort.to_owned())),
            max_tokens: var("MAX_TOKENS")
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|n| *n > 0),
        })
    }
}

/// The client this environment asks for.
pub fn from_env() -> Result<Arc<dyn LlmClient>, String> {
    let config = Config::from_env(
        super::openai_completions::DEFAULT_BASE_URL,
        super::openai_completions::DEFAULT_MODEL,
        super::openai_completions::DEFAULT_EFFORT,
    )?;
    Ok(match config.api {
        Api::Completions => Arc::new(super::OpenAiCompletions::from_config(&config)),
        Api::Responses => Arc::new(super::OpenAiResponses::from_config(&config)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every model this project actually points at has a window.**
    ///
    /// `AGENT2_CONTEXT_TOKENS` was set in one place in the repository —
    /// a unit test — so every run took the byte path: a flat 64 KB,
    /// about 14k tokens after headroom, whatever the model. Against a
    /// 1,048,576-token window that is 1.3%, and an exploratory run on
    /// 2026-09-22 spent five completions compacting a document the
    /// model could have held seventy-five times over.
    #[test]
    fn the_models_we_point_at_have_a_window_and_the_rest_do_not() {
        for (model, want) in [
            ("deepseek-v4-flash", 1_048_576),
            // A dated build is the same model.
            ("deepseek-v4-flash-0423", 1_048_576),
            ("deepseek-v4-pro", 1_048_576),
            ("gpt-5.6-luna", 1_050_000),
            ("Qwen3.8-27B", 65_536),
        ] {
            assert_eq!(context_window(model), Some(want), "{model}");
        }
        // **Longest prefix wins**, so `deepseek-flash` (an older, much
        // smaller model) is not swallowed by `deepseek-v4`'s entry, and
        // `gpt-5.6` is not swallowed by `gpt-5`.
        assert_eq!(context_window("deepseek-flash"), Some(131_072));
        assert_eq!(context_window("gpt-5-codex"), Some(400_000));
        // Unknown is `None`, which keeps the byte budget — the old
        // behaviour, and the right default for an unmeasured model.
        assert_eq!(context_window("some-model-nobody-has-measured"), None);
    }

    /// **The endpoint decides.** Every URL this project has actually
    /// pointed at, and the one it is about to.
    #[test]
    fn the_api_is_inferred_from_where_the_request_goes() {
        for (url, want) in [
            ("https://opencode.ai/zen/go/v1", Api::Completions),
            ("https://openrouter.ai/api/v1", Api::Completions),
            ("http://192.168.1.216:8080/v1", Api::Completions),
            ("https://api.deepseek.com/v1", Api::Completions),
            ("https://api.openai.com/v1", Api::Completions),
            ("https://api.openai.com/v1/responses", Api::Responses),
            ("https://chatgpt.com/backend-api/codex", Api::Responses),
        ] {
            assert_eq!(Api::infer(url), want, "{url}");
        }
    }
}
