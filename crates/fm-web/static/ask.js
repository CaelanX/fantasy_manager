// The Ask tab: writes the answer to a chat's pending question as the model produces it.
// templates/ask.html marks the pending turn with data-ask-stream (the /ask/stream URL) and
// data-ask-done (where to go once the turn is saved).
(function () {
  "use strict";
  var source = null;

  function start() {
    var turn = document.querySelector("[data-ask-stream]");
    // both events below can fire for one page: never ask twice for the same turn
    if (turn && turn.askStarted) return;
    if (source) {
      source.close();
      source = null;
    }
    if (!turn || !window.EventSource) return;
    turn.askStarted = true;
    var status = turn.querySelector(".chat-status");
    var think = turn.querySelector(".chat-think");
    var thinkBody = turn.querySelector(".chat-think-body");
    var answer = turn.querySelector(".chat-a");
    var retry = turn.querySelector(".chat-retry");
    var es = new EventSource(turn.getAttribute("data-ask-stream"));
    source = es;

    function stop(message) {
      es.close();
      if (source === es) source = null;
      status.textContent = message;
      retry.hidden = false;
    }
    es.addEventListener("thinking", function (e) {
      think.hidden = false;
      thinkBody.textContent += JSON.parse(e.data);
      status.textContent = "Thinking…";
    });
    es.addEventListener("answer", function (e) {
      answer.hidden = false;
      answer.textContent += JSON.parse(e.data);
      status.textContent = "Writing…";
    });
    es.addEventListener("done", function () {
      es.close();
      if (source === es) source = null;
      // the saved turn, and the follow-up box, come from the server
      window.location.replace(turn.getAttribute("data-ask-done"));
    });
    es.addEventListener("fail", function (e) {
      stop(JSON.parse(e.data));
    });
    // EventSource reconnects by itself, which would ask the model again
    es.onerror = function () {
      stop("The connection to the server was lost.");
    };
  }

  document.addEventListener("htmx:afterSettle", start);
  window.addEventListener("pageshow", start);
})();
