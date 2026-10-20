// Integration cards shown in the overview's left card — DOM ports of
// IntegrationCardView and friends from IslandViewContent.swift.
//
// Cal.com is the one simplification: macOS shows a three-level calendar
// (month → day → booking); here it is the list of upcoming bookings.

import { h, svg, clear, dot } from "./dom";
import { ICONS } from "./icons";
import { State, type AgentTask } from "../core/state";
import { Bridge } from "../core/bridge";
import { isComingSoon, pillDefinition } from "../core/pills";
import { refreshHookPills } from "../island/integrations";
import { readActivity, readPulse, readStats } from "../core/github";
import { githubDetail, githubPulseCard } from "./github";
import { N_, language, t } from "../i18n/i18n";

/** Same shape as the Swift `timeAgo` computed properties. */
export function timeAgo(value: unknown): string {
  const date = typeof value === "number" ? new Date(value) : new Date(String(value));
  const diff = (Date.now() - date.getTime()) / 1000;
  if (!Number.isFinite(diff)) return "";
  if (diff < 60) return t("just now");
  if (diff < 3600) return t("{n}m", { n: Math.floor(diff / 60) });
  if (diff < 86400) return t("{n}h", { n: Math.floor(diff / 3600) });
  return t("{n}d", { n: Math.floor(diff / 86400) });
}

function header(color: string, name: string, kind: string, extra?: Node): HTMLElement {
  const row = h("div", { class: "int-head" }, dot(color, 7), h("b", { text: name }), h("span", { text: kind }));
  if (extra) row.append(extra);
  return row;
}

/** Highlighted first row + plain rows, the layout every list card shares. */
function listRow(accent: string, first: boolean, ...children: Node[]): HTMLElement {
  const row = h("div", { class: first ? "int-row first" : "int-row" }, dot(accent, 5), ...children);
  if (first) row.style.background = `${accent}14`;
  return row;
}

function get(id: string): Record<string, unknown> {
  return (State.integrations[id]?.data ?? {}) as Record<string, unknown>;
}

function arr(id: string, key: string): Record<string, unknown>[] {
  const v = get(id)[key];
  return Array.isArray(v) ? (v as Record<string, unknown>[]) : [];
}

// ── Not configured / idle ─────────────────────────────────────────────────────

const OPEN_URLS: Record<string, string> = {
  integration_resend: "https://resend.com/emails",
  integration_vercel: "https://vercel.com/dashboard",
  integration_github: "https://github.com",
  integration_stripe: "https://dashboard.stripe.com/payments",
  integration_notion: "https://notion.so",
  integration_calcom: "https://app.cal.com/bookings",
  integration_whoop: "https://app.whoop.com",
};

/** IntegrationCardView.statusLabel on macOS. */
export function idleStatus(
  id: string,
  info: { configured: boolean; error: string | null } | undefined,
  chatModel: string,
): { label: string; color: string } {
  if (isComingSoon(id)) return { label: t("Coming soon"), color: "#6B7079" };
  if (info?.error) return { label: info.error, color: "#F4505E" };
  const configured = info?.configured ?? false;
  const def = pillDefinition(id);
  const ok = (label: string) => ({ label, color: "#22C55E" });
  const missing = (label: string) => ({ label, color: "#F4505E" });
  // Pills driven by hooks never have a key: they are connected once the hooks
  // are in place (Mac #183). A session replaces this card; nothing is loading.
  if (def?.connect.kind === "hooks") return configured ? ok(t("Hooks installed")) : missing(t("Hooks not installed"));
  if (def?.connect.kind === "none") return ok(t("Ready · no setup needed"));
  if (def?.connect.kind === "server") return configured ? ok(t("Connected")) : missing(t("Not connected"));
  if (def?.category === "ai") {
    if (!configured) return missing(t("Key not configured"));
    return ok(id === "ai_anthropic" ? t("Key configured · {model}", { model: chatModel }) : t("Key configured"));
  }
  return configured ? ok(t("Connected · loading…")) : missing(t("Key not configured"));
}

