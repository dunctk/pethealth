document.addEventListener("click", async (event) => {
  const button = event.target.closest("[data-copy-target]");
  if (!button) return;
  const target = document.querySelector(button.dataset.copyTarget);
  if (!target) return;
  const value = target.value || target.textContent.trim();
  try {
    await navigator.clipboard.writeText(value);
  } catch {
    target.focus();
    target.select();
    document.execCommand("copy");
  }
  const original = button.textContent;
  button.textContent = "Copied";
  window.setTimeout(() => { button.textContent = original; }, 1600);
});

function markerKey(name) {
  const value = name.normalize("NFD").replace(/[\u0300-\u036f]/g, "").toLowerCase().trim();
  if (value.includes("sdma")) return "sdma";
  if (value === "cre" || value.includes("creat")) return "creatinina";
  if (value === "bun" || value.startsWith("bun/")) return "bun";
  if (value.includes("urea")) return "urea";
  if (value.includes("album")) return "albumina";
  if (value.includes("gluc")) return "glucosa";
  if (value.includes("colesterol")) return "colesterol";
  if (value.includes("fosfor") || value.includes("phosph")) return "fosforo";
  if (value.includes("leucoc") || value.includes("leukoc")) return "leucocitos";
  if (value.includes("hemoglob")) return "hemoglobina";
  if (value.includes("hematocrit")) return "hematocrito";
  if (value.includes("plaquet") || value.includes("platelet")) return "plaquetas";
  return value.replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "") || "other";
}

function markerLabel(key, fallback) {
  const labels = {
    sdma: "SDMA",
    creatinina: "Creatinina",
    bun: "BUN",
    urea: "Urea",
    albumina: "Albúmina",
    glucosa: "Glucosa",
    colesterol: "Colesterol",
    fosforo: "Fósforo",
    leucocitos: "Leucocitos",
    hemoglobina: "Hemoglobina",
    hematocrito: "Hematocrito",
    plaquetas: "Plaquetas",
  };
  return labels[key] || fallback;
}

function escapeHtml(value) {
  return String(value).replace(/[&<>'"]/g, (character) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", "\"": "&quot;",
  }[character]));
}

function formatNumber(value) {
  return new Intl.NumberFormat(undefined, { maximumFractionDigits: 3 }).format(value);
}

// Full precision (day included) — used for the detail header, dot tooltips,
// and the results table. `formatAxisDate` below is the shorter per-tick label
// used directly on the chart, since a chart with many readings needs to show
// every point without the labels colliding.
function formatDate(value) {
  const date = new Date(`${value}T12:00:00Z`);
  return Number.isNaN(date.getTime()) ? value : new Intl.DateTimeFormat(undefined, { day: "numeric", month: "short", year: "numeric" }).format(date);
}

function formatAxisDate(value, includeYear) {
  const date = new Date(`${value}T12:00:00Z`);
  if (Number.isNaN(date.getTime())) return value;
  const options = includeYear ? { day: "numeric", month: "short", year: "numeric" } : { day: "numeric", month: "short" };
  return new Intl.DateTimeFormat(undefined, options).format(date);
}

// Shared canvas 2D context used only to measure rendered text width so the
// chart's left axis gutter (`pad.left`) grows only as much as the tick labels
// actually need, and so date labels can be thinned by real collision instead
// of a guessed character count.
let measureCtx = null;
function measureTextWidth(text, font) {
  if (!measureCtx) measureCtx = document.createElement("canvas").getContext("2d");
  measureCtx.font = font;
  return measureCtx.measureText(text).width;
}

let cachedAxisFont = null;
function axisFont() {
  if (!cachedAxisFont) {
    const mono = getComputedStyle(document.documentElement).getPropertyValue("--mono").trim() || "monospace";
    cachedAxisFont = `9px ${mono}`;
  }
  return cachedAxisFont;
}

