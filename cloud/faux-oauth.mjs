// Faux domaine OAuth de Cognito, pour l'essai local de la connexion par Google : l'émulateur n'en a pas.
// Usage : node cloud/faux-oauth.mjs PORT
//   GET  /oauth2/authorize  → renvoie aussitôt à redirect_uri avec un code (comme si Google avait dit oui)
//   POST /oauth2/token      → rend les jetons déposés par l'essai, si le vérificateur PKCE est le bon
//   POST /_jetons           → (essai seulement) les jetons que rendra le prochain échange
import { createServer } from "node:http";
import { createHash, randomUUID } from "node:crypto";

const port = Number(process.argv[2] || 12113);
const codes = new Map();   // code → {challenge, client_id, redirect_uri}
let tokens = null;
const body = (req) => new Promise((ok) => { let b = ""; req.on("data", (c) => (b += c)); req.on("end", () => ok(b)); });
const json = (res, status, value) => { res.writeHead(status, { "Content-Type": "application/json" }); res.end(JSON.stringify(value)); };

createServer(async (req, res) => {
  const url = new URL(req.url, `http://127.0.0.1:${port}`);
  if (req.method === "GET" && url.pathname === "/oauth2/authorize") {
    const q = url.searchParams;
    const scope = (q.get("scope") || "").split(" ");
    if (q.get("identity_provider") !== "Google" || q.get("response_type") !== "code" || q.get("code_challenge_method") !== "S256"
        || !q.get("code_challenge") || !q.get("state") || !["openid", "email", "aws.cognito.signin.user.admin"].every((s) => scope.includes(s)))
      return json(res, 400, { error: "invalid_request" });
    const code = randomUUID();
    codes.set(code, { challenge: q.get("code_challenge"), client_id: q.get("client_id"), redirect_uri: q.get("redirect_uri") });
    const back = new URL(q.get("redirect_uri"));
    back.searchParams.set("code", code);
    back.searchParams.set("state", q.get("state"));
    res.writeHead(302, { Location: back.href });
    return res.end();
  }
  if (req.method === "POST" && url.pathname === "/oauth2/token") {
    const f = new URLSearchParams(await body(req));
    const c = codes.get(f.get("code"));
    codes.delete(f.get("code"));   // un code ne sert qu'une fois
    const challenge = createHash("sha256").update(f.get("code_verifier") || "").digest("base64url");
    if (!c || !tokens || f.get("grant_type") !== "authorization_code" || c.challenge !== challenge
        || c.client_id !== f.get("client_id") || c.redirect_uri !== f.get("redirect_uri"))
      return json(res, 400, { error: "invalid_grant" });
    return json(res, 200, { ...tokens, token_type: "Bearer" });
  }
  if (req.method === "POST" && url.pathname === "/_jetons") {
    tokens = JSON.parse(await body(req));
    return json(res, 200, { ok: true });
  }
  json(res, 404, { error: "not_found" });
}).listen(port, "127.0.0.1");
