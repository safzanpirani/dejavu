import type { TranscriptEvent, TranscriptView } from "./transcript-view.ts";

export interface WindowOptions {
  fromEvent?: number;
  limit?: number;
  maxChars?: number;
  toolChars?: number;
  budgetChars?: number;
  /** Prefer a matching region when clipping a search excerpt. */
  focusTerms?: string[];
}

export interface ClippedEvent {
  index: number;
  field: "text" | "input" | "output";
  originalChars: number;
  returnedChars: number;
  startChar: number;
}

export interface WindowedTranscript extends TranscriptView {
  window: {
    availableEvents: number;
    returnedEvents: number;
    usedChars: number;
    nextEvent: number | null;
    clipped: ClippedEvent[];
  };
}

export function eventBody(event: TranscriptEvent): string {
  if (event.kind === "tool_call") return typeof event.input === "string" ? event.input : JSON.stringify(event.input) ?? "";
  return event.kind === "tool_result" ? event.output : event.text;
}

export function validateBound(value: number | undefined, name: string, minimum = 1): void {
  if (value !== undefined && (!Number.isSafeInteger(value) || value < minimum)) {
    throw new Error(`${name} needs an integer >= ${minimum}`);
  }
}

/** Character budgets count event bodies, including ellipses; labels and metadata are excluded. */
export function windowTranscript(view: TranscriptView, options: WindowOptions = {}): WindowedTranscript {
  validateBound(options.fromEvent, "--from-event", 0);
  for (const key of ["limit", "maxChars", "toolChars", "budgetChars"] as const) validateBound(options[key], key);
  const available = view.events.map((event, index) => ({ ...event, index: event.index ?? index }))
    .filter((event) => event.index >= (options.fromEvent ?? 0));
  const events: TranscriptEvent[] = [];
  const clipped: ClippedEvent[] = [];
  let usedChars = 0;
  let nextEvent: number | null = null;
  for (const event of available) {
    const remaining = (options.budgetChars ?? Infinity) - usedChars;
    if (events.length >= (options.limit ?? Infinity) || remaining <= 0) {
      nextEvent = event.index;
      break;
    }
    const body = eventBody(event);
    const tool = event.kind === "tool_call" || event.kind === "tool_result";
    const cap = Math.min(tool ? options.toolChars ?? Infinity : options.maxChars ?? Infinity, remaining);
    let startChar = 0;
    if (body.length > cap && options.focusTerms?.length) {
      const lower = body.toLowerCase();
      const positions = options.focusTerms.map((term) => lower.indexOf(term.toLowerCase())).filter((position) => position >= 0);
      if (positions.length) startChar = Math.max(0, Math.min(body.length - cap + 2, Math.min(...positions) - Math.floor(cap / 4)));
    }
    const text = body.length > cap
      ? `${startChar && cap > 1 ? "…" : ""}${body.slice(startChar, startChar + Math.max(0, cap - (startChar && cap > 1 ? 2 : 1)))}…`
      : body;
    usedChars += text.length;
    if (text.length < body.length) {
      const field = event.kind === "tool_call" ? "input" : event.kind === "tool_result" ? "output" : "text";
      clipped.push({ index: event.index, field, originalChars: body.length, returnedChars: text.length, startChar });
      // A clipped input is explicitly a preview string; the clipping record identifies the changed field.
      events.push({ ...event, [field]: text });
    } else events.push(event);
  }
  return { ...view, events, window: { availableEvents: available.length, returnedEvents: events.length, usedChars, nextEvent, clipped } };
}

export function renderWindow(window: WindowedTranscript["window"]): string {
  const next = window.nextEvent === null ? "end" : `continue with --from-event ${window.nextEvent}`;
  const clipped = window.clipped.length ? `; clipped events ${window.clipped.map((event) => `#${event.index}`).join(", ")} (read each with --full --from-event N --limit 1)` : "";
  return `[${window.returnedEvents}/${window.availableEvents} events; ${window.usedChars} body chars; ${next}${clipped}]`;
}