// Reference ranges are free-text from OCR (§5c/§6): "3.5 - 5.5", "(29,0-52,0)",
// "< 1.4", "5.50 ­ 19.50" (a soft hyphen, seen in real uploads), plain
// junk with no numbers at all, or Spanish text. This never throws — it either
// returns a low/high pair (either end may be null for an open-ended bound
// like "< 1.4") or null, and the caller skips the band on null. It does not
// try to be locale-perfect: numbers are assumed non-negative (true for every
// blood-marker unit this app renders), so a bare hyphen is read as a range
// separator, not a minus sign — the alternative (treating "20-100" as
// "20, -100") is worse.
function parseReferenceRange(raw) {
  if (!raw) return null;
  const text = String(raw).trim();
  if (!text || text === "—" || /^n\/?a$/i.test(text)) return null;
  // Two or more three-digit grouping dots ("6.500.000") are ambiguous between
  // thousands-grouping and decimals without locale metadata. Real uploads in
  // this repo contain exactly this pattern (haematocrit/platelet counts) —
  // rather than guess a magnitude, skip the band for that row entirely.
  if (/\d\.\d{3}\.\d{3}/.test(text)) return null;
  const normalized = text.replace(/(\d),(\d)/g, "$1.$2");
  const numberPattern = /\d+(?:\.\d+)?/g;
  const boundMatch = normalized.match(/(<=|>=|≤|≥|<|>)/);
  if (boundMatch) {
    const numbers = normalized.match(numberPattern);
    if (!numbers || !numbers.length) return null;
    const value = Number(numbers[0]);
    if (!Number.isFinite(value)) return null;
    return boundMatch[1].includes("<") ? { low: null, high: value } : { low: value, high: null };
  }
  const numbers = normalized.match(numberPattern);
  if (!numbers || numbers.length < 2) return null;
  let low = Number(numbers[0]);
  let high = Number(numbers[1]);
  if (!Number.isFinite(low) || !Number.isFinite(high) || low === high) return null;
  if (low > high) [low, high] = [high, low];
  return { low, high };
}

// A reference range can legitimately shift between reports (different lab,
// different assay). The most recent parseable one is the currently-relevant
// one, so scan from the latest reading backwards and use the first that
// parses — if the latest reading's range is junk, an earlier valid one is
// still better than no band at all.
function pickReferenceBand(seriesAscending) {
  for (let index = seriesAscending.length - 1; index >= 0; index -= 1) {
    const band = parseReferenceRange(seriesAscending[index].reference);
    if (band) return band;
  }
  return null;
}

function pointFlagCount(series) {
  return series.filter((point) => point.flag).length;
}

