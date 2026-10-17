// PWA : enregistre le service worker (sw.js) et propose de recharger quand une nouvelle
// version est prête. Branché une fois, importé par app.js (donc actif sur index.html et
// mobile.html, les deux interfaces).

function showUpdateBanner(reg) {
  if (document.getElementById("pwa-update")) return;
  const bar = document.createElement("div");
  bar.id = "pwa-update";
  bar.style.cssText = "position:fixed;left:0;right:0;bottom:0;z-index:9999;display:flex;align-items:center;" +
    "gap:10px;padding:10px 14px;background:#1a1d23;border-top:1px solid #2a2f38;color:#f1f3f5;font:13px system-ui,sans-serif;";
  bar.innerHTML = `<span style="flex:1">Nouvelle version disponible.</span>
    <button id="pwa-update-go" style="padding:6px 12px;border-radius:8px;border:1px solid #f5a524;background:#f5a524;color:#111317;font-weight:600;cursor:pointer">Mettre à jour</button>
    <button id="pwa-update-close" style="padding:6px 10px;border-radius:8px;border:1px solid #2a2f38;background:transparent;color:inherit;cursor:pointer">✕</button>`;
  document.body.appendChild(bar);
  bar.querySelector("#pwa-update-go").onclick = () => { reg.waiting?.postMessage({ type: "SKIP_WAITING" }); bar.remove(); };
  bar.querySelector("#pwa-update-close").onclick = () => bar.remove();
}

if (typeof window !== "undefined" && "serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js", { scope: "/" }).then((reg) => {
    if (reg.waiting) showUpdateBanner(reg);
    reg.addEventListener("updatefound", () => {
      const worker = reg.installing;
      if (!worker) return;
      worker.addEventListener("statechange", () => {
        if (worker.state === "installed" && navigator.serviceWorker.controller) showUpdateBanner(reg);
      });
    });
  }).catch(() => {});   // pas bloquant : l'appli marche sans PWA (ex. réseau d'entreprise filtré)

  let reloading = false;
  navigator.serviceWorker.addEventListener("controllerchange", () => {
    if (reloading) return;
    reloading = true;
    location.reload();
  });
}
