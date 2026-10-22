// What the island's service pills show, written as small read-only cards for the phone (docs/ANDROID_LINK.md,
// "Service cards"). Rust sends a phone only the services its user ticked in Settings -> Android phone; what is built
// here is the most each card can say, kept short on purpose: a headline, why, and three lines.
//
// What never goes in a card: an email address or subject (Resend), a payment's description (Stripe), the text of an
// n8n error, a link, a token. A card says what the pill's own header says and counts, nothing a click would reveal.

import type { PhoneLinkService } from "../core/bridge";
import { State } from "../core/state";
import { t, tn } from "../i18n/i18n";

const obj = (id: string): Record<string, unknown> => (State.integrations[id]?.data ?? {}) as Record<string, unknown>;
const list = (id: string, key: string): Record<string, unknown>[] => {
  const v = obj(id)[key];
  return Array.isArray(v) ? (v as Record<string, unknown>[]) : [];
};

/** "just now", "5m", "2h", "3d": the shorthand the pills use. */
export function ago(value: unknown, now = Date.now()): string {
  const date = typeof value === "number" ? new Date(value) : new Date(String(value));
  const secs = (now - date.getTime()) / 1000;
  if (!Number.isFinite(secs)) return "";
  if (secs < 60) return t("just now");
  if (secs < 3600) return t("{n}m", { n: Math.floor(secs / 60) });
  if (secs < 86400) return t("{n}h", { n: Math.floor(secs / 3600) });
  return t("{n}d", { n: Math.floor(secs / 86400) });
}

const money = (cents: unknown) => (Number(cents ?? 0) / 100).toFixed(2);
const compact = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n));

type Builder = (now: number) => PhoneLinkService | null;

function usable(id: string): boolean {
  const info = State.integrations[id];
  return !!info && info.configured && !info.error;
}