// §5d: render in pixel space. `width`/`height` are real CSS pixels supplied
// by the caller (the container's measured size), set as SVG attributes
// instead of a `viewBox` — so `font: 9px var(--mono)`, `stroke-width: 2.5`,
// and `r="5"` dots stay literally that size at any container width, rather
// than being stretched by the browser the way `viewBox` + `width:100%` would.
function renderMainChart(group, width, height) {
  const series = [...group.points].sort((left, right) => left.date.localeCompare(right.date));
  const latest = series[series.length - 1];

  // §5c.1: x is proportional to elapsed time, not reading index — three tests
  // in a week and one a year later now sit close together and far apart,
  // respectively, instead of evenly spaced. A single reading (or several on
  // the same calendar day) has no time axis to be proportional to, so it
  // falls back to centering (one point) or even spacing (same-day cluster).
  const times = series.map((point) => Date.parse(`${point.date}T12:00:00Z`));
  const validTimes = times.filter((t) => Number.isFinite(t));
  const minT = validTimes.length ? Math.min(...validTimes) : 0;
  const maxT = validTimes.length ? Math.max(...validTimes) : 0;
  const timeRange = maxT - minT;

  const band = pickReferenceBand(series);
  const values = series.map((point) => point.value);
  let min = Math.min(...values);
  let max = Math.max(...values);
  if (band) {
    if (band.low !== null) min = Math.min(min, band.low);
    if (band.high !== null) max = Math.max(max, band.high);
  }
  if (min === max) {
    const bump = Math.max(Math.abs(min) * .1, 1);
    min -= bump;
    max += bump;
  }
  const range = max - min;

  // §5c.3: gridline count scales with the rendered height instead of a fixed
  // three, so a near-fullscreen chart gets more of them.
  const gridCount = Math.max(2, Math.min(8, Math.round((height - 60) / 55)));
  const gridValues = Array.from({ length: gridCount + 1 }, (_, index) => max - (range * index) / gridCount);
  const font = axisFont();
  const gridLabelWidths = gridValues.map((value) => measureTextWidth(formatNumber(value), font));
  const padLeft = Math.max(30, Math.ceil(Math.max(0, ...gridLabelWidths)) + 14);
  const pad = { top: 20, right: 16, bottom: 34, left: padLeft };
  const plotWidth = Math.max(1, width - pad.left - pad.right);
  const plotHeight = Math.max(1, height - pad.top - pad.bottom);

  const x = (index) => {
    if (series.length === 1 || timeRange === 0 || !Number.isFinite(times[index])) {
      return pad.left + (series.length === 1 ? plotWidth / 2 : (index * plotWidth) / (series.length - 1));
    }
    return pad.left + ((times[index] - minT) / timeRange) * plotWidth;
  };
  const y = (value) => pad.top + ((max - value) / range) * plotHeight;

  const line = series.map((point, index) => `${index ? "L" : "M"}${x(index).toFixed(1)},${y(point.value).toFixed(1)}`).join(" ");
  const area = `${line} L${x(series.length - 1).toFixed(1)},${(pad.top + plotHeight).toFixed(1)} L${x(0).toFixed(1)},${(pad.top + plotHeight).toFixed(1)} Z`;

  // §5c.2: the reference range as a shaded band, when it parses. An
  // open-ended bound (only `low` or only `high`) shades to the edge of the
  // plotted area in that direction rather than guessing the missing edge.
  let bandRect = "";
  if (band) {
    const yTop = band.high !== null ? y(Math.min(band.high, max)) : pad.top;
    const yBottom = band.low !== null ? y(Math.max(band.low, min)) : pad.top + plotHeight;
    if (yBottom > yTop) {
      bandRect = `<rect class="chart-band" x="${pad.left}" y="${yTop.toFixed(1)}" width="${plotWidth.toFixed(1)}" height="${(yBottom - yTop).toFixed(1)}"/>`;
    }
  }

  const grid = gridValues.map((value) => {
    const yPosition = y(value);
    return `<line class="chart-grid" x1="${pad.left}" y1="${yPosition.toFixed(1)}" x2="${width - pad.right}" y2="${yPosition.toFixed(1)}"/><text class="chart-axis" x="${pad.left - 7}" y="${(yPosition + 3).toFixed(1)}" text-anchor="end">${escapeHtml(formatNumber(value))}</text>`;
  }).join("");

  // §5c.4: every point gets a date label; labels are thinned only when they
  // would actually collide (measured, not guessed), and the first/last are
  // always kept as anchors.
  const spansMultipleYears = new Set(series.map((point) => point.date.slice(0, 4))).size > 1;
  const dateLabels = series.map((point) => formatAxisDate(point.date, spansMultipleYears));
  const dateWidths = dateLabels.map((label) => measureTextWidth(label, font));
  const xs = series.map((_, index) => x(index));
  const shown = new Array(series.length).fill(false);
  if (series.length) {
    shown[0] = true;
    shown[series.length - 1] = true;
    const minGap = 8;
    let lastRight = xs[0] + dateWidths[0] / 2;
    const finalLeft = xs[series.length - 1] - dateWidths[series.length - 1] / 2;
    for (let index = 1; index < series.length - 1; index += 1) {
      const left = xs[index] - dateWidths[index] / 2;
      const right = xs[index] + dateWidths[index] / 2;
      if (left > lastRight + minGap && right + minGap < finalLeft) {
        shown[index] = true;
        lastRight = right;
      }
    }
  }
  const dateAxis = series.map((point, index) => {
    if (!shown[index]) return "";
    const anchor = index === 0 ? "start" : index === series.length - 1 ? "end" : "middle";
    const labelX = anchor === "start" ? pad.left : anchor === "end" ? width - pad.right : xs[index];
    return `<text class="chart-axis" x="${labelX.toFixed(1)}" y="${height - 9}" text-anchor="${anchor}">${escapeHtml(dateLabels[index])}</text>`;
  }).join("");

  // §5c.5: flagged points get an inline label (the flag text itself, e.g.
  // "H"/"ALTO"), not just a recolored dot. Placed above the dot normally, but
  // below it when the dot sits close to the top edge so the label does not
  // clip.
  const dots = series.map((point, index) => {
    const cx = xs[index];
    const cy = y(point.value);
    const dot = `<circle class="chart-dot${point.flag ? " flagged" : ""}" cx="${cx.toFixed(1)}" cy="${cy.toFixed(1)}" r="5"><title>${escapeHtml(formatDate(point.date))}: ${escapeHtml(formatNumber(point.value))}${point.unit ? ` ${escapeHtml(point.unit)}` : ""}</title></circle>`;
    if (!point.flag) return dot;
    const above = cy - pad.top > 18;
    const labelY = above ? cy - 10 : cy + 16;
    return `${dot}<text class="chart-flag" x="${cx.toFixed(1)}" y="${labelY.toFixed(1)}" text-anchor="middle">${escapeHtml(point.flag)}</text>`;
  }).join("");

  return `<svg width="${width}" height="${height}" role="img" aria-label="${escapeHtml(group.label)} over time"><defs><linearGradient id="trend-fill-${escapeHtml(group.key)}" x1="0" x2="0" y1="0" y2="1"><stop offset="0" stop-color="#62c7ff" stop-opacity=".23"/><stop offset="1" stop-color="#62c7ff" stop-opacity="0"/></linearGradient></defs>${bandRect}${grid}${dateAxis}<path class="chart-area" d="${area}" fill="url(#trend-fill-${escapeHtml(group.key)})"/><path class="chart-line" d="${line}"/>${dots}</svg>`;
}

