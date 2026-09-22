//! **Signing in to OpenAI with a ChatGPT subscription.**
//!
//! Authorization Code with PKCE against `auth.openai.com`, which is how
//! the Codex CLI authenticates and therefore the only way a
//! subscription reaches a model. The token it returns is a bearer for
//! the ChatGPT backend; `host::provider` hands it to the Responses
//! client in place of an API key.
//!
//! **This presents itself as the Codex CLI.** `CLIENT_ID` below is
//! OpenAI's own first-party client id, because the authorization server
//! will not issue a subscription-scoped token to anything else. That is
//! a deliberate choice the owner of this repo made knowingly, on their
//! own account and their own credentials; it is worth being plain about
//! rather than leaving for someone to discover in a constant.
//!
//! **No OAuth crate.** The flow is one redirect, one form POST and a
//! refresh; `ureq` and `serde_json` are already here, and the popular
//! crate would bring its own HTTP abstraction to bridge back to `ureq`.
//! What it would have saved is about fifty lines, none of them subtle.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN: &str = "https://auth.openai.com/oauth/token";
const SCOPE: &str = "openid profile email offline_access";
/// The port the client id's redirect is registered against. Not ours to
/// choose: a different one is rejected by the authorization server.
const REDIRECT_PORT: u16 = 1455;
const REDIRECT_PATH: &str = "/auth/callback";

fn redirect_uri() -> String {
    format!("http://localhost:{REDIRECT_PORT}{REDIRECT_PATH}")
}

/// What a successful sign-in leaves behind.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds. Refreshed a minute early, so a request never
    /// starts against a token that expires while it is in flight.
    #[serde(default)]
    pub expires_at: u64,
}

impl Tokens {
    fn stale(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.expires_at != 0 && self.expires_at <= now + 60
    }
}

/// `$AGENT2_STATE_DIR/openai-codex.json`, else `$HOME/.agent2/…`.
///
/// Mode 0600 on write: it is a bearer token for the owner's ChatGPT
/// account, and the default umask is not a permission model.
pub fn token_path() -> Result<PathBuf, String> {
    let dir = match std::env::var("AGENT2_STATE_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => PathBuf::from(
            std::env::var("HOME").map_err(|_| "neither AGENT2_STATE_DIR nor HOME is set")?,
        )
        .join(".agent2"),
    };
    Ok(dir.join("openai-codex.json"))
}

