use anyhow::{anyhow, bail, Context};
use aes_gcm::aead::Aead;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::SigningKey;
use p256::elliptic_curve::ecdh::diffie_hellman;
use p256::elliptic_curve::PublicKey as CurvePoint;
use p256::NistP256;
use rand_core::{OsRng, TryRngCore};
use sha2::Sha256;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::store::Subscription;

pub const TTL_SECS: u64 = 86_400;
const VAPID_EXPIRY_SECS: u64 = 12 * 3600;
const RECORD_SIZE: u32 = 4096;

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

// ---------------------------------------------------------------- VAPID keys

pub fn generate_vapid_keypair() -> anyhow::Result<(String, String)> {
    let sk = random_sk()?;
    let priv_b64 = B64.encode(sk.to_bytes());
    let pub_b64 = pubkey_b64(&sk)?;
    Ok((priv_b64, pub_b64))
}

pub fn vapid_public_key(priv_b64: &str) -> anyhow::Result<String> {
    let sk = SigningKey::from_slice(&B64.decode(priv_b64)?)?;
    pubkey_b64(&sk)
}

fn pubkey_b64(sk: &SigningKey) -> anyhow::Result<String> {
    Ok(B64.encode(sk.verifying_key().to_sec1_bytes()))
}

/// Random P-256 scalar (rejects out-of-range values; probability ~2^-32).
fn random_sk() -> anyhow::Result<SigningKey> {
    for _ in 0..8 {
        let mut b = [0u8; 32];
        OsRng.try_fill_bytes(&mut b).context("os rng failure")?;
        if let Ok(sk) = SigningKey::from_slice(&b) {
            return Ok(sk);
        }
    }
    bail!("failed to generate a valid P-256 key")
}

// ------------------------------------------------------------- message crypto

/// Encrypt a push payload per RFC 8291 + RFC 8188 (aes128gcm), single record.
pub fn encrypt_payload(p256dh: &[u8], auth: &[u8], message: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut salt = [0u8; 16];
    OsRng.try_fill_bytes(&mut salt).context("os rng failure")?;
    let as_private = random_sk()?;
    encrypt_payload_with(p256dh, auth, message, &salt, &as_private.to_bytes().into())
}

/// Testable core: fixed salt and application-server private key.
pub fn encrypt_payload_with(
    p256dh: &[u8],
    auth: &[u8],
    message: &[u8],
    salt: &[u8; 16],
    as_private: &[u8; 32],
) -> anyhow::Result<Vec<u8>> {
    if p256dh.len() != 65 {
        bail!("p256dh must be 65 bytes");
    }
    if auth.len() != 16 {
        bail!("auth secret must be 16 bytes");
    }
    let ua_public = CurvePoint::<NistP256>::from_sec1_bytes(p256dh).context("invalid p256dh public key")?;
    let as_sk = SigningKey::from_slice(as_private).context("invalid as_private")?;
    let as_scalar = as_sk.as_nonzero_scalar();
    let as_public = CurvePoint::<NistP256>::from_secret_scalar(&as_scalar);
    let as_sec1 = as_public.to_sec1_bytes();

    let ecdh_secret = diffie_hellman(as_scalar, ua_public.as_affine()).raw_secret_bytes().to_vec();

    // Stage 1 (RFC 8291 §3.3): combine ECDH secret with auth secret -> IKM.
    let mut ikm = [0u8; 32];
    let mut key_info = b"WebPush: info".to_vec();
    key_info.push(0x00);
    key_info.extend_from_slice(p256dh);
    key_info.extend_from_slice(&as_sec1);
    Hkdf::<Sha256>::new(Some(auth), &ecdh_secret).expand(&key_info, &mut ikm)?;

    // Stage 2 (RFC 8188): derive CEK + NONCE from header salt and IKM.
    let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)?;
    let mut nonce = [0u8; 12];
    hk.expand(b"Content-Encoding: nonce\0", &mut nonce)?;

    let mut plaintext = message.to_vec();
    plaintext.push(0x02); // single-record padding delimiter (RFC 8291 §4)

    use aes_gcm::KeyInit;
    let cipher = aes_gcm::Aes128Gcm::new(&aes_gcm::Key::<aes_gcm::Aes128Gcm>::from(cek));
    let ct = cipher
        .encrypt(&aes_gcm::Nonce::from(nonce), plaintext.as_slice())
        .context("aes-gcm encryption failed")?;

    // aes128gcm header: salt || rs || idlen || keyid, then ciphertext+tag.
    let mut body = Vec::with_capacity(86 + ct.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(65);
    body.extend_from_slice(&as_sec1);
    body.extend_from_slice(&ct);
    Ok(body)
}

// ------------------------------------------------------------------- delivery

#[derive(Debug, PartialEq, Eq)]
pub enum PushResult {
    Sent,
    /// Push service says the subscription is gone (404/410) — prune it.
    Gone,
}

pub async fn send_push(
    http: &reqwest::Client,
    cfg: &Config,
    sub: &Subscription,
    message: &[u8],
) -> anyhow::Result<PushResult> {
    let url = reqwest::Url::parse(&sub.endpoint).context("invalid subscription endpoint")?;
    let origin = origin_of(&url)?;

    let priv_bytes = B64.decode(&cfg.vapid_private_key)?;
    let sk = SigningKey::from_slice(&priv_bytes)?;
    let pub_b64 = pubkey_b64(&sk)?;

    let jwt = build_vapid_jwt(&sk, &origin, &cfg.vapid_subject)?;

    let p256dh = B64.decode(&sub.p256dh)?;
    let auth = B64.decode(&sub.auth)?;
    let body = encrypt_payload(&p256dh, &auth, message)?;

    let resp = http
        .post(url)
        .header("authorization", format!("vapid t={jwt}, k={pub_b64}"))
        .header("content-encoding", "aes128gcm")
        .header("content-type", "application/octet-stream")
        .header("ttl", TTL_SECS.to_string())
        .timeout(Duration::from_secs(10))
        .body(body)
        .send()
        .await
        .context("push request failed")?;

    match resp.status().as_u16() {
        200 | 201 | 202 => Ok(PushResult::Sent),
        404 | 410 => Ok(PushResult::Gone),
        s => {
            let detail = resp.text().await.unwrap_or_default();
            Err(anyhow!("push service status {s}: {}", detail.chars().take(160).collect::<String>()))
        }
    }
}

