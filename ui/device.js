// Choix de l'interface : ordinateur (/ = index.html) ou téléphone (/ui/mobile.html).
// Script classique chargé en tête des deux pages (avant tout le reste), qui portent
// <html data-ui="desktop|mobile">. Au chargement : le choix mémorisé dans ce navigateur,
// sinon automatique (écran tactile de téléphone ou fenêtre étroite → téléphone).
// Les liens [data-ui-switch="mobile|desktop"] basculent et mémorisent le choix ;
// « ?ui=mobile » ou « ?ui=desktop » dans l'adresse fait de même.
(function () {
  var KEY = "bike360.ui";
  var here = document.documentElement.getAttribute("data-ui");
  var url = function (ui) { return (ui === "mobile" ? "/ui/mobile.html" : "/") + location.hash; };
  var remember = function (ui) { try { localStorage.setItem(KEY, ui); } catch (e) { /* stockage indisponible */ } };
  var stored = null;
  try { stored = localStorage.getItem(KEY); } catch (e) { /* stockage indisponible */ }
  var q = /[?&]ui=(desktop|mobile)\b/.exec(location.search);
  if (q) { stored = q[1]; remember(stored); }
  var phone = matchMedia("(pointer: coarse)").matches && Math.min(screen.width, screen.height) <= 820;
  var auto = phone || innerWidth <= 700 ? "mobile" : "desktop";
  var want = stored === "desktop" || stored === "mobile" ? stored : auto;
  if (want !== here) { window.bike360Leaving = true; location.replace(url(want)); return; }   // app.js ne démarre pas

  document.addEventListener("click", function (e) {
    var a = e.target.closest && e.target.closest("[data-ui-switch]");
    if (!a) return;
    e.preventDefault();
    var to = a.getAttribute("data-ui-switch");
    remember(to);
    location.href = url(to);
  });
})();
