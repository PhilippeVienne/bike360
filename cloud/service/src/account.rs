//! Comptes : inscription, confirmation, connexion et déconnexion par Amazon Cognito, puis
//! vérification du jeton à chaque requête. Le client d'une requête est l'identifiant Cognito
//! (`sub`) du compte connecté : il sert de préfixe à tout ce qui lui appartient.
//!
//! Le navigateur ne parle qu'à ce service ; les jetons vivent dans des témoins inaccessibles au
//! code de la page (HttpOnly). Un programme peut aussi présenter le jeton d'accès en `Bearer`.
//!
//! Routes (JSON) :
//!   GET  /api/compte                              → {auth, signed_in, email}
//!   POST /api/compte/inscription  {email, password} → {ok, confirmed}
//!   POST /api/compte/confirmation {email, code}     → {ok}
//!   POST /api/compte/connexion    {email, password} → {ok}  (pose les témoins)
//!   POST /api/compte/rafraichir                     → {ok}  (nouveau jeton d'accès)
//!   POST /api/compte/deconnexion                    → {ok}
//!   POST /api/compte/oubli        {email}           → {ok}  (un code part par courriel si le compte existe)
//!   POST /api/compte/reinitialisation {email, code, password} → {ok}
//!   POST /api/compte/suppression  {password}        → {ok}  (efface le compte et tout ce qu'il contient ;
//!                                                          {email} pour un compte Google, qui n'a pas de mot de passe)
//!   GET  /api/compte/google                         → renvoi vers Google, par l'interface OAuth de Cognito
//!   GET  /api/compte/google/retour ?code&state      → pose les témoins, puis renvoi vers la Bibliothèque

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use aws_sdk_cognitoidentityprovider::types::{AttributeType, AuthFlowType};
use axum::extract::{FromRequestParts, Query, State};
use axum::http::header::{AUTHORIZATION, COOKIE, LOCATION, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{Ctx, Fail};

const ACCESS_COOKIE: &str = "bike360_acces";
const REFRESH_COOKIE: &str = "bike360_suite";
const REFRESH_MAX_AGE_S: i64 = 30 * 24 * 3600;
/// Témoin du trajet par Google : l'état et le vérificateur PKCE, le temps de l'aller-retour.
const GOOGLE_COOKIE: &str = "bike360_google";
const GOOGLE_PATH: &str = "/api/compte/google";
const GOOGLE_MAX_AGE_S: i64 = 600;

pub struct Auth {
    idp: aws_sdk_cognitoidentityprovider::Client,
    /// Émetteur des jetons (adresse du groupe d'utilisateurs) ; ses clés publiques sont sous /.well-known/jwks.json.
    issuer: String,
    /// Application cliente Cognito à laquelle les jetons doivent être destinés.
    app_client: String,
    /// Interface OAuth du groupe d'utilisateurs (son domaine) : active la connexion par Google.
    oauth: Option<String>,
    /// Adresse publique du site, où Cognito renvoie après Google.
    site: String,
    keys: RwLock<HashMap<String, DecodingKey>>,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    token_use: String,
    client_id: String,
}

impl Auth {
    pub async fn new(conf: &aws_config::SdkConfig, issuer: String, app_client: String, oauth: Option<String>, site: String) -> Result<Auth> {
        let auth = Auth { idp: aws_sdk_cognitoidentityprovider::Client::new(conf), issuer: issuer.trim_end_matches('/').to_string(),
                          app_client, oauth: oauth.map(|o| o.trim_end_matches('/').to_string()).filter(|o| !o.is_empty()), site,
                          keys: RwLock::new(HashMap::new()) };
        auth.load_keys().await?;
        Ok(auth)
    }

    /// Les jetons d'un émetteur en clair (émulateur local) ne sont pas réservés au HTTPS.
    fn secure(&self) -> bool {
        !self.issuer.starts_with("http://")
    }

    async fn load_keys(&self) -> Result<()> {
        let url = format!("{}/.well-known/jwks.json", self.issuer);
        let jwks: Value = tokio::task::spawn_blocking(move || -> Result<Value> {
            Ok(ureq::get(&url).timeout(std::time::Duration::from_secs(10)).call()?.into_json()?)
        }).await?.context("clés publiques de l'émetteur")?;
        let mut keys = HashMap::new();
        for k in jwks["keys"].as_array().into_iter().flatten() {
            if let (Some(kid), Some(n), Some(e)) = (k["kid"].as_str(), k["n"].as_str(), k["e"].as_str()) {
                keys.insert(kid.to_string(), DecodingKey::from_rsa_components(n, e)?);
            }
        }
        anyhow::ensure!(!keys.is_empty(), "aucune clé publique chez l'émetteur");
        *self.keys.write().unwrap() = keys;
        Ok(())
    }

    /// Client (`sub`) d'un jeton d'accès valide : signé par l'émetteur, non expiré, destiné à cette application.
    pub async fn verify(&self, token: &str) -> Option<String> {
        let kid = decode_header(token).ok()?.kid?;
        if !self.keys.read().unwrap().contains_key(&kid) {
            self.load_keys().await.ok()?;   // l'émetteur a pu renouveler ses clés
        }
        let key = self.keys.read().unwrap().get(&kid)?.clone();
        let mut rules = Validation::new(Algorithm::RS256);
        rules.set_issuer(&[&self.issuer]);
        rules.validate_aud = false;   // un jeton d'accès Cognito porte client_id, pas aud
        let claims = decode::<Claims>(token, &key, &rules).ok()?.claims;
        (claims.token_use == "access" && claims.client_id == self.app_client).then_some(claims.sub)
    }

    /// Adresse de courriel d'un compte, lue auprès du groupe d'utilisateurs (son nom termine l'adresse de l'émetteur).
    pub async fn email_of(&self, client: &str) -> Result<String> {
        let user = self.idp.admin_get_user().user_pool_id(self.pool()?).username(client).send().await?;
        user.user_attributes().iter().find(|a| a.name() == "email").and_then(|a| a.value().map(String::from)).context("compte sans adresse")
    }

    fn pool(&self) -> Result<&str> {
        self.issuer.rsplit('/').next().context("émetteur sans groupe d'utilisateurs")
    }

    fn cookie(&self, name: &str, value: &str, path: &str, max_age: i64) -> String {
        format!("{name}={value}; Path={path}; Max-Age={max_age}; HttpOnly; SameSite=Strict{}", if self.secure() { "; Secure" } else { "" })
    }

    /// Témoin du trajet par Google. Le retour vient d'un autre site : `Lax`, sans quoi le navigateur ne le renverrait pas.
    fn transit(&self, value: &str, max_age: i64) -> String {
        format!("{GOOGLE_COOKIE}={value}; Path={GOOGLE_PATH}; Max-Age={max_age}; HttpOnly; SameSite=Lax{}", if self.secure() { "; Secure" } else { "" })
    }

    fn redirect_uri(&self) -> String {
        format!("{}{GOOGLE_PATH}/retour", self.site)
    }

    /// Comptes du groupe qui portent cette adresse (identifiants Cognito).
    async fn holders(&self, email: &str) -> Result<Vec<String>> {
        anyhow::ensure!(!email.contains(['"', '\\']), "adresse invalide");
        let out = self.idp.list_users().user_pool_id(self.pool()?).filter(format!("email = \"{email}\"")).send().await?;
        Ok(out.users().iter()
            .filter(|u| u.attributes().iter().any(|a| a.name() == "email" && a.value().is_some_and(|v| v.eq_ignore_ascii_case(email))))
            .filter_map(|u| u.username().map(String::from)).collect())
    }

    /// Retour de Google : échange le code contre les jetons du compte. En cas de refus, le motif
    /// (« google » ou « doublon ») est affiché par la page du compte.
    async fn federated(&self, headers: &HeaderMap, q: Back) -> Result<(String, i64, Option<String>), &'static str> {
        let oauth = self.oauth.as_deref().ok_or("google")?;
        let (state, verifier) = cookie_of(headers, GOOGLE_COOKIE).and_then(|v| v.split_once('.')).ok_or("google")?;
        let code = q.code.filter(|_| q.state.as_deref() == Some(state)).ok_or("google")?;
        let (url, form) = (format!("{oauth}/oauth2/token"),
                           [("grant_type", "authorization_code".to_string()), ("client_id", self.app_client.clone()), ("code", code),
                            ("redirect_uri", self.redirect_uri()), ("code_verifier", verifier.to_string())]);
        let tokens: Value = tokio::task::spawn_blocking(move || -> Result<Value> {
            let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
            Ok(ureq::post(&url).timeout(std::time::Duration::from_secs(10)).send_form(&form)?.into_json()?)
        }).await.ok().and_then(|r| r.map_err(|e| eprintln!("comptes : échange du code Google : {e:#}")).ok()).ok_or("google")?;
        let access = tokens["access_token"].as_str().ok_or("google")?;
        self.verify(access).await.ok_or("google")?;
        // Cognito crée pour Google un compte distinct de celui ouvert par mot de passe à la même adresse :
        // plutôt que deux comptes pour une personne, on refuse et on retire celui qui vient de naître.
        let user = self.idp.get_user().access_token(access).send().await.map_err(|e| { eprintln!("comptes : {e:?}"); "google" })?;
        let email = user.user_attributes().iter().find(|a| a.name() == "email").and_then(|a| a.value()).ok_or("google")?;
        let holders = self.holders(email).await.map_err(|e| { eprintln!("comptes : {e:#}"); "google" })?;
        if holders.iter().any(|h| h != user.username()) {
            let pool = self.pool().map_err(|_| "google")?;
            if let Err(e) = self.idp.admin_delete_user().user_pool_id(pool).username(user.username()).send().await {
                eprintln!("comptes : doublon {} non retiré : {e:?}", user.username());
            }
            return Err("doublon");
        }
        Ok((access.to_string(), tokens["expires_in"].as_i64().unwrap_or(3600), tokens["refresh_token"].as_str().map(String::from)))
    }
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("aléa indisponible : {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn see(location: &str) -> Response {
    (StatusCode::SEE_OTHER, [(LOCATION, location)]).into_response()
}

/// Un compte venu de Google n'a pas de mot de passe ici.
fn federated_user(attributes: &[AttributeType]) -> bool {
    attributes.iter().any(|a| a.name() == "identities")
}

fn cookie_of<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get_all(COOKIE).iter().filter_map(|v| v.to_str().ok())
        .flat_map(|c| c.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

fn access_token(headers: &HeaderMap) -> Option<&str> {
    headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| cookie_of(headers, ACCESS_COOKIE))
}

/// Contexte d'une requête : le service et le client pour qui elle agit.
pub struct Scope {
    ctx: Arc<Ctx>,
    pub client: String,
}

impl Scope {
    /// Contexte pour un client déjà identifié (tâche de fond du service, sans requête).
    pub fn of(ctx: Arc<Ctx>, client: String) -> Scope {
        Scope { ctx, client }
    }
}

impl Deref for Scope {
    type Target = Ctx;
    fn deref(&self) -> &Ctx {
        &self.ctx
    }
}

impl FromRequestParts<Arc<Ctx>> for Scope {
    type Rejection = Fail;

    async fn from_request_parts(parts: &mut Parts, ctx: &Arc<Ctx>) -> Result<Scope, Fail> {
        let client = match &ctx.auth {
            // sans émetteur configuré : un seul client, fixé au lancement (essai local)
            None => ctx.client.clone(),
            Some(auth) => {
                let token = access_token(&parts.headers).ok_or_else(|| Fail(StatusCode::UNAUTHORIZED, "connexion requise".into()))?;
                auth.verify(token).await.ok_or_else(|| Fail(StatusCode::UNAUTHORIZED, "session expirée ou invalide".into()))?
            }
        };
        Ok(Scope { ctx: ctx.clone(), client })
    }
}

fn auth_of(c: &Ctx) -> Result<&Auth, Fail> {
    c.auth.as_ref().ok_or_else(|| Fail(StatusCode::NOT_FOUND, "les comptes ne sont pas activés sur ce service".into()))
}

fn unavailable<E: std::fmt::Debug>(e: E) -> Fail {
    eprintln!("comptes : {e:?}");
    Fail(StatusCode::BAD_GATEWAY, "service de comptes indisponible".into())
}

#[derive(Deserialize)]
pub struct Credentials {
    email: String,
    password: String,
}

#[derive(Deserialize)]
pub struct Confirmation {
    email: String,
    code: String,
}

pub async fn status(State(c): State<Arc<Ctx>>, headers: HeaderMap) -> Json<Value> {
    let Some(auth) = &c.auth else { return Json(json!({"auth": false, "signed_in": true, "email": null, "google": false})) };
    let token = access_token(&headers);
    let signed_in = match token {
        Some(t) => auth.verify(t).await.is_some(),
        None => false,
    };
    let user = match token.filter(|_| signed_in) {
        Some(t) => auth.idp.get_user().access_token(t).send().await.ok(),
        None => None,
    };
    let email = user.as_ref().and_then(|u| u.user_attributes().iter().find(|a| a.name() == "email").and_then(|a| a.value().map(String::from)));
    Json(json!({"auth": true, "signed_in": signed_in, "email": email, "can_refresh": cookie_of(&headers, REFRESH_COOKIE).is_some(),
                "google": auth.oauth.is_some(), "password": !user.is_some_and(|u| federated_user(u.user_attributes()))}))
}

pub async fn sign_up(State(c): State<Arc<Ctx>>, Json(b): Json<Credentials>) -> Result<Json<Value>, Fail> {
    let auth = auth_of(&c)?;
    // une adresse déjà connue par Google ne s'ouvre pas une seconde fois par mot de passe
    if auth.oauth.is_some() && !auth.holders(&b.email).await.map_err(unavailable)?.is_empty() {
        return Err(Fail(StatusCode::CONFLICT, "un compte existe déjà avec cette adresse".into()));
    }
    let email = AttributeType::builder().name("email").value(&b.email).build().map_err(unavailable)?;
    match auth.idp.sign_up().client_id(&auth.app_client).username(&b.email).password(&b.password).user_attributes(email).send().await {
        Ok(out) => Ok(Json(json!({"ok": true, "confirmed": out.user_confirmed()}))),
        Err(e) => Err(match e.as_service_error() {
            Some(s) if s.is_username_exists_exception() => Fail(StatusCode::CONFLICT, "un compte existe déjà avec cette adresse".into()),
            Some(s) if s.is_invalid_password_exception() => Fail(StatusCode::BAD_REQUEST, "mot de passe trop faible".into()),
            Some(s) if s.is_invalid_parameter_exception() => Fail(StatusCode::BAD_REQUEST, "adresse ou mot de passe invalide".into()),
            _ => unavailable(e),
        }),
    }
}

pub async fn confirm(State(c): State<Arc<Ctx>>, Json(b): Json<Confirmation>) -> Result<Json<Value>, Fail> {
    let auth = auth_of(&c)?;
    match auth.idp.confirm_sign_up().client_id(&auth.app_client).username(&b.email).confirmation_code(&b.code).send().await {
        Ok(_) => Ok(Json(json!({"ok": true}))),
        Err(e) => Err(match e.as_service_error() {
            Some(s) if s.is_code_mismatch_exception() || s.is_expired_code_exception() => Fail(StatusCode::BAD_REQUEST, "code incorrect ou expiré".into()),
            _ => unavailable(e),
        }),
    }
}

/// Réponse {ok} qui pose le jeton d'accès (et le jeton de suite s'il est fourni) dans des témoins.
fn signed_in(auth: &Auth, access: &str, expires_in: i32, refresh: Option<&str>) -> Response {
    let mut res = Json(json!({"ok": true})).into_response();
    set_session(&mut res, auth, access, expires_in as i64, refresh);
    res
}

fn set_session(res: &mut Response, auth: &Auth, access: &str, expires_in: i64, refresh: Option<&str>) {
    let mut set = |c: String| res.headers_mut().append(SET_COOKIE, c.parse().unwrap());
    set(auth.cookie(ACCESS_COOKIE, access, "/", expires_in));
    if let Some(r) = refresh {
        set(auth.cookie(REFRESH_COOKIE, r, "/api/compte", REFRESH_MAX_AGE_S));
    }
}

/// Départ vers Google : l'état et le vérificateur PKCE restent dans un témoin, le temps de l'aller-retour.
pub async fn google(State(c): State<Arc<Ctx>>) -> Result<Response, Fail> {
    let auth = auth_of(&c)?;
    let oauth = auth.oauth.as_deref().ok_or_else(|| Fail(StatusCode::NOT_FOUND, "la connexion par Google n'est pas activée sur ce service".into()))?;
    let (state, verifier) = (random_token().map_err(unavailable)?, random_token().map_err(unavailable)?);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    // la portée « aws.cognito.signin.user.admin » laisse le compte lire son adresse et se supprimer avec ce jeton
    let mut res = see(&format!("{oauth}/oauth2/authorize?identity_provider=Google&response_type=code&client_id={}&redirect_uri={}\
                                &scope=openid+email+aws.cognito.signin.user.admin&state={state}&code_challenge={challenge}&code_challenge_method=S256",
                               enc(&auth.app_client), enc(&auth.redirect_uri())));
    res.headers_mut().append(SET_COOKIE, auth.transit(&format!("{state}.{verifier}"), GOOGLE_MAX_AGE_S).parse().unwrap());
    Ok(res)
}

#[derive(Deserialize)]
pub struct Back {
    code: Option<String>,
    state: Option<String>,
}

pub async fn google_back(State(c): State<Arc<Ctx>>, headers: HeaderMap, Query(q): Query<Back>) -> Response {
    let Some(auth) = &c.auth else { return see("/ui/compte.html") };
    let mut res = match auth.federated(&headers, q).await {
        Ok((access, expires_in, refresh)) => {
            let mut res = see("/ui/bibliotheque.html");
            set_session(&mut res, auth, &access, expires_in, refresh.as_deref());
            res
        }
        Err(reason) => see(&format!("/ui/compte.html?erreur={reason}")),
    };
    res.headers_mut().append(SET_COOKIE, auth.transit("", 0).parse().unwrap());
    res
}

pub async fn sign_in(State(c): State<Arc<Ctx>>, Json(b): Json<Credentials>) -> Result<Response, Fail> {
    let auth = auth_of(&c)?;
    let out = auth.idp.initiate_auth().client_id(&auth.app_client).auth_flow(AuthFlowType::UserPasswordAuth)
        .auth_parameters("USERNAME", &b.email).auth_parameters("PASSWORD", &b.password).send().await
        .map_err(|e| match e.as_service_error() {
            // même réponse pour un compte inconnu et un mauvais mot de passe
            Some(s) if s.is_not_authorized_exception() || s.is_user_not_found_exception() => Fail(StatusCode::UNAUTHORIZED, "adresse ou mot de passe incorrect".into()),
            Some(s) if s.is_user_not_confirmed_exception() => Fail(StatusCode::FORBIDDEN, "compte à confirmer : saisis le code reçu par courriel".into()),
            _ => unavailable(e),
        })?;
    let r = out.authentication_result().ok_or_else(|| Fail(StatusCode::FORBIDDEN, "connexion à compléter (étape supplémentaire non prise en charge)".into()))?;
    let access = r.access_token().ok_or_else(|| unavailable("connexion sans jeton"))?;
    Ok(signed_in(auth, access, r.expires_in(), r.refresh_token()))
}

pub async fn refresh(State(c): State<Arc<Ctx>>, headers: HeaderMap) -> Result<Response, Fail> {
    let auth = auth_of(&c)?;
    let token = cookie_of(&headers, REFRESH_COOKIE).ok_or_else(|| Fail(StatusCode::UNAUTHORIZED, "connexion requise".into()))?;
    let out = auth.idp.initiate_auth().client_id(&auth.app_client).auth_flow(AuthFlowType::RefreshTokenAuth)
        .auth_parameters("REFRESH_TOKEN", token).send().await
        .map_err(|_| Fail(StatusCode::UNAUTHORIZED, "connexion requise".into()))?;
    let r = out.authentication_result().ok_or_else(|| Fail(StatusCode::UNAUTHORIZED, "connexion requise".into()))?;
    let access = r.access_token().ok_or_else(|| unavailable("rafraîchissement sans jeton"))?;
    Ok(signed_in(auth, access, r.expires_in(), None))
}

pub async fn sign_out(State(c): State<Arc<Ctx>>) -> Result<Response, Fail> {
    let auth = auth_of(&c)?;
    let mut res = Json(json!({"ok": true})).into_response();
    for (name, path) in [(ACCESS_COOKIE, "/"), (REFRESH_COOKIE, "/api/compte")] {
        res.headers_mut().append(SET_COOKIE, auth.cookie(name, "", path, 0).parse().unwrap());
    }
    Ok(res)
}

#[derive(Deserialize)]
pub struct Forgot {
    email: String,
}

/// Mot de passe oublié : un code part par courriel. La réponse est la même que le compte existe ou non.
pub async fn forgot(State(c): State<Arc<Ctx>>, Json(b): Json<Forgot>) -> Result<Json<Value>, Fail> {
    let auth = auth_of(&c)?;
    match auth.idp.forgot_password().client_id(&auth.app_client).username(&b.email).send().await {
        Ok(_) => Ok(Json(json!({"ok": true}))),
        Err(e) => match e.as_service_error() {
            Some(s) if s.is_limit_exceeded_exception() || s.is_too_many_requests_exception() => Err(Fail(StatusCode::TOO_MANY_REQUESTS, "trop de demandes : réessaie plus tard".into())),
            // compte inconnu, non confirmé ou sans adresse vérifiée : rien ne le distingue d'un envoi réussi
            Some(_) => Ok(Json(json!({"ok": true}))),
            None => Err(unavailable(e)),
        },
    }
}

#[derive(Deserialize)]
pub struct Reset {
    email: String,
    code: String,
    password: String,
}

pub async fn reset(State(c): State<Arc<Ctx>>, Json(b): Json<Reset>) -> Result<Json<Value>, Fail> {
    let auth = auth_of(&c)?;
    match auth.idp.confirm_forgot_password().client_id(&auth.app_client).username(&b.email).confirmation_code(&b.code).password(&b.password).send().await {
        Ok(_) => Ok(Json(json!({"ok": true}))),
        Err(e) => Err(match e.as_service_error() {
            Some(s) if s.is_invalid_password_exception() => Fail(StatusCode::BAD_REQUEST, "mot de passe trop faible".into()),
            Some(s) if s.is_limit_exceeded_exception() || s.is_too_many_failed_attempts_exception() => Fail(StatusCode::TOO_MANY_REQUESTS, "trop d'essais : réessaie plus tard".into()),
            Some(_) => Fail(StatusCode::BAD_REQUEST, "code incorrect ou expiré".into()),
            None => unavailable(e),
        }),
    }
}

#[derive(Deserialize)]
pub struct Deletion {
    #[serde(default)]
    password: String,
    /// Un compte Google n'a pas de mot de passe : il confirme en recopiant son adresse.
    #[serde(default)]
    email: String,
}

/// Efface le compte à la demande de son titulaire, qui le confirme par son mot de passe (ou, pour un
/// compte Google, en recopiant son adresse) : abonnement
/// arrêté, fichiers et index supprimés, puis le compte lui-même. Rien n'est récupérable ensuite.
pub async fn delete(c: Scope, headers: HeaderMap, Json(b): Json<Deletion>) -> Result<Response, Fail> {
    let auth = auth_of(&c)?;
    let token = access_token(&headers).ok_or_else(|| Fail(StatusCode::UNAUTHORIZED, "connexion requise".into()))?;
    let user = auth.idp.get_user().access_token(token).send().await.map_err(unavailable)?;
    let email = user.user_attributes().iter().find(|a| a.name() == "email").and_then(|a| a.value()).ok_or_else(|| unavailable("compte sans adresse"))?;
    if federated_user(user.user_attributes()) {
        if !b.email.trim().eq_ignore_ascii_case(email) {
            return Err(Fail(StatusCode::FORBIDDEN, "adresse incorrecte".into()));
        }
    } else {
        auth.idp.initiate_auth().client_id(&auth.app_client).auth_flow(AuthFlowType::UserPasswordAuth)
            .auth_parameters("USERNAME", email).auth_parameters("PASSWORD", &b.password).send().await
            .map_err(|e| match e.as_service_error() {
                Some(s) if s.is_not_authorized_exception() => Fail(StatusCode::FORBIDDEN, "mot de passe incorrect".into()),
                _ => unavailable(e),
            })?;
    }
    // l'abonnement d'abord : un compte effacé ne doit plus être prélevé
    let acc = c.account().await?;
    match &c.payment {
        Some(p) => p.end_now(&acc).await?,
        None if acc.subscription.is_some() => return Err(Fail(StatusCode::CONFLICT, "abonnement en cours, et le paiement n'est pas configuré sur ce service".into())),
        None => {}
    }
    let files = c.erase(acc.customer.as_deref()).await.map_err(|e| {
        eprintln!("effacement de {} : {e:#}", c.client);
        Fail(StatusCode::BAD_GATEWAY, "effacement interrompu : recommence, rien de ce qui reste n'est perdu de vue".into())
    })?;
    auth.idp.delete_user().access_token(token).send().await.map_err(unavailable)?;
    println!("compte {} effacé ({files} objet(s))", c.client);
    let mut res = Json(json!({"ok": true, "files": files})).into_response();
    for (name, path) in [(ACCESS_COOKIE, "/"), (REFRESH_COOKIE, "/api/compte")] {
        res.headers_mut().append(SET_COOKIE, auth.cookie(name, "", path, 0).parse().unwrap());
    }
    Ok(res)
}
