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

  function init() {
    connect();
    if (window.uPlot) document.querySelectorAll(".chart[data-src]").forEach(setupChart);
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init);
  else init();
})();
