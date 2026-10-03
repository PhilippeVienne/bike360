//! Authentification : un mot de passe unique (BIKE360_PASSWORD), page de connexion et cookie signé.
//!
//! Sans mot de passe configuré, l'accès reste libre (usage local). Avec, toute requête doit porter
//! soit le cookie de session (posé par /login, valable 30 jours, signé HMAC-SHA1 avec le mot de
//! passe : changer le mot de passe déconnecte tout le monde), soit `Authorization: Bearer <mot de passe>`.
//! Les essais ratés sont ralentis (1 s) puis bloqués par adresse (5 par 10 min).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::{header, HeaderMap};

pub const COOKIE: &str = "bike360_session";
const SESSION_S: u64 = 30 * 24 * 3600;
const MAX_FAILS: usize = 5;
const FAIL_WINDOW: Duration = Duration::from_secs(600);

static PASSWORD: OnceLock<Option<String>> = OnceLock::new();
static FAILS: OnceLock<Mutex<HashMap<IpAddr, Vec<Instant>>>> = OnceLock::new();

/// Lit BIKE360_PASSWORD (une valeur vide désactive l'authentification).
pub fn init() -> bool {
    let p = std::env::var("BIKE360_PASSWORD").ok().filter(|p| !p.is_empty());
    let on = p.is_some();
    let _ = PASSWORD.set(p);
    on
}

pub fn enabled() -> bool {
    PASSWORD.get().is_some_and(Option::is_some)
}

fn password() -> &'static str {
    PASSWORD.get().and_then(|p| p.as_deref()).unwrap_or("")
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Comparaison en temps constant.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut k = key.to_vec();
    if k.len() > BLOCK {
        k = sha1_smol::Sha1::from(&k).digest().bytes().to_vec();
    }
    k.resize(BLOCK, 0);
    let mut inner = sha1_smol::Sha1::new();
    inner.update(&k.iter().map(|b| b ^ 0x36).collect::<Vec<_>>());
    inner.update(msg);
    let mut outer = sha1_smol::Sha1::new();
    outer.update(&k.iter().map(|b| b ^ 0x5c).collect::<Vec<_>>());
    outer.update(&inner.digest().bytes());
    outer.digest().to_string()
}

/// Jeton de session « horodatage.signature ».
fn token(ts: u64) -> String {
    format!("{ts}.{}", hmac_sha1(password().as_bytes(), format!("bike360-session|{ts}").as_bytes()))
}

fn token_ok(tok: &str) -> bool {
    let Some((ts, _)) = tok.split_once('.') else { return false };
    let Ok(t) = ts.parse::<u64>() else { return false };
    let age = now_s().saturating_sub(t);
    t <= now_s() + 60 && age < SESSION_S && same(tok, &token(t))
}

/// La requête porte-t-elle un cookie de session valide ou le mot de passe en Bearer ?
pub fn authorized(headers: &HeaderMap) -> bool {
    if !enabled() {
        return true;
    }
    if let Some(a) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")) {
        if same(a.trim(), password()) {
            return true;
        }
    }
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok())
        .flat_map(|c| c.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .any(|(k, v)| k == COOKIE && token_ok(v))
}

/// Trop d'essais ratés récents depuis cette adresse ?
pub fn blocked(ip: IpAddr) -> bool {
    let mut m = FAILS.get_or_init(Default::default).lock().unwrap();
    let v = m.entry(ip).or_default();
    v.retain(|t| t.elapsed() < FAIL_WINDOW);
    v.len() >= MAX_FAILS
}

pub fn record_fail(ip: IpAddr) {
    FAILS.get_or_init(Default::default).lock().unwrap().entry(ip).or_default().push(Instant::now());
}

/// Vérifie le mot de passe saisi ; en cas de succès renvoie la valeur du cookie à poser.
pub fn login(given: &str) -> Option<String> {
    same(given, password()).then(|| token(now_s()))
}

/// En-tête Set-Cookie (secure derrière HTTPS, p. ex. Tailscale Serve).
pub fn set_cookie(value: &str, https: bool) -> String {
    let max = if value.is_empty() { 0 } else { SESSION_S };
    format!("{COOKIE}={value}; Path=/; Max-Age={max}; HttpOnly; SameSite=Lax{}", if https { "; Secure" } else { "" })
}

/// Décodage d'un champ de formulaire (« + » = espace, %xx).
pub fn form_field(body: &str, name: &str) -> String {
    let raw = body.split('&').filter_map(|p| p.split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v).unwrap_or("");
    let bytes = raw.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() && raw.is_char_boundary(i + 1) && raw.is_char_boundary(i + 3) => {
                match u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                    Ok(b) => { out.push(b); i += 2; }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Adresse de retour acceptée : chemin local uniquement.
pub fn safe_next(next: &str) -> String {
    if next.starts_with('/') && !next.starts_with("//") && !next.contains('\\') && !next.starts_with("/login") { next.to_string() } else { "/".into() }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn login_page(error: &str, next: &str) -> String {
    format!(r#"<!doctype html><html lang="fr"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>Bike360 — connexion</title>
<style>
body{{margin:0;min-height:100vh;display:grid;place-items:center;background:#111317;color:#e6e8eb;font:15px/1.4 system-ui,sans-serif}}
form{{width:min(340px,90vw);display:flex;flex-direction:column;gap:12px;padding:24px;background:#1a1d23;border:1px solid #2a2f38;border-radius:12px}}
h1{{margin:0;font-size:20px}}h1 span{{color:#f5a524}}
input,button{{font:inherit;padding:11px 12px;border-radius:8px;border:1px solid #2a2f38;background:#111317;color:inherit}}
button{{background:#f5a524;color:#111;font-weight:600;cursor:pointer}}
.err{{color:#ff7875;font-size:13px}}.hint{{color:#8a919c;font-size:12px}}
</style></head><body>
<form method="post" action="/login">
<h1><span>◉</span> Bike360</h1>
<input type="password" name="password" placeholder="Mot de passe" autocomplete="current-password" autofocus required>
<input type="hidden" name="next" value="{}">
{}
<button type="submit">Se connecter</button>
<span class="hint">Accès protégé : le mot de passe est dans le réglage BIKE360_PASSWORD du serveur.</span>
</form></body></html>"#, esc(next), if error.is_empty() { String::new() } else { format!(r#"<div class="err">{}</div>"#, esc(error)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc2202() {
        assert_eq!(hmac_sha1(b"Jefe", b"what do ya want for nothing?"), "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");
        // clé plus longue qu'un bloc
        assert_eq!(hmac_sha1(&[0xaa; 80], b"Test Using Larger Than Block-Size Key - Hash Key First"), "aa4ae5e15272d00e95705637ce8a3b55ed402112");
    }

    #[test]
    fn form_fields_are_decoded() {
        assert_eq!(form_field("password=a%20b%2Bc+d%C3%A9&next=%2Fx", "password"), "a b+c dé");
        assert_eq!(form_field("password=100%&next=/", "password"), "100%");
        assert_eq!(form_field("a=1", "b"), "");
    }

    #[test]
    fn next_is_local_only() {
        assert_eq!(safe_next("/ui/mobile.html"), "/ui/mobile.html");
        for bad in ["//evil.com", "https://evil.com", "/\\evil", "/login", ""] {
            assert_eq!(safe_next(bad), "/", "{bad}");
        }
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
    }
}