function renderDetailHead(group) {
  const series = [...group.points].sort((left, right) => left.date.localeCompare(right.date));
  const latest = series[series.length - 1];
  const previous = series[series.length - 2];
  const delta = previous ? latest.value - previous.value : null;
  const deltaText = delta === null ? "first reading" : `${delta >= 0 ? "+" : "−"}${formatNumber(Math.abs(delta))} since previous`;
  return `<div><span class="trend-card-label">${escapeHtml(group.unit || "RESULT")}</span><h4>${escapeHtml(group.label)}</h4></div><div class="trend-card-latest"><strong>${escapeHtml(formatNumber(latest.value))}${group.unit ? ` ${escapeHtml(group.unit)}` : ""}</strong><span>${escapeHtml(formatDate(latest.date))} · ${escapeHtml(deltaText)}</span></div><div class="trend-detail-foot"><span>${series.length} reading${series.length === 1 ? "" : "s"}</span><span>${pointFlagCount(series)} flagged</span></div>`;
}

// A small, axis-free line for the filmstrip — still pixel-space (explicit
// width/height, no viewBox) for the same reason as the main chart, just at a
// size where axes and gridlines would only be noise.
function renderSparkline(group, width, height) {
  const series = [...group.points].sort((left, right) => left.date.localeCompare(right.date));
  const times = series.map((point) => Date.parse(`${point.date}T12:00:00Z`));
  const validTimes = times.filter((t) => Number.isFinite(t));
  const minT = validTimes.length ? Math.min(...validTimes) : 0;
  const maxT = validTimes.length ? Math.max(...validTimes) : 0;
  const timeRange = maxT - minT;
  const values = series.map((point) => point.value);
  let min = Math.min(...values);
  let max = Math.max(...values);
  if (min === max) { const bump = Math.max(Math.abs(min) * .1, 1); min -= bump; max += bump; }
  const range = max - min;
  const padY = 4;
  const x = (index) => (series.length === 1 || timeRange === 0 || !Number.isFinite(times[index]))
    ? (series.length === 1 ? width / 2 : 2 + (index * (width - 4)) / (series.length - 1))
    : 2 + ((times[index] - minT) / timeRange) * (width - 4);
  const y = (value) => padY + ((max - value) / range) * (height - padY * 2);
  const line = series.map((point, index) => `${index ? "L" : "M"}${x(index).toFixed(1)},${y(point.value).toFixed(1)}`).join(" ");
  const latest = series[series.length - 1];
  const anyFlagged = series.some((point) => point.flag);
  return `<svg width="${width}" height="${height}" aria-hidden="true"><path class="spark-line${anyFlagged ? " flagged" : ""}" d="${line}"/><circle class="spark-dot" cx="${x(series.length - 1).toFixed(1)}" cy="${y(latest.value).toFixed(1)}" r="2.4"/></svg>`;
}

