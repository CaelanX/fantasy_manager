// Player card: a click or tap on any player link (/player/<cid>) opens a popup with the card
// (/player/<cid>?card=1) instead of leaving the page. Ctrl/Cmd/Shift/middle click and links marked
// data-nocard still open the full player page. Runs before htmx's boosted navigation (capture phase).
(function () {
  "use strict";

  function dialog() {
    var d = document.getElementById("pcard-dialog");
    if (d) return d;
    d = document.createElement("dialog");
    d.id = "pcard-dialog";
    d.className = "pcard-dialog";
    d.setAttribute("aria-label", "Player card");
    d.addEventListener("click", function (e) {
      // a click on the backdrop (outside the card) or on the close button closes it
      if (e.target === d || e.target.closest("[data-card-close]")) d.close();
    });
    document.body.appendChild(d);
    return d;
  }

  function open(href) {
    var d = dialog();
    d.innerHTML = '<p class="pcard-loading" role="status">Loading player&hellip;</p>';
    if (!d.open) d.showModal();
    var url = href + (href.indexOf("?") >= 0 ? "&" : "?") + "card=1";
    fetch(url, { credentials: "same-origin", headers: { "HX-Request": "false" } })
      .then(function (r) {
        if (!r.ok) throw new Error("HTTP " + r.status);
        return r.text();
      })
      .then(function (html) {
        d.innerHTML = html;
        document.dispatchEvent(new CustomEvent("fm:card")); // players.js: compare button state
        var x = d.querySelector("[data-card-close]");
        if (x) x.focus();
      })
      .catch(function () {
        window.location.href = href; // fall back to the full page
      });
  }

  document.addEventListener(
    "click",
    function (e) {
      if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
      var a = e.target.closest && e.target.closest('a[href^="/player/"]');
      if (!a || a.hasAttribute("data-nocard") || a.target === "_blank") return;
      e.preventDefault();
      e.stopPropagation();
      open(a.getAttribute("href"));
    },
    true
  );
})();
