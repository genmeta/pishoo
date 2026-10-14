import {
  For,
  Show,
  createEffect,
  createMemo,
  createResource,
  createSignal,
  onCleanup,
} from "solid-js";
import type { JSX } from "solid-js";
import {
  createEvent,
  deleteEvent,
  getEvent,
  listEvents,
  updateEvent,
} from "./api";
import type {
  CalendarEvent,
  CalendarView,
  EventColor,
  EventInput,
} from "./api";

const messages = {
  en: {
    calendar: "Calendar",
    personal: "YOUR TIME, YOUR SPACE",
    newEvent: "New event",
    month: "Month",
    week: "Week",
    agenda: "Agenda",
    today: "Today",
    previous: "Previous period",
    next: "Next period",
    jump: "Go to date",
    navigation: "Calendar navigation",
    workspace: "Back to Workspace",
    language: "Language",
    search: "Search this view",
    searchHint: "Search events, places, notes…",
    timezone: "Time zone",
    categories: "EVENT COLORS",
    all: "All colors",
    blue: "Blue",
    green: "Green",
    yellow: "Yellow",
    purple: "Purple",
    pink: "Pink",
    selected: "SELECTED DAY",
    noEvents: "A little room to breathe.",
    noEventsHint: "No events for this day. Make space for something good.",
    empty: "Your calendar is clear.",
    emptyHint: "Create an event to start planning your time.",
    noResults: "No matching events.",
    noResultsHint: "Try another search or color.",
    loading: "Loading calendar…",
    updating: "Updating…",
    retry: "Try again",
    refresh: "Refresh calendar",
    allDay: "All day",
    more: "View all events",
    title: "Title",
    titleHint: "What are you planning?",
    location: "Location",
    locationHint: "Add a place or meeting link",
    notes: "Notes",
    notesHint: "Add details, an agenda, or anything to remember",
    start: "Starts",
    end: "Ends",
    endDate: "Last day",
    color: "Color",
    save: "Save event",
    saving: "Saving…",
    cancel: "Cancel",
    close: "Close editor",
    edit: "Edit event",
    delete: "Delete event",
    deleting: "Deleting…",
    deleteConfirm: "Delete this event?",
    deleteHint: "This event will be removed from your calendar.",
    keep: "Keep event",
    created: "Event created.",
    saved: "Changes saved.",
    deleted: "Event deleted.",
    dateError: "Choose valid dates. The end must be after the start.",
    titleError: "Enter a title of 1–120 characters.",
    rangeHint:
      "Times use your local time zone. All-day events keep their calendar dates.",
    invalid: "Check the event details and try again.",
    forbidden:
      "Access denied. Open this calendar with its owner identity and the required permissions.",
    notFound: "This event is no longer available. Refresh the calendar.",
    conflict:
      "This event changed in another window. Reopen it before saving or deleting.",
    tooLarge:
      "Too much data. Shorten the notes or choose a smaller date range.",
    pending: "Access approval is pending. Try again after approval.",
    unavailable:
      "The calendar service is unavailable. Please try again shortly.",
    network: "Unable to connect. Check your connection and try again.",
    failed: "Unable to complete the request. Please try again.",
    updated: "Updated",
    dayEvents: "Events for selected day",
  },
  "zh-CN": {
    calendar: "日历",
    personal: "把时间留给重要的事",
    newEvent: "新建日程",
    month: "月视图",
    week: "周视图",
    agenda: "日程列表",
    today: "今天",
    previous: "上一时段",
    next: "下一时段",
    jump: "跳转日期",
    navigation: "日历导航",
    workspace: "返回工作台",
    language: "语言",
    search: "搜索当前视图",
    searchHint: "搜索日程、地点、备注…",
    timezone: "时区",
    categories: "日程颜色",
    all: "全部颜色",
    blue: "蓝色",
    green: "绿色",
    yellow: "黄色",
    purple: "紫色",
    pink: "粉色",
    selected: "所选日期",
    noEvents: "给这一天留一点空白。",
    noEventsHint: "当天暂无日程，可以安排一件期待的事。",
    empty: "日历还是空白的。",
    emptyHint: "新建一条日程，开始安排你的时间。",
    noResults: "没有匹配的日程。",
    noResultsHint: "试试其他关键词或颜色。",
    loading: "正在加载日历…",
    updating: "正在更新…",
    retry: "重试",
    refresh: "刷新日历",
    allDay: "全天",
    more: "查看全部日程",
    title: "标题",
    titleHint: "准备安排什么？",
    location: "地点",
    locationHint: "添加地点或会议链接",
    notes: "备注",
    notesHint: "添加详情、议程或需要记住的事",
    start: "开始时间",
    end: "结束时间",
    endDate: "最后一天",
    color: "颜色",
    save: "保存日程",
    saving: "正在保存…",
    cancel: "取消",
    close: "关闭编辑器",
    edit: "编辑日程",
    delete: "删除日程",
    deleting: "正在删除…",
    deleteConfirm: "删除这条日程？",
    deleteHint: "这条日程将从日历中移除。",
    keep: "保留日程",
    created: "日程已创建。",
    saved: "修改已保存。",
    deleted: "日程已删除。",
    dateError: "请选择有效日期，结束时间必须晚于开始时间。",
    titleError: "请输入 1–120 个字符的标题。",
    rangeHint: "时间按本地时区显示，全天日程始终保留原始日期。",
    invalid: "请检查日程信息后重试。",
    forbidden: "访问被拒绝。请使用日历主人的身份，并确认已获得所需权限。",
    notFound: "日程已不存在，请刷新日历。",
    conflict: "日程已在其他窗口修改，请重新打开后再保存或删除。",
    tooLarge: "数据过多，请缩短备注或缩小日期范围。",
    pending: "访问审批尚未完成，请在审批通过后重试。",
    unavailable: "日历服务暂时不可用，请稍后重试。",
    network: "无法连接，请检查网络后重试。",
    failed: "请求未能完成，请重试。",
    updated: "更新时间",
    dayEvents: "所选日期的日程",
  },
};
const colors: EventColor[] = ["blue", "green", "yellow", "purple", "pink"];
const dayMillis = 86_400_000;