function initLabTrends(root) {
  if (!root || root.dataset.ready === "true") return;
  root.dataset.ready = "true";
  const chart = root.querySelector("[data-trend-chart]");
  const detailHead = root.querySelector("[data-trend-detail-head]");
  const detailInner = root.querySelector("[data-trend-movable]");
  const detailHome = root.querySelector("[data-trend-detail-home]");
  const filmstrip = root.querySelector("[data-trend-filmstrip]");
  const summary = root.querySelector("[data-trend-summary]");
  const count = root.querySelector("[data-trend-count]");
  const table = root.querySelector("[data-trend-table]");
  const expand = root.querySelector("[data-trend-expand]");
  const dialog = root.querySelector("[data-trend-dialog]");
  const dialogSlot = root.querySelector("[data-trend-dialog-slot]");
  const dialogClose = root.querySelector("[data-trend-dialog-close]");
  const points = [...root.querySelectorAll("[data-lab-point]")];
  const groups = new Map();

  points.forEach((point) => {
    const value = Number(point.dataset.value);
    if (!Number.isFinite(value) || !point.dataset.date) return;
    const key = markerKey(point.dataset.test || "Other");
    if (!groups.has(key)) groups.set(key, { key, label: markerLabel(key, point.dataset.test), unit: point.dataset.unit || "", points: [] });
    groups.get(key).points.push({
      date: point.dataset.date,
      value,
      unit: point.dataset.unit || "",
      reference: point.dataset.reference || "—",
      flag: point.dataset.flag || "",
    });
  });

  const ordered = [...groups.values()].sort((left, right) => left.label.localeCompare(right.label));
  const readingCount = ordered.reduce((total, group) => total + group.points.length, 0);
  count.textContent = ordered.length ? `${ordered.length} markers` : "—";
  if (!ordered.length) {
    chart.innerHTML = '<div class="chart-empty">No dated numeric results yet. The reports are still saved below.</div>';
    summary.textContent = "";
    detailHead.innerHTML = "";
    filmstrip.innerHTML = "";
    return;
  }

  summary.innerHTML = `<strong>${readingCount}</strong><span>readings across ${ordered.length} markers</span>`;
  table.innerHTML = ordered.flatMap((group) => group.points.map((point) => ({ ...point, label: group.label }))).sort((left, right) => right.date.localeCompare(left.date)).map((point) => `<tr><td>${escapeHtml(point.label)}</td><td>${escapeHtml(formatDate(point.date))}</td><td>${escapeHtml(formatNumber(point.value))}${point.unit ? ` ${escapeHtml(point.unit)}` : ""}${point.flag ? ` <b>${escapeHtml(point.flag)}</b>` : ""}</td><td>${escapeHtml(point.reference)}</td></tr>`).join("");

  // §5b default selection: the marker with the most flagged readings — that
  // history is the one most worth seeing large by default. `ordered` is
  // already alphabetical, and this reduce keeps the first strictly-greater
  // candidate, so ties fall back to alphabetical order. Falls back to the
  // first marker alphabetically when nothing is flagged anywhere.
  const flaggedTotals = ordered.map((group) => pointFlagCount(group.points));
  let bestIndex = 0;
  flaggedTotals.forEach((total, index) => { if (total > flaggedTotals[bestIndex]) bestIndex = index; });
  let selectedKey = flaggedTotals[bestIndex] > 0 ? ordered[bestIndex].key : ordered[0].key;

  const findGroup = (key) => ordered.find((group) => group.key === key) || ordered[0];

  const renderFilmstrip = () => {
    filmstrip.innerHTML = ordered.map((group) => {
      const isActive = group.key === selectedKey;
      const series = [...group.points].sort((left, right) => left.date.localeCompare(right.date));
      const latest = series[series.length - 1];
      const flaggedCount = pointFlagCount(series);
      return `<button type="button" class="trend-spark${isActive ? " active" : ""}" data-marker-key="${escapeHtml(group.key)}" aria-pressed="${isActive}"><span class="trend-spark-label"><b>${escapeHtml(group.label)}</b>${flaggedCount ? `<i>${flaggedCount} flagged</i>` : ""}</span>${renderSparkline(group, 108, 34)}<span class="trend-spark-value">${escapeHtml(formatNumber(latest.value))}${group.unit ? ` <small>${escapeHtml(group.unit)}</small>` : ""}</span></button>`;
    }).join("");
  };

  // §5d: one renderer (`renderMainChart`), driven by a single ResizeObserver
  // on `chart`. The same node is reparented into the dialog on open and back
  // on close (see below), so this one observer and this one draw() serve
  // both the inline size and the fullscreen dialog — nothing is rendered
  // twice, and there is only ever one observer per `.lab-trends` instance.
  const draw = () => {
    const group = findGroup(selectedKey);
    detailHead.innerHTML = renderDetailHead(group);
    const box = chart.getBoundingClientRect();
    const width = Math.max(200, Math.round(box.width || 0));
    const height = Math.max(140, Math.round(Math.min(box.height || 0, window.innerHeight * .6) || 260));
    chart.innerHTML = renderMainChart(group, width, height);
  };

  let frame = null;
  const scheduleDraw = () => {
    if (frame !== null) return;
    frame = requestAnimationFrame(() => { frame = null; draw(); });
  };

  const observer = new ResizeObserver(scheduleDraw);
  observer.observe(chart);
  // Kept on the element itself so `htmx:beforeCleanupElement` (fired for
  // every element in a subtree just before htmx removes it — see the
  // document-level listener below) can find and disconnect it. `hx-swap`
  // replaces `#tab-body`'s innerHTML on every tab switch, so without this a
  // fresh `.lab-trends` node (and a fresh observer) is created each time the
  // Labs tab is revisited, while the previous one — still holding a strong
  // reference to its now-detached `chart` node — leaks.
  root._trendResizeObserver = observer;

  const selectMarker = (key) => {
    if (key === selectedKey) return;
    selectedKey = key;
    renderFilmstrip();
    draw();
  };

  filmstrip.addEventListener("click", (event) => {
    const button = event.target.closest("[data-marker-key]");
    if (!button || !filmstrip.contains(button)) return;
    selectMarker(button.dataset.markerKey);
  });

  renderFilmstrip();
  draw();

  // §5a: replace the hand-rolled `.is-expanded` overlay with a native
  // <dialog>. It brings Escape handling, focus trapping and inertness for
  // free, so there is no manual keydown listener and no body class here.
  const detailAnchor = document.createComment("trend-detail-anchor");
  detailHome.insertBefore(detailAnchor, detailInner);
  let previouslyFocused = null;

  const openDialog = () => {
    previouslyFocused = document.activeElement;
    dialogSlot.appendChild(detailInner);
    dialog.showModal();
    scheduleDraw();
  };
  const closeDialog = () => {
    detailAnchor.parentNode.insertBefore(detailInner, detailAnchor);
    scheduleDraw();
    if (previouslyFocused && document.contains(previouslyFocused) && typeof previouslyFocused.focus === "function") {
      previouslyFocused.focus();
    }
  };

  expand.addEventListener("click", openDialog);
  dialogClose.addEventListener("click", () => dialog.close());
  // Fires for Escape, the close button, and any other path that closes the
  // dialog — one place to move the chart node back home and restore focus.
  dialog.addEventListener("close", closeDialog);
}

