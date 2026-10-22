// Dev harness: the island in a named scene for screenshots, with sample data
// for the cards. `npm run dev`, then open /dev/ui-review.html?scene=overview
// (compact, empty, approval, question, finished, error, prompt, settings,
// wardrobe, update, integration_<id>, wake) and &lang=<code>. Not shipped.
import "../src/style.css";
import { Island } from "../src/island/island";
import { State } from "../src/core/state";
import { setLanguage } from "../src/i18n/i18n";
import { Updates } from "../src/island/update";

const q = new URLSearchParams(location.search);
const scene = q.get("scene") ?? "overview";
setLanguage(q.get("lang") ?? "it");

State.settings.activeIntegrations = ["integration_github", "integration_homeassistant", "integration_whoop", "integration_calendar"];
const island = new Island(document.getElementById("root")!);
State.loadIntegrationTasks();

const now = Date.now();
function claude(state: any, steps: string[]) {
  const t = State.tasks.find((x) => x.id === "claude_code") ?? State.tasks[0];
  t.state = state; t.steps = steps; t.stepIndex = steps.length - 1;
  return t;
}
State.integrations["integration_homeassistant"] = { loaded: true, configured: true, error: null, data: { instances: [
  { slot: 1, name: "Casa", connected: true, lightsOn: 3, alarms: [{ name: "Allarme", state: "armed_away" }],
    notifications: [{ id: "n1", title: "Lavatrice", message: "Ciclo terminato" }], updates: ["Core 2026.10.3"], problems: [] },
  { slot: 2, name: "Mare", connected: false, error: "Connessione rifiutata", alarms: [], notifications: [], updates: [], problems: [] },
] } } as any;
State.integrations["integration_whoop"] = { loaded: true, configured: true, error: null, data: {
  recovery: { id: "1", score: 72, hrv: 64.2, rhr: 52, calibrating: false, at: new Date(now - 3 * 3600e3).toISOString() },
  strain: 9.8, sleep: { performance: 88, hours: 7.4, end: new Date(now - 4 * 3600e3).toISOString() },
  workout: { sport: "running", strain: 11.2, end: new Date(now - 26 * 3600e3).toISOString() } } } as any;
const S = Math.floor(now / 1000);
State.integrations["integration_calendar"] = { loaded: true, configured: true, error: null, data: { now: S, offset: 7200, calendars: 2, errors: [], events: [
  { uid: "a", title: "Standup", calendar: 1, start: S - 10 * 60, end: S + 5 * 60, allDay: false },
  { uid: "b", title: "Pranzo con Marco e il team prodotto", calendar: 2, start: S + 3 * 3600, end: S + 4 * 3600, allDay: false },
  { uid: "c", title: "Compleanno Anna", calendar: 2, start: S + 86400, end: S + 2 * 86400, allDay: true },
  { uid: "d", title: "Revisione sprint", calendar: 1, start: S + 86400 + 3600, end: S + 86400 + 7200, allDay: false },
] } } as any;

const go = (fn: () => void) => setTimeout(fn, 50);
switch (scene) {
  case "compact": claude("working", ["Leggo island.rs", "Modifico apply_geometry"]); go(() => island.reveal()); break;
  case "empty": State.tasks = []; State.focusId = null; go(() => island.expand("empty")); break;
  case "overview": claude("working", ["Leggo island.rs", "Modifico apply_geometry", "Eseguo cargo test"]); go(() => island.expand("overview")); break;
  case "approval":
    claude("approval", ["Eseguo comando"]);
    State.beginApproval({ requestId: "r", sessionId: "s", pillId: State.tasks[0].id, tool: "Bash", command: "git push -u fork feat/homeassistant && npm run build -- --mode production" } as any);
    go(() => island.alert("approval")); break;
  case "question":
    claude("question", ["Domanda"]);
    State.beginApproval({ requestId: "r", sessionId: "s", pillId: State.tasks[0].id, tool: "AskUserQuestion", command: "", questions: [{ question: "Quale approccio preferisci per la sincronizzazione?", options: [{ label: "WebSocket", description: "Tempo reale" }, { label: "Polling", description: "Ogni 30 s" }, { label: "Webhook", description: "Push" }], multiSelect: false }] } as any);
    go(() => island.alert("question")); break;
  case "finished": claude("finished", ["Fatto"]); State.tasks[0].finalLine = "Ho aggiunto il fix della barra e i test passano."; go(() => island.expand("finished")); break;
  case "error": claude("error", ["Errore"]); go(() => island.expand("error")); break;
  case "prompt":
    State.chatHistory = [{ id: 1, role: "user", content: "Come attivo la pill Home Assistant?" }, { id: 2, role: "assistant", content: "Vai in **Impostazioni → Integrazioni**, inserisci l'URL e un token a lunga durata." }] as any;
    go(() => island.expand("prompt")); break;
  case "settings": go(() => island.expand("settings")); break;
  case "wardrobe": go(() => island.expand("wardrobe")); break;
  case "update": go(() => island.expand("update")); break;
  case "update-ask": Updates.asking = true; go(() => island.expand("update")); break;
  case "update-new":
    Updates.status = { ...Updates.status, current: "0.5.1", latest: { version: "0.6.0", headline: "Home Assistant, Whoop e Calendario", pageUrl: "" }, canInstall: true } as any;
    go(() => island.expand("update")); break;
  default:
    if (scene.startsWith("integration_")) { State.setFocus(scene); go(() => island.expand("overview")); }
}
if (scene === "wake") {
  claude("working", ["Leggo island.rs", "Modifico apply_geometry"]);
  // Driven from the console or a Playwright script, one step at a time.
  Object.assign(window, {
    revealIsland: () => island.reveal(),
    expandIsland: () => island.expand("overview"),
    askUpdate: () => { Updates.asking = true; island.alert("update"); },
    announceUpdate: () => {
      Updates.status = { ...Updates.status, current: "0.5.1", latest: { version: "0.6.0", headline: "Home Assistant, Whoop e Calendario", pageUrl: "" }, canInstall: true } as any;
      island.alert("update"); island.emote("surprised");
    },
  });
}
