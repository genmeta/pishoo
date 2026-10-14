export type EventColor = "blue" | "green" | "yellow" | "purple" | "pink";
export type CalendarView = "month" | "week" | "agenda";

export interface EventInput {
  title: string;
  description: string;
  location: string;
  start_at: number;
  end_at: number;
  all_day: boolean;
  color: EventColor;
}

export interface CalendarEvent extends EventInput {
  id: string;
  created_at: number;
  updated_at: number;
}

async function request(path: string, init?: RequestInit): Promise<unknown> {
  const response = await fetch(`/std/calendar/${path}`, {
    ...init,
    credentials: "same-origin",
    cache: "no-store",
    headers: {
      Accept: "application/json, application/problem+json",
      ...init?.headers,
    },
  });
  // Retain the native Response in a standard Error, including host authorization failures.
  // Solid resources normalize thrown values to Error; using cause preserves HTTP status.
  // An approval response is not a completed business operation.
  if (!response.ok || response.status === 202)
    throw new Error(`Calendar request failed (${response.status})`, {
      cause: response,
    });
  if (response.status === 204) return undefined;
  return response.json();
}

export async function listEvents(
  from: number,
  to: number,
  signal?: AbortSignal,
): Promise<CalendarEvent[]> {
  const result = (await request(
    `events?${new URLSearchParams({ from: String(from), to: String(to) })}`,
    { signal },
  )) as { events: CalendarEvent[] };
  if (!Array.isArray(result.events))
    throw new TypeError("Invalid Calendar response");
  return result.events;
}

export async function getEvent(
  id: string,
  signal?: AbortSignal,
): Promise<CalendarEvent> {
  return (await request(`event?${new URLSearchParams({ id })}`, {
    signal,
  })) as CalendarEvent;
}

export async function createEvent(input: EventInput): Promise<CalendarEvent> {
  return (await request("events", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(input),
  })) as CalendarEvent;
}

export async function updateEvent(
  id: string,
  input: EventInput,
  updatedAt: number,
): Promise<CalendarEvent> {
  return (await request(`event?${new URLSearchParams({ id })}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ...input, updated_at: updatedAt }),
  })) as CalendarEvent;
}

export async function deleteEvent(
  id: string,
  updatedAt: number,
): Promise<void> {
  await request(
    `event?${new URLSearchParams({ id, updated_at: String(updatedAt) })}`,
    { method: "DELETE" },
  );
}