function dateKey(date: Date): string {
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;
}
function fromKey(key: string): Date {
  return new Date(`${key}T00:00:00`);
}
function addDays(date: Date, count: number): Date {
  return new Date(date.getFullYear(), date.getMonth(), date.getDate() + count);
}
function monday(date: Date): Date {
  return addDays(date, -((date.getDay() + 6) % 7));
}
function utcDay(date: Date): number {
  return Date.UTC(date.getFullYear(), date.getMonth(), date.getDate());
}
function inputTime(time: number, allDay: boolean): string {
  const date = new Date(time);
  if (allDay) return date.toISOString().slice(0, 10);
  return `${dateKey(date)}T${String(date.getHours()).padStart(2, "0")}:${String(date.getMinutes()).padStart(2, "0")}`;
}
function intersects(event: CalendarEvent, day: Date): boolean {
  const start = event.all_day ? utcDay(day) : day.getTime();
  const end = event.all_day
    ? utcDay(addDays(day, 1))
    : addDays(day, 1).getTime();
  return event.start_at < end && event.end_at > start;
}
function Icon(props: {
  name:
    | "calendar"
    | "plus"
    | "left"
    | "right"
    | "search"
    | "arrow"
    | "close"
    | "refresh"
    | "pin"
    | "clock";
}): JSX.Element {
  const paths = {
    calendar:
      "M8 2v4M16 2v4M3 10h18M5 4h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2ZM8 14h1M12 14h1M16 14h1M8 18h1M12 18h1",
    plus: "M12 5v14M5 12h14",
    left: "m15 5-7 7 7 7",
    right: "m9 5 7 7-7 7",
    search: "M21 21l-5-5M18 10a8 8 0 1 1-16 0 8 8 0 0 1 16 0Z",
    arrow: "M19 12H5m6-6-6 6 6 6",
    close: "m6 6 12 12M6 18 18 6",
    refresh: "M20 7v5h-5M4 17v-5h5M6 6a8 8 0 0 1 14 6M18 18a8 8 0 0 1-14-6",
    pin: "M20 10c0 6-8 12-8 12S4 16 4 10a8 8 0 1 1 16 0ZM15 10a3 3 0 1 1-6 0 3 3 0 0 1 6 0Z",
    clock: "M12 7v5l3 2M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0Z",
  };
  return (
    <svg
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      <path d={paths[props.name]} />
    </svg>
  );
}