function idleCard(task: AgentTask, openSettings: () => void): HTMLElement {
  const info = State.integrations[task.id];
  const configured = info?.configured ?? false;
  const def = pillDefinition(task.id);
  const status = idleStatus(task.id, info, State.settings.model);

  const actions = h("div", { class: "int-actions" });
  if (task.id === "integration_claude") {
    actions.append(
      h("button", {
        class: "link-btn",
        style: `color:${task.color}b3`,
        text: t("Open Visual Studio Code"),
        onclick: () => void Bridge.openInVSCode(task.sessionCwd ?? null),
      }),
    );
  } else if (task.id === "agent_claude-desktop") {
    actions.append(
      h("button", {
        class: "link-btn",
        style: `color:${task.color}d9`,
        text: t("Open Claude"),
        onclick: () => void Bridge.openClaudeDesktop(),
      }),
    );
  } else if (task.id === "integration_n8n") {
    actions.append(
      h("button", {
        class: "link-btn",
        style: `color:${task.color}d9`,
        text: t("Open {name}", { name: "n8n" }),
        onclick: () => void Bridge.openN8n(),
      }),
    );
  } else if (task.id === "integration_homeassistant") {
    actions.append(
      h("button", {
        class: "link-btn",
        style: `color:${task.color}d9`,
        text: t("Open {name}", { name: "Home Assistant" }),
        onclick: () => void Bridge.openHomeAssistant(1),
      }),
    );
  } else if (task.id === "integration_whoop" && configured) {
    const connect = h("button", { class: "link-btn", style: `color:${task.color}d9`, text: t("Connect Whoop") });
    connect.addEventListener("click", async () => {
      connect.setAttribute("disabled", "");
      connect.textContent = t("Waiting for Whoop in the browser…");
      try {
        await Bridge.whoopConnect();
      } catch (err) {
        connect.textContent = String(err);
        connect.removeAttribute("disabled");
      }
    });
    actions.append(connect);
  } else if (OPEN_URLS[task.id]) {
    actions.append(
      h("button", {
        class: "link-btn",
        style: `color:${task.color}d9`,
        text: t("Open {name}", { name: task.name }),
        onclick: () => void Bridge.openUrl(OPEN_URLS[task.id]),
      }),
    );
  }
  const hookPill = def?.connect.kind === "hooks";
  if (isComingSoon(task.id) || def?.connect.kind === "none") {
    // Nothing to set up, and nothing to refresh.
  } else if (configured && (hookPill || def?.category !== "ai")) {
    actions.append(
      h("button", {
        class: "link-btn",
        style: `color:${task.color}d9`,
        text: t("Refresh"),
        // A hook pill has nothing to poll: look at its hooks again instead.
        onclick: () => void (hookPill ? refreshHookPills() : Bridge.refreshIntegration(task.id)),
      }),
    );
  } else if (!configured) {
    actions.append(
      h("button", { class: "link-btn", style: "color:#8e939c", text: t("Settings…"), onclick: openSettings }),
    );
  }

  return h(
    "div",
    { class: "int-card" },
    header(
      task.color,
      task.id === "integration_claude" ? def!.name : task.name,
      t(def?.subtitle ?? N_("Integration")),
    ),
    h("div", { class: "int-status" }, dot(status.color, 5), h("span", { text: status.label })),
    actions,
  );
}

// ── Vercel ────────────────────────────────────────────────────────────────────

function vercelCard(onDetail: () => void): HTMLElement {
  const deployments = arr("integration_vercel", "deployments");
  const rows = h("div", { class: "int-rows" });
  deployments.slice(0, 3).forEach((d, i) => {
    const accent = d.state === "READY" ? "#22C55E" : "#F4505E";
    const name = h("span", { class: "int-name", text: String(d.projectName ?? "") });
    const ago = h("span", { class: "int-ago", text: timeAgo(d.createdAt) });
    if (i === 0) {
      const more = h(
        "button",
        { class: "int-more", title: t("Details"), onclick: onDetail },
        svg(ICONS.ellipsis, 8),
      );
      rows.append(listRow(accent, true, name, ago, more));
    } else {
      rows.append(listRow(accent, false, name, ago));
    }
  });
  return h("div", { class: "int-card" }, header("#7C5CFF", "Vercel", t("Deployments")), rows);
}