const BUILDERS: Record<string, Builder> = {
  integration_stripe: (now) => {
    const d = obj("integration_stripe");
    return {
      id: "integration_stripe", title: "Stripe",
      headline: `${money(d.balance)} ${String(d.currency ?? "eur").toUpperCase()}`,
      reason: t("Payments"),
      items: list("integration_stripe", "payments").slice(0, 3).map((p) => ({
        label: p.status === "succeeded" ? t("Payment") : t("Payment failed"),
        detail: `+${money(p.amount)} · ${ago(p.createdAt, now)}`,
      })),
    };
  },
  integration_github: () => {
    const d = obj("integration_github");
    return {
      id: "integration_github", title: "GitHub",
      headline: `${compact(Number(d.totalStars ?? 0))} ${t("Total stars")}`,
      reason: t("Overview"),
      items: [{ label: t("Repositories"), detail: String(Number(d.totalRepos ?? 0)) }],
    };
  },
  integration_vercel: (now) => {
    const deployments = list("integration_vercel", "deployments");
    const ready = (d: Record<string, unknown>) => d.state === "READY";
    return {
      id: "integration_vercel", title: "Vercel",
      headline: deployments.length === 0 ? "—" : ready(deployments[0]) ? t("Ready") : t("Failed"),
      reason: t("Deployments"),
      items: deployments.slice(0, 3).map((d) => ({ label: String(d.projectName ?? ""), detail: `${ready(d) ? t("Ready") : t("Failed")} · ${ago(d.createdAt, now)}` })),
    };
  },
  integration_resend: (now) => {
    const emails = list("integration_resend", "emails");
    const total = obj("integration_resend").total;
    return {
      id: "integration_resend", title: "Resend",
      headline: total != null ? String(total) : String(emails.length),
      reason: t("Emails"),
      // No address and no subject: only whether it arrived.
      items: emails.slice(0, 3).map((e) => ({ label: e.lastEvent === "delivered" ? t("Delivered") : t("Not delivered"), detail: ago(e.createdAt, now) })),
    };
  },
  integration_notion: (now) => {
    const pages = list("integration_notion", "pages");
    return {
      id: "integration_notion", title: "Notion",
      headline: String(pages.length),
      reason: t("Recent"),
      items: pages.slice(0, 3).map((p) => ({ label: String(p.title ?? t("Untitled")), detail: ago(p.lastEditedAt, now) })),
    };
  },
  integration_calcom: () => {
    const bookings = list("integration_calcom", "bookings")
      .slice()
      .sort((a, b) => new Date(String(a.start)).getTime() - new Date(String(b.start)).getTime());
    const when = (b: Record<string, unknown>) => {
      const d = new Date(String(b.start));
      return `${String(d.getDate()).padStart(2, "0")}/${String(d.getMonth() + 1).padStart(2, "0")} ${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
    };
    return {
      id: "integration_calcom", title: "Cal.com",
      headline: bookings.length === 0 ? t("No calls scheduled") : when(bookings[0]),
      reason: t("Schedule"),
      items: bookings.slice(0, 3).map((b) => ({ label: String(b.title ?? t("Meeting")), detail: when(b) })),
    };
  },
  integration_homeassistant: () => {
    // What waits across the houses: notifications, alarm panels, updates,
    // problems. Never an address.
    const instances = list("integration_homeassistant", "instances");
    const sum = (key: string) => instances.reduce((n, i) => n + (Array.isArray(i[key]) ? (i[key] as unknown[]).length : 0), 0);
    const triggered = instances.some((i) => Array.isArray(i.alarms) && (i.alarms as Record<string, unknown>[]).some((a) => a.state === "triggered"));
    const notifications = sum("notifications");
    const updates = sum("updates");
    const problems = sum("problems");
    const headline = triggered
      ? t("Alarm triggered")
      : notifications > 0
        ? tn("{count} notification", "{count} notifications", notifications)
        : updates > 0
          ? tn("{count} update available", "{count} updates available", updates)
          : problems > 0
            ? tn("{count} problem", "{count} problems", problems)
            : t("All quiet");
    const first = instances[0] ?? {};
    const items: { label: string; detail: string }[] = [];
    for (const inst of instances) {
      const notes = Array.isArray(inst.notifications) ? (inst.notifications as Record<string, unknown>[]) : [];
      // The title only: a message can carry a link, an address or a name.
      for (const n of notes) items.push({ label: String(n.title ?? t("Notification")), detail: "" });
      const alarms = Array.isArray(inst.alarms) ? (inst.alarms as Record<string, unknown>[]) : [];
      for (const a of alarms) items.push({ label: String(a.name ?? ""), detail: String(a.state ?? "") });
      const ups = Array.isArray(inst.updates) ? (inst.updates as unknown[]).map(String) : [];
      for (const u of ups) items.push({ label: u, detail: t("Update available") });
    }
    return {
      id: "integration_homeassistant", title: "Home Assistant",
      headline,
      reason: instances.length > 1 ? tn("{count} house", "{count} houses", instances.length) : typeof first.name === "string" && first.name ? first.name : t("Home"),
      items: items.slice(0, 3),
    };
  },
  integration_whoop: () => {
    // The day's figures only: recovery, sleep, strain. Nothing else is in the data.
    const d = (State.integrations.integration_whoop?.data ?? {}) as Record<string, unknown>;
    const recovery = (d.recovery ?? null) as Record<string, unknown> | null;
    const sleep = (d.sleep ?? null) as Record<string, unknown> | null;
    const score = recovery && typeof recovery.score === "number" ? Math.round(recovery.score) : null;
    const items: { label: string; detail: string }[] = [];
    if (sleep && typeof sleep.performance === "number") items.push({ label: t("Sleep"), detail: `${Math.round(sleep.performance)} %` });
    if (typeof d.strain === "number") items.push({ label: t("Strain"), detail: d.strain.toFixed(1) });
    if (recovery && typeof recovery.hrv === "number") items.push({ label: t("HRV"), detail: `${Math.round(recovery.hrv)} ms` });
    return {
      id: "integration_whoop", title: "Whoop",
      headline: score == null ? t("Nothing scored yet") : t("Recovery {n} %", { n: score }),
      reason: t("Recovery"),
      items: items.slice(0, 3),
    };
  },
  integration_calendar: () => {
    // The next events' titles and times; a location can be an address, so it stays here.
    const d = (State.integrations.integration_calendar?.data ?? {}) as Record<string, unknown>;
    const now = typeof d.now === "number" ? d.now : Math.floor(Date.now() / 1000);
    const offset = typeof d.offset === "number" ? d.offset : 0;
    const clock = (secs: number) => {
      const s = ((secs + offset) % 86_400 + 86_400) % 86_400;
      return `${String(Math.floor(s / 3600)).padStart(2, "0")}:${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}`;
    };
    const events = list("integration_calendar", "events").filter((e) => Number(e.end) > now);
    const next = events.find((e) => !e.allDay && Number(e.start) > now);
    return {
      id: "integration_calendar", title: t("Calendar"),
      headline: next ? t("Next at {time}", { time: clock(Number(next.start)) }) : t("No events"),
      reason: t("Schedule"),
      items: events.slice(0, 3).map((e) => ({ label: String(e.title ?? ""), detail: e.allDay ? t("All day") : clock(Number(e.start)) })),
    };
  },
  integration_n8n: () => {
    const task = State.tasks.find((x) => x.id === "integration_n8n");
    if (!task || (task.state !== "finished" && task.state !== "error")) return null;
    // The workflow's name and whether it worked; the error text stays on the computer.
    return {
      id: "integration_n8n", title: "n8n",
      headline: task.state === "finished" ? t("Success") : t("Failed"),
      reason: t("Workflow"),
      items: task.steps[0] ? [{ label: task.steps[0], detail: "" }] : [],
    };
  },
};

/** The cards of the services the island has data for (Rust then keeps only the ones the user allowed). */
export function serviceCards(now = Date.now()): PhoneLinkService[] {
  const cards: PhoneLinkService[] = [];
  for (const [id, build] of Object.entries(BUILDERS)) {
    if (id !== "integration_n8n" && !usable(id)) continue;
    const card = build(now);
    if (card) cards.push(card);
  }
  return cards;
}
