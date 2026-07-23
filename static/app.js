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