function vercelDetail(onBack: () => void): HTMLElement {
  const d = arr("integration_vercel", "deployments")[0] ?? {};
  const success = d.state === "READY";
  const accent = success ? "#22C55E" : "#F4505E";
  const status = success ? t("Ready") : d.state === "CANCELED" ? t("Canceled") : t("Error");
  const body = h("div", { class: "int-detail-body" });
  if (d.commitMessage) body.append(h("div", { class: "int-commit", text: String(d.commitMessage) }));
  const meta = h("div", { class: "int-meta" });
  if (d.branch) meta.append(h("span", { text: String(d.branch) }));
  meta.append(h("span", { text: t("{time} ago", { time: timeAgo(d.createdAt) }) }));
  body.append(meta);
  if (d.url) {
    body.append(
      h("button", {
        class: "int-link",
        text: String(d.url),
        onclick: () => void Bridge.openUrl(`https://${d.url}`),
      }),
    );
  }
  return h(
    "div",
    { class: "int-card detail" },
    h(
      "div",
      { class: "int-detail-head" },
      h("button", { class: "int-back", onclick: onBack }, svg(ICONS.chevronLeft, 10, { stroke: 2.4 })),
      dot(accent, 6),
      h("b", { text: String(d.projectName ?? t("Deployment")) }),
      h("span", { class: "int-badge", style: `color:${accent};background:${accent}24`, text: status }),
    ),
    body,
  );
}

// ── Resend ────────────────────────────────────────────────────────────────────

function resendCard(): HTMLElement {
  const emails = arr("integration_resend", "emails");
  const total = get("integration_resend").total;
  const extra =
    total != null
      ? h("span", { class: "int-total" }, h("i", { class: "pulse" }), h("span", { text: String(total) }))
      : undefined;
  const rows = h("div", { class: "int-rows" });
  emails.slice(0, 3).forEach((e, i) => {
    const delivered = e.lastEvent === "delivered";
    const accent = delivered ? "#22C55E" : "#F4505E";
    const to = Array.isArray(e.to) ? String(e.to[0] ?? "?") : "?";
    const short = to.split("@")[0];
    const cells: Node[] = [
      h("span", { class: "int-name", text: short }),
      h("span", { class: "int-ago", text: timeAgo(e.createdAt) }),
    ];
    if (i === 0 && e.subject) cells.push(h("span", { class: "int-sub", text: String(e.subject) }));
    rows.append(listRow(accent, i === 0, ...cells));
  });
  return h("div", { class: "int-card" }, header("#22C55E", "Resend", t("Emails"), extra), rows);
}

// ── GitHub ────────────────────────────────────────────────────────────────────

function statRow(icon: string, color: string, label: string, value: string): HTMLElement {
  return h(
    "div",
    { class: "int-stat" },
    h("i", { class: "int-stat-icon", style: `color:${color}` }, svg(icon, 10)),
    h("span", { class: "int-stat-label", text: label }),
    h("span", { class: "int-stat-value", text: value }),
  );
}

function githubCard(): HTMLElement {
  const d = get("integration_github");
  const stars = Number(d.totalStars ?? 0);
  const repos = Number(d.totalRepos ?? 0);
  const fmt = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n));
  return h(
    "div",
    { class: "int-card" },
    header("#F4505E", "GitHub", t("Overview")),
    h(
      "div",
      { class: "int-stats" },
      statRow(ICONS.star, "#F5A524", t("Total stars"), fmt(stars)),
      statRow(ICONS.stack, "#6B7079", t("Repositories"), String(repos)),
    ),
  );
}

// ── Stripe ────────────────────────────────────────────────────────────────────

function stripeCard(): HTMLElement {
  const d = get("integration_stripe");
  const balance = (Number(d.balance ?? 0) / 100).toFixed(2);
  const currency = String(d.currency ?? "eur").toUpperCase();
  const rows = h("div", { class: "int-rows tight" });
  for (const p of arr("integration_stripe", "payments")) {
    const success = p.status === "succeeded";
    const accent = success ? "#22C55E" : "#F4505E";
    rows.append(
      h(
        "div",
        { class: "int-row" },
        dot(accent, 5),
        h("span", { class: "int-name", text: String(p.description ?? t("Payment")) }),
        h("span", {
          class: "int-amount",
          style: "color:#22c55e",
          text: `+${(Number(p.amount ?? 0) / 100).toFixed(2)}`,
        }),
        h("span", { class: "int-ago", text: timeAgo(p.createdAt) }),
      ),
    );
  }
  return h(
    "div",
    { class: "int-card" },
    header("#0570DE", "Stripe", t("Payments")),
    h("div", { class: "int-balance" }, h("span", { text: balance }), h("i", { text: currency })),
    rows,
  );
}

