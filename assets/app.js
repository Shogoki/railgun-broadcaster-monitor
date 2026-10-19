// railgun-broadcaster-monitor – live status + uPlot charts. No build step.
(function () {
  "use strict";

  // ---- live indicator (htmx SSE extension events)
  function setLive(state, text) {
    var el = document.getElementById("live");
    if (!el) return;
    el.className = "live " + state;
    el.textContent = text;
  }
  document.addEventListener("htmx:sseOpen", function () { setLive("on", "live"); });
  document.addEventListener("htmx:sseError", function () { setLive("off", "reconnecting…"); });

  // ---- charts
  var charts = [];

  function css(name) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  }

  function fmtUsd(v) {
    if (v == null || !isFinite(v)) return "–";
    var a = Math.abs(v), s = v < 0 ? "−$" : "$";
    if (a >= 100) return s + a.toFixed(0);
    if (a >= 0.01) return s + a.toFixed(2);
    if (a === 0) return "$0";
    return s + a.toPrecision(2);
  }

  function fmtDate(ts) {
    var d = new Date(ts * 1000);
    return d.toISOString().slice(0, 10);
  }

  // Tooltip: values lead, names follow; line keys; every series at the hovered x.
  function tooltipPlugin(fmtVal) {
    var tt;
    return {
      hooks: {
        init: function (u) {
          tt = document.createElement("div");
          tt.className = "tt";
          u.over.appendChild(tt);
          u.over.addEventListener("mouseleave", function () { tt.style.display = "none"; });
        },
        setCursor: function (u) {
          var i = u.cursor.idx;
          if (i == null) { tt.style.display = "none"; return; }
          tt.textContent = "";
          var x = document.createElement("div");
          x.className = "x";
          x.textContent = fmtDate(u.data[0][i]);
          tt.appendChild(x);
          for (var s = 1; s < u.series.length; s++) {
            var ser = u.series[s];
            if (!ser.show) continue;
            var row = document.createElement("div");
            row.className = "row";
            var key = document.createElement("span");
            key.className = "key";
            key.style.background = ser._color;
            var val = document.createElement("span");
            val.className = "val";
            val.textContent = fmtVal(u.data[s][i]);
            var name = document.createElement("span");
            name.className = "name";
            name.textContent = ser.label;
            row.appendChild(key); row.appendChild(val); row.appendChild(name);
            tt.appendChild(row);
          }
          var left = u.cursor.left, top = u.cursor.top;
          tt.style.display = "block";
          var w = tt.offsetWidth, ow = u.over.clientWidth;
          tt.style.left = Math.min(Math.max(left + 12, 0), ow - w) + "px";
          tt.style.top = Math.max(top - tt.offsetHeight - 8, 0) + "px";
        },
      },
    };
  }

  function axes(fmtY) {
    var ink = css("--text-muted"), grid = css("--grid"), axis = css("--axis");
    return [
      { stroke: ink, grid: { show: false }, ticks: { stroke: axis, width: 1 }, values: function (u, vals) { return vals.map(function (v) { return fmtDate(v).slice(5); }); } },
      { stroke: ink, grid: { stroke: grid, width: 1 }, ticks: { show: false }, size: 64, values: function (u, vals) { return vals.map(fmtY); } },
    ];
  }

  function make(el, opts, data) {
    var width = el.clientWidth || 600;
    opts.width = width;
    opts.height = 240;
    var u = new uPlot(opts, data, el);
    charts.push({ u: u, el: el });
    return u;
  }

  function render(root) {
    charts.forEach(function (c) { c.u.destroy(); });
    charts = [];
    var dataEl = (root || document).querySelector("#chart-data");
    if (!dataEl || typeof uPlot === "undefined") return;
    var d = JSON.parse(dataEl.textContent);
    var surface = css("--surface-1");

    document.querySelectorAll(".plot[data-chart]").forEach(function (el) {
      el.textContent = "";
      var kind = el.getAttribute("data-chart");
      if (kind === "profit" || kind === "count") {
        var isUsd = kind === "profit";
        var color = css("--series-1");
        var bars = uPlot.paths.bars({ size: [0.6, 24], radius: 0.25, gap: 2 });
        make(el, {
          legend: { show: false },
          cursor: { y: false, points: { show: false } },
          scales: { x: { time: true }, y: { range: function (u, min, max) { return [Math.min(0, min), Math.max(0, max) || 1]; } } },
          axes: axes(isUsd ? fmtUsd : function (v) { return String(v); }),
          series: [
            {},
            { label: d[kind].label, _color: color, stroke: color, fill: color, width: 0, paths: bars, points: { show: false }, fillTo: 0 },
          ],
          plugins: [tooltipPlugin(isUsd ? fmtUsd : function (v) { return v == null ? "–" : String(v); })],
        }, [d.x, d[kind].values]);
      } else if (kind === "cumulative") {
        var series = [{}];
        var data = [d.x];
        d.cumulative.forEach(function (s) {
          var c = css("--series-" + s.slot);
          series.push({ label: s.label, _color: c, stroke: c, width: 2, points: { show: d.x.length <= 14, size: 8, stroke: surface, width: 2, fill: c } });
          data.push(s.values);
        });
        make(el, {
          legend: { show: true, live: false },
          cursor: { y: false, points: { size: 8 } },
          scales: { x: { time: true } },
          axes: axes(fmtUsd),
          series: series,
          plugins: [tooltipPlugin(fmtUsd)],
        }, data);
      }
    });
  }

  var ro = new ResizeObserver(function () {
    charts.forEach(function (c) { c.u.setSize({ width: c.el.clientWidth, height: 240 }); });
  });

  function init(root) {
    render(root);
    document.querySelectorAll(".plot[data-chart]").forEach(function (el) { ro.observe(el); });
  }

  document.addEventListener("DOMContentLoaded", function () { init(document); });
  document.addEventListener("htmx:afterSettle", function () { init(document); });
  if (window.matchMedia) {
    window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", function () { init(document); });
  }
})();