pub fn load() -> Option<Tokens> {
    let path = token_path().ok()?;
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// **Can the token be stored?** Checked before a person is sent to a
/// browser, not after.
///
/// The first live sign-in got all the way through — state matched, code
/// exchanged — and then lost the token to `EROFS` on `$HOME`, reporting
/// a bare errno. The browser round trip had to be repeated for a
/// filesystem fact knowable in advance, and the message did not say
/// which knob fixes it.
pub fn check_writable() -> Result<(), String> {
    let path = token_path()?;
    let dir = path.parent().unwrap_or(&path).to_owned();
    std::fs::create_dir_all(&dir)
        .and_then(|()| {
            let probe = dir.join(".agent2-write-probe");
            std::fs::write(&probe, b"")?;
            std::fs::remove_file(&probe)
        })
        .map_err(|e| {
            format!(
                "cannot write the token to {}: {e}\n\
                 Set AGENT2_STATE_DIR to somewhere writable, or run `agent login` \
                 from a shell where $HOME is not read-only.",
                dir.display()
            )
        })
}

pub fn save(tokens: &Tokens) -> Result<(), String> {
    let path = token_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(tokens).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// The account the stored token belongs to, out of its own claims.
///
/// **Needed for cache affinity, not for authorisation.** The backend
/// accepts a request without it; what it does not do is route the
/// request anywhere in particular, and a prompt cache you land on one
/// request in six is not a cache. See `openai_responses`.
pub fn account_id() -> Option<String> {
    let token = load()?.access_token;
    let payload = token.split('.').nth(1)?;
    let claims: serde_json::Value = serde_json::from_slice(&b64url_decode(payload)?).ok()?;
    claims["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .map(str::to_owned)
}

/// Base64url in, bytes out. Padding optional, as JWT omits it.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = A.iter().position(|&a| a == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Base64url without padding, per RFC 7636. Fifteen lines, and the
/// alternative was a dependency for an alphabet swap.
pub fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..c.len() + 1 {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// 256 bits from two v4 UUIDs. `uuid` is already a dependency and its
/// v4 is drawn from the OS; pulling `rand` in for the same bytes buys
/// nothing.
fn random_b64url() -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    b64url(&bytes)
}

pub fn challenge_for(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

fn form_post(fields: &[(&str, &str)]) -> Result<Tokens, String> {
    let body = fields
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let mut got = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        .build()
        .new_agent()
        .post(TOKEN)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", "agent2/0.1")
        .send(&body)
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = got.status();
    let text = got
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("token response unreadable: {e}"))?;
    if !status.is_success() {
        return Err(format!("token http {status}: {text}"));
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("token response is not JSON: {e}"))?;
    let access_token = v["access_token"]
        .as_str()
        .ok_or_else(|| format!("no access_token in the response: {text}"))?
        .to_owned();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(Tokens {
        access_token,
        refresh_token: v["refresh_token"].as_str().map(str::to_owned),
        expires_at: v["expires_in"].as_u64().map(|s| now + s).unwrap_or(0),
    })
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The URL a person opens, and the verifier that must survive until the
/// code comes back.
pub fn authorize_url() -> (String, String, String) {
    let verifier = random_b64url();
    let state = random_b64url();
    let url = format!(
        "{AUTHORIZE}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        urlencode(CLIENT_ID),
        urlencode(&redirect_uri()),
        urlencode(SCOPE),
        urlencode(&challenge_for(&verifier)),
        urlencode(&state),
    );
    (url, verifier, state)
}

/// `?code=…&state=…` out of a request line like
/// `GET /auth/callback?code=x&state=y HTTP/1.1`.
pub fn params_of(request_line: &str) -> Vec<(String, String)> {
    let Some(target) = request_line.split_whitespace().nth(1) else {
        return Vec::new();
    };
    let Some((_, query)) = target.split_once('?') else {
        return Vec::new();
    };
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_owned(), urldecode(v)))
        .collect()
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(v) => out.push(v),
                    Err(_) => out.push(b'%'),
                }
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Run the whole flow: print a URL, wait for the redirect, exchange the
/// code. Blocks until a browser comes back or the listener is killed.
pub fn login() -> Result<Tokens, String> {
    check_writable()?;
    let (url, verifier, state) = authorize_url();
    // Bound before the URL is printed: if the port is taken there is no
    // point sending anybody to a redirect nothing is listening for.
    let listener = TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).map_err(|e| {
        format!("cannot listen on {REDIRECT_PORT}: {e} — the Codex CLI registers this exact port, so it is not ours to change")
    })?;
    eprintln!("Open this and sign in with the ChatGPT account:\n\n{url}\n");
    eprintln!("Waiting for the redirect on {} …", redirect_uri());

    let (stream, _) = listener
        .accept()
        .map_err(|e| format!("redirect never arrived: {e}"))?;
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|e| format!("could not read the redirect: {e}"))?;
    let params = params_of(&request_line);
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());

    let reply = |body: &str| {
        let mut s = &stream;
        let _ = write!(
            s,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    };
    if let Some(err) = get("error") {
        reply("<h1>Sign-in failed</h1><p>You can close this tab.</p>");
        return Err(format!("authorization denied: {err}"));
    }
    // **Checked, and not merely carried.** The state is the only thing
    // that ties this redirect to the request we made.
    match get("state") {
        Some(s) if s == state => {}
        _ => {
            reply("<h1>Sign-in failed</h1><p>State did not match.</p>");
            return Err("the redirect's `state` did not match the one sent".into());
        }
    }
    let Some(code) = get("code") else {
        reply("<h1>Sign-in failed</h1><p>No code.</p>");
        return Err("the redirect carried no `code`".into());
    };
    reply("<h1>Signed in</h1><p>You can close this tab and go back to the terminal.</p>");

    let tokens = form_post(&[
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", &code),
        ("redirect_uri", &redirect_uri()),
        ("code_verifier", &verifier),
    ])?;
    save(&tokens)?;
    Ok(tokens)
}