// ── Notion ────────────────────────────────────────────────────────────────────

function notionCard(): HTMLElement {
  const rows = h("div", { class: "int-rows tight" });
  for (const p of arr("integration_notion", "pages").slice(0, 3)) {
    rows.append(
      h(
        "button",
        {
          class: "int-page",
          onclick: () => {
            if (typeof p.url === "string") void Bridge.openUrl(p.url);
          },
        },
        p.emoji
          ? h("span", { class: "int-emoji", text: String(p.emoji) })
          : h("i", { class: "int-emoji" }, svg(ICONS.doc, 9)),
        h("span", { class: "int-name", text: String(p.title ?? t("Untitled")) }),
        h("span", { class: "int-ago", text: timeAgo(p.lastEditedAt) }),
      ),
    );
  }
  return h("div", { class: "int-card" }, header("#E8E8E8", "Notion", t("Recent")), rows);
}

// ── Cal.com ───────────────────────────────────────────────────────────────────

function calcomCard(): HTMLElement {
  const bookings = arr("integration_calcom", "bookings")
    .slice()
    .sort((a, b) => new Date(String(a.start)).getTime() - new Date(String(b.start)).getTime());
  const rows = h("div", { class: "int-rows tight" });
  if (bookings.length === 0) {
    rows.append(h("div", { class: "int-empty", text: t("No calls scheduled") }));
  }
  for (const b of bookings.slice(0, 3)) {
    const when = new Date(String(b.start));
    const day = when.toLocaleDateString(language(), { day: "2-digit", month: "2-digit" });
    const time = when.toLocaleTimeString(language(), { hour: "2-digit", minute: "2-digit" });
    rows.append(
      h(
        "div",
        { class: "int-row" },
        dot("#C9956A", 4),
        h("span", { class: "int-time", text: `${day} ${time}` }),
        h("span", { class: "int-name", text: String(b.title ?? t("Meeting")) }),
      ),
    );
  }
  return h("div", { class: "int-card" }, header("#C9956A", "Cal.com", t("Schedule")), rows);
}

// ── Home Assistant ────────────────────────────────────────────────────────────
//
// One card, up to four instances (homeassistant.rs): each gets its own header
// with the house's name, then its persistent notifications and what waits
// (alarm, updates, problems).

const HA_BLUE = "#41BDF5";

/** The alarm panel's state, in words. */
function alarmLabel(state: string): string {
  switch (state) {
    case "disarmed": return t("Disarmed");
    case "armed_home": return t("Armed home");
    case "armed_away": return t("Armed away");
    case "armed_night": return t("Armed night");
    case "armed_vacation": return t("Armed vacation");
    case "arming": return t("Arming…");
    case "pending": return t("Pending…");
    case "triggered": return t("Alarm triggered");
    default: return state;
  }
}

export function homeassistantInstances(): Record<string, unknown>[] {
  return arr("integration_homeassistant", "instances");
}

