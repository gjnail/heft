// Copy buttons on code blocks. The site works without this script.
document.querySelectorAll("pre > code").forEach((code) => {
  if (!navigator.clipboard) return;
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "copy";
  btn.textContent = "Copy";
  btn.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(code.innerText.trim());
      btn.textContent = "Copied";
    } catch {
      btn.textContent = "Press Ctrl+C";
    }
    setTimeout(() => (btn.textContent = "Copy"), 1500);
  });
  code.parentElement.appendChild(btn);
});