/// A usable bearer, refreshing if the stored one is stale.
pub fn access_token() -> Result<String, String> {
    let stored = load().ok_or(
        "not signed in — run `agent login` to authorise with a ChatGPT subscription",
    )?;
    if !stored.stale() {
        return Ok(stored.access_token);
    }
    let Some(refresh) = stored.refresh_token.clone() else {
        return Err("the stored token has expired and there is no refresh token — run `agent login` again".into());
    };
    let mut fresh = form_post(&[
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", &refresh),
    ])?;
    // A refresh does not always return a new refresh token; keeping the
    // old one is what stops the next refresh being a fresh sign-in.
    if fresh.refresh_token.is_none() {
        fresh.refresh_token = Some(refresh);
    }
    save(&fresh)?;
    Ok(fresh.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 §5 vectors, with the padding stripped as RFC 7636 asks.
    #[test]
    fn base64url_matches_the_rfc() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(b"foob"), "Zm9vYg");
        assert_eq!(b64url(b"fooba"), "Zm9vYmE");
        assert_eq!(b64url(b"foobar"), "Zm9vYmFy");
        // The two characters that differ from standard base64, which is
        // the whole reason this is not `base64::encode`.
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
    }

    /// The worked example from RFC 7636 appendix B.
    #[test]
    fn the_pkce_challenge_matches_the_rfc_example() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            challenge_for(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    /// Round-trips, because the decode side is what reads a JWT's
    /// claims and a wrong alphabet there fails as "no account id"
    /// rather than as an error.
    #[test]
    fn base64url_decodes_what_it_encodes() {
        for case in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foobar",
            &[0xfb, 0xff],
            br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"abc-123"}}"#,
        ] {
            assert_eq!(b64url_decode(&b64url(case)).as_deref(), Some(case), "{case:?}");
        }
        assert_eq!(b64url_decode("Zm9vYmFy==").as_deref(), Some(&b"foobar"[..]));
        assert!(b64url_decode("not valid!").is_none());
    }

    #[test]
    fn the_authorize_url_carries_what_the_server_needs() {
        let (url, verifier, state) = authorize_url();
        for needle in [
            "response_type=code",
            "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
            "code_challenge_method=S256",
            "redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
            "scope=openid%20profile%20email%20offline_access",
        ] {
            assert!(url.contains(needle), "missing {needle} in {url}");
        }
        assert!(url.contains(&format!("code_challenge={}", challenge_for(&verifier))));
        assert!(url.contains(&format!("state={}", urlencode(&state))));
        // The verifier itself must never be in the URL — that is the
        // entire point of the exchange.
        assert!(!url.contains(&verifier), "the verifier leaked into the URL");
    }

    #[test]
    fn a_redirect_is_read_back_into_its_parameters() {
        let p = params_of("GET /auth/callback?code=abc%2Fdef&state=xy-z HTTP/1.1");
        assert_eq!(p[0], ("code".into(), "abc/def".into()));
        assert_eq!(p[1], ("state".into(), "xy-z".into()));
        assert!(params_of("GET / HTTP/1.1").is_empty());
        assert!(params_of("garbage").is_empty());
    }

    /// A token with no expiry is not stale — some issuers omit it — and
    /// one a minute from expiring is, so a request never starts against
    /// a token that dies in flight.
    #[test]
    fn staleness_leaves_a_margin() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let t = |expires_at| Tokens {
            access_token: "a".into(),
            refresh_token: None,
            expires_at,
        };
        assert!(!t(0).stale(), "no expiry means no opinion");
        assert!(!t(now + 3600).stale());
        assert!(t(now + 30).stale(), "inside the margin");
        assert!(t(now - 1).stale());
    }
}