function homeassistantSection(inst: Record<string, unknown>, several: boolean): Node[] {
  const slot = typeof inst.slot === "number" ? inst.slot : 1;
  const list = (key: string) => (Array.isArray(inst[key]) ? (inst[key] as Record<string, unknown>[]) : []);
  const strings = (key: string) => (Array.isArray(inst[key]) ? (inst[key] as unknown[]).map(String) : []);
  const alarms = list("alarms");
  const notifications = list("notifications");
  const updates = strings("updates");
  const problems = strings("problems");
  const connected = inst.connected === true;
  const name = typeof inst.name === "string" && inst.name ? inst.name : t("Instance {n}", { n: slot });
  const out: Node[] = [];

  if (several) {
    // Each house gets a row of its own: its name, and a red dot with the reason when it is unreachable.
    const accent = connected ? "#22C55E" : "#F4505E";
    out.push(listRow(accent, true,
      h("span", { class: "int-name", text: name }),
      h("span", { class: "int-time", style: `color:${accent}`, text: connected ? "" : String(inst.error ?? t("Not connected")) }),
    ));
  } else if (!connected && inst.error) {
    out.push(h("div", { class: "int-status", text: String(inst.error) }));
  }

  for (const n of notifications.slice(0, 4)) {
    const id = String(n.id ?? "");
    const title = typeof n.title === "string" && n.title ? n.title : t("Notification");
    const message = typeof n.message === "string" ? n.message : "";
    const row = listRow("#F5A524", false,
      h("span", { class: "int-name", title: message, text: message ? `${title} · ${message}` : title }),
    );
    const dismiss = h("button", { class: "link-btn", style: "color:#F5A524d9;margin-left:auto", text: t("Dismiss") });
    dismiss.addEventListener("click", async () => {
      dismiss.setAttribute("disabled", "");
      try {
        await Bridge.homeAssistantDismiss(slot, id);
      } catch (err) {
        dismiss.textContent = String(err);
      }
    });
    row.append(dismiss);
    out.push(row);
  }
  for (const a of alarms) {
    const triggered = a.state === "triggered";
    const accent = triggered ? "#F4505E" : a.state === "disarmed" ? "#6B7079" : "#22C55E";
    out.push(listRow(accent, triggered,
      h("span", { class: "int-name", text: String(a.name ?? "") }),
      h("span", { class: "int-time", style: `color:${accent}`, text: alarmLabel(String(a.state ?? "")) }),
    ));
  }
  if (updates.length > 0) {
    out.push(listRow("#F5A524", false,
      h("span", { class: "int-name", text: t("{n} updates available", { n: updates.length }) }),
      h("span", { class: "int-time", style: "color:#F5A524", text: updates.slice(0, 2).join(", ") }),
    ));
  }
  if (problems.length > 0) {
    out.push(listRow("#F4505E", false,
      h("span", { class: "int-name", text: t("{n} problems", { n: problems.length }) }),
      h("span", { class: "int-time", style: "color:#F4505E", text: problems.slice(0, 2).join(", ") }),
    ));
  }
  if (connected && notifications.length === 0 && alarms.length === 0 && updates.length === 0 && problems.length === 0) {
    out.push(listRow("#22C55E", false, h("span", { class: "int-name", text: t("All quiet") })));
  }
  return out;
}

function homeassistantCard(): HTMLElement {
  const instances = homeassistantInstances();
  const several = instances.length > 1;
  const first = instances[0] ?? {};
  const name = !several && typeof first.name === "string" && first.name ? first.name : "Home Assistant";
  const lightsOn = instances.reduce((n, i) => n + (typeof i.lightsOn === "number" ? i.lightsOn : 0), 0);
  const rows = h("div", { class: "int-rows tight" });
  for (const inst of instances) rows.append(...homeassistantSection(inst, several));
  const extra = lightsOn > 0 ? h("span", { text: t("{n} lights on", { n: lightsOn }) }) : undefined;
  return h("div", { class: "int-card" }, header(HA_BLUE, name, t("Home"), extra), rows);
}

// ── Whoop ─────────────────────────────────────────────────────────────────────

const WHOOP_GREEN = "#44D7B6";

/** Whoop's own colours for a recovery: green from 67, yellow from 34, red below. */
function recoveryColor(score: number): string {
  return score >= 67 ? "#22C55E" : score >= 34 ? "#F5A524" : "#F4505E";
}