export default function App(): JSX.Element {
  const requestedLanguage = new URLSearchParams(location.search).get("lang");
  const [locale, setLocale] = createSignal<"en" | "zh-CN">(
    requestedLanguage === "en"
      ? "en"
      : requestedLanguage === "zh-CN" || navigator.language.startsWith("zh")
        ? "zh-CN"
        : "en",
  );
  const t = () => messages[locale()];
  const [view, setView] = createSignal<CalendarView>("month");
  const [selected, setSelected] = createSignal(dateKey(new Date()));
  const [anchor, setAnchor] = createSignal(fromKey(selected()));
  const [search, setSearch] = createSignal("");
  const [color, setColor] = createSignal<EventColor | "all">("all");
  const [editor, setEditor] = createSignal<CalendarEvent | "new" | null>(null);
  const [pending, setPending] = createSignal<
    "idle" | "open" | "save" | "delete"
  >("idle");
  const [formError, setFormError] = createSignal<unknown>(null);
  const [notice, setNotice] = createSignal<
    "created" | "saved" | "deleted" | null
  >(null);
  const [openError, setOpenError] = createSignal<unknown>(null);
  const [allDay, setAllDay] = createSignal(false);
  const [deleting, setDeleting] = createSignal(false);
  let dialog: HTMLDialogElement | undefined;
  let opener: HTMLElement | null = null;
  let noticeTimer: ReturnType<typeof setTimeout> | undefined;
  const zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  const busy = () => pending() !== "idle";
  const today = () => dateKey(new Date());
  const formatDate = (date: Date, options: Intl.DateTimeFormatOptions) =>
    new Intl.DateTimeFormat(locale(), options).format(date);
  const days = createMemo(() => {
    const start =
      view() === "week"
        ? monday(anchor())
        : view() === "month"
          ? monday(new Date(anchor().getFullYear(), anchor().getMonth(), 1))
          : new Date(anchor().getFullYear(), anchor().getMonth(), 1);
    const count =
      view() === "week"
        ? 7
        : view() === "month"
          ? 42
          : new Date(
              anchor().getFullYear(),
              anchor().getMonth() + 1,
              0,
            ).getDate();
    return Array.from({ length: count }, (_, i) => addDays(start, i));
  });
  const bounds = createMemo(() => {
    const start = days()[0]!;
    const end = addDays(days().at(-1)!, 1);
    return [
      Math.min(start.getTime(), utcDay(start)),
      Math.max(end.getTime(), utcDay(end)),
    ] as [number, number];
  });
  const [data, { refetch }] = createResource(bounds, ([from, to]) =>
    listEvents(from, to),
  );
  const filtered = createMemo(() => {
    if (data.error) return [];
    const text = search().trim().toLocaleLowerCase(locale());
    return (data() ?? []).filter(
      (event) =>
        (color() === "all" || event.color === color()) &&
        (!text ||
          `${event.title}\n${event.location}\n${event.description}`
            .toLocaleLowerCase(locale())
            .includes(text)),
    );
  });
  const dayEvents = (day: Date) =>
    filtered().filter((event) => intersects(event, day));
  const weekdayNames = () =>
    Array.from({ length: 7 }, (_, i) =>
      formatDate(addDays(new Date(2026, 0, 5), i), { weekday: "short" }),
    );
  const periodTitle = () =>
    view() === "week"
      ? new Intl.DateTimeFormat(locale(), {
          month: "short",
          day: "numeric",
          year: "numeric",
        }).formatRange(days()[0]!, days()[6]!)
      : formatDate(anchor(), { month: "long", year: "numeric" });
  const errorText = (error: unknown): string => {
    if (error instanceof Error && error.cause instanceof Response)
      error = error.cause;
    if (error === "dateError" || error === "titleError") return t()[error];
    if (error instanceof Response) {
      switch (error.status) {
        case 202:
          return t().pending;
        case 400:
        case 415:
        case 422:
          return t().invalid;
        case 401:
        case 403:
          return t().forbidden;
        case 404:
          return t().notFound;
        case 409:
        case 412:
          return t().conflict;
        case 413:
          return t().tooLarge;
        case 502:
        case 503:
        case 504:
          return t().unavailable;
        default:
          return t().failed;
      }
    }
    return error instanceof TypeError ? t().network : t().failed;
  };
  const eventTime = (event: CalendarEvent) =>
    event.all_day
      ? t().allDay
      : formatDate(new Date(event.start_at), {
          hour: "2-digit",
          minute: "2-digit",
        });
  const eventRange = (event: CalendarEvent) => {
    const options: Intl.DateTimeFormatOptions = event.all_day
      ? { month: "short", day: "numeric", timeZone: "UTC" }
      : { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" };
    return new Intl.DateTimeFormat(locale(), options).formatRange(
      new Date(event.start_at),
      new Date(event.end_at - (event.all_day ? dayMillis : 0)),
    );
  };

  createEffect(() => {
    document.documentElement.lang = locale();
    document.title = `${t().calendar} · pishoo`;
  });
  createEffect(() => {
    if (editor()) {
      if (dialog && !dialog.open) dialog.showModal();
    } else if (dialog?.open) {
      dialog.close();
      opener?.focus();
    }
  });
  onCleanup(() => {
    clearTimeout(noticeTimer);
  });

  function changeLanguage(language: "en" | "zh-CN") {
    setLocale(language);
    const url = new URL(location.href);
    url.searchParams.set("lang", language);
    history.replaceState(null, "", url);
  }
  function goTo(key: string) {
    const date = fromKey(key);
    if (
      Number.isNaN(date.getTime()) ||
      date.getFullYear() < 1970 ||
      date.getFullYear() > 9998
    )
      return;
    setSelected(key);
    setAnchor(date);
  }
  function move(direction: number) {
    const current = anchor();
    goTo(
      dateKey(
        view() === "week"
          ? addDays(current, direction * 7)
          : new Date(current.getFullYear(), current.getMonth() + direction, 1),
      ),
    );
  }
  function newEvent(day = selected()) {
    opener = document.activeElement as HTMLElement;
    setSelected(day);
    setAllDay(false);
    setDeleting(false);
    setFormError(null);
    setEditor("new");
  }
  async function openEvent(event: CalendarEvent) {
    if (busy()) return;
    opener = document.activeElement as HTMLElement;
    setPending("open");
    setOpenError(null);
    try {
      const fresh = await getEvent(event.id);
      setAllDay(fresh.all_day);
      setDeleting(false);
      setFormError(null);
      setEditor(fresh);
    } catch (error) {
      setOpenError(error);
    } finally {
      setPending("idle");
    }
  }
  function closeEditor() {
    if (!busy()) {
      setEditor(null);
      setFormError(null);
      setDeleting(false);
    }
  }
  function notify(message: "created" | "saved" | "deleted") {
    clearTimeout(noticeTimer);
    setNotice(message);
    noticeTimer = setTimeout(() => setNotice(null), 5000);
  }
  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (busy()) return;
    const form = event.currentTarget as HTMLFormElement;
    const fields = new FormData(form);
    const day = allDay();
    const startText = String(fields.get("start"));
    const endText = String(fields.get("end"));
    const current = editor();
    if (!current) return;
    let start = new Date(day ? `${startText}T00:00:00Z` : startText).getTime();
    let end =
      new Date(day ? `${endText}T00:00:00Z` : endText).getTime() +
      (day ? dayMillis : 0);
    // Keep the original instant for unchanged times, including the second
    // occurrence of a repeated local hour when daylight saving time ends.
    if (current !== "new" && current.all_day === day) {
      if (inputTime(current.start_at, day) === startText)
        start = current.start_at;
      if (inputTime(current.end_at - (day ? dayMillis : 0), day) === endText)
        end = current.end_at;
    }
    const title = String(fields.get("title") ?? "").trim();
    if (!title || [...title].length > 120) {
      setFormError("titleError");
      return;
    }
    // Reject normalized invalid dates and local times in the DST spring-forward gap.
    if (
      !Number.isFinite(start) ||
      !Number.isFinite(end) ||
      end <= start ||
      start < 0 ||
      end >= 253402300800000 ||
      inputTime(start, day) !== startText ||
      inputTime(end - (day ? dayMillis : 0), day) !== endText
    ) {
      setFormError("dateError");
      return;
    }
    const input: EventInput = {
      title,
      start_at: start,
      end_at: end,
      all_day: day,
      description: String(fields.get("description") ?? ""),
      location: String(fields.get("location") ?? ""),
      color: String(fields.get("color")) as EventColor,
    };
    setPending("save");
    setFormError(null);
    try {
      if (current === "new") await createEvent(input);
      else await updateEvent(current.id, input, current.updated_at);
      setEditor(null);
      notify(current === "new" ? "created" : "saved");
      void refetch();
    } catch (error) {
      setFormError(error);
    } finally {
      setPending("idle");
    }
  }
  async function remove() {
    const current = editor();
    if (!current || current === "new" || busy()) return;
    setPending("delete");
    setFormError(null);
    try {
      await deleteEvent(current.id, current.updated_at);
      setEditor(null);
      notify("deleted");
      void refetch();
    } catch (error) {
      setFormError(error);
    } finally {
      setPending("idle");
    }
  }
  function toggleAllDay(form: HTMLFormElement, enabled: boolean) {
    const start = form.elements.namedItem("start") as HTMLInputElement;
    const end = form.elements.namedItem("end") as HTMLInputElement;
    const startValue = start.value.slice(0, 10);
    const endValue = end.value.slice(0, 10);
    setAllDay(enabled);
    start.value = enabled ? startValue : `${startValue}T09:00`;
    end.value = enabled ? endValue : `${endValue}T10:00`;
  }
  const eventButton = (event: CalendarEvent, compact = false) => (
    <button
      type="button"
      class={`event-chip color-${event.color}${compact ? " compact" : ""}`}
      disabled={busy()}
      onClick={() => void openEvent(event)}
      title={`${event.title} · ${eventRange(event)}`}
    >
      <span class="event-dot" />
      <span class="event-copy">
        <strong>{event.title}</strong>
        <small>{eventTime(event)}</small>
      </span>
      <Show when={!compact && event.location}>
        <span class="event-location">
          <Icon name="pin" />
          {event.location}
        </span>
      </Show>
    </button>
  );

  return (
    <div class="app-shell">
      <aside class="sidebar">
        <a class="brand" href="/std/workspace/">
          <span class="brand-mark">p</span>
          <span>
            <strong>pishoo</strong>
            <small>{t().calendar}</small>
          </span>
        </a>
        <div class="sidebar-title">
          <span class="eyebrow">{t().personal}</span>
          <h1>
            {t().calendar}
            <span class="title-dot">.</span>
          </h1>
        </div>
        <button
          class="button primary create-button"
          disabled={!!data.error || busy()}
          onClick={() => newEvent()}
        >
          <Icon name="plus" />
          {t().newEvent}
        </button>
        <nav aria-label={t().navigation} class="view-nav">
          <For each={["month", "week", "agenda"] as CalendarView[]}>
            {(item) => (
              <button
                type="button"
                aria-pressed={view() === item}
                classList={{ active: view() === item }}
                onClick={() => {
                  setAnchor(fromKey(selected()));
                  setView(item);
                }}
              >
                <Icon name={item === "agenda" ? "clock" : "calendar"} />
                {t()[item]}
                <span aria-hidden="true">↗</span>
              </button>
            )}
          </For>
        </nav>
        <section class="color-filter">
          <h2 class="eyebrow">{t().categories}</h2>
          <button
            type="button"
            classList={{ chosen: color() === "all" }}
            aria-pressed={color() === "all"}
            onClick={() => setColor("all")}
          >
            <span class="color-swatch all-colors" />
            {t().all}
          </button>
          <For each={colors}>
            {(item) => (
              <button
                type="button"
                classList={{ chosen: color() === item }}
                aria-pressed={color() === item}
                onClick={() => setColor(item)}
              >
                <span class={`color-swatch color-${item}`} />
                {t()[item]}
              </button>
            )}
          </For>
        </section>
        <div class="sidebar-bottom">
          <div class="language-switch" role="group" aria-label={t().language}>
            <button
              aria-label="English"
              aria-pressed={locale() === "en"}
              onClick={() => changeLanguage("en")}
            >
              EN
            </button>
            <button
              aria-label="中文"
              aria-pressed={locale() === "zh-CN"}
              onClick={() => changeLanguage("zh-CN")}
            >
              中文
            </button>
          </div>
          <a class="workspace-link" href="/std/workspace/">
            <Icon name="arrow" />
            {t().workspace}
          </a>
        </div>
      </aside>
      <main class="workspace">
        <header class="topbar">
          <div>
            <span class="eyebrow">
              {t().calendar} / {t()[view()]}
            </span>
            <h2>{periodTitle()}</h2>
          </div>
          <button
            class="icon-button"
            aria-label={t().refresh}
            disabled={data.loading || busy()}
            onClick={() => void refetch()}
          >
            <Icon name="refresh" />
          </button>
        </header>
        <div class="toolbar">
          <div class="date-navigation">
            <button
              class="icon-button"
              aria-label={t().previous}
              onClick={() => move(-1)}
            >
              <Icon name="left" />
            </button>
            <button class="button today-button" onClick={() => goTo(today())}>
              {t().today}
            </button>
            <button
              class="icon-button"
              aria-label={t().next}
              onClick={() => move(1)}
            >
              <Icon name="right" />
            </button>
            <input
              class="date-jump"
              type="date"
              aria-label={t().jump}
              min="1970-01-01"
              max="9998-12-31"
              value={selected()}
              onChange={(event) => goTo(event.currentTarget.value)}
            />
          </div>
          <label class="search-field">
            <Icon name="search" />
            <input
              type="search"
              aria-label={t().search}
              placeholder={t().searchHint}
              value={search()}
              onInput={(event) => setSearch(event.currentTarget.value)}
            />
          </label>
        </div>
        <Show when={openError()}>
          <div class="error-banner" role="alert">
            {errorText(openError())}
            <button
              class="text-button"
              onClick={() => {
                setOpenError(null);
                void refetch();
              }}
            >
              {t().retry}
            </button>
          </div>
        </Show>
        <Show
          when={!data.error}
          fallback={
            <div class="state-panel error-panel" role="alert">
              <Icon name="calendar" />
              <h3>{errorText(data.error)}</h3>
              <button class="button" onClick={() => void refetch()}>
                {t().retry}
              </button>
            </div>
          }
        >
          <Show
            when={data() !== undefined}
            fallback={
              <div class="state-panel" role="status">
                <span class="spinner" />
                {t().loading}
              </div>
            }
          >
            <div
              class="calendar-layout"
              classList={{ "agenda-layout": view() === "agenda" }}
            >
              <section
                class="calendar-panel"
                aria-label={t()[view()]}
                aria-busy={data.loading}
              >
                <Show
                  when={view() !== "agenda"}
                  fallback={
                    <div class="agenda-list">
                      <Show
                        when={days().some((day) => dayEvents(day).length)}
                        fallback={
                          <div class="state-panel">
                            <Icon name="calendar" />
                            <h3>
                              {search() || color() !== "all"
                                ? t().noResults
                                : t().empty}
                            </h3>
                            <p>
                              {search() || color() !== "all"
                                ? t().noResultsHint
                                : t().emptyHint}
                            </p>
                          </div>
                        }
                      >
                        <For
                          each={days().filter((day) => dayEvents(day).length)}
                        >
                          {(day) => (
                            <section class="agenda-day">
                              <header>
                                <strong>
                                  {formatDate(day, { day: "numeric" })}
                                </strong>
                                <span>
                                  {formatDate(day, {
                                    weekday: "long",
                                    month: "short",
                                  })}
                                </span>
                              </header>
                              <div>
                                <For each={dayEvents(day)}>
                                  {(event) => eventButton(event)}
                                </For>
                              </div>
                            </section>
                          )}
                        </For>
                      </Show>
                    </div>
                  }
                >
                  <div class="weekday-header">
                    <For each={weekdayNames()}>
                      {(name) => <span>{name}</span>}
                    </For>
                  </div>
                  <div
                    class="calendar-grid"
                    classList={{ "week-grid": view() === "week" }}
                  >
                    <For each={days()}>
                      {(day) => (
                        <div
                          class="day-cell"
                          classList={{
                            outside:
                              day.getMonth() !== anchor().getMonth() &&
                              view() === "month",
                            selected: dateKey(day) === selected(),
                            "is-today": dateKey(day) === today(),
                          }}
                        >
                          <button
                            class="day-number"
                            type="button"
                            aria-label={formatDate(day, { dateStyle: "full" })}
                            aria-pressed={dateKey(day) === selected()}
                            onClick={() => setSelected(dateKey(day))}
                          >
                            {day.getDate()}
                          </button>
                          <div class="cell-events">
                            <For
                              each={dayEvents(day).slice(
                                0,
                                view() === "month" ? 3 : undefined,
                              )}
                            >
                              {(event) => eventButton(event, true)}
                            </For>
                            <Show
                              when={
                                view() === "month" && dayEvents(day).length > 3
                              }
                            >
                              <button
                                class="more-events"
                                onClick={() => setSelected(dateKey(day))}
                                aria-label={t().more}
                              >
                                +{dayEvents(day).length - 3}
                              </button>
                            </Show>
                          </div>
                          <button
                            class="cell-add"
                            disabled={busy()}
                            aria-label={`${t().newEvent} · ${formatDate(day, { dateStyle: "full" })}`}
                            onClick={() => newEvent(dateKey(day))}
                          >
                            <Icon name="plus" />
                          </button>
                        </div>
                      )}
                    </For>
                  </div>
                </Show>
              </section>
              <aside class="day-panel" aria-label={t().dayEvents}>
                <header>
                  <span class="eyebrow">{t().selected}</span>
                  <div class="selected-date">
                    <strong>{fromKey(selected()).getDate()}</strong>
                    <span>
                      {formatDate(fromKey(selected()), { weekday: "long" })}
                      <small>
                        {formatDate(fromKey(selected()), {
                          year: "numeric",
                          month: "long",
                        })}
                      </small>
                    </span>
                  </div>
                </header>
                <div class="day-panel-events">
                  <Show
                    when={dayEvents(fromKey(selected())).length}
                    fallback={
                      <div class="day-empty">
                        <Icon name="calendar" />
                        <h3>
                          {search() || color() !== "all"
                            ? t().noResults
                            : t().noEvents}
                        </h3>
                        <p>
                          {search() || color() !== "all"
                            ? t().noResultsHint
                            : t().noEventsHint}
                        </p>
                      </div>
                    }
                  >
                    <For each={dayEvents(fromKey(selected()))}>
                      {(event) => eventButton(event)}
                    </For>
                  </Show>
                </div>
                <button
                  class="button day-add"
                  disabled={busy()}
                  onClick={() => newEvent()}
                >
                  <Icon name="plus" />
                  {t().newEvent}
                </button>
              </aside>
            </div>
            <footer class="calendar-footer">
              <span>
                <Icon name="clock" />
                {t().timezone}: {zone}
              </span>
              <Show when={data.loading}>
                <span role="status">{t().updating}</span>
              </Show>
              <span>pishoo / calendar</span>
            </footer>
          </Show>
        </Show>
      </main>
      <Show when={notice()}>
        <div class="toast" role="status">
          <span aria-hidden="true">✓</span>
          {t()[notice()!]}
        </div>
      </Show>
      <dialog
        ref={dialog}
        class="editor-dialog"
        onCancel={(event) => {
          event.preventDefault();
          closeEditor();
        }}
        onClick={(event) => {
          if (event.target === dialog) closeEditor();
        }}
        aria-labelledby="editor-heading"
      >
        <Show when={editor()} keyed>
          {(current) => {
            const existing = current === "new" ? null : current;
            const defaultStart = existing
              ? inputTime(existing.start_at, existing.all_day)
              : `${selected()}T09:00`;
            const defaultEnd = existing
              ? inputTime(
                  existing.end_at - (existing.all_day ? dayMillis : 0),
                  existing.all_day,
                )
              : `${selected()}T10:00`;
            return (
              <form noValidate onSubmit={save}>
                <header class="editor-header">
                  <div>
                    <span class="eyebrow">{t().calendar}</span>
                    <h2 id="editor-heading">
                      {existing ? t().edit : t().newEvent}
                    </h2>
                  </div>
                  <button
                    type="button"
                    class="icon-button"
                    aria-label={t().close}
                    disabled={busy()}
                    onClick={closeEditor}
                  >
                    <Icon name="close" />
                  </button>
                </header>
                <div class="editor-fields">
                  <label class="field">
                    <span>{t().title}</span>
                    <input
                      name="title"
                      required
                      maxlength="120"
                      placeholder={t().titleHint}
                      value={existing?.title ?? ""}
                      autofocus
                      disabled={busy()}
                    />
                  </label>
                  <label class="all-day-toggle">
                    <input
                      type="checkbox"
                      checked={allDay()}
                      disabled={busy()}
                      onChange={(event) =>
                        toggleAllDay(
                          event.currentTarget.form!,
                          event.currentTarget.checked,
                        )
                      }
                    />
                    {t().allDay}
                  </label>
                  <div class="time-fields">
                    <label class="field">
                      <span>{t().start}</span>
                      <input
                        name="start"
                        type={allDay() ? "date" : "datetime-local"}
                        required
                        min={allDay() ? "1970-01-01" : "1970-01-01T00:00"}
                        value={defaultStart}
                        disabled={busy()}
                      />
                    </label>
                    <label class="field">
                      <span>{allDay() ? t().endDate : t().end}</span>
                      <input
                        name="end"
                        type={allDay() ? "date" : "datetime-local"}
                        required
                        value={defaultEnd}
                        disabled={busy()}
                      />
                    </label>
                  </div>
                  <p class="field-hint">{t().rangeHint}</p>
                  <label class="field">
                    <span>{t().location}</span>
                    <input
                      name="location"
                      maxlength="240"
                      value={existing?.location ?? ""}
                      placeholder={t().locationHint}
                      disabled={busy()}
                    />
                  </label>
                  <label class="field">
                    <span>{t().notes}</span>
                    <textarea
                      name="description"
                      rows="4"
                      maxlength="8000"
                      value={existing?.description ?? ""}
                      placeholder={t().notesHint}
                      disabled={busy()}
                    />
                  </label>
                  <fieldset class="color-picker">
                    <legend>{t().color}</legend>
                    <For each={colors}>
                      {(item) => (
                        <label
                          class={`color-choice color-${item}`}
                          title={t()[item]}
                        >
                          <input
                            type="radio"
                            name="color"
                            value={item}
                            checked={(existing?.color ?? "blue") === item}
                            disabled={busy()}
                          />
                          <span>{t()[item]}</span>
                        </label>
                      )}
                    </For>
                  </fieldset>
                  <Show when={existing}>
                    <p class="field-hint">
                      {t().updated}:{" "}
                      {formatDate(new Date(existing!.updated_at), {
                        dateStyle: "medium",
                        timeStyle: "short",
                      })}
                    </p>
                  </Show>
                  <Show when={formError()}>
                    <div class="form-error" role="alert">
                      {errorText(formError())}
                    </div>
                  </Show>
                  <Show when={deleting()}>
                    <section class="delete-confirm">
                      <h3>{t().deleteConfirm}</h3>
                      <p>{t().deleteHint}</p>
                      <div>
                        <button
                          type="button"
                          class="button"
                          disabled={busy()}
                          onClick={() => setDeleting(false)}
                        >
                          {t().keep}
                        </button>
                        <button
                          type="button"
                          class="button danger"
                          disabled={busy()}
                          onClick={() => void remove()}
                        >
                          {pending() === "delete" ? t().deleting : t().delete}
                        </button>
                      </div>
                    </section>
                  </Show>
                </div>
                <footer class="editor-actions">
                  <Show when={existing && !deleting()}>
                    <button
                      type="button"
                      class="text-button delete-link"
                      disabled={busy()}
                      onClick={() => {
                        setDeleting(true);
                        setFormError(null);
                      }}
                    >
                      {t().delete}
                    </button>
                  </Show>
                  <div>
                    <button
                      type="button"
                      class="button"
                      disabled={busy()}
                      onClick={closeEditor}
                    >
                      {t().cancel}
                    </button>
                    <button
                      type="submit"
                      class="button primary"
                      disabled={busy() || deleting()}
                    >
                      {pending() === "save" ? t().saving : t().save}
                    </button>
                  </div>
                </footer>
              </form>
            );
          }}
        </Show>
      </dialog>
    </div>
  );
}
