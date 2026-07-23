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

function formatDate(value) {
  const date = new Date(`${value}T12:00:00Z`);
  return Number.isNaN(date.getTime()) ? value : new Intl.DateTimeFormat(undefined, { month: "short", year: "numeric" }).format(date);
}

function initLabTrends(root) {
  if (!root || root.dataset.ready === "true") return;
  root.dataset.ready = "true";
  const select = root.querySelector("[data-trend-select]");
  const chart = root.querySelector("[data-trend-chart]");
  const summary = root.querySelector("[data-trend-summary]");
  const table = root.querySelector("[data-trend-table]");
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
  ordered.forEach((group) => {
    const option = document.createElement("option");
    option.value = group.key;
    option.textContent = group.unit ? `${group.label} · ${group.unit}` : group.label;
    select.append(option);
  });
  if (!ordered.length) {
    select.disabled = true;
    chart.innerHTML = '<div class="chart-empty">No dated numeric results yet. The reports are still saved below.</div>';
    summary.textContent = "";
    return;
  }

  const render = (key) => {
    const group = groups.get(key) || ordered[0];
    const series = [...group.points].sort((left, right) => left.date.localeCompare(right.date));
    const latest = series[series.length - 1];
    const previous = series[series.length - 2];
    const delta = previous ? latest.value - previous.value : null;
    const deltaText = delta === null ? "first reading" : `${delta >= 0 ? "+" : "−"}${formatNumber(Math.abs(delta))} since previous`;
    summary.innerHTML = `<strong>${formatNumber(latest.value)}${group.unit ? ` ${escapeHtml(group.unit)}` : ""}</strong><span>latest · ${escapeHtml(formatDate(latest.date))} · ${escapeHtml(deltaText)}</span>`;

    const width = 600;
    const height = 190;
    const pad = { top: 18, right: 14, bottom: 31, left: 42 };
    const values = series.map((point) => point.value);
    let min = Math.min(...values);
    let max = Math.max(...values);
    if (min === max) { min -= Math.max(Math.abs(min) * .1, 1); max += Math.max(Math.abs(max) * .1, 1); }
    const range = max - min;
    const x = (index) => pad.left + (series.length === 1 ? (width - pad.left - pad.right) / 2 : index * (width - pad.left - pad.right) / (series.length - 1));
    const y = (value) => pad.top + (max - value) * (height - pad.top - pad.bottom) / range;
    const line = series.map((point, index) => `${index ? "L" : "M"}${x(index).toFixed(1)},${y(point.value).toFixed(1)}`).join(" ");
    const area = `${line} L${x(series.length - 1).toFixed(1)},${height - pad.bottom} L${x(0).toFixed(1)},${height - pad.bottom} Z`;
    const grid = [0, .5, 1].map((step) => {
      const value = max - range * step;
      const yPosition = pad.top + (height - pad.top - pad.bottom) * step;
      return `<line class="chart-grid" x1="${pad.left}" y1="${yPosition}" x2="${width - pad.right}" y2="${yPosition}"/><text class="chart-axis" x="${pad.left - 7}" y="${yPosition + 3}" text-anchor="end">${escapeHtml(formatNumber(value))}</text>`;
    }).join("");
    const dates = series.length === 1 ? `<text class="chart-axis" x="${x(0)}" y="${height - 9}" text-anchor="middle">${escapeHtml(formatDate(series[0].date))}</text>` : `<text class="chart-axis" x="${x(0)}" y="${height - 9}" text-anchor="start">${escapeHtml(formatDate(series[0].date))}</text><text class="chart-axis" x="${x(series.length - 1)}" y="${height - 9}" text-anchor="end">${escapeHtml(formatDate(series[series.length - 1].date))}</text>`;
    const dots = series.map((point, index) => `<circle class="chart-dot${point.flag ? " flagged" : ""}" cx="${x(index)}" cy="${y(point.value)}" r="4"><title>${escapeHtml(formatDate(point.date))}: ${escapeHtml(formatNumber(point.value))}${point.unit ? ` ${escapeHtml(point.unit)}` : ""}</title></circle>`).join("");
    chart.innerHTML = `<svg viewBox="0 0 ${width} ${height}" role="img" aria-label="${escapeHtml(group.label)} over time"><defs><linearGradient id="trend-fill" x1="0" x2="0" y1="0" y2="1"><stop offset="0" stop-color="#62c7ff" stop-opacity=".23"/><stop offset="1" stop-color="#62c7ff" stop-opacity="0"/></linearGradient></defs>${grid}${dates}<path class="chart-area" d="${area}"/><path class="chart-line" d="${line}"/>${dots}</svg>`;
    table.innerHTML = [...series].reverse().map((point) => `<tr><td>${escapeHtml(formatDate(point.date))}</td><td>${escapeHtml(formatNumber(point.value))}${point.unit ? ` ${escapeHtml(point.unit)}` : ""}${point.flag ? ` <b>${escapeHtml(point.flag)}</b>` : ""}</td><td>${escapeHtml(point.reference)}</td></tr>`).join("");
  };
  select.addEventListener("change", () => render(select.value));
  render(ordered[0].key);
}

function initAllLabTrends() {
  document.querySelectorAll("[data-lab-trends]").forEach(initLabTrends);
}

document.addEventListener("DOMContentLoaded", initAllLabTrends);
document.addEventListener("htmx:afterSwap", initAllLabTrends);