function whoopCard(): HTMLElement {
  const d = get("integration_whoop");
  const recovery = (d.recovery ?? null) as Record<string, unknown> | null;
  const sleep = (d.sleep ?? null) as Record<string, unknown> | null;
  const workout = (d.workout ?? null) as Record<string, unknown> | null;
  const strain = typeof d.strain === "number" ? d.strain : null;
  const rows = h("div", { class: "int-rows tight" });
  const row = (accent: string, label: string, value: string, ago?: unknown) => {
    const r = listRow(accent, false,
      h("span", { class: "int-name", text: label }),
      h("span", { class: "int-time", style: `color:${accent}`, text: value }),
    );
    if (ago) r.append(h("span", { class: "int-ago", text: timeAgo(ago) }));
    rows.append(r);
  };
  let headline: HTMLElement;
  if (recovery && typeof recovery.score === "number") {
    const score = Math.round(recovery.score);
    const color = recoveryColor(score);
    headline = h("div", { class: "int-balance" }, h("span", { style: `color:${color}`, text: String(score) }), h("i", { text: "%" }));
    const hrv = typeof recovery.hrv === "number" ? `${Math.round(recovery.hrv)} ms` : "";
    const rhr = typeof recovery.rhr === "number" ? `${Math.round(recovery.rhr)} bpm` : "";
    if (hrv || rhr) row(color, `${t("HRV")} · ${t("Resting heart rate")}`, [hrv, rhr].filter(Boolean).join(" · "), recovery.at);
  } else {
    headline = h("div", { class: "int-status", text: t("Nothing scored yet") });
  }
  if (sleep) {
    const perf = typeof sleep.performance === "number" ? `${Math.round(sleep.performance)} %` : "";
    const hours = typeof sleep.hours === "number" ? t("{n} h", { n: sleep.hours }) : "";
    row("#7C5CFF", t("Sleep"), [perf, hours].filter(Boolean).join(" · "), sleep.end);
  }
  if (strain != null) row("#F5A524", t("Strain"), strain.toFixed(1));
  if (workout) {
    const sport = typeof workout.sport === "string" && workout.sport ? workout.sport : t("Last workout");
    const ws = typeof workout.strain === "number" ? workout.strain.toFixed(1) : "";
    row(WHOOP_GREEN, sport, ws, workout.end);
  }
  return h("div", { class: "int-card" }, header(WHOOP_GREEN, "Whoop", t("Recovery")), headline, rows);
}

// ── Calendar ──────────────────────────────────────────────────────────────────

const CAL_ORANGE = "#FF7A59";

/** The local day (days since 1970) an instant falls on, with the computer's offset. */
function localDay(secs: number, offset: number): number {
  return Math.floor((secs + offset) / 86_400);
}

function clock(secs: number, offset: number): string {
  const s = ((secs + offset) % 86_400 + 86_400) % 86_400;
  return `${String(Math.floor(s / 3600)).padStart(2, "0")}:${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}`;
}

/** Today, Tomorrow, or the date, for the day an event starts on. */
export function calendarDayLabel(day: number, today: number): string {
  if (day === today) return t("Today");
  if (day === today + 1) return t("Tomorrow");
  return new Date(day * 86_400_000).toLocaleDateString(language(), { weekday: "short", day: "2-digit", month: "2-digit", timeZone: "UTC" });
}

function calendarCard(): HTMLElement {
  const d = get("integration_calendar");
  const now = typeof d.now === "number" ? d.now : Math.floor(Date.now() / 1000);
  const offset = typeof d.offset === "number" ? d.offset : -new Date().getTimezoneOffset() * 60;
  const events = arr("integration_calendar", "events");
  const errors = arr("integration_calendar", "errors");
  const today = localDay(now, offset);
  const rows = h("div", { class: "int-rows tight" });
  for (const e of errors) {
    rows.append(h("div", { class: "int-status", text: `${t("Calendar {n}", { n: Number(e.calendar) })}: ${String(e.error ?? "")}` }));
  }
  if (events.length === 0 && errors.length === 0) {
    rows.append(h("div", { class: "int-empty", text: t("No events") }));
  }
  let lastDay: number | null = null;
  for (const e of events.slice(0, 8)) {
    const start = Number(e.start);
    const end = Number(e.end);
    const day = localDay(start, offset);
    if (day !== lastDay) {
      rows.append(h("div", { class: "int-status", text: calendarDayLabel(day, today) }));
      lastDay = day;
    }
    const ongoing = start <= now && end > now;
    const when = e.allDay ? t("All day") : `${clock(start, offset)}–${clock(end, offset)}`;
    rows.append(listRow(ongoing ? "#22C55E" : CAL_ORANGE, ongoing,
      h("span", { class: "int-time", style: `color:${ongoing ? "#22C55E" : CAL_ORANGE}`, text: when }),
      h("span", { class: "int-name", title: typeof e.location === "string" ? e.location : "", text: String(e.title ?? "") }),
    ));
  }
  const next = events.find((e) => Number(e.start) > now && !e.allDay);
  const extra = next ? h("span", { text: t("Next at {time}", { time: clock(Number(next.start), offset) }) }) : undefined;
  return h("div", { class: "int-card" }, header(CAL_ORANGE, t("Calendar"), t("Schedule"), extra), rows);
}

// ── n8n ───────────────────────────────────────────────────────────────────────