/// RFC 8292: Authorization: vapid t=<jwt>, k=<public key>
fn build_vapid_jwt(sk: &SigningKey, aud: &str, subject: &str) -> anyhow::Result<String> {
    let pub_b64 = pubkey_b64(sk)?;
    let header = format!(r#"{{"typ":"JWT","alg":"ES256","kid":"{pub_b64}"}}"#);
    let claims = serde_json::json!({ "aud": aud, "exp": now_unix() + VAPID_EXPIRY_SECS, "sub": subject });
    let signing_input = format!("{}.{}", B64.encode(header.as_bytes()), B64.encode(claims.to_string().as_bytes()));
    let sig: p256::ecdsa::Signature = sk.sign(signing_input.as_bytes());
    Ok(format!("{signing_input}.{}", B64.encode(sig.to_bytes())))
}

fn origin_of(url: &reqwest::Url) -> anyhow::Result<String> {
    let scheme = url.scheme();
    let host = url.host_str().ok_or_else(|| anyhow!("endpoint has no host"))?;
    match url.port() {
        Some(p) => Ok(format!("{scheme}://{host}:{p}")),
        None => Ok(format!("{scheme}://{host}")),
    }
}

// ------------------------------------------------------------- RFC 8291 vectors

 #[cfg(test)]
 mod tests {
     use super::*;

     /// Manual diagnostics against a local production config; not part of CI.
     /// Requires ~/.config/omp-ntfy/config.json + a stored Apple subscription.
     #[ignore]
     fn diagnostics_apple_jwt() {
         let home = std::env::var("HOME").unwrap();
         let cfg_path = std::path::PathBuf::from(&home).join(".config/omp-ntfy/config.json");
         let cfg = crate::config::Config::load(&cfg_path).expect("load production config");
         let sk = SigningKey::from_slice(&B64.decode(&cfg.vapid_private_key).unwrap()).unwrap();

         let store =
             crate::store::Store::load(std::path::PathBuf::from(&home).join(".local/state/omp-ntfy/store.json"));
         let apple = store
             .data
             .subscriptions
             .iter()
             .find(|s| s.endpoint.contains("web.push.apple.com"))
             .map(|s| s.endpoint.clone());
         let aud = apple
             .as_deref()
             .map(|e| origin_of(&reqwest::Url::parse(e).unwrap()).unwrap())
             .unwrap_or_else(|| "https://web.push.apple.com".to_string());

         let jwt = build_vapid_jwt(&sk, &aud, &cfg.vapid_subject).unwrap();
         let pub_b64 = pubkey_b64(&sk).unwrap();
         let mut parts = jwt.split('.');
         let header = parts.next().unwrap();
         let claims = parts.next().unwrap();
         let sig = parts.next().unwrap();
         let header_json: serde_json::Value =
             serde_json::from_slice(&B64.decode(header).unwrap()).unwrap();
         let claims_json: serde_json::Value =
             serde_json::from_slice(&B64.decode(claims).unwrap()).unwrap();

         println!("== Apple Web Push JWT diagnostics (real production key) ==");
         println!("endpoint: {}", apple.unwrap_or_default());
         println!(
             "header: alg={} typ={} kid_b64url_len={}",
             header_json["alg"],
             header_json["typ"],
             header_json["kid"].as_str().map(|k| k.len()).unwrap_or(0)
         );
         println!("claims: {}", claims_json);
         println!(
             "exp horizon: {}h ahead (limit 24h)",
             (claims_json["exp"].as_u64().unwrap() - now_unix()) / 3600
         );
         println!("aud == origin check: {}", claims_json["aud"] == aud.as_str());
         println!("sig raw bytes: {}", B64.decode(sig).unwrap().len());
         println!("k bytes (uncompressed point): {}", B64.decode(&pub_b64).unwrap().len());
         println!("k={}", pub_b64);
         println!("t={}", jwt);
         assert_eq!(aud, "https://web.push.apple.com");
     }

    #[test]
    fn rfc8291_encryption_vector() {
        let ua = B64
            .decode("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4")
            .unwrap();
        let auth = B64.decode("BTBZMqHH6r4Tts7J_aSIgg").unwrap();
        let as_private: [u8; 32] =
            B64.decode("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw").unwrap().try_into().unwrap();
        let salt: [u8; 16] = B64.decode("DGv6ra1nlYgDCS1FRnbzlw").unwrap().try_into().unwrap();
        let msg = b"When I grow up, I want to be a watermelon";

        let body = encrypt_payload_with(&ua, &auth, msg, &salt, &as_private).unwrap();

        let expected = B64
            .decode("DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN")
            .unwrap();
        assert_eq!(body, expected);
    }

    #[test]
    fn vapid_keypair_roundtrip() {
        let (priv_b64, pub_b64) = generate_vapid_keypair().unwrap();
        let derived = vapid_public_key(&priv_b64).unwrap();
        assert_eq!(derived, pub_b64);
        assert_eq!(B64.decode(&pub_b64).unwrap().len(), 65);
    }
}