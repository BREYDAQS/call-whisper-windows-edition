// Wspólne dla okna głównego, nakładki i paska u góry.

function escapeHtml(text) {
  return text.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
}

// Odpowiedzi modelu są w Markdownie: pogrubienia, kod, listy, akapity.
function markdown(text) {
  const inline = (s) => escapeHtml(s)
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>")
    .replace(/\*([^*]+)\*/g, "<i>$1</i>");
  return text.split(/\n{2,}/).map((block) => {
    const lines = block.split("\n");
    if (lines.every((l) => /^\s*[-*•]\s+/.test(l))) {
      return "<ul>" + lines.map((l) => `<li>${inline(l.replace(/^\s*[-*•]\s+/, ""))}</li>`).join("") + "</ul>";
    }
    return `<p>${lines.map(inline).join("<br>")}</p>`;
  }).join("");
}

// Karta pytania i odpowiedzi. Przy oczekiwaniu licznik sekund: przez most
// do Claude Code odpowiedź potrafi iść 5 s i bez licznika wygląda to na zawieszenie.
// Licznik przy oczekujących: zmieniamy sam tekst, zamiast przebudowywać
// listę co pół sekundy (to restartowało też animację paska).
function tickPending(root) {
  for (const el of root.querySelectorAll("[data-since]")) {
    el.textContent = `${Math.max(0, Math.round((Date.now() - Number(el.dataset.since)) / 1000))} s`;
  }
}

function answerCard(item) {
  const el = document.createElement("div");
  el.className = "answer";
  const icon = item.hasImage ? "🖼" : item.auto ? "✨" : "🗣";
  let body;
  if (item.status === "pending") {
    const secs = Math.max(0, Math.round((Date.now() - item.at) / 1000));
    body = `<div style="display:flex;gap:8px;align-items:center"><div class="progress"></div><small data-since="${item.at}">${secs} s</small></div>`;
  } else if (item.status === "error") {
    body = `<div class="err">${escapeHtml(item.error ?? "Błąd")}</div>`;
  } else {
    body = `<div class="a">${markdown(item.answer)}</div>`;
  }
  el.innerHTML = `<div class="q"><span>${icon}</span><span>${escapeHtml(item.question)}</span></div>${body}
    ${item.ttftMs != null ? `<small>pierwszy token: ${Math.round(item.ttftMs)} ms</small>` : ""}`;
  return el;
}