function n8nCard(task: AgentTask, onDetail: () => void, openSettings: () => void): HTMLElement {
  const hasActivity = task.steps.length > 0 && (task.state === "finished" || task.state === "error");
  if (!hasActivity) return idleCard(task, openSettings);
  const success = task.state === "finished";
  const accent = success ? "#22C55E" : "#F4505E";
  return h(
    "div",
    { class: "int-card" },
    header("#F29B38", "n8n", t("Workflow")),
    h(
      "div",
      { class: "int-actions" },
      h(
        "button",
        {
          class: "int-pill",
          style: `background:${accent}1a;border-color:${accent}38`,
          onclick: onDetail,
        },
        dot(accent, 5),
        h("span", { class: "int-name", text: task.steps[0] ?? t("Workflow") }),
        svg(ICONS.ellipsis, 8),
      ),
    ),
  );
}

function n8nDetail(task: AgentTask, onBack: () => void): HTMLElement {
  const success = task.state === "finished";
  const accent = success ? "#22C55E" : "#F4505E";
  const detail = task.steps[1];
  return h(
    "div",
    { class: "int-card detail" },
    h(
      "div",
      { class: "int-detail-head" },
      h("button", { class: "int-back", onclick: onBack }, svg(ICONS.chevronLeft, 10, { stroke: 2.4 })),
      dot(accent, 6),
      h("b", { text: task.steps[0] ?? t("Workflow") }),
      h("span", {
        class: "int-badge",
        style: `color:${accent};background:${accent}24`,
        text: success ? t("Success") : t("Failed"),
      }),
    ),
    detail
      ? h("pre", { class: "int-detail-text", text: detail })
      : h("div", {
          class: "int-status",
          text: success ? t("Completed successfully.") : t("No error details available."),
        }),
  );
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

export interface IntegrationCardHooks {
  detailOpen: boolean;
  openDetail(): void;
  closeDetail(): void;
  openSettings(): void;
}

/** True when this integration has data worth showing instead of the idle card. */
export function hasIntegrationData(id: string): boolean {
  const info = State.integrations[id];
  if (!info || info.error) return false;
  switch (id) {
    case "integration_vercel":
      return arr(id, "deployments").length > 0;
    case "integration_resend":
      return arr(id, "emails").length > 0;
    case "integration_github":
      return get(id).totalRepos != null || readPulse(get(id)) != null;
    case "integration_stripe":
      return info.loaded;
    case "integration_notion":
      return arr(id, "pages").length > 0;
    case "integration_calcom":
      return info.loaded;
    case "integration_homeassistant":
      return info.loaded;
    case "integration_whoop":
      return info.loaded;
    case "integration_calendar":
      return info.loaded;
    default:
      return false;
  }
}

export function renderIntegrationCard(task: AgentTask, hooks: IntegrationCardHooks): HTMLElement {
  if (task.id === "integration_n8n") {
    const hasActivity = task.steps.length > 0 && (task.state === "finished" || task.state === "error");
    return hooks.detailOpen && hasActivity
      ? n8nDetail(task, hooks.closeDetail)
      : n8nCard(task, hooks.openDetail, hooks.openSettings);
  }
  if (task.id === "integration_vercel" && hasIntegrationData(task.id)) {
    return hooks.detailOpen ? vercelDetail(hooks.closeDetail) : vercelCard(hooks.openDetail);
  }
  if (!hasIntegrationData(task.id)) return idleCard(task, hooks.openSettings);

  // With the pulse in, GitHub gets the Mac's richer card and its lists.
  if (task.id === "integration_github") {
    const d = get(task.id);
    const pulse = readPulse(d);
    if (pulse) {
      return hooks.detailOpen
        ? githubDetail(pulse, readStats(d), readActivity(d), hooks.closeDetail)
        : githubPulseCard(pulse, readStats(d), readActivity(d), hooks.openDetail);
    }
  }

  switch (task.id) {
    case "integration_resend":
      return resendCard();
    case "integration_github":
      return githubCard();
    case "integration_stripe":
      return stripeCard();
    case "integration_notion":
      return notionCard();
    case "integration_calcom":
      return calcomCard();
    case "integration_homeassistant":
      return homeassistantCard();
    case "integration_whoop":
      return whoopCard();
    case "integration_calendar":
      return calendarCard();
    default:
      return idleCard(task, hooks.openSettings);
  }
}

export { clear };
