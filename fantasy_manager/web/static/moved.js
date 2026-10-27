// "Did it" retry: when the league hasn't shown a move yet, the plan page renders a hidden form
// (#moved-retry, data-retry-after seconds) that re-checks once after that delay.
(function () {
  var form = document.getElementById("moved-retry");
  if (!form || form.dataset.armed) return;
  form.dataset.armed = "1";
  var secs = parseInt(form.dataset.retryAfter || "60", 10);
  setTimeout(function () {
    if (document.body.contains(form)) form.submit();
  }, secs * 1000);
})();
