// Player tools: the watch star (POST /watch with fetch, updated in place; the plain form is the
// fallback) and the compare tray (up to 4 players per league in localStorage, a fixed tray that
// links to /compare). Event delegation, so it works after htmx swaps and inside the player card.
(function () {
  "use strict";

  var MAX = 4;

  // ------------------------------------------------------------------ watch star

  function setWatched(cid, on) {
    document.querySelectorAll("form[data-watch]").forEach(function (f) {
      if (f.elements.cid.value !== cid) return;
      f.elements.on.value = on ? "0" : "1";
      var b = f.querySelector(".watch-btn");
      if (!b) return;
      b.classList.toggle("is-on", on);
      if (b.hasAttribute("aria-pressed")) b.setAttribute("aria-pressed", on ? "true" : "false");
      var t = b.querySelector(".w-txt");
      if (t) t.textContent = on ? b.dataset.onText : b.dataset.offText;
    });
  }

  function refreshWatchlist() {
    if (!document.getElementById("watchlist") || !window.htmx) return;
    window.htmx.ajax("GET", window.location.href, { target: "#watchlist", select: "#watchlist", swap: "outerHTML" });
  }

  document.addEventListener(
    "submit",
    function (e) {
      var f = e.target;
      if (!f.matches || !f.matches("form[data-watch]")) return;
      e.preventDefault();
      e.stopPropagation();
      var data = new URLSearchParams(new FormData(f));
      data.set("format", "json");
      // show the new state right away; the server answer confirms it
      setWatched(data.get("cid"), data.get("on") !== "0");
      fetch(f.action, { method: "POST", body: data, credentials: "same-origin" })
        .then(function (r) {
          if (!r.ok) throw new Error("HTTP " + r.status);
          return r.json();
        })
        .then(function (j) {
          setWatched(j.cid, j.on);
          refreshWatchlist();
        })
        .catch(function () {
          setWatched(data.get("cid"), data.get("on") === "0"); // undo, then the plain form post
          f.submit();
        });
    },
    true
  );

  // ------------------------------------------------------------------ compare tray

  function pageLeague() {
    var a = document.querySelector(".leagues a[aria-current]");
    var q = a ? a.getAttribute("href").split("?")[1] : window.location.search.slice(1);
    return new URLSearchParams(q || "").get("league") || "espn";
  }

  function key(league) {
    return "fm-compare-" + league;
  }

  function load(league) {
    try {
      var v = JSON.parse(window.localStorage.getItem(key(league)) || "[]");
      return Array.isArray(v) ? v.slice(0, MAX) : [];
    } catch (err) {
      return [];
    }
  }

  function save(league, list) {
    try {
      window.localStorage.setItem(key(league), JSON.stringify(list.slice(0, MAX)));
    } catch (err) {
      /* storage full or blocked: the tray just doesn't persist */
    }
  }

  function has(list, cid) {
    return list.some(function (x) {
      return x.cid === cid;
    });
  }

  function compareHref(league, list) {
    return "/compare?league=" + encodeURIComponent(league) + list.map(function (x) {
      return "&p=" + encodeURIComponent(x.cid);
    }).join("");
  }

  function lastName(name) {
    var parts = name.split(" ");
    return parts.length > 1 ? parts.slice(1).join(" ") : name;
  }

  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text) n.textContent = text;
    return n;
  }

  function renderTray(league, list) {
    var old = document.getElementById("cmp-tray");
    if (old) old.remove();
    var onComparePage = !!document.getElementById("cmp-page");
    document.body.classList.toggle("has-tray", list.length > 0 && !onComparePage);
    if (!list.length || onComparePage) return;
    var tray = el("aside", "cmp-tray");
    tray.id = "cmp-tray";
    tray.setAttribute("aria-label", "Players to compare");
    var ul = el("ul", "cmp-chips");
    list.forEach(function (x) {
      var li = el("li", "cmp-chip");
      var shared = list.filter(function (y) {
        return lastName(y.name) === lastName(x.name);
      }).length > 1;
      li.appendChild(el("span", "", shared ? x.name : lastName(x.name)));
      var rm = el("button", "cmp-chip-x", "×");
      rm.type = "button";
      rm.dataset.trayRemove = x.cid;
      rm.setAttribute("aria-label", "Remove " + x.name + " from compare");
      li.appendChild(rm);
      ul.appendChild(li);
    });
    tray.appendChild(ul);
    var go = el("a", "btn cmp-go", "Compare (" + list.length + ")");
    go.href = compareHref(league, list);
    tray.appendChild(go);
    var clear = el("button", "cmp-clear", "Clear");
    clear.type = "button";
    clear.dataset.trayClear = "1";
    tray.appendChild(clear);
    document.body.appendChild(tray);
    if (window.htmx) window.htmx.process(tray);
  }

  function syncButtons(league, list) {
    document.querySelectorAll("[data-compare]").forEach(function (b) {
      var lg = b.dataset.league || league;
      var l = lg === league ? list : load(lg);
      var on = has(l, b.dataset.compare);
      b.hidden = false;
      b.setAttribute("aria-pressed", on ? "true" : "false");
      b.textContent = on ? "✓ Compare" : "+ Compare";
      b.disabled = !on && l.length >= MAX;
      b.title = b.disabled ? "Compare holds " + MAX + " players: remove one first" : "";
    });
  }

  function sync() {
    var league = pageLeague();
    var page = document.getElementById("cmp-page");
    if (page) {
      // the compare page's columns are the tray
      var cols = Array.prototype.map.call(page.querySelectorAll("[data-compare-col]"), function (c) {
        return { cid: c.dataset.cid, name: c.dataset.name };
      });
      save(league, cols);
    }
    var list = load(league);
    renderTray(league, list);
    syncButtons(league, list);
  }

  document.addEventListener("click", function (e) {
    var t = e.target.closest && e.target.closest("[data-compare], [data-tray-remove], [data-tray-clear]");
    if (!t) return;
    var league = t.dataset.league || pageLeague();
    var list = load(league);
    if (t.dataset.compare) {
      if (has(list, t.dataset.compare)) {
        list = list.filter(function (x) {
          return x.cid !== t.dataset.compare;
        });
      } else if (list.length < MAX) {
        list.push({ cid: t.dataset.compare, name: t.dataset.name });
      }
    } else if (t.dataset.trayRemove) {
      list = list.filter(function (x) {
        return x.cid !== t.dataset.trayRemove;
      });
    } else {
      list = [];
    }
    save(league, list);
    sync();
  });

  document.addEventListener("htmx:afterSettle", sync);
  document.addEventListener("fm:card", sync);
  window.addEventListener("storage", sync);
  window.addEventListener("pageshow", sync);
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", sync);
  else sync();
})();