function initAllLabTrends() {
  document.querySelectorAll("[data-lab-trends]").forEach(initLabTrends);
}

document.addEventListener("DOMContentLoaded", initAllLabTrends);
document.addEventListener("htmx:afterSwap", initAllLabTrends);

// See the comment on `root._trendResizeObserver` above: htmx fires this event
// for every element in a subtree right before removing it (a tab switch swaps
// `#tab-body`'s innerHTML), which is the one reliable moment to disconnect an
// observer bound to a node that is about to become unreachable.
document.addEventListener("htmx:beforeCleanupElement", (event) => {
  const target = event.target;
  if (!target || typeof target.matches !== "function") return;
  const roots = target.matches("[data-lab-trends]")
    ? [target]
    : [...(target.querySelectorAll ? target.querySelectorAll("[data-lab-trends]") : [])];
  roots.forEach((root) => {
    if (root._trendResizeObserver) {
      root._trendResizeObserver.disconnect();
      root._trendResizeObserver = null;
    }
  });
});

// The console tab bar (`.tab-bar`) sits outside the `#tab-body` fragment that
// `GET /app/tab/{view}` swaps in, so switching tabs would otherwise leave the
// old tab marked active. Re-derive the active tab from the request path that
// was actually fetched, rather than from location/history timing.
function highlightActiveTab(requestPath) {
  const match = /\/app\/tab\/([a-z]+)/.exec(requestPath || "");
  if (!match) return;
  document.querySelectorAll(".tab-bar .tab-link").forEach((link) => {
    const isActive = new URL(link.href, window.location.origin).searchParams.get("view") === match[1];
    link.classList.toggle("active", isActive);
    if (isActive) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  });
}

