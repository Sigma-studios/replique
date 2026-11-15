// Réplique: live updates over Server-Sent Events, charts with uPlot, small helpers.
// Pages stay server-rendered; this only refreshes fragments (through htmx) and charts.
(function () {
  "use strict";

  // Live events. Fragments that follow them declare
  // hx-trigger="live:runs from:body" (or live:devices, live:problems, live:measured).
  function connect() {
    if (!document.body.dataset.live || !window.EventSource) return;
    var source = new EventSource("/events");
    ["runs", "devices", "problems", "measured"].forEach(function (name) {
      source.addEventListener(name, function (e) {
        var detail = {};
        try { detail = JSON.parse(e.data); } catch (_) {}
        if (window.htmx) htmx.trigger(document.body, "live:" + name, detail);
        document.dispatchEvent(new CustomEvent("replique:" + name, { detail: detail }));
      });
    });
    // EventSource reconnects by itself; nothing to do on error.
  }

  // Every form that posts says so while the server works, and cannot be sent
  // twice. Several take a while: each sub-account change is a Hetzner action of
  // ten seconds or more, and a click with no answer invites a second one. A
  // form's data-busy (or its button's) names what is happening; "Working…"
  // otherwise.
  document.addEventListener("submit", function (e) {
    var form = e.target;
    if (!form.matches || !form.matches("form") || (form.method || "").toLowerCase() !== "post") return;
    // A confirm() declined in the form's own onsubmit.
    if (e.defaultPrevented) return;
    if (form.dataset.sent) { e.preventDefault(); return; }
    form.dataset.sent = "1";
    var button = e.submitter || form.querySelector("button[type=submit], button:not([type])");
    var label = (button && button.dataset.busy) || form.dataset.busy || "Working…";
    document.body.classList.add("busy");
    // After this tick, so the browser still sends the form (a disabled
    // submitter would drop out of it).
    setTimeout(function () {
      form.querySelectorAll("button").forEach(function (b) { b.disabled = true; });
      if (button) {
        button.dataset.label = button.innerHTML;
        button.classList.add("working");
        button.innerHTML = '<span class="spinner" aria-hidden="true"></span>';
        button.appendChild(document.createTextNode(label));
      }
    }, 0);
  });

  // Back to a page kept in the browser's cache: undo the waiting state.
  window.addEventListener("pageshow", function (e) {
    if (!e.persisted) return;
    document.body.classList.remove("busy");
    document.querySelectorAll("form[data-sent]").forEach(function (form) {
      delete form.dataset.sent;
      form.querySelectorAll("button").forEach(function (b) {
        b.disabled = false;
        if (b.dataset.label !== undefined) { b.innerHTML = b.dataset.label; b.classList.remove("working"); }
      });
    });
  });

  // Copy buttons: <button data-copy="text">.
  document.addEventListener("click", function (e) {
    var b = e.target.closest("[data-copy]");
    if (!b || !navigator.clipboard) return;
    navigator.clipboard.writeText(b.dataset.copy).then(function () {
      var old = b.textContent;
      b.textContent = "Copied";
      setTimeout(function () { b.textContent = old; }, 1200);
    });
  });

  // ---------------------------------------------------------------- charts

  function css(name) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  }

  var UNITS = ["B", "KB", "MB", "GB", "TB", "PB"];
  function bytes(v) {
    if (v == null) return "–";
    var i = 0;
    while (Math.abs(v) >= 1000 && i < UNITS.length - 1) { v /= 1000; i++; }
    return (i === 0 ? v.toFixed(0) : v.toFixed(v >= 100 ? 0 : 1)) + " " + UNITS[i];
  }

  function draw(el, data) {
    var stacked = !!data.stacked;
    var n = data.series.length;
    // Stacked areas are cumulative sums drawn top band first, so each band
    // paints over the one above it; the legend still shows each own value.
    var order = [];
    for (var i = 0; i < n; i++) order.push(i);
    if (stacked) order.reverse();
    var cumulative = data.series.map(function () { return []; });
    if (stacked) {
      for (var j = 0; j < data.x.length; j++) {
        var sum = 0;
        for (var s = 0; s < n; s++) {
          var v = data.series[s].values[j];
          sum += v == null ? 0 : v;
          cumulative[s][j] = sum;
        }
      }
    }
    var surface = css("--surface");
    var series = [{}];
    var columns = [data.x];
    order.forEach(function (s) {
      var info = data.series[s];
      var color = css("--series-" + ((info.slot % 8) + 1));
      series.push({
        label: info.label,
        stroke: stacked ? surface : color,
        width: 2,
        fill: stacked ? color : color + "22",
        points: { show: false },
        value: function (u, v, si, idx) { return idx == null ? "–" : bytes(info.values[idx]); },
      });
      columns.push(stacked ? cumulative[s] : info.values);
    });
    var axis = {
      stroke: css("--text-2"),
      grid: { stroke: css("--grid"), width: 1 },
      ticks: { stroke: css("--grid"), width: 1 },
    };
    var opts = {
      width: el.clientWidth || 600,
      height: 260,
      series: series,
      scales: { y: { range: function (u, min, max) { return [0, max > 0 ? max * 1.08 : 1]; } } },
      axes: [
        axis,
        Object.assign({}, axis, { size: 64, values: function (u, ticks) { return ticks.map(bytes); } }),
      ],
      cursor: { points: { size: 8 } },
      legend: { live: true },
      // The footer says times are UTC; the axes agree.
      tzDate: function (ts) { return uPlot.tzDate(new Date(ts * 1e3), "Etc/UTC"); },
    };
    el.innerHTML = "";
    if (!data.x.length) {
      el.innerHTML = '<p class="muted">No measurements yet.</p>';
      return null;
    }
    return new uPlot(opts, columns, el);
  }

  function setupChart(el) {
    var plot = null;
    var pending = null;
    function load() {
      fetch(el.dataset.src, { headers: { Accept: "application/json" } })
        .then(function (r) { return r.json(); })
        .then(function (data) {
          if (plot) plot.destroy();
          plot = draw(el, data);
        })
        .catch(function () {});
    }
    load();
    el.addEventListener("replique:reload", load);
    var scopes = (el.dataset.scopes || "").split(" ");
    document.addEventListener("replique:measured", function (e) {
      var scope = e.detail && e.detail.scope;
      var wanted = scopes.indexOf(scope) >= 0 || (scopes.indexOf("repo:*") >= 0 && /^repo:/.test(scope || ""));
      if (!wanted) return;
      // Several repositories are measured within seconds: one reload for all.
      clearTimeout(pending);
      pending = setTimeout(load, 1500);
    });
    var resize = null;
    window.addEventListener("resize", function () {
      clearTimeout(resize);
      resize = setTimeout(function () { if (plot) plot.setSize({ width: el.clientWidth, height: 260 }); }, 150);
    });
  }

  // Range buttons (24h, 7d…) next to a chart: switch it in place, keep the
  // choice in the address for a reload or a shared link.
  document.addEventListener("click", function (e) {
    var a = e.target.closest(".ranges[data-chart] a[data-range]");
    if (!a) return;
    var chart = document.getElementById(a.parentNode.dataset.chart);
    if (!chart || !window.uPlot) return;
    e.preventDefault();
    chart.dataset.src = chart.dataset.src.replace(/range=[^&]*/, "range=" + a.dataset.range);
    a.parentNode.querySelectorAll("a").forEach(function (o) { o.classList.toggle("active", o === a); });
    history.replaceState(null, "", "?range=" + a.dataset.range + location.hash);
    chart.dispatchEvent(new Event("replique:reload"));
  });

  function init() {
    connect();
    if (window.uPlot) document.querySelectorAll(".chart[data-src]").forEach(setupChart);
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init);
  else init();
})();