document.addEventListener("htmx:afterSwap", (event) => {
  if (event.target && event.target.id === "tab-body") {
    highlightActiveTab(event.detail && event.detail.pathInfo && event.detail.pathInfo.requestPath);
  }
});

// The type filter chips (`All · Symptoms · Meds · Weight · Labs`) on the
// Timeline tab are a client-side filter over already-rendered rows, per
// UI_REDESIGN_PLAN.md §4 Phase 2: cheaper than a round trip, and the rows
// already carry a `data-kind` attribute set server-side in
// `_timeline_days.html`. Re-run on every htmx:afterSwap (capture, undo, tab
// switch, and "Load older" all swap or append into this region) so a filter
// the visitor already chose keeps applying to rows that arrive afterward,
// instead of silently resetting or leaving new rows unfiltered.
function applyTimelineFilter(region, filter) {
  const entries = region.querySelector("#timeline-entries");
  if (!entries) return;
  [...entries.querySelectorAll(".event-row")].forEach((row) => {
    row.hidden = filter !== "all" && row.dataset.kind !== filter;
  });
  // Hide a date header once every row under it (up to the next header) is hidden.
  [...entries.querySelectorAll(".timeline-day")].forEach((day) => {
    let node = day.nextElementSibling;
    let anyVisible = false;
    while (node && !node.classList.contains("timeline-day")) {
      if (!node.hidden) anyVisible = true;
      node = node.nextElementSibling;
    }
    day.hidden = !anyVisible;
  });
}

function initTimelineFilters(region) {
  const chips = [...region.querySelectorAll(".timeline-filters .chip")];
  if (!chips.length) return;
  if (!region.dataset.filterBound) {
    region.dataset.filterBound = "true";
    region.addEventListener("click", (event) => {
      const chip = event.target.closest(".chip");
      if (!chip || !region.contains(chip)) return;
      chips.forEach((candidate) => candidate.classList.toggle("active", candidate === chip));
      applyTimelineFilter(region, chip.dataset.filter);
    });
  }
  const active = chips.find((chip) => chip.classList.contains("active"));
  applyTimelineFilter(region, active ? active.dataset.filter : "all");
}

function initAllTimelineFilters() {
  document.querySelectorAll("#timeline-region").forEach(initTimelineFilters);
}

document.addEventListener("DOMContentLoaded", initAllTimelineFilters);
document.addEventListener("htmx:afterSwap", initAllTimelineFilters);
